//! Tail + cluster-dedup for container logs (docs/ORIGINAL-SPEC.md §5).
//!
//! The problem this solves: a crash-looping container emits the same stacktrace
//! five hundred times. Relaying that verbatim burns a context window to say one
//! thing. So we normalize each line into a *skeleton* — numbers, timestamps,
//! UUIDs and hex ids replaced by placeholders — and group lines that share one.
//! "500 identical stacktraces" becomes one cluster with `count: 500` and a
//! first/last-seen window.
//!
//! Normalization is hand-rolled rather than regex-driven: it is a single pass
//! over each line, it adds no dependency, and the placeholder set is small
//! enough to keep in your head when reading a template.

use std::collections::HashMap;

/// Cap on the text kept per cluster, applied to both `sample` and `template`.
///
/// One pathological line — a serialized request body, an HTTP body logged on
/// error — could otherwise dominate a whole digest. A hostile fixture emitting a
/// single 64 KB line proved this needs to cover *both* fields: capping only the
/// sample left a 16 KB template that was 91% of the response.
///
/// The template is truncated on the way *out*, never before it is used as the
/// grouping key — truncating the key would make two different long lines
/// collide and report as one cluster, which is worse than a large payload.
const MAX_CLUSTER_TEXT_CHARS: usize = 200;

/// Severity parsed out of the line text itself.
///
/// Container logs have no structured level field — this is a text heuristic and
/// is reported as such. It exists to let `level` filtering and diagnostics focus
/// on the lines that usually matter, not to be authoritative.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
    Fatal,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Trace => "trace",
            Level::Debug => "debug",
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Error => "error",
            Level::Fatal => "fatal",
        }
    }

    /// Parse a caller-supplied minimum level filter.
    pub fn parse(s: &str) -> Option<Level> {
        match s.trim().to_ascii_lowercase().as_str() {
            "trace" => Some(Level::Trace),
            "debug" => Some(Level::Debug),
            "info" => Some(Level::Info),
            "warn" | "warning" => Some(Level::Warn),
            "error" | "err" => Some(Level::Error),
            "fatal" | "critical" | "crit" => Some(Level::Fatal),
            _ => None,
        }
    }
}

/// Infer a level from the line's text.
///
/// Checked most-severe-first so a line mentioning both "error" and "warn" lands
/// on the more alarming one — under-reporting severity is the worse failure here.
pub fn infer_level(line: &str) -> Level {
    let lower = line.to_ascii_lowercase();

    // Bare-word markers that appear in structured and unstructured logs alike.
    const FATAL: [&str; 6] = [
        "fatal",
        "panic:",
        "critical",
        "emerg",
        "segfault",
        "core dumped",
    ];
    const ERROR: [&str; 7] = [
        "error",
        "err!",
        "exception",
        "traceback",
        "failed",
        "failure",
        "[e]",
    ];
    const WARN: [&str; 4] = ["warn", "deprecat", "[w]", "retrying"];
    const DEBUG: [&str; 3] = ["debug", "[d]", "trace"];

    if FATAL.iter().any(|m| lower.contains(m)) {
        return Level::Fatal;
    }
    if ERROR.iter().any(|m| lower.contains(m)) {
        return Level::Error;
    }
    if WARN.iter().any(|m| lower.contains(m)) {
        return Level::Warn;
    }
    if DEBUG.iter().any(|m| lower.contains(m)) {
        return Level::Debug;
    }
    Level::Info
}

/// A group of log lines that share a normalized skeleton.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Cluster {
    /// The normalized skeleton these lines share.
    ///
    /// Omitted when `count == 1`: for a singleton the template is a redacted
    /// duplicate of `sample`, conveying nothing and costing the same. A field
    /// session had 9 of 12 clusters at `count: 1`, so this is most of the
    /// payload on exactly the low-repetition logs where the digest is already
    /// least valuable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
    /// One verbatim line from the group, so the agent sees real values.
    pub sample: String,
    /// How many lines in the scanned window collapsed into this cluster.
    pub count: usize,
    /// Inferred severity (text heuristic — see [`infer_level`]).
    pub level: Level,
    /// Timestamp of the earliest line in this cluster, if timestamps were available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_seen: Option<String>,
    /// Timestamp of the most recent line in this cluster.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<String>,
    /// Whether any line in the cluster came from stderr.
    pub stderr: bool,
}

/// One parsed log line, before clustering.
#[derive(Debug, Clone)]
pub struct LogLine {
    pub timestamp: Option<String>,
    pub text: String,
    pub stderr: bool,
}

/// Split a raw log chunk into lines, lifting off the RFC3339 timestamp prefix
/// that the Engine API prepends when `timestamps=true`.
/// `strip_escapes` should be true for anything that will be clustered or
/// summarized, and false only for `raw=true`, where "raw" means unprocessed and
/// the caller may specifically be asking whether their app emits escapes.
pub fn parse_lines(raw: &str, stderr: bool, strip_escapes: bool) -> Vec<LogLine> {
    raw.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let (timestamp, text) = split_timestamp(line);
            LogLine {
                timestamp,
                // Strip before anything else sees the line. Escapes reaching
                // `normalize()` is what broke clustering in the field — see
                // [`strip_ansi`].
                text: if strip_escapes {
                    strip_ansi(text)
                } else {
                    text.to_string()
                },
                stderr,
            }
        })
        .collect()
}

/// Remove ANSI escape sequences from a log line.
///
/// Any app using coloured structured logging — `tracing`, `zap`, `pino`, most
/// Rust and Go services — emits these, and until this existed they flowed
/// straight into clustering with three separate costs:
///
/// 1. **Payload.** A raw ESC byte costs six characters (`\u001b`) once
///    JSON-encoded, and is paid twice — in `template` and in `sample`. Measured
///    at ~1.2x on a realistic coloured log; worse on densely-coloured lines.
/// 2. **Unreadable templates.** `normalize()` splits on `[`, so an escape is
///    torn apart and its digits placeholdered, yielding `[<NUM>m<NUM>-<NUM>…`.
///    Noise, not a skeleton.
/// 3. **Grouping, in the mixed-colour case.** The *same* message coloured in one
///    place and plain in another hashes apart. That happens when a logger
///    detects a TTY, or when two sources share a stream.
///
/// A field report attributed broken clustering more broadly, to level colouring
/// making "otherwise-identical messages" hash apart. Measurement does not support
/// that: INFO and WARN lines differ in the level word regardless of colour, and
/// uniformly-coloured lines group identically with or without stripping. The
/// tests below record both what stripping does and what it does not.
///
/// Hand-rolled rather than pulling a crate, matching [`normalize`] — the grammar
/// is small and the alternative is a dependency for twenty lines. Handles CSI
/// (`ESC[…`), the ~99% case in logs, plus OSC (`ESC]…`) which some tools use for
/// hyperlinks, and bare two-character sequences.
pub fn strip_ansi(line: &str) -> String {
    if !line.contains('\x1b') {
        // Overwhelmingly the common path; don't allocate for it.
        return line.to_string();
    }

    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();

    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            // CSI: ESC [ params... final-byte in @-~
            Some('[') => {
                chars.next();
                for c in chars.by_ref() {
                    if ('\x40'..='\x7e').contains(&c) {
                        break;
                    }
                }
            }
            // OSC: ESC ] ... terminated by BEL or ST (ESC \)
            Some(']') => {
                chars.next();
                while let Some(c) = chars.next() {
                    if c == '\x07' {
                        break;
                    }
                    if c == '\x1b' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            // Two-character sequences such as ESC c or ESC M.
            Some(_) => {
                chars.next();
            }
            // Trailing lone ESC — drop it.
            None => {}
        }
    }

    out
}

/// Detach a leading RFC3339 timestamp, if present.
///
/// The daemon emits `2026-08-04T10:22:33.123456789Z <message>`. We only treat
/// the first token as a timestamp when it actually looks like one, so a log line
/// that happens to start with a word is left intact.
fn split_timestamp(line: &str) -> (Option<String>, &str) {
    let Some((head, rest)) = line.split_once(' ') else {
        return (None, line);
    };
    if looks_like_rfc3339(head) {
        (Some(head.to_string()), rest)
    } else {
        (None, line)
    }
}

fn looks_like_rfc3339(token: &str) -> bool {
    // Cheapest sufficient check: `NNNN-NN-NNT…` with a trailing zone marker.
    let b = token.as_bytes();
    b.len() >= 20
        && b[0..4].iter().all(u8::is_ascii_digit)
        && b[4] == b'-'
        && b[5..7].iter().all(u8::is_ascii_digit)
        && b[7] == b'-'
        && b[8..10].iter().all(u8::is_ascii_digit)
        && (b[10] == b'T' || b[10] == b't')
}

/// Collapse a line to its skeleton so near-identical lines hash together.
///
/// Placeholders, deliberately few: `<TS>` `<UUID>` `<HEX>` `<IP>` `<NUM>` `<QUOTED>`.
/// Everything else survives verbatim, which is what makes a template readable
/// as a sentence rather than a redaction.
pub fn normalize(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut first = true;

    for token in line.split_whitespace() {
        if !first {
            out.push(' ');
        }
        first = false;
        out.push_str(&normalize_token(token));
    }

    out
}

/// Normalize one whitespace-delimited token.
///
/// Whole-token classification runs first, so a bare `2026-08-04T10:22:33Z` is
/// recognized as a timestamp before any splitting could pull it apart. Only if
/// the token as a whole matches nothing do we split it on structural punctuation
/// and classify the pieces — that is what makes `worker(a1b2c3d4e5)` and
/// `id:a1b2c3d4e5` collapse rather than surviving as unique noise.
fn normalize_token(token: &str) -> String {
    if let Some(placeholder) = classify(token) {
        return placeholder.to_string();
    }

    let mut out = String::with_capacity(token.len());
    let mut segment = String::new();

    for c in token.chars() {
        if is_delimiter(c) {
            if !segment.is_empty() {
                out.push_str(&normalize_segment(&segment));
                segment.clear();
            }
            out.push(c);
        } else {
            segment.push(c);
        }
    }
    if !segment.is_empty() {
        out.push_str(&normalize_segment(&segment));
    }

    out
}

/// Recognize a whole token (or segment) as a known kind of noise.
fn classify(s: &str) -> Option<&'static str> {
    if is_quoted(s) {
        Some("<QUOTED>")
    } else if is_uuid(s) {
        Some("<UUID>")
    } else if is_timestamp_like(s) {
        Some("<TS>")
    } else if is_ip(s) {
        Some("<IP>")
    } else if is_long_hex(s) {
        Some("<HEX>")
    } else {
        None
    }
}

fn normalize_segment(s: &str) -> String {
    match classify(s) {
        Some(placeholder) => placeholder.to_string(),
        None if s.chars().any(|c| c.is_ascii_digit()) => replace_digit_runs(s),
        None => s.to_string(),
    }
}

/// Structural punctuation that separates a value from its surroundings.
///
/// Note `-`, `.` and `/` are absent: they occur *inside* timestamps, IPs, and
/// paths, which are classified as whole tokens above.
fn is_delimiter(c: char) -> bool {
    matches!(
        c,
        '(' | ')' | '[' | ']' | '{' | '}' | ',' | ';' | ':' | '|' | '=' | '@' | '<' | '>'
    )
}

fn is_quoted(s: &str) -> bool {
    s.len() >= 2
        && ((s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')))
}

/// 8-4-4-4-12 hex, the canonical UUID shape.
fn is_uuid(s: &str) -> bool {
    let groups: Vec<&str> = s.split('-').collect();
    groups.len() == 5
        && [8usize, 4, 4, 4, 12]
            .iter()
            .zip(&groups)
            .all(|(len, g)| g.len() == *len && g.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Date-ish or time-ish: has digits plus the separators that only occur in
/// timestamps. Catches `2026-08-04T10:22:33Z`, `10:22:33.123`, `2026/08/04`.
fn is_timestamp_like(s: &str) -> bool {
    let digits = s.chars().filter(char::is_ascii_digit).count();
    if digits < 4 {
        return false;
    }
    let has_date = s.matches('-').count() >= 2 || s.matches('/').count() >= 2;
    let has_time = s.matches(':').count() >= 2;
    let only_ts_chars = s.chars().all(|c| {
        c.is_ascii_digit() || matches!(c, '-' | '/' | ':' | '.' | 'T' | 't' | 'Z' | 'z' | '+')
    });
    only_ts_chars && (has_date || has_time)
}

fn is_ip(s: &str) -> bool {
    let octets: Vec<&str> = s.split('.').collect();
    octets.len() == 4
        && octets
            .iter()
            .all(|o| !o.is_empty() && o.len() <= 3 && o.chars().all(|c| c.is_ascii_digit()))
}

/// Long hex runs are container ids, image digests, request ids — always noise.
/// The 7-char floor is deliberate: it keeps short words like `cafe` and `dead`
/// intact while catching real ids.
fn is_long_hex(s: &str) -> bool {
    let core = s.strip_prefix("0x").unwrap_or(s);
    core.len() >= 7 && core.chars().all(|c| c.is_ascii_hexdigit())
}

fn replace_digit_runs(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_digits = false;
    for c in s.chars() {
        if c.is_ascii_digit() {
            if !in_digits {
                out.push_str("<NUM>");
                in_digits = true;
            }
        } else {
            in_digits = false;
            out.push(c);
        }
    }
    out
}

/// Result of clustering a window of log lines.
#[derive(Debug, serde::Serialize)]
pub struct ClusterSummary {
    /// How many lines we actually looked at.
    pub lines_scanned: usize,
    /// Distinct clusters found before any cap was applied.
    pub distinct_clusters: usize,
    /// The clusters we are returning, most recent last-seen first.
    pub clusters: Vec<Cluster>,
    /// Set when `max_clusters` dropped some groups.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clusters_omitted: Option<usize>,
}

/// Group lines by skeleton and return the most relevant clusters.
///
/// Ranking is by severity first, then recency — an error that happened once
/// matters more than an info line that happened a thousand times, and that is
/// exactly the judgement a bounded view has to make on the caller's behalf.
pub fn cluster(lines: &[LogLine], max_clusters: usize) -> ClusterSummary {
    let mut groups: HashMap<String, Cluster> = HashMap::new();
    let mut order: Vec<String> = Vec::new();

    for line in lines {
        let template = normalize(&line.text);
        let level = infer_level(&line.text);

        match groups.get_mut(&template) {
            Some(existing) => {
                existing.count += 1;
                existing.stderr |= line.stderr;
                if let Some(ts) = &line.timestamp {
                    existing.last_seen = Some(ts.clone());
                    if existing.first_seen.is_none() {
                        existing.first_seen = Some(ts.clone());
                    }
                }
                // Keep the most severe reading we have seen for this skeleton.
                if level > existing.level {
                    existing.level = level;
                }
            }
            None => {
                order.push(template.clone());
                groups.insert(
                    template.clone(),
                    Cluster {
                        // Filled in below, once the final count is known.
                        template: Some(template),
                        sample: crate::bound::project::clip(&line.text, MAX_CLUSTER_TEXT_CHARS),
                        count: 1,
                        level,
                        first_seen: line.timestamp.clone(),
                        last_seen: line.timestamp.clone(),
                        stderr: line.stderr,
                    },
                );
            }
        }
    }

    let distinct_clusters = groups.len();

    // Preserve input order as the recency tiebreak, then sort by severity.
    let mut clusters: Vec<Cluster> = order
        .into_iter()
        .filter_map(|k| groups.remove(&k))
        .collect();
    // Stable sort, so clusters of equal severity keep insertion order — which is
    // recency. Severity first, recency as the tiebreak.
    clusters.sort_by_key(|c| std::cmp::Reverse(c.level));

    let clusters_omitted = distinct_clusters.saturating_sub(max_clusters);
    clusters.truncate(max_clusters);

    // Finalize the templates now that counts are settled and grouping is done.
    // Both steps must happen here rather than at insertion: the full template is
    // the HashMap key, and mutating it earlier would change what groups with what.
    for cluster in &mut clusters {
        if cluster.count == 1 {
            // For a group of one the template is a placeholdered copy of the
            // sample sitting right beside it.
            cluster.template = None;
        } else if let Some(template) = &cluster.template {
            cluster.template = Some(crate::bound::project::clip(
                template,
                MAX_CLUSTER_TEXT_CHARS,
            ));
        }
    }

    ClusterSummary {
        lines_scanned: lines.len(),
        distinct_clusters,
        clusters,
        clusters_omitted: (clusters_omitted > 0).then_some(clusters_omitted),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(text: &str) -> LogLine {
        LogLine {
            timestamp: None,
            text: text.to_string(),
            stderr: false,
        }
    }

    /// What ANSI stripping actually buys, measured rather than assumed.
    ///
    /// A field report attributed broken clustering to level colouring — the
    /// claim being that INFO and WARN escapes make identical messages hash
    /// apart. That is not what happens: those messages differ in the level word
    /// itself, and identically-coloured lines group fine either way.
    ///
    /// The real grouping win is narrower: the *same* message coloured in one
    /// place and not another, which happens when a logger detects a TTY or when
    /// two sources share a stream.
    #[test]
    fn stripping_groups_a_message_that_is_sometimes_coloured() {
        let coloured = LogLine {
            timestamp: None,
            text: strip_ansi("\x1b[31mconnection refused\x1b[0m"),
            stderr: false,
        };
        let plain = LogLine {
            timestamp: None,
            text: strip_ansi("connection refused"),
            stderr: false,
        };

        let summary = cluster(&[coloured, plain], 10);
        assert_eq!(
            summary.distinct_clusters, 1,
            "the same message must group whether or not it arrived coloured"
        );
    }

    /// Identically-coloured lines already grouped before stripping existed. This
    /// pins that stripping did not regress the common case.
    #[test]
    fn identically_coloured_lines_still_group() {
        let mk = |pct: &str| LogLine {
            timestamp: None,
            text: strip_ansi(&format!("\x1b[33m WARN\x1b[0m disk usage at {pct}%")),
            stderr: false,
        };
        let summary = cluster(&[mk("91"), mk("93")], 10);
        assert_eq!(summary.distinct_clusters, 1);
    }

    /// Guards the property a field report said was violated: on a
    /// high-cardinality, long-line, ANSI-coloured log, the digest came back
    /// roughly twice the size of the raw lines it was summarizing.
    ///
    /// The inversion does not reproduce against the current code — with
    /// singleton templates dropped and samples capped, the digest stays ~2.5x
    /// smaller even at worse cardinality than reported (44 groups from 54 lines
    /// versus their 34). The most plausible original cause is the one now fixed:
    /// an uncapped `sample` plus a near-duplicate `template`, which on long
    /// lines cost twice per cluster what the raw line cost once.
    ///
    /// Kept as a regression test so that if the digest ever inverts again, it
    /// fails here rather than in someone's incident.
    #[test]
    fn a_digest_never_costs_more_than_the_lines_it_summarizes() {
        // Match the reported shape: 54 lines yielding ~34 distinct groups, so
        // most clusters are singletons — the case where the digest is least
        // able to earn its keep.
        //
        // Distinctness has to come from different WORDS. Numbered variants of
        // one sentence collapse to a single cluster, correctly, because that is
        // what normalization is for.
        const VERBS: [&str; 9] = [
            "loaded",
            "registered",
            "compiled",
            "hydrated",
            "verified",
            "reclaimed",
            "bound",
            "applied",
            "scraped",
        ];
        const NOUNS: [&str; 9] = [
            "configuration",
            "endpoint",
            "template",
            "cache",
            "certificate",
            "index",
            "listener",
            "migration",
            "exporter",
        ];
        let mut raw = Vec::new();
        for i in 0..54usize {
            // 20 lines drawn from 10 shapes (each twice), 34 unique after that.
            let shape = if i < 20 { i / 2 } else { i - 10 };
            // Long lines, as real structured logs are — key/value context
            // trailing every message. This is the variable that actually
            // inverted the digest in the field: `sample` was unbounded and
            // `template` a near-duplicate, so each cluster cost twice a long
            // line while raw cost it once.
            let msg = format!(
                "{} the {} for subsystem {} \
                 request_id=abc123 trace_id=def456 span=789 user=someone@example.com \
                 path=/api/v1/resource/sub method=POST status=200 duration_ms=42",
                VERBS[shape % VERBS.len()],
                NOUNS[(shape / VERBS.len()) % NOUNS.len()],
                (b'a' + (shape / (VERBS.len() * NOUNS.len())) as u8) as char
            );
            raw.push(format!(
                "\x1b[2m2026-08-05T12:47:{:02}.545204Z\x1b[0m \x1b[33m WARN\x1b[0m {msg}",
                i % 60
            ));
        }
        let raw_bytes: usize = raw.iter().map(|l| l.len()).sum();

        let build = |strip: bool| {
            let lines: Vec<LogLine> = raw
                .iter()
                .map(|t| LogLine {
                    timestamp: None,
                    text: if strip { strip_ansi(t) } else { t.clone() },
                    stderr: false,
                })
                .collect();
            let summary = cluster(&lines, 12);
            let json = serde_json::to_string(&summary.clusters).unwrap();
            (summary, json.len())
        };

        // What the raw lines cost once JSON-encoded, which is how they would
        // actually reach the agent.
        let raw_json = serde_json::to_string(&raw).unwrap().len();

        let (kept_summary, kept_bytes) = build(false);
        let (stripped_summary, stripped_bytes) = build(true);

        // The invariant the field report says was violated: a digest must never
        // cost more than the lines it summarizes. If this fails, the tool is
        // inverted and the bounded view has become the expensive one.
        assert!(
            stripped_bytes < raw_json,
            "digest ({stripped_bytes} B) must be smaller than the raw lines it \
             summarizes ({raw_json} B) — {} distinct clusters from {} lines",
            stripped_summary.distinct_clusters,
            raw.len()
        );

        // Stripping is worth real payload, mostly because a raw ESC byte costs
        // six characters (`\\u001b`) once JSON-encoded, and is paid in both
        // `template` and `sample`.
        assert!(
            kept_bytes > stripped_bytes,
            "stripping should shrink the payload: {kept_bytes} -> {stripped_bytes}"
        );

        // Recorded deliberately: ANSI did NOT change the cluster count here.
        // The field report attributed broken clustering to colour, and on this
        // shape that is not the mechanism — grouping is identical either way.
        // Colour matters for payload size and template readability, and for the
        // narrower mixed-colour case covered by its own test.
        assert_eq!(
            kept_summary.distinct_clusters, stripped_summary.distinct_clusters,
            "cluster count should not depend on colour for uniformly-coloured logs"
        );

        let _ = raw_bytes;
    }

    #[test]
    fn ansi_escapes_are_removed_without_touching_the_message() {
        assert_eq!(strip_ansi("\x1b[33m WARN\x1b[0m boom"), " WARN boom");
        assert_eq!(strip_ansi("\x1b[1;31merror\x1b[0m"), "error");
        // OSC hyperlink, as some tools emit.
        assert_eq!(strip_ansi("\x1b]8;;http://x\x07link\x1b]8;;\x07"), "link");
        // Lines without escapes must pass through untouched and unallocated.
        assert_eq!(strip_ansi("plain line"), "plain line");
        // A lone trailing ESC must not panic or leak.
        assert_eq!(strip_ansi("trailing\x1b"), "trailing");
    }

    #[test]
    fn stripping_makes_templates_readable_again() {
        // Before: normalize() split on '[' inside the escape and placeholdered
        // its digits, yielding "[<NUM>m<NUM>-<NUM>-<NUM>T..." — noise, not a
        // skeleton.
        let raw = "\x1b[2m2026-08-05T12:47:53Z\x1b[0m served request 42";
        let template = normalize(&strip_ansi(raw));
        assert!(!template.contains('\x1b'), "escape survived: {template}");
        assert!(template.contains("served request"), "got: {template}");
        assert!(
            !template.contains("<NUM>m"),
            "escape was placeholdered: {template}"
        );
    }

    #[test]
    fn a_singleton_cluster_omits_its_redundant_template() {
        let summary = cluster(&[line("a one-off message")], 10);
        assert_eq!(summary.clusters[0].count, 1);
        assert!(
            summary.clusters[0].template.is_none(),
            "a template for a group of one is a redacted copy of the sample"
        );
        // The sample still carries the information.
        assert_eq!(summary.clusters[0].sample, "a one-off message");
    }

    #[test]
    fn a_grouped_cluster_keeps_its_template() {
        let lines = vec![line("req 1 ok"), line("req 2 ok")];
        let summary = cluster(&lines, 10);
        assert_eq!(summary.clusters[0].count, 2);
        assert!(summary.clusters[0].template.is_some());
    }

    /// A hostile fixture emitting one 64 KB line found that capping only the
    /// sample left a 16 KB *template* — 91% of the response. Both fields have to
    /// be capped, so this checks both, and checks a grouped cluster rather than
    /// a singleton, since singletons drop the template anyway and would hide it.
    #[test]
    fn one_enormous_line_cannot_dominate_the_digest() {
        let huge = format!("payload {}", "x".repeat(64_000));
        let summary = cluster(&[line(&huge), line(&huge)], 10);
        let c = &summary.clusters[0];

        assert_eq!(c.count, 2, "must be a grouped cluster to exercise template");
        assert!(
            c.sample.chars().count() < 250,
            "sample was {}",
            c.sample.chars().count()
        );
        assert!(c.sample.ends_with("(clipped)"));

        let template = c
            .template
            .as_ref()
            .expect("a grouped cluster keeps its template");
        assert!(
            template.chars().count() < 250,
            "template was {}",
            template.chars().count()
        );
        assert!(template.ends_with("(clipped)"));
    }

    /// Truncation must happen on the way out, never to the grouping key: two
    /// different long lines sharing a 200-char prefix must stay separate.
    #[test]
    fn capping_the_template_does_not_make_long_lines_collide() {
        let prefix = "x".repeat(400);
        let a = format!("{prefix} alpha branch taken");
        let b = format!("{prefix} bravo branch taken");

        let summary = cluster(&[line(&a), line(&a), line(&b), line(&b)], 10);
        assert_eq!(
            summary.distinct_clusters, 2,
            "lines sharing a long prefix must not be merged by truncation"
        );
    }

    #[test]
    fn identical_stacktraces_collapse_to_one_cluster() {
        // The motivating case from docs/ORIGINAL-SPEC.md §5.
        let lines: Vec<LogLine> = (0..500)
            .map(|i| line(&format!("ERROR connection refused to db after {i}ms")))
            .collect();

        let summary = cluster(&lines, 10);

        assert_eq!(summary.lines_scanned, 500);
        assert_eq!(summary.distinct_clusters, 1);
        assert_eq!(summary.clusters[0].count, 500);
        assert_eq!(summary.clusters[0].level, Level::Error);
    }

    #[test]
    fn numbers_uuids_hex_and_ips_are_normalized_away() {
        let a = normalize("req 550e8400-e29b-41d4-a716-446655440000 from 10.0.0.7 took 42ms");
        let b = normalize("req 6ba7b810-9dad-11d1-80b4-00c04fd430c8 from 192.168.1.1 took 9001ms");
        assert_eq!(a, b);
        assert!(a.contains("<UUID>"), "template was: {a}");
        assert!(a.contains("<IP>"), "template was: {a}");
        assert!(a.contains("<NUM>ms"), "template was: {a}");
    }

    #[test]
    fn short_hex_like_words_survive_normalization() {
        // "failed" is hex-ish but must stay readable; only 7+ char runs collapse.
        let t = normalize("cafe beef failed a1b2c3d4e5f6");
        assert!(t.starts_with("cafe beef failed"), "template was: {t}");
        assert!(t.ends_with("<HEX>"), "template was: {t}");
    }

    #[test]
    fn ids_embedded_in_punctuation_still_match() {
        // The common shapes: bracketed, and key:value. Both must collapse, or a
        // per-request id makes every line look unique and clustering does nothing.
        let a = normalize("worker(a1b2c3d4e5) started");
        let b = normalize("worker(f9e8d7c6b5) started");
        assert_eq!(a, b);
        assert_eq!(a, "worker(<HEX>) started");

        let c = normalize("req_id:a1b2c3d4e5 done");
        let d = normalize("req_id:f9e8d7c6b5 done");
        assert_eq!(c, d);
        assert_eq!(c, "req_id:<HEX> done");
    }

    #[test]
    fn whole_token_timestamps_survive_the_delimiter_split() {
        // ':' is a delimiter, so a clock time must be recognized as a whole
        // token first or it would shatter into three separate numbers.
        assert_eq!(normalize("2026-08-04T10:22:33Z"), "<TS>");
        assert_eq!(normalize("10:22:33.123"), "<TS>");
    }

    #[test]
    fn urls_keep_their_shape() {
        let a = normalize("GET http://api.internal:8080/v1/users took 12ms");
        let b = normalize("GET http://api.internal:9090/v1/users took 340ms");
        assert_eq!(a, b);
        assert!(
            a.contains("http://api.internal:<NUM>/v<NUM>/users"),
            "got: {a}"
        );
    }

    #[test]
    fn distinct_messages_stay_distinct() {
        let lines = vec![
            line("connection refused"),
            line("disk full"),
            line("connection refused"),
        ];
        let summary = cluster(&lines, 10);
        assert_eq!(summary.distinct_clusters, 2);
    }

    #[test]
    fn errors_outrank_chatty_info_lines_when_capped() {
        let mut lines: Vec<LogLine> = (0..100)
            .map(|i| line(&format!("serving request {i}")))
            .collect();
        lines.push(line("ERROR out of memory"));

        // Only room for one cluster: the error must be the one that survives,
        // even though the info cluster is 100x more frequent.
        let summary = cluster(&lines, 1);
        assert_eq!(summary.clusters.len(), 1);
        assert_eq!(summary.clusters[0].level, Level::Error);
        assert_eq!(summary.clusters_omitted, Some(1));
    }

    #[test]
    fn first_and_last_seen_span_the_cluster() {
        let lines = vec![
            LogLine {
                timestamp: Some("2026-08-04T10:00:00Z".into()),
                text: "boom 1".into(),
                stderr: false,
            },
            LogLine {
                timestamp: Some("2026-08-04T10:05:00Z".into()),
                text: "boom 2".into(),
                stderr: false,
            },
        ];
        let summary = cluster(&lines, 10);
        assert_eq!(summary.clusters[0].count, 2);
        assert_eq!(
            summary.clusters[0].first_seen.as_deref(),
            Some("2026-08-04T10:00:00Z")
        );
        assert_eq!(
            summary.clusters[0].last_seen.as_deref(),
            Some("2026-08-04T10:05:00Z")
        );
    }

    /// A fixture cannot deliver invalid UTF-8 through the json-file driver — the
    /// daemon sanitizes at write time, because it stores entries as JSON and
    /// JSON demands valid UTF-8. Raw `0xff 0xfe 0xfd 0xfc` comes back as U+FFFD
    /// ×4. So the lossy path is exercised here instead, at the level where the
    /// bytes actually become a string.
    #[test]
    fn replacement_characters_normalize_cluster_and_serialize_cleanly() {
        // What `String::from_utf8_lossy` produces from invalid bytes.
        let lossy = String::from_utf8_lossy(b"binary follows: \xff\xfe\xfd\xfc done").into_owned();
        assert!(
            lossy.contains('\u{fffd}'),
            "expected replacement chars: {lossy:?}"
        );

        let lines: Vec<LogLine> = (0..3)
            .map(|_| LogLine {
                timestamp: None,
                text: lossy.clone(),
                stderr: false,
            })
            .collect();

        let summary = cluster(&lines, 10);
        assert_eq!(
            summary.distinct_clusters, 1,
            "identical lossy lines must group"
        );
        assert_eq!(summary.clusters[0].count, 3);

        // The whole point: it must survive serialization, since the digest is
        // handed to the client as JSON.
        let json = serde_json::to_string(&summary.clusters).expect("must serialize");
        assert!(serde_json::from_str::<serde_json::Value>(&json).is_ok());
    }

    /// Invalid bytes mid-line must not swallow the good lines around them.
    #[test]
    fn a_lossy_line_does_not_corrupt_its_neighbours() {
        let lossy = String::from_utf8_lossy(b"\xff\xfe bad").into_owned();
        let lines = vec![
            line("valid line before"),
            line(&lossy),
            line("valid line after"),
        ];

        let summary = cluster(&lines, 10);
        assert_eq!(summary.distinct_clusters, 3);
        assert!(
            summary
                .clusters
                .iter()
                .any(|c| c.sample == "valid line before"),
            "the preceding good line must survive intact"
        );
        assert!(
            summary
                .clusters
                .iter()
                .any(|c| c.sample == "valid line after"),
            "the following good line must survive intact"
        );
    }

    #[test]
    fn rfc3339_prefix_is_lifted_off_the_message() {
        let parsed = parse_lines("2026-08-04T10:22:33.123456789Z hello world\n", false, true);
        assert_eq!(parsed.len(), 1);
        assert_eq!(
            parsed[0].timestamp.as_deref(),
            Some("2026-08-04T10:22:33.123456789Z")
        );
        assert_eq!(parsed[0].text, "hello world");
    }

    #[test]
    fn lines_without_a_timestamp_keep_their_full_text() {
        let parsed = parse_lines("hello world\n", true, true);
        assert_eq!(parsed[0].timestamp, None);
        assert_eq!(parsed[0].text, "hello world");
        assert!(parsed[0].stderr);
    }

    #[test]
    fn level_inference_prefers_the_more_severe_marker() {
        assert_eq!(infer_level("WARN retrying after error"), Level::Error);
        assert_eq!(infer_level("panic: nil dereference"), Level::Fatal);
        assert_eq!(infer_level("listening on port 8080"), Level::Info);
    }

    #[test]
    fn quoted_payloads_collapse() {
        let a = normalize(r#"rejected payload "alpha" "#);
        let b = normalize(r#"rejected payload "bravo" "#);
        assert_eq!(a, b);
        assert!(a.contains("<QUOTED>"));
    }
}
