//! MCP tool surface (HANDOFF §4).
//!
//! Tools are split across modules by kind — bounded reads, guarded actions,
//! deterministic diagnostics, compose — each contributing its own
//! `#[tool_router]` impl block on [`crate::server::BosunServer`]. The routers are
//! merged in `BosunServer::new`.

pub mod actions;
pub mod compose;
pub mod diagnose;
pub mod read;

use rmcp::model::{CallToolResult, ContentBlock};

/// A tool failure the agent should read and act on, not retry blindly.
pub fn tool_error(message: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(message.into())])
}

/// Turn a bollard error into something an agent can act on.
///
/// bollard's 404 carries the daemon's own message, which is usually good; the
/// value we add is naming the container the caller asked for, since the agent
/// may have passed a stale id from an earlier listing.
pub fn engine_error(context: &str, id: &str, err: bollard::errors::Error) -> CallToolResult {
    let message = match &err {
        bollard::errors::Error::DockerResponseServerError {
            status_code: 404, ..
        } => format!("{context}: no such container or image '{id}'. It may have been removed — re-run list_containers."),
        bollard::errors::Error::DockerResponseServerError {
            status_code: 409,
            message,
        } => format!("{context}: conflict on '{id}': {message}"),
        other => format!("{context} for '{id}': {other}"),
    };
    tracing::debug!(%err, context, id, "engine call failed");
    tool_error(message)
}

/// Parse a caller-supplied `since` value into a unix timestamp.
///
/// Accepts a relative duration (`30s`, `5m`, `2h`, `3d`) because that is how a
/// human describes a debugging window, or a bare unix timestamp for precision.
pub fn parse_since(since: &str, now: i64) -> Result<i64, String> {
    let s = since.trim();
    if s.is_empty() {
        return Err("since must not be empty".into());
    }

    let (value, multiplier) = match s.chars().last() {
        Some('s') => (&s[..s.len() - 1], 1),
        Some('m') => (&s[..s.len() - 1], 60),
        Some('h') => (&s[..s.len() - 1], 3600),
        Some('d') => (&s[..s.len() - 1], 86_400),
        // No suffix: treat as an absolute unix timestamp.
        _ => {
            return s
                .parse::<i64>()
                .map_err(|_| format!("could not parse since='{s}'. Use '30s', '5m', '2h', '3d', or a unix timestamp."));
        }
    };

    let n: i64 = value
        .parse()
        .map_err(|_| format!("could not parse since='{s}'. Use '30s', '5m', '2h', '3d', or a unix timestamp."))?;

    Ok(now.saturating_sub(n.saturating_mul(multiplier)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_000_000_000;

    #[test]
    fn relative_durations_resolve_against_now() {
        assert_eq!(parse_since("30s", NOW).unwrap(), NOW - 30);
        assert_eq!(parse_since("5m", NOW).unwrap(), NOW - 300);
        assert_eq!(parse_since("2h", NOW).unwrap(), NOW - 7200);
        assert_eq!(parse_since("3d", NOW).unwrap(), NOW - 259_200);
    }

    #[test]
    fn a_bare_number_is_an_absolute_timestamp() {
        assert_eq!(parse_since("1700000000", NOW).unwrap(), 1_700_000_000);
    }

    #[test]
    fn garbage_is_rejected_with_a_usable_message() {
        let err = parse_since("yesterday", NOW).unwrap_err();
        assert!(err.contains("30s"), "message should show the accepted forms: {err}");
        assert!(parse_since("", NOW).is_err());
        assert!(parse_since("abcm", NOW).is_err());
    }

    #[test]
    fn whitespace_is_tolerated() {
        assert_eq!(parse_since("  5m  ", NOW).unwrap(), NOW - 300);
    }
}
