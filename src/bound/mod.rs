//! Token-bounded I/O — the core design constraint (HANDOFF §5).
//!
//! Design rule of thumb from the spec: *the default response to any Bosun tool
//! should be safe to put in a context window unread.* This module holds the
//! shared machinery for keeping that promise — the final byte cap that applies
//! to every tool result regardless of which tool produced it.

pub mod logs;
pub mod project;

use rmcp::model::CallToolResult;
use serde::Serialize;

/// Hard ceiling on a single tool response, in bytes of serialized JSON.
///
/// ~24 KB is roughly 6k tokens: large enough that no well-behaved bounded tool
/// ever hits it, small enough that hitting it can't wreck a session. It is a
/// backstop for the cases the per-tool caps failed to anticipate, not the
/// primary bounding mechanism.
pub const MAX_RESPONSE_BYTES: usize = 24_000;

/// Serialize a tool result, degrading to a summary if it blew the byte cap.
///
/// The degraded form is deliberately *self-describing*: it says what happened,
/// how big the response was, and which knob to turn. Per §5, the agent should
/// never be surprised by a truncation it can't undo.
pub fn bounded_json<T: Serialize>(value: &T, tool: &str, escape_hint: &str) -> CallToolResult {
    let json = match serde_json::to_string_pretty(value) {
        Ok(json) => json,
        Err(e) => {
            return CallToolResult::error(vec![rmcp::model::ContentBlock::text(format!(
                "failed to serialize {tool} result: {e}"
            ))]);
        }
    };

    if json.len() <= MAX_RESPONSE_BYTES {
        return CallToolResult::success(vec![rmcp::model::ContentBlock::text(json)]);
    }

    tracing::warn!(
        tool,
        bytes = json.len(),
        cap = MAX_RESPONSE_BYTES,
        "response exceeded byte cap, degrading to summary"
    );

    let summary = serde_json::json!({
        "bosun_truncated": true,
        "tool": tool,
        "response_bytes": json.len(),
        "cap_bytes": MAX_RESPONSE_BYTES,
        "reason": "Response exceeded Bosun's per-call byte cap and was withheld to protect the context window.",
        "what_to_do": escape_hint,
    });

    CallToolResult::success(vec![rmcp::model::ContentBlock::text(
        serde_json::to_string_pretty(&summary).unwrap_or_else(|_| "{}".into()),
    )])
}

/// Render a byte count the way a human reads it.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Render a unix timestamp as an approximate age ("3 days ago").
///
/// Deliberately coarse: for "is this image stale?" the order of magnitude is the
/// answer, and a precise duration would just be more characters.
pub fn human_age(created_epoch_secs: i64, now_epoch_secs: i64) -> String {
    let delta = now_epoch_secs.saturating_sub(created_epoch_secs);
    if delta < 0 {
        return "in the future".into();
    }
    const MINUTE: i64 = 60;
    const HOUR: i64 = 60 * MINUTE;
    const DAY: i64 = 24 * HOUR;
    const WEEK: i64 = 7 * DAY;
    const MONTH: i64 = 30 * DAY;
    const YEAR: i64 = 365 * DAY;

    match delta {
        d if d < MINUTE => "just now".into(),
        d if d < HOUR => format!("{} minutes ago", d / MINUTE),
        d if d < DAY => format!("{} hours ago", d / HOUR),
        d if d < WEEK => format!("{} days ago", d / DAY),
        d if d < MONTH => format!("{} weeks ago", d / WEEK),
        d if d < YEAR => format!("{} months ago", d / MONTH),
        d => format!("{} years ago", d / YEAR),
    }
}

/// Pull the text body out of a tool result, for assertions.
#[cfg(test)]
pub(crate) fn result_text(result: &CallToolResult) -> &str {
    match &result.content[0] {
        rmcp::model::ContentBlock::Text(t) => &t.text,
        other => panic!("expected text content, got {other:?}"),
    }
}

/// Parse a tool result's JSON body, for assertions.
#[cfg(test)]
pub(crate) fn result_json(result: &CallToolResult) -> serde_json::Value {
    serde_json::from_str(result_text(result)).expect("tool result should be valid JSON")
}

/// Seconds since the unix epoch, for age calculations.
pub fn now_epoch_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_responses_degrade_instead_of_flooding_context() {
        // A payload well past the cap must come back as a self-describing
        // summary, not as truncated-and-unparseable JSON.
        let huge = vec!["x".repeat(1000); 100];
        let result = bounded_json(&huge, "test_tool", "pass tail=20");
        let parsed = result_json(&result);

        assert_eq!(parsed["bosun_truncated"], true);
        assert_eq!(parsed["tool"], "test_tool");
        assert_eq!(parsed["what_to_do"], "pass tail=20");
        assert!(result_text(&result).len() < MAX_RESPONSE_BYTES);
    }

    #[test]
    fn responses_under_the_cap_pass_through_verbatim() {
        let small = serde_json::json!({ "hello": "world" });
        let result = bounded_json(&small, "test_tool", "n/a");
        let parsed = result_json(&result);

        assert_eq!(parsed["hello"], "world");
        assert!(parsed.get("bosun_truncated").is_none());
    }

    #[test]
    fn byte_rendering_switches_units() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1024), "1.0 KB");
        assert_eq!(human_bytes(1_048_576), "1.0 MB");
        assert_eq!(human_bytes(1_073_741_824), "1.0 GB");
    }

    #[test]
    fn age_rendering_picks_a_sensible_granularity() {
        let now = 1_000_000_000;
        assert_eq!(human_age(now - 30, now), "just now");
        assert_eq!(human_age(now - 300, now), "5 minutes ago");
        assert_eq!(human_age(now - 7200, now), "2 hours ago");
        assert_eq!(human_age(now - 172_800, now), "2 days ago");
        assert_eq!(human_age(now - 31_536_000, now), "1 years ago");
    }

    #[test]
    fn clock_skew_does_not_produce_nonsense() {
        assert_eq!(human_age(2000, 1000), "in the future");
    }
}
