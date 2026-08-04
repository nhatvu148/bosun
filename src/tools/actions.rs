//! Action tools (HANDOFF §4 "Actions — guarded", M2).
//!
//! Lifecycle verbs are reversible and allowed directly. Removal and exec are
//! destructive and routed through [`crate::safety`], which requires a `dry_run`
//! preview or a `confirm` token echoing the target before anything happens.
//!
//! ## Why `container_exec` exists after all
//!
//! HANDOFF §11 leaned toward leaving exec out of v1, and it shipped that way.
//! First real session, the agent hit the gap and reached for
//! `Bash(docker exec …)` instead — which is the observation that changes the
//! calculus. **Omitting exec did not prevent exec.** It pushed the agent to an
//! unbounded, unaudited, ungated path that Bosun cannot see.
//!
//! A gated exec is strictly safer than the fallback the omission was creating,
//! so it is here: argv-only (never a shell string), output-bounded, timeout-
//! enforced, and classified `Destructive` so every call goes through §6 — which
//! is exactly what §6 itself anticipated when it listed "maybe `exec`" among
//! the destructive tools.

use bollard::exec::{CreateExecOptions, StartExecOptions, StartExecResults};
use bollard::query_parameters::{
    CreateImageOptionsBuilder, InspectContainerOptions, RemoveContainerOptionsBuilder,
    RestartContainerOptions, StartContainerOptions, StopContainerOptionsBuilder,
};
use futures_util::StreamExt;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::bound::bounded_json;
use crate::bound::project::{clip, short_id, strip_leading_slash};
use crate::safety::{self, Authorization, Decision, Guarded};
use crate::server::BosunServer;
use crate::tools::{engine_error, tool_error};

/// Default grace period before SIGKILL, matching the Docker CLI.
const DEFAULT_STOP_TIMEOUT: i32 = 10;

/// Default seconds an exec may run before Bosun stops reading and reports a timeout.
const DEFAULT_EXEC_TIMEOUT: u64 = 30;
/// Ceiling on the exec timeout, so a stuck command can't pin the server forever.
const MAX_EXEC_TIMEOUT: u64 = 300;
/// Cap on captured exec output per stream. Exec is bounded like every other read.
const MAX_EXEC_OUTPUT_CHARS: usize = 8_000;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ContainerIdParams {
    /// Container id or name.
    pub id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct StopContainerParams {
    /// Container id or name.
    pub id: String,
    /// Seconds to wait for graceful shutdown before SIGKILL. Default 10.
    #[serde(default)]
    pub timeout: Option<i32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RemoveContainerParams {
    /// Container id or name.
    pub id: String,
    /// Remove a running container by killing it first. Never defaulted on.
    #[serde(default)]
    pub force: bool,
    /// Also remove anonymous volumes attached to the container. Data loss.
    #[serde(default)]
    pub volumes: bool,
    /// Preview what would happen without removing anything.
    #[serde(default)]
    pub dry_run: bool,
    /// Authorization token — must exactly equal the container's name.
    #[serde(default)]
    pub confirm: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PullImageParams {
    /// Image reference, e.g. 'nginx:1.27' or 'ghcr.io/org/app:sha-abc123'.
    pub image: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExecParams {
    /// Container id or name.
    pub id: String,
    /// Command as an argv array, e.g. ["ls", "-la", "/app"]. NOT a shell string —
    /// nothing here is interpreted by a shell. For shell features (pipes,
    /// globs, redirection) pass them explicitly: ["sh", "-c", "ls /app | head"].
    pub cmd: Vec<String>,
    /// Seconds before the command is abandoned. Default 30, max 300.
    #[serde(default)]
    pub timeout: Option<u64>,
    /// Working directory inside the container.
    #[serde(default)]
    pub workdir: Option<String>,
    /// User to run as, e.g. 'root' or '1000:1000'.
    #[serde(default)]
    pub user: Option<String>,
    /// Preview the exact argv that would run, without running it.
    #[serde(default)]
    pub dry_run: bool,
    /// Authorization token — must exactly equal the container's name.
    #[serde(default)]
    pub confirm: Option<String>,
}

#[tool_router(router = actions_router, vis = "pub(crate)")]
impl BosunServer {
    /// Start a stopped container.
    #[tool(
        name = "container_start",
        description = "Start a stopped container. Low-risk and reversible — no confirmation required. \
                       Returns the container's state after starting.",
        annotations(title = "Start container", destructive_hint = false, idempotent_hint = true)
    )]
    pub async fn container_start(
        &self,
        Parameters(params): Parameters<ContainerIdParams>,
    ) -> CallToolResult {
        if let Err(e) = self
            .engine()
            .docker()
            .start_container(&params.id, None::<StartContainerOptions>)
            .await
        {
            return engine_error("container_start failed", &params.id, e);
        }
        tracing::info!(id = %params.id, "container started");
        self.state_after(&params.id, "started").await
    }

    /// Stop a running container with a grace period.
    #[tool(
        name = "container_stop",
        description = "Stop a running container, waiting `timeout` seconds (default 10) for graceful \
                       shutdown before SIGKILL. Low-risk and reversible — no confirmation required. \
                       Returns the container's state after stopping, including its exit code.",
        annotations(title = "Stop container", destructive_hint = false, idempotent_hint = true)
    )]
    pub async fn container_stop(
        &self,
        Parameters(params): Parameters<StopContainerParams>,
    ) -> CallToolResult {
        let options = StopContainerOptionsBuilder::new()
            .t(params.timeout.unwrap_or(DEFAULT_STOP_TIMEOUT))
            .build();

        if let Err(e) = self
            .engine()
            .docker()
            .stop_container(&params.id, Some(options))
            .await
        {
            return engine_error("container_stop failed", &params.id, e);
        }
        tracing::info!(id = %params.id, "container stopped");
        self.state_after(&params.id, "stopped").await
    }

    /// Restart a container.
    #[tool(
        name = "container_restart",
        description = "Restart a container (stop then start). Low-risk and reversible — no confirmation \
                       required. Note this changes nothing about the container's configuration; to pick up \
                       a new image you must recreate it.",
        annotations(title = "Restart container", destructive_hint = false, idempotent_hint = true)
    )]
    pub async fn container_restart(
        &self,
        Parameters(params): Parameters<ContainerIdParams>,
    ) -> CallToolResult {
        if let Err(e) = self
            .engine()
            .docker()
            .restart_container(&params.id, None::<RestartContainerOptions>)
            .await
        {
            return engine_error("container_restart failed", &params.id, e);
        }
        tracing::info!(id = %params.id, "container restarted");
        self.state_after(&params.id, "restarted").await
    }

    /// Remove a container. Destructive — gated per §6.
    #[tool(
        name = "container_rm",
        description = "Remove a container. DESTRUCTIVE and GATED: this call does nothing unless you pass \
                       either dry_run=true (returns exactly what would be removed, including which volumes \
                       would be deleted) or confirm=\"<container-name>\" echoing the target's name exactly. \
                       force=true kills a running container first; volumes=true also deletes its anonymous \
                       volumes and is irreversible data loss. Neither is ever defaulted on. \
                       Call with dry_run=true first.",
        annotations(title = "Remove container", destructive_hint = true)
    )]
    pub async fn container_rm(
        &self,
        Parameters(params): Parameters<RemoveContainerParams>,
    ) -> CallToolResult {
        // Resolve the container before gating: the confirm token is checked
        // against the real name, and the dry run needs to report real volumes.
        let inspect = match self
            .engine()
            .docker()
            .inspect_container(&params.id, None::<InspectContainerOptions>)
            .await
        {
            Ok(i) => i,
            Err(e) => return engine_error("container_rm failed", &params.id, e),
        };

        let name = strip_leading_slash(inspect.name.as_deref().unwrap_or(&params.id));
        let running = inspect
            .state
            .as_ref()
            .and_then(|s| s.running)
            .unwrap_or(false);

        if running && !params.force {
            return tool_error(format!(
                "container '{name}' is running. Stop it first with container_stop, \
                 or pass force=true to kill and remove it in one step."
            ));
        }

        let mut consequences = vec![format!("container '{name}' would be removed")];
        if running {
            consequences.push(format!("running container '{name}' would be KILLED first"));
        }

        // Name the volumes explicitly — "volumes would be deleted" is not enough
        // information for a human to approve.
        let anonymous_volumes: Vec<String> = inspect
            .mounts
            .as_deref()
            .unwrap_or_default()
            .iter()
            .filter(|m| {
                m.typ.as_deref() == Some("volume")
                    && m.name.as_deref().is_some_and(is_anonymous_volume)
            })
            .filter_map(|m| m.name.clone())
            .collect();

        if params.volumes {
            if anonymous_volumes.is_empty() {
                consequences.push(
                    "no anonymous volumes are attached, so volumes=true removes nothing extra".into(),
                );
            } else {
                for v in &anonymous_volumes {
                    consequences
                        .push(format!("anonymous volume '{v}' would be DELETED (irreversible)"));
                }
            }
        } else if !anonymous_volumes.is_empty() {
            consequences.push(format!(
                "{} anonymous volume(s) would be LEFT BEHIND (pass volumes=true to delete them)",
                anonymous_volumes.len()
            ));
        }

        let guarded = Guarded {
            tool: "container_rm",
            target: &name,
            effect: format!(
                "remove container '{name}'{}{}",
                if params.force { " (force-killing it)" } else { "" },
                if params.volumes {
                    " and its anonymous volumes"
                } else {
                    ""
                }
            ),
            consequences,
        };

        match safety::gate(
            &guarded,
            Authorization {
                dry_run: params.dry_run,
                confirm: params.confirm.as_deref(),
            },
        ) {
            Decision::DryRun(report) => {
                return bounded_json(&report, "container_rm", "Unexpectedly large — report this.");
            }
            Decision::Refused(refusal) => {
                return bounded_json(&refusal, "container_rm", "Unexpectedly large — report this.");
            }
            Decision::Authorized => {}
        }

        let options = RemoveContainerOptionsBuilder::new()
            .force(params.force)
            .v(params.volumes)
            .build();

        if let Err(e) = self
            .engine()
            .docker()
            .remove_container(&params.id, Some(options))
            .await
        {
            return engine_error("container_rm failed", &params.id, e);
        }

        safety::audit_completed(
            "container_rm",
            &name,
            &format!("force={} volumes={}", params.force, params.volumes),
        );

        let payload = serde_json::json!({
            "removed": true,
            "container": name,
            "id": short_id(inspect.id.as_deref().unwrap_or_default()),
            "forced": params.force,
            "volumes_removed": if params.volumes { anonymous_volumes } else { Vec::new() },
        });
        bounded_json(&payload, "container_rm", "Unexpectedly large — report this.")
    }

    /// Pull an image, collapsing layer progress into one summary.
    #[tool(
        name = "pull_image",
        description = "Pull an image. Layer-by-layer progress is CONSUMED AND COLLAPSED into a single \
                       summary — you get layer counts and the final digest, never the progress spam. \
                       Defaults to the ':latest' tag if the reference has none.",
        annotations(title = "Pull image", destructive_hint = false, idempotent_hint = true)
    )]
    pub async fn pull_image(
        &self,
        Parameters(params): Parameters<PullImageParams>,
    ) -> CallToolResult {
        let reference = params.image.trim();
        if reference.is_empty() {
            return tool_error("image must not be empty");
        }

        let (image, tag) = split_reference(reference);

        let options = CreateImageOptionsBuilder::new()
            .from_image(&image)
            .tag(&tag)
            .build();

        let mut stream = self
            .engine()
            .docker()
            .create_image(Some(options), None, None);

        // Consume the whole progress stream and keep only what survives as a
        // one-line answer: how many layers, and what we ended up with.
        let mut layers = std::collections::HashSet::new();
        let mut downloaded = 0usize;
        let mut already_present = 0usize;
        let mut digest: Option<String> = None;
        let mut last_status: Option<String> = None;

        while let Some(event) = stream.next().await {
            let event = match event {
                Ok(e) => e,
                Err(e) => return engine_error("pull_image failed", reference, e),
            };

            if let Some(id) = &event.id {
                layers.insert(id.clone());
            }
            if let Some(status) = &event.status {
                last_status = Some(status.clone());
                let lower = status.to_lowercase();
                if lower.contains("pull complete") || lower.contains("download complete") {
                    downloaded += 1;
                } else if lower.contains("already exists") {
                    already_present += 1;
                }
                if let Some((_, rest)) = status.split_once("Digest: ") {
                    digest = Some(rest.trim().to_string());
                }
            }
        }

        tracing::info!(image = %reference, layers = layers.len(), "image pulled");

        let payload = serde_json::json!({
            "pulled": true,
            "image": format!("{image}:{tag}"),
            "layers_total": layers.len(),
            "layers_downloaded": downloaded,
            "layers_already_present": already_present,
            "digest": digest,
            "final_status": last_status,
            "note": "Per-layer progress was consumed and collapsed; only this summary is returned.",
        });
        bounded_json(&payload, "pull_image", "Unexpectedly large — report this.")
    }

    /// Run a command inside a container. Destructive — gated per §6.
    #[tool(
        name = "container_exec",
        description = "Run a command inside a running container and capture bounded stdout/stderr. \
                       DESTRUCTIVE and GATED: arbitrary code execution, so it does nothing unless you pass \
                       dry_run=true (shows the exact argv) or confirm=\"<container-name>\". \
                       cmd is an ARGV ARRAY, never a shell string — [\"ls\",\"-la\"], not \"ls -la\". For \
                       pipes or globs be explicit: [\"sh\",\"-c\",\"ls /app | head\"]. Output is capped at \
                       8000 chars per stream and the command is killed after `timeout` seconds (default 30). \
                       Prefer this over shelling out to `docker exec`, which is unbounded and unaudited.",
        annotations(title = "Exec in container", destructive_hint = true)
    )]
    pub async fn container_exec(
        &self,
        Parameters(params): Parameters<ExecParams>,
    ) -> CallToolResult {
        if params.cmd.is_empty() {
            return tool_error(
                "cmd must not be empty. Pass an argv array, e.g. [\"sh\", \"-c\", \"ls /app\"].",
            );
        }

        let inspect = match self
            .engine()
            .docker()
            .inspect_container(&params.id, None::<InspectContainerOptions>)
            .await
        {
            Ok(i) => i,
            Err(e) => return engine_error("container_exec failed", &params.id, e),
        };

        let name = strip_leading_slash(inspect.name.as_deref().unwrap_or(&params.id));

        if !inspect.state.as_ref().and_then(|s| s.running).unwrap_or(false) {
            return tool_error(format!(
                "container '{name}' is not running, so nothing can exec inside it. \
                 Start it first with container_start."
            ));
        }

        let timeout = params.timeout.unwrap_or(DEFAULT_EXEC_TIMEOUT).clamp(1, MAX_EXEC_TIMEOUT);
        let rendered = render_argv(&params.cmd);

        let mut consequences = vec![
            format!("'{rendered}' would run inside container '{name}'"),
            "it runs with the container's own privileges and can modify or delete data there".into(),
            format!("it would be abandoned after {timeout}s if it has not finished"),
        ];
        if let Some(user) = &params.user {
            consequences.push(format!("it would run as user '{user}'"));
        }
        if let Some(dir) = &params.workdir {
            consequences.push(format!("working directory would be '{dir}'"));
        }

        let guarded = Guarded {
            tool: "container_exec",
            target: &name,
            effect: format!("execute '{rendered}' inside container '{name}'"),
            consequences,
        };

        match safety::gate(
            &guarded,
            Authorization {
                dry_run: params.dry_run,
                confirm: params.confirm.as_deref(),
            },
        ) {
            Decision::DryRun(report) => {
                return bounded_json(&report, "container_exec", "Unexpectedly large — report this.");
            }
            Decision::Refused(refusal) => {
                return bounded_json(&refusal, "container_exec", "Unexpectedly large — report this.");
            }
            Decision::Authorized => {}
        }

        let config = CreateExecOptions {
            cmd: Some(params.cmd.clone()),
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            // No stdin and no TTY: this is a capture, not a session. A TTY would
            // also merge stderr into stdout and inject control codes.
            attach_stdin: Some(false),
            tty: Some(false),
            working_dir: params.workdir.clone(),
            user: params.user.clone(),
            ..Default::default()
        };

        let exec = match self.engine().docker().create_exec(&params.id, config).await {
            Ok(e) => e,
            Err(e) => return engine_error("container_exec failed", &params.id, e),
        };

        let started = self
            .engine()
            .docker()
            .start_exec(&exec.id, Some(StartExecOptions {
                detach: false,
                tty: false,
                output_capacity: None,
            }))
            .await;

        let StartExecResults::Attached { mut output, .. } = (match started {
            Ok(s) => s,
            Err(e) => return engine_error("container_exec failed", &params.id, e),
        }) else {
            return tool_error("exec started detached unexpectedly; no output was captured");
        };

        let mut stdout = String::new();
        let mut stderr = String::new();

        // Drain under a wall-clock deadline. On timeout we keep whatever was
        // captured — partial output from a hung command is usually the most
        // useful thing we have.
        let drained = tokio::time::timeout(std::time::Duration::from_secs(timeout), async {
            while let Some(chunk) = output.next().await {
                match chunk {
                    Ok(bollard::container::LogOutput::StdErr { message }) => {
                        stderr.push_str(&String::from_utf8_lossy(&message));
                    }
                    Ok(other) => stdout.push_str(&String::from_utf8_lossy(other.as_ref())),
                    Err(e) => return Err(e),
                }
            }
            Ok(())
        })
        .await;

        let timed_out = drained.is_err();
        if let Ok(Err(e)) = drained {
            return engine_error("container_exec failed while reading output", &params.id, e);
        }

        // Exit code is only meaningful once the process has actually finished.
        let exit_code = if timed_out {
            None
        } else {
            self.engine()
                .docker()
                .inspect_exec(&exec.id)
                .await
                .ok()
                .and_then(|i| i.exit_code)
        };

        safety::audit_completed(
            "container_exec",
            &name,
            &format!("argv={rendered} exit={exit_code:?} timed_out={timed_out}"),
        );

        let payload = serde_json::json!({
            "container": name,
            "command": params.cmd,
            "exit_code": exit_code,
            "timed_out": timed_out,
            "stdout": clip(stdout.trim_end(), MAX_EXEC_OUTPUT_CHARS),
            "stderr": clip(stderr.trim_end(), MAX_EXEC_OUTPUT_CHARS),
            "note": if timed_out {
                format!("Command did not finish within {timeout}s. Output captured so far is included; \
                         the process may still be running inside the container.")
            } else {
                format!("Output capped at {MAX_EXEC_OUTPUT_CHARS} chars per stream.")
            },
        });
        bounded_json(
            &payload,
            "container_exec",
            "Output too large — narrow the command, or pipe it through head inside the container.",
        )
    }
}

impl BosunServer {
    /// Re-inspect after a lifecycle change so the caller sees the real outcome
    /// rather than having to trust that the call did what it said.
    async fn state_after(&self, id: &str, action: &str) -> CallToolResult {
        let inspect = match self
            .engine()
            .docker()
            .inspect_container(id, None::<InspectContainerOptions>)
            .await
        {
            Ok(i) => i,
            // The action itself succeeded; only the confirmation read failed.
            Err(e) => {
                tracing::debug!(%e, id, "post-action inspect failed");
                let payload = serde_json::json!({
                    "action": action,
                    "container": id,
                    "ok": true,
                    "note": "Action succeeded, but the follow-up inspect failed. \
                             Call inspect_container to confirm state.",
                });
                return bounded_json(&payload, "container_action", "n/a");
            }
        };

        let state = inspect.state.as_ref();
        let payload = serde_json::json!({
            "action": action,
            "container": strip_leading_slash(inspect.name.as_deref().unwrap_or(id)),
            "id": short_id(inspect.id.as_deref().unwrap_or_default()),
            "ok": true,
            "state": state
                .and_then(|s| s.status)
                .map(|s| format!("{s:?}").to_lowercase()),
            "running": state.and_then(|s| s.running).unwrap_or(false),
            "exit_code": state.and_then(|s| s.exit_code),
            "health": state
                .and_then(|s| s.health.as_ref())
                .and_then(|h| h.status)
                .map(|s| format!("{s:?}").to_lowercase()),
        });
        bounded_json(&payload, "container_action", "Unexpectedly large — report this.")
    }
}

/// Render an argv vector for human review in a gate preview.
///
/// Quoting is for *display only* — the vector itself is what gets executed, and
/// it never passes through a shell. Arguments containing whitespace or quotes are
/// shown quoted so the reader can see the word boundaries the daemon will see.
fn render_argv(cmd: &[String]) -> String {
    cmd.iter()
        .map(|arg| {
            if arg.is_empty() || arg.contains(|c: char| c.is_whitespace() || c == '"' || c == '\'')
            {
                format!("{:?}", arg)
            } else {
                arg.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Anonymous volumes are the ones Docker names with a 64-char hex id. Named
/// volumes are user-created and outlive their container by design, so `-v` does
/// not touch them — reporting them as at-risk would be a false alarm.
fn is_anonymous_volume(name: &str) -> bool {
    name.len() == 64 && name.chars().all(|c| c.is_ascii_hexdigit())
}

/// Split `repo:tag` / `repo@sha256:…`, defaulting to `latest`.
///
/// The registry-port case (`localhost:5000/app`) is why this can't just split on
/// the last colon blindly: a colon before a `/` is a port, not a tag separator.
fn split_reference(reference: &str) -> (String, String) {
    if let Some((repo, digest)) = reference.split_once('@') {
        return (repo.to_string(), digest.to_string());
    }

    match reference.rsplit_once(':') {
        Some((repo, tag)) if !tag.contains('/') => (repo.to_string(), tag.to_string()),
        _ => (reference.to_string(), "latest".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_tag_is_split_off() {
        assert_eq!(split_reference("nginx:1.27"), ("nginx".into(), "1.27".into()));
    }

    #[test]
    fn a_missing_tag_defaults_to_latest() {
        assert_eq!(split_reference("nginx"), ("nginx".into(), "latest".into()));
        assert_eq!(
            split_reference("ghcr.io/org/app"),
            ("ghcr.io/org/app".into(), "latest".into())
        );
    }

    #[test]
    fn a_registry_port_is_not_mistaken_for_a_tag() {
        // The regression this guards: splitting on the last ':' blindly turns
        // the port into a tag and the pull silently targets the wrong thing.
        assert_eq!(
            split_reference("localhost:5000/app"),
            ("localhost:5000/app".into(), "latest".into())
        );
        assert_eq!(
            split_reference("localhost:5000/app:v2"),
            ("localhost:5000/app".into(), "v2".into())
        );
    }

    #[test]
    fn digest_references_are_preserved() {
        let (repo, digest) = split_reference("nginx@sha256:abc123");
        assert_eq!(repo, "nginx");
        assert_eq!(digest, "sha256:abc123");
    }

    #[test]
    fn only_64_char_hex_names_count_as_anonymous_volumes() {
        assert!(is_anonymous_volume(&"a".repeat(64)));
        assert!(is_anonymous_volume(&"0123456789abcdef".repeat(4)));
        // A named volume must never be reported as at-risk from `-v`.
        assert!(!is_anonymous_volume("pgdata"));
        assert!(!is_anonymous_volume("my-project_db-data"));
        assert!(!is_anonymous_volume(&"a".repeat(63)));
        assert!(!is_anonymous_volume(&"z".repeat(64)));
    }
}
