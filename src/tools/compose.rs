//! Compose tools (HANDOFF §4, M4).
//!
//! Compose is the one sanctioned exception to "never shell out to the docker CLI"
//! (HANDOFF §11, confirmed): the Engine API has no Compose endpoints, Compose is a
//! *client-side* spec, and reimplementing dependency ordering and healthcheck
//! gating would be a large source of subtle divergence from what the user gets
//! from their own terminal.
//!
//! The exception is scoped as tightly as possible:
//!   * `compose_ps` needs no CLI at all — it reads Compose's own labels off the
//!     Engine API, so the read path stays pure.
//!   * `compose_up` / `compose_down` shell out, but arguments are passed as an
//!     argv vector (never a shell string), so no input is ever interpreted by a
//!     shell.
//!   * `compose_down --volumes` is gated exactly like `container_rm`.

use std::collections::BTreeMap;
use std::process::Stdio;

use bollard::models::ContainerSummary;
use bollard::query_parameters::ListContainersOptionsBuilder;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::process::Command;

use crate::bound::bounded_json;
use crate::bound::project::{clip, strip_leading_slash};
use crate::engine::client::EngineClient;
use crate::safety::{self, Guarded};
use crate::server::KagoniServer;
use crate::tools::tool_error;

/// Compose's own labels. These are how Compose itself identifies a stack, so
/// reading them is not a heuristic — it is the same source of truth `docker
/// compose ps` uses.
const LABEL_PROJECT: &str = "com.docker.compose.project";
const LABEL_SERVICE: &str = "com.docker.compose.service";
const LABEL_WORKDIR: &str = "com.docker.compose.project.working_dir";

/// How long to let a compose invocation run before giving up.
const COMPOSE_TIMEOUT_SECS: u64 = 600;

/// Cap on captured CLI output — compose can be chatty on failure.
const MAX_CLI_OUTPUT_CHARS: usize = 4_000;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ComposePsParams {
    /// Compose project name.
    pub project: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ComposeUpParams {
    /// Compose project name.
    pub project: String,
    /// Directory containing the compose file. Defaults to the project's recorded
    /// working directory if the stack has run before.
    #[serde(default)]
    pub working_dir: Option<String>,
    /// Run detached. Default true — a foreground up would never return.
    #[serde(default = "default_true")]
    pub detach: bool,
    /// Limit the operation to these services. Empty means all.
    #[serde(default)]
    pub services: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ComposeDownParams {
    /// Compose project name.
    pub project: String,
    /// Directory containing the compose file. Defaults to the project's recorded
    /// working directory.
    #[serde(default)]
    pub working_dir: Option<String>,
    /// Also delete named volumes declared by the project. Irreversible data loss.
    #[serde(default)]
    pub volumes: bool,
    /// Preview what would happen without changing anything.
    #[serde(default)]
    pub dry_run: bool,
    /// Authorization token — must exactly equal the project name. Required when
    /// volumes=true.
    #[serde(default)]
    pub confirm: Option<String>,
}

fn default_true() -> bool {
    true
}

#[tool_router(router = compose_router, vis = "pub(crate)")]
impl KagoniServer {
    /// Per-service state and health for a Compose project.
    #[tool(
        name = "compose_ps",
        description = "Per-service state and health for one Compose project: service name, container, state, \
                       status, health, ports and restart count. Reads Compose's own labels off the Engine API \
                       — no CLI involved. Only sees stacks that have been started; for a project that has \
                       never run, there is nothing to list.",
        annotations(title = "Compose project status", read_only_hint = true)
    )]
    pub async fn compose_ps(
        &self,
        Parameters(params): Parameters<ComposePsParams>,
    ) -> CallToolResult {
        let services = match project_services(self.engine(), &params.project).await {
            Ok(s) => s,
            Err(e) => return tool_error(e),
        };

        if services.is_empty() {
            return tool_error(format!(
                "no containers found for compose project '{}'. \
                 List known projects by calling list_containers(all=true) and looking at names, \
                 or confirm the stack has been started.",
                params.project
            ));
        }

        let payload = serde_json::json!({
            "project": params.project,
            "service_count": services.len(),
            "services": services,
        });
        bounded_json(&payload, "compose_ps", "Unexpectedly large — report this.")
    }

    /// Bring a Compose project up.
    #[tool(
        name = "compose_up",
        description = "Start a Compose project via `docker compose up`, detached by default. Output is \
                       COLLAPSED to a summary plus the resulting per-service state — not the raw build and \
                       pull spam. Requires working_dir unless the project has run before (in which case its \
                       recorded directory is reused). Pass services to limit which ones start.",
        annotations(title = "Compose up", destructive_hint = false, idempotent_hint = true)
    )]
    pub async fn compose_up(
        &self,
        Parameters(params): Parameters<ComposeUpParams>,
    ) -> CallToolResult {
        let working_dir = match self
            .resolve_working_dir(&params.project, params.working_dir.as_deref())
            .await
        {
            Ok(dir) => dir,
            Err(e) => return tool_error(e),
        };

        let mut args = vec![
            "compose".to_string(),
            "-p".to_string(),
            params.project.clone(),
            "up".to_string(),
        ];
        if params.detach {
            args.push("-d".to_string());
        }
        args.extend(params.services.iter().cloned());

        let output = match run_compose(&args, &working_dir).await {
            Ok(o) => o,
            Err(e) => return tool_error(e),
        };

        // Report the resulting state, not just the CLI's own claim of success.
        let services = project_services(self.engine(), &params.project)
            .await
            .unwrap_or_default();

        let payload = serde_json::json!({
            "project": params.project,
            "action": "up",
            "working_dir": working_dir,
            "ok": output.success,
            "exit_code": output.code,
            "summary": summarize_compose_output(&output),
            "services": services,
            "note": "Build and pull progress was consumed and collapsed. \
                     'services' is the state read back from the daemon after the command ran.",
        });
        bounded_json(
            &payload,
            "compose_up",
            "Call compose_ps for the service list instead.",
        )
    }

    /// Bring a Compose project down. Gated when volumes are involved.
    #[tool(
        name = "compose_down",
        description = "Stop and remove a Compose project via `docker compose down`. Without volumes=true this \
                       is reversible and runs directly. WITH volumes=true it is DESTRUCTIVE and GATED: it \
                       deletes the project's named volumes and their data irreversibly, and does nothing \
                       unless you pass dry_run=true (preview) or confirm=\"<project-name>\" echoing the \
                       project name exactly. Call with dry_run=true first.",
        annotations(title = "Compose down", destructive_hint = true)
    )]
    pub async fn compose_down(
        &self,
        Parameters(params): Parameters<ComposeDownParams>,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> CallToolResult {
        let working_dir = match self
            .resolve_working_dir(&params.project, params.working_dir.as_deref())
            .await
        {
            Ok(dir) => dir,
            Err(e) => return tool_error(e),
        };

        let services = project_services(self.engine(), &params.project)
            .await
            .unwrap_or_default();

        // `down` without -v is reversible: containers go, declared volumes stay.
        // Gating it would train the agent to reach for confirm reflexively,
        // which is exactly what makes a gate stop working.
        if params.volumes || params.dry_run {
            let mut consequences = vec![format!(
                "{} container(s) in project '{}' would be stopped and removed",
                services.len(),
                params.project
            )];
            consequences.push("the project's default network would be removed".into());

            if params.volumes {
                let volumes = match self.project_volumes(&params.project).await {
                    Ok(v) => v,
                    Err(e) => return tool_error(e),
                };
                if volumes.is_empty() {
                    consequences
                        .push("no named volumes belong to this project, so volumes=true deletes nothing extra".into());
                } else {
                    for v in &volumes {
                        consequences.push(format!(
                            "named volume '{v}' would be DELETED with all its data (irreversible)"
                        ));
                    }
                }
            } else {
                consequences
                    .push("named volumes would be KEPT (volumes=true would delete them)".into());
            }

            let guarded = Guarded {
                tool: "compose_down",
                target: &params.project,
                effect: format!(
                    "tear down compose project '{}'{}",
                    params.project,
                    if params.volumes {
                        " AND DELETE ITS NAMED VOLUMES"
                    } else {
                        ""
                    }
                ),
                consequences,
            };

            if let Err(response) = crate::tools::authorize(
                &context.peer,
                &guarded,
                params.dry_run,
                params.confirm.as_deref(),
            )
            .await
            {
                return response;
            }
        }

        let mut args = vec![
            "compose".to_string(),
            "-p".to_string(),
            params.project.clone(),
            "down".to_string(),
        ];
        if params.volumes {
            args.push("--volumes".to_string());
        }

        let output = match run_compose(&args, &working_dir).await {
            Ok(o) => o,
            Err(e) => return tool_error(e),
        };

        if params.volumes {
            safety::audit_completed("compose_down", &params.project, "volumes=true");
        }

        let payload = serde_json::json!({
            "project": params.project,
            "action": "down",
            "working_dir": working_dir,
            "volumes_removed": params.volumes,
            "ok": output.success,
            "exit_code": output.code,
            "summary": summarize_compose_output(&output),
        });
        bounded_json(
            &payload,
            "compose_down",
            "Unexpectedly large — report this.",
        )
    }
}

impl KagoniServer {
    /// Find where to run compose from.
    ///
    /// Compose records the directory it was invoked in as a container label, so
    /// a stack that has run before does not need the caller to remember its path.
    async fn resolve_working_dir(
        &self,
        project: &str,
        explicit: Option<&str>,
    ) -> Result<String, String> {
        if let Some(dir) = explicit {
            return Ok(dir.to_string());
        }

        let containers = project_containers(self.engine(), project).await?;
        containers
            .iter()
            .filter_map(|c| c.labels.as_ref()?.get(LABEL_WORKDIR).cloned())
            .next()
            .ok_or_else(|| {
                format!(
                    "cannot determine a working directory for compose project '{project}'. \
                     Pass working_dir explicitly — Kagoni can only infer it for a project \
                     that has been started before."
                )
            })
    }

    /// Named volumes labelled as belonging to this project.
    async fn project_volumes(&self, project: &str) -> Result<Vec<String>, String> {
        let volumes = self
            .engine()
            .docker()
            .list_volumes(None::<bollard::query_parameters::ListVolumesOptions>)
            .await
            .map_err(|e| format!("could not list volumes: {e}"))?;

        let mut names: Vec<String> = volumes
            .volumes
            .unwrap_or_default()
            .into_iter()
            .filter(|v| v.labels.get(LABEL_PROJECT).map(String::as_str) == Some(project))
            .map(|v| v.name)
            .collect();
        names.sort();
        Ok(names)
    }
}

/// All containers belonging to a Compose project, by Compose's own label.
pub async fn project_containers(
    engine: &EngineClient,
    project: &str,
) -> Result<Vec<ContainerSummary>, String> {
    let filters = std::collections::HashMap::from([(
        "label".to_string(),
        vec![format!("{LABEL_PROJECT}={project}")],
    )]);

    engine
        .docker()
        .list_containers(Some(
            ListContainersOptionsBuilder::new()
                .all(true)
                .filters(&filters)
                .build(),
        ))
        .await
        .map_err(|e| format!("could not list containers for project '{project}': {e}"))
}

/// Per-service rows for a Compose project, sorted by service name.
///
/// Shared by `compose_ps` and the `docker://compose/{project}` resource so the
/// two can't drift into disagreeing about the same stack.
pub async fn project_services(
    engine: &EngineClient,
    project: &str,
) -> Result<Vec<serde_json::Value>, String> {
    let containers = project_containers(engine, project).await?;

    // BTreeMap so output ordering is stable across calls — an unstable service
    // list would look like churn to an agent diffing two readings.
    let mut by_service: BTreeMap<String, serde_json::Value> = BTreeMap::new();

    for c in &containers {
        let name = c
            .names
            .as_deref()
            .unwrap_or_default()
            .first()
            .map(|n| strip_leading_slash(n))
            .unwrap_or_default();

        let service = c
            .labels
            .as_ref()
            .and_then(|l| l.get(LABEL_SERVICE))
            .cloned()
            .unwrap_or_else(|| name.clone());

        let ports: Vec<String> = c
            .ports
            .as_deref()
            .unwrap_or_default()
            .iter()
            .filter_map(|p| {
                let proto = p
                    .typ
                    .as_ref()
                    .map_or("tcp".to_string(), |t| format!("{t:?}").to_lowercase());
                p.public_port
                    .map(|public| format!("{public}->{}/{proto}", p.private_port))
            })
            .collect();

        by_service.insert(
            service.clone(),
            serde_json::json!({
                "service": service,
                "container": name,
                "state": c.state.map(|s| format!("{s:?}").to_lowercase()),
                "status": c.status,
                "health": c.health.as_ref().and_then(|h| h.status)
                    .map(|s| format!("{s:?}").to_lowercase()),
                "ports": ports,
            }),
        );
    }

    Ok(by_service.into_values().collect())
}

/// Captured result of a compose invocation.
#[derive(Debug)]
struct CommandOutput {
    success: bool,
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

/// Run `docker <args>` in `working_dir`, capturing bounded output.
///
/// Arguments are passed as an argv vector, never a shell string, so nothing the
/// caller supplies is ever interpreted by a shell.
async fn run_compose(args: &[String], working_dir: &str) -> Result<CommandOutput, String> {
    if !std::path::Path::new(working_dir).is_dir() {
        return Err(format!(
            "working_dir '{working_dir}' does not exist or is not a directory"
        ));
    }

    tracing::info!(?args, working_dir, "running docker compose");

    let child = Command::new("docker")
        .args(args)
        .current_dir(working_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output();

    let output = tokio::time::timeout(std::time::Duration::from_secs(COMPOSE_TIMEOUT_SECS), child)
        .await
        .map_err(|_| {
            format!(
                "`docker compose` timed out after {COMPOSE_TIMEOUT_SECS}s. It may still be running."
            )
        })?
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => {
                "the `docker` CLI was not found on PATH. Compose tools need it — \
             the read and container tools do not."
                    .to_string()
            }
            _ => format!("failed to run `docker compose`: {e}"),
        })?;

    Ok(CommandOutput {
        success: output.status.success(),
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// Collapse compose's progress output into something worth returning.
///
/// Compose writes progress to stderr even on success, so a non-empty stderr is
/// not itself a failure signal — the exit status is. On success we keep only the
/// tail; on failure we keep the whole (clipped) stderr, because that is the part
/// that explains what went wrong.
fn summarize_compose_output(output: &CommandOutput) -> serde_json::Value {
    if output.success {
        let tail: Vec<&str> = output
            .stderr
            .lines()
            .chain(output.stdout.lines())
            .filter(|l| !l.trim().is_empty())
            .rev()
            .take(10)
            .collect();
        let mut tail: Vec<&str> = tail.into_iter().rev().collect();
        tail.dedup();

        serde_json::json!({
            "outcome": "success",
            "last_lines": tail,
            "note": "Progress output was collapsed to the last few lines.",
        })
    } else {
        serde_json::json!({
            "outcome": "failure",
            "stderr": clip(&output.stderr, MAX_CLI_OUTPUT_CHARS),
            "stdout": clip(&output.stdout, MAX_CLI_OUTPUT_CHARS),
            "note": "Command failed; stderr is retained (clipped) because it explains why.",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output(success: bool, stdout: &str, stderr: &str) -> CommandOutput {
        CommandOutput {
            success,
            code: Some(if success { 0 } else { 1 }),
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
        }
    }

    #[test]
    fn successful_output_is_collapsed_to_a_tail() {
        let noisy = (0..200)
            .map(|i| format!("layer {i} pulling"))
            .collect::<Vec<_>>()
            .join("\n");
        let summary = summarize_compose_output(&output(true, "", &noisy));

        assert_eq!(summary["outcome"], "success");
        assert_eq!(summary["last_lines"].as_array().unwrap().len(), 10);
    }

    #[test]
    fn failure_output_keeps_stderr_because_it_explains_why() {
        let summary = summarize_compose_output(&output(
            false,
            "",
            "service 'web': port 8080 is already allocated",
        ));
        assert_eq!(summary["outcome"], "failure");
        assert!(
            summary["stderr"]
                .as_str()
                .unwrap()
                .contains("already allocated")
        );
    }

    #[test]
    fn enormous_failure_output_is_clipped_not_dropped() {
        let huge = "x".repeat(50_000);
        let summary = summarize_compose_output(&output(false, "", &huge));
        let stderr = summary["stderr"].as_str().unwrap();
        assert!(stderr.len() < 5_000);
        assert!(stderr.ends_with("(clipped)"));
    }

    #[tokio::test]
    async fn a_missing_working_dir_fails_before_spawning_anything() {
        let err = run_compose(&["compose".into(), "ps".into()], "/nonexistent/path/xyz")
            .await
            .unwrap_err();
        assert!(err.contains("does not exist"));
    }
}
