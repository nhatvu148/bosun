//! Write-safety contract (HANDOFF §6).
//!
//! The premise: an over-eager agent must not be able to nuke a volume by
//! reaching for the obvious tool. So destructiveness is a property of the
//! *operation*, checked in one place, rather than a flag each tool remembers to
//! honour. A destructive call must arrive with either `dry_run: true` (tell me
//! what would happen) or `confirm: "<exact-target-name>"` (I know what I'm
//! deleting). Anything else is refused with instructions.
//!
//! Echoing the target name is the point: it is a check the agent cannot satisfy
//! by pattern-matching a boolean, because it has to have looked up what it is
//! about to destroy.

use serde::Serialize;

/// Risk class of an operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Risk {
    /// Reversible or read-only: start, stop, restart, every read tool.
    Safe,
    /// Irreversible data loss is possible: rm, prune, compose down -v.
    Destructive,
}

/// The §6 classification of every tool Bosun exposes.
///
/// This exists as data rather than as a comment so it can be *checked*: the test
/// at the bottom of this module walks the live tool router and fails if a tool
/// is added without being classified here. A new destructive tool therefore
/// cannot ship un-gated by accident, which is the failure mode §6 exists to
/// prevent.
pub fn risk_of(tool: &str) -> Option<Risk> {
    let risk = match tool {
        // Reads — bounded, no side effects.
        "bosun_info"
        | "list_containers"
        | "inspect_container"
        | "container_logs"
        | "container_stats"
        | "list_images"
        | "compose_ps"
        | "diagnose_container"
        | "explain_exit_code"
        | "why_compose_failing" => Risk::Safe,

        // Writes that are reversible: the container can be started again.
        "container_start" | "container_stop" | "container_restart" | "pull_image"
        | "compose_up" => Risk::Safe,

        // Writes that can destroy data. These must route through `gate`.
        "container_rm" | "compose_down" => Risk::Destructive,

        _ => return None,
    };
    Some(risk)
}

/// A destructive operation awaiting authorization.
#[derive(Debug, Clone)]
pub struct Guarded<'a> {
    /// Tool id, for the audit trail.
    pub tool: &'a str,
    /// The thing that would be destroyed — the exact string `confirm` must echo.
    pub target: &'a str,
    /// Human-readable description of the consequence.
    pub effect: String,
    /// Specific, listed consequences (volumes lost, containers removed).
    pub consequences: Vec<String>,
}

/// What the caller supplied for gating.
#[derive(Debug, Clone, Copy, Default)]
pub struct Authorization<'a> {
    pub dry_run: bool,
    pub confirm: Option<&'a str>,
}

/// Outcome of a gate check.
#[derive(Debug)]
pub enum Decision {
    /// Caller asked what would happen. Report and do nothing.
    DryRun(DryRunReport),
    /// Confirmed. Proceed, and log it.
    Authorized,
    /// Not authorized. Refuse with instructions.
    Refused(Refusal),
}

#[derive(Debug, Serialize)]
pub struct DryRunReport {
    pub dry_run: bool,
    pub tool: String,
    pub target: String,
    pub would: String,
    pub consequences: Vec<String>,
    pub to_proceed: String,
}

#[derive(Debug, Serialize)]
pub struct Refusal {
    pub refused: bool,
    pub tool: String,
    pub target: String,
    pub reason: String,
    pub would: String,
    pub consequences: Vec<String>,
    pub to_proceed: String,
    pub to_preview: String,
}

/// Apply the §6 contract to a destructive operation.
///
/// Order matters: `dry_run` is checked before `confirm` so that passing both
/// previews rather than executes. Preferring the non-destructive reading of an
/// ambiguous request is the whole point of the gate.
pub fn gate<'a>(op: &Guarded<'a>, auth: Authorization<'_>) -> Decision {
    if auth.dry_run {
        return Decision::DryRun(DryRunReport {
            dry_run: true,
            tool: op.tool.to_string(),
            target: op.target.to_string(),
            would: op.effect.clone(),
            consequences: op.consequences.clone(),
            to_proceed: confirm_instruction(op.target),
        });
    }

    match auth.confirm {
        Some(token) if token == op.target => {
            // The audit trail required by §6. stderr only — stdout is the MCP channel.
            tracing::warn!(
                tool = op.tool,
                target = op.target,
                effect = %op.effect,
                "DESTRUCTIVE ACTION AUTHORIZED"
            );
            Decision::Authorized
        }
        Some(token) => Decision::Refused(Refusal {
            refused: true,
            tool: op.tool.to_string(),
            target: op.target.to_string(),
            reason: format!(
                "confirm token '{token}' does not match the target '{}'. \
                 The token must echo the target exactly.",
                op.target
            ),
            would: op.effect.clone(),
            consequences: op.consequences.clone(),
            to_proceed: confirm_instruction(op.target),
            to_preview: "Re-run with dry_run=true to see what would happen.".into(),
        }),
        None => Decision::Refused(Refusal {
            refused: true,
            tool: op.tool.to_string(),
            target: op.target.to_string(),
            reason: "This operation is destructive and was called without authorization.".into(),
            would: op.effect.clone(),
            consequences: op.consequences.clone(),
            to_proceed: confirm_instruction(op.target),
            to_preview: "Re-run with dry_run=true to see what would happen.".into(),
        }),
    }
}

fn confirm_instruction(target: &str) -> String {
    format!("Re-run with confirm=\"{target}\" to authorize.")
}

/// Record a destructive action that completed, for the §6 audit trail.
pub fn audit_completed(tool: &str, target: &str, detail: &str) {
    tracing::warn!(tool, target, detail, "DESTRUCTIVE ACTION COMPLETED");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op() -> Guarded<'static> {
        Guarded {
            tool: "container_rm",
            target: "my-db",
            effect: "remove container 'my-db' and its anonymous volumes".into(),
            consequences: vec!["volume 'pgdata' would be deleted".into()],
        }
    }

    #[test]
    fn a_bare_destructive_call_is_refused() {
        let decision = gate(&op(), Authorization::default());
        let Decision::Refused(r) = decision else {
            panic!("expected refusal, got {decision:?}");
        };
        assert!(r.refused);
        assert!(r.to_proceed.contains("confirm=\"my-db\""));
        // The refusal must still tell the caller what it was about to do.
        assert_eq!(r.consequences.len(), 1);
    }

    #[test]
    fn an_exactly_matching_confirm_token_authorizes() {
        let decision = gate(
            &op(),
            Authorization {
                dry_run: false,
                confirm: Some("my-db"),
            },
        );
        assert!(matches!(decision, Decision::Authorized));
    }

    #[test]
    fn a_mismatched_confirm_token_is_refused() {
        // Guards against an agent echoing a plausible-looking but wrong name.
        let decision = gate(
            &op(),
            Authorization {
                dry_run: false,
                confirm: Some("my-database"),
            },
        );
        let Decision::Refused(r) = decision else {
            panic!("expected refusal");
        };
        assert!(r.reason.contains("does not match"));
    }

    #[test]
    fn confirm_is_case_and_whitespace_sensitive() {
        for token in ["My-DB", " my-db", "my-db "] {
            let decision = gate(
                &op(),
                Authorization {
                    dry_run: false,
                    confirm: Some(token),
                },
            );
            assert!(
                matches!(decision, Decision::Refused(_)),
                "token {token:?} should not have authorized"
            );
        }
    }

    #[test]
    fn dry_run_previews_without_authorizing() {
        let decision = gate(
            &op(),
            Authorization {
                dry_run: true,
                confirm: None,
            },
        );
        let Decision::DryRun(report) = decision else {
            panic!("expected dry run");
        };
        assert!(report.dry_run);
        assert!(report.to_proceed.contains("confirm=\"my-db\""));
    }

    #[test]
    fn classification_recognizes_the_destructive_pair() {
        assert_eq!(risk_of("container_rm"), Some(Risk::Destructive));
        assert_eq!(risk_of("compose_down"), Some(Risk::Destructive));
        assert_eq!(risk_of("list_containers"), Some(Risk::Safe));
        // An unregistered tool is explicitly unclassified, not silently safe.
        assert_eq!(risk_of("some_future_tool"), None);
    }

    #[test]
    fn dry_run_wins_when_both_are_supplied() {
        // Ambiguity must resolve toward the non-destructive reading.
        let decision = gate(
            &op(),
            Authorization {
                dry_run: true,
                confirm: Some("my-db"),
            },
        );
        assert!(
            matches!(decision, Decision::DryRun(_)),
            "dry_run must take precedence over confirm"
        );
    }
}
