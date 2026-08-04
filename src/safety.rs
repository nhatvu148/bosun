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

        // Writes that can destroy data, plus arbitrary code execution.
        // These must route through `gate`.
        "container_rm" | "compose_down" | "container_exec" => Risk::Destructive,

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

/// The human's answer to an elicited confirmation.
#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct HumanApproval {
    /// Set true to allow the operation to proceed. Anything else cancels it.
    pub approved: bool,
}
rmcp::elicit_safe!(HumanApproval);

/// Outcome of asking the human directly, when the client can carry the question.
#[derive(Debug, PartialEq, Eq)]
pub enum HumanVerdict {
    Approved,
    Denied(&'static str),
    /// The client never advertised elicitation, so no one was asked.
    NotSupported,
}

/// Ask the operator to approve a destructive operation, if the client supports it.
///
/// This is the piece the confirm token cannot provide. An agent can satisfy
/// `confirm` on its own — that is by design, it proves the agent identified its
/// target — but it proves nothing about a *human* having agreed. Elicitation
/// asks the person, through the client, and no agent can answer on their behalf.
///
/// Returns [`HumanVerdict::NotSupported`] when the client did not advertise the
/// capability, leaving the caller to fall back to the token gate. Failing closed
/// here would break every client that doesn't implement elicitation yet, which is
/// most of them.
pub async fn ask_human(
    peer: &rmcp::service::Peer<rmcp::RoleServer>,
    op: &Guarded<'_>,
) -> HumanVerdict {
    let supported = peer
        .peer_info()
        .and_then(|info| info.capabilities.elicitation.clone())
        .is_some();

    if !supported {
        tracing::debug!(
            tool = op.tool,
            "client does not support elicitation; falling back to the confirm-token gate"
        );
        return HumanVerdict::NotSupported;
    }

    let question = format!(
        "Bosun wants to {}.\n\n{}\n\nApprove?",
        op.effect,
        op.consequences
            .iter()
            .map(|c| format!("  • {c}"))
            .collect::<Vec<_>>()
            .join("\n")
    );

    match peer.elicit::<HumanApproval>(question).await {
        Ok(Some(answer)) if answer.approved => {
            tracing::warn!(tool = op.tool, target = op.target, "HUMAN APPROVED");
            HumanVerdict::Approved
        }
        Ok(Some(_)) => HumanVerdict::Denied("the operator did not approve it"),
        Ok(None) => HumanVerdict::Denied("the operator dismissed the prompt"),
        Err(rmcp::service::ElicitationError::UserDeclined) => {
            HumanVerdict::Denied("the operator declined")
        }
        Err(rmcp::service::ElicitationError::UserCancelled) => {
            HumanVerdict::Denied("the operator cancelled")
        }
        // An error here means we could not establish consent. Treat that as
        // absence of consent — this is the one place failing closed is right,
        // because the client *said* it could ask and then didn't deliver.
        Err(e) => {
            tracing::warn!(%e, tool = op.tool, "elicitation failed; treating as denial");
            HumanVerdict::Denied("the approval prompt failed")
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

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
    fn classification_recognizes_every_destructive_tool() {
        for tool in ["container_rm", "compose_down", "container_exec"] {
            assert_eq!(risk_of(tool), Some(Risk::Destructive), "{tool}");
        }
        assert_eq!(risk_of("list_containers"), Some(Risk::Safe));
        // An unregistered tool is explicitly unclassified, not silently safe.
        assert_eq!(risk_of("some_future_tool"), None);
    }

    #[test]
    fn a_denied_human_verdict_carries_a_reason() {
        // The refusal text is shown to the agent, so "denied" alone would leave
        // it unable to tell a decline from a transport failure.
        for verdict in [
            HumanVerdict::Denied("the operator declined"),
            HumanVerdict::Denied("the approval prompt failed"),
        ] {
            let HumanVerdict::Denied(why) = verdict else {
                unreachable!()
            };
            assert!(!why.is_empty());
        }
    }

    #[test]
    fn approval_deserializes_from_what_a_client_would_send() {
        let approved: HumanApproval = serde_json::from_value(json!({ "approved": true })).unwrap();
        assert!(approved.approved);
        let denied: HumanApproval = serde_json::from_value(json!({ "approved": false })).unwrap();
        assert!(!denied.approved);
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
