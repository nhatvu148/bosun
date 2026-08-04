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
    pub template: String,
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
pub fn parse_lines(raw: &str, stderr: bool) -> Vec<LogLine> {
    raw.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let (timestamp, text) = split_timestamp(line);
            LogLine {
                timestamp,
                text: text.to_string(),
                stderr,
            }
        })
        .collect()
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
                        template,
                        sample: line.text.clone(),
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

    #[test]
    fn rfc3339_prefix_is_lifted_off_the_message() {
        let parsed = parse_lines("2026-08-04T10:22:33.123456789Z hello world\n", false);
        assert_eq!(parsed.len(), 1);
        assert_eq!(
            parsed[0].timestamp.as_deref(),
            Some("2026-08-04T10:22:33.123456789Z")
        );
        assert_eq!(parsed[0].text, "hello world");
    }

    #[test]
    fn lines_without_a_timestamp_keep_their_full_text() {
        let parsed = parse_lines("hello world\n", true);
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
