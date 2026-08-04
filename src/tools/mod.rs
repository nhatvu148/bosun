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

/// The full authorization path for a destructive operation, shared by every
/// tool that has one so they cannot drift apart.
///
/// Order, and why:
/// 1. `dry_run` — nothing will happen, so there is nothing to approve.
/// 2. Ask the operator, if the client supports elicitation. This is the only
///    check an agent cannot satisfy by itself, so it outranks the token; once a
///    human has said yes, demanding a token too would be asking twice.
/// 3. Otherwise fall back to the §6 confirm token.
///
/// `Ok(())` means proceed. `Err(result)` is the response to return unchanged.
pub async fn authorize(
    peer: &rmcp::service::Peer<rmcp::RoleServer>,
    op: &crate::safety::Guarded<'_>,
    dry_run: bool,
    confirm: Option<&str>,
) -> Result<(), CallToolResult> {
    use crate::safety::{Authorization, Decision, HumanVerdict, ask_human, gate};

    if dry_run {
        let Decision::DryRun(report) = gate(
            op,
            Authorization {
                dry_run: true,
                confirm: None,
            },
        ) else {
            unreachable!("dry_run always yields a preview");
        };
        return Err(crate::bound::bounded_json(
            &report,
            op.tool,
            "Unexpectedly large — report this.",
        ));
    }

    match ask_human(peer, op).await {
        HumanVerdict::Approved => Ok(()),
        HumanVerdict::Denied(why) => Err(tool_error(format!(
            "{} on '{}' was not run because {why}.",
            op.tool, op.target
        ))),
        HumanVerdict::NotSupported => match gate(
            op,
            Authorization {
                dry_run: false,
                confirm,
            },
        ) {
            Decision::Authorized => Ok(()),
            Decision::Refused(refusal) => Err(crate::bound::bounded_json(
                &refusal,
                op.tool,
                "Unexpectedly large — report this.",
            )),
            Decision::DryRun(_) => unreachable!("dry_run handled above"),
        },
    }
}

/// Resolve the container selector shared by the batch-capable read tools.
///
/// Accepts a single `id`, an explicit `ids` list, or the literal `"*"` meaning
/// every container. The wildcard is the point: a "how is everything doing"
/// question otherwise costs one tool call per container, and the call count —
/// not the bytes per call — is what dominates that shape of request.
pub async fn resolve_ids(
    engine: &crate::engine::client::EngineClient,
    id: Option<&str>,
    ids: &[String],
    include_stopped: bool,
) -> Result<Vec<String>, String> {
    let requested: Vec<&str> = if !ids.is_empty() {
        ids.iter().map(String::as_str).collect()
    } else if let Some(one) = id {
        vec![one]
    } else {
        return Err("pass either id=\"<name>\" or ids=[\"a\",\"b\"], or ids=[\"*\"] for all containers".into());
    };

    if !requested.contains(&"*") {
        return Ok(requested.into_iter().map(str::to_string).collect());
    }

    let containers = engine
        .docker()
        .list_containers(Some(
            bollard::query_parameters::ListContainersOptionsBuilder::new()
                .all(include_stopped)
                .build(),
        ))
        .await
        .map_err(|e| format!("could not expand ids=[\"*\"]: {e}"))?;

    let mut names: Vec<String> = containers
        .iter()
        .filter_map(|c| {
            c.names
                .as_deref()
                .unwrap_or_default()
                .first()
                .map(|n| crate::bound::project::strip_leading_slash(n))
                .or_else(|| c.id.clone())
        })
        .collect();
    names.sort();
    Ok(names)
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
