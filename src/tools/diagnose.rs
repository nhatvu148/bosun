//! Deterministic diagnostics (HANDOFF §4 "Diagnostic", M3).
//!
//! **No LLM call happens inside Bosun.** The calling agent is the LLM; Bosun's
//! job is to be a fast, honest data source that hands it bounded ground truth.
//! Every verdict here comes from plain heuristics over facts the daemon already
//! knows: exit code, `State.OOMKilled`, restart count, healthcheck history, and
//! log-cluster signals.
//!
//! Each verdict carries the `evidence` it was built from, so the agent can see
//! *why* — and disagree when the heuristic is wrong. A confident-sounding guess
//! with no evidence would be worse than no diagnosis at all.

use bollard::query_parameters::{InspectContainerOptions, LogsOptionsBuilder};
use futures_util::StreamExt;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::bound::bounded_json;
use crate::bound::logs::{self, Cluster, Level};
use crate::bound::project::strip_leading_slash;
use crate::server::BosunServer;
use crate::tools::tool_error;

/// Log lines to sample when diagnosing. Enough to see a crash-loop pattern
/// without turning a diagnosis into a log dump.
const DIAGNOSTIC_TAIL: i32 = 300;

/// Restart count above which we call it a crash loop rather than a blip.
const CRASH_LOOP_THRESHOLD: i64 = 3;

/// Uptime below which a running container with many restarts is judged to be
/// *actively* crash-looping rather than merely having a restart history.
///
/// This is what distinguishes "restarted 5 times over three months" (fine) from
/// "restarted 5 times and has been up for 2 seconds" (a live crash loop). Without
/// it, sampling a looping container in the instant it happens to be running
/// reports it as healthy.
const CRASH_LOOP_UPTIME_SECS: i64 = 60;

/// How often an error must recur before it downgrades an otherwise-healthy
/// running container.
///
/// One error line in a 300-line window is weak evidence — long-lived services
/// log routine, self-recovering errors (Postgres cancelling an autovacuum, a
/// client disconnecting mid-request). Calling those "degraded" produces exactly
/// the alert fatigue that makes a status field worthless. A *repeated* error is
/// a different claim, so that is what the verdict keys on; the one-off is still
/// reported as evidence, just not treated as a diagnosis.
const ERROR_CLUSTER_THRESHOLD: usize = 3;

/// How recently an error must have occurred to describe the container's *current*
/// condition rather than its history.
///
/// A log window holds hours of history, so a container that failed at startup and
/// has been fine since still shows those errors. Reporting it as degraded confuses
/// "this broke once" with "this is broken" — and the second claim is the one a
/// status field exists to make. Stale errors stay in `log_signals` and in the
/// verdict's note, so nothing is hidden; they just stop driving the verdict.
const RECENT_ERROR_WINDOW_SECS: i64 = 3_600;

/// Overall health of the container, as Bosun reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Running, and healthy if it has a healthcheck.
    Healthy,
    /// Running, but something is wrong (failing healthcheck, error logs).
    Degraded,
    /// Not running, and did not exit cleanly.
    Failing,
    /// Stopped deliberately, exit 0. Not a problem.
    Stopped,
    /// Running but too young, or too little signal, to judge.
    Unknown,
}

#[derive(Debug, Serialize)]
pub struct Diagnosis {
    pub container: String,
    pub status: Verdict,
    /// One-line summary of the most likely cause. `None` when nothing is wrong.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub likely_cause: Option<String>,
    /// The facts this verdict was built from. Always populated.
    pub evidence: Vec<String>,
    /// Concrete next steps, most useful first.
    pub suggested_actions: Vec<String>,
    /// The log clusters that informed the verdict, if any were relevant.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub log_signals: Vec<Cluster>,
    pub method: &'static str,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DiagnoseParams {
    /// A single container id or name.
    #[serde(default)]
    pub id: Option<String>,
    /// Several containers in one call. Use ["*"] for every container,
    /// including stopped ones — which is usually what you want, since a
    /// stopped container is often the thing that needs diagnosing.
    #[serde(default)]
    pub ids: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExplainExitCodeParams {
    /// The exit code to decode, e.g. 137.
    pub code: i64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WhyComposeFailingParams {
    /// Compose project name (the `com.docker.compose.project` label value).
    pub project: String,
}

#[tool_router(router = diagnose_router, vis = "pub(crate)")]
impl BosunServer {
    /// Diagnose a container from exit code, OOM state, restarts, health and logs.
    #[tool(
        name = "diagnose_container",
        description = "ALWAYS call this before reading raw logs when a container is failing. Returns a \
                       structured verdict — status, likely_cause, evidence[], suggested_actions[] — computed \
                       DETERMINISTICALLY from exit code, State.OOMKilled, restart count, healthcheck history \
                       and clustered log signals. No LLM inference happens inside Bosun; every conclusion \
                       lists the evidence it came from, so you can check the reasoning and disagree. \
                       BATCH-CAPABLE — for a whole-fleet health question pass ids=[\"*\"] to diagnose every \
                       container in ONE call rather than looping.",
        annotations(title = "Diagnose container", read_only_hint = true)
    )]
    pub async fn diagnose_container(
        &self,
        Parameters(params): Parameters<DiagnoseParams>,
    ) -> CallToolResult {
        let ids = match crate::tools::resolve_ids(
            self.engine(),
            params.id.as_deref(),
            &params.ids,
            // Stopped containers are frequently the ones worth diagnosing.
            true,
        )
        .await
        {
            Ok(ids) => ids,
            Err(e) => return tool_error(e),
        };

        if ids.len() == 1 {
            return match self.diagnose_one(&ids[0]).await {
                Ok(d) => bounded_json(
                    &d,
                    "diagnose_container",
                    "Unexpectedly large — call container_logs directly instead.",
                ),
                Err(e) => tool_error(e),
            };
        }

        // Each diagnosis is an inspect plus a log pull, so concurrency is what
        // makes a fleet-wide call practical rather than merely possible.
        let results =
            futures_util::future::join_all(ids.iter().map(|id| self.diagnose_one(id))).await;

        let mut diagnoses = Vec::new();
        let mut failed = Vec::new();
        for (id, result) in ids.iter().zip(results) {
            match result {
                Ok(d) => diagnoses.push(d),
                Err(e) => failed.push(serde_json::json!({ "container": id, "error": e })),
            }
        }

        // Surface the ones that need attention up front — with a fleet-sized
        // response, "which of these should I read" is the first question.
        let needs_attention: Vec<&str> = diagnoses
            .iter()
            .filter(|d| !matches!(d.status, Verdict::Healthy | Verdict::Stopped))
            .map(|d| d.container.as_str())
            .collect();

        let mut payload = serde_json::json!({
            "diagnosed": diagnoses.len(),
            "needs_attention": needs_attention,
            "diagnoses": diagnoses,
        });
        if !failed.is_empty() {
            payload["unavailable"] = failed.into();
        }

        bounded_json(
            &payload,
            "diagnose_container",
            "Too many containers at once — pass a shorter ids list.",
        )
    }

    /// Decode a container exit code into its human meaning.
    #[tool(
        name = "explain_exit_code",
        description = "Decode a container exit code (137, 143, 139, 126, 127, 1, …) into its meaning, the \
                       signal behind it, likely causes and what to check. Pure lookup — no daemon call, so \
                       it works for a code you read anywhere, not just a live container.",
        annotations(title = "Explain exit code", read_only_hint = true)
    )]
    pub async fn explain_exit_code(
        &self,
        Parameters(params): Parameters<ExplainExitCodeParams>,
    ) -> CallToolResult {
        bounded_json(
            &explain_exit_code(params.code),
            "explain_exit_code",
            "Unexpectedly large — report this.",
        )
    }

    /// Cross-service diagnosis for a Compose project.
    #[tool(
        name = "why_compose_failing",
        description = "Diagnose a whole Compose project at once: which services are down, host PORT CONFLICTS \
                       between services, failed healthchecks blocking dependents, OOM kills, and crash loops. \
                       Returns per-service verdicts plus project-level findings that only appear when you look \
                       across services. Deterministic — every finding lists its evidence.",
        annotations(title = "Diagnose compose project", read_only_hint = true)
    )]
    pub async fn why_compose_failing(
        &self,
        Parameters(params): Parameters<WhyComposeFailingParams>,
    ) -> CallToolResult {
        let containers = match crate::tools::compose::project_containers(
            self.engine(),
            &params.project,
        )
        .await
        {
            Ok(c) => c,
            Err(e) => return tool_error(e),
        };

        if containers.is_empty() {
            return tool_error(format!(
                "no containers found for compose project '{}'. Check the project name with \
                 compose_ps, or confirm the stack was started with `docker compose up`.",
                params.project
            ));
        }

        let mut services = Vec::new();
        let mut all_ports: Vec<(String, String)> = Vec::new();

        for summary in &containers {
            let Some(id) = summary.id.as_deref() else {
                continue;
            };

            let service = summary
                .labels
                .as_ref()
                .and_then(|l| l.get("com.docker.compose.service"))
                .cloned()
                .unwrap_or_else(|| {
                    summary
                        .names
                        .as_deref()
                        .unwrap_or_default()
                        .first()
                        .map(|n| strip_leading_slash(n))
                        .unwrap_or_default()
                });

            for port in summary.ports.as_deref().unwrap_or_default() {
                if let Some(public) = port.public_port {
                    let proto = port.typ.as_ref().map_or("tcp".into(), |t| {
                        format!("{t:?}").to_lowercase()
                    });
                    all_ports.push((format!("{public}/{proto}"), service.clone()));
                }
            }

            let Ok(inspect) = self
                .engine()
                .docker()
                .inspect_container(id, None::<InspectContainerOptions>)
                .await
            else {
                continue;
            };

            let clusters = self.sample_log_clusters(id).await;
            let mut diagnosis = diagnose(&inspect, &clusters);
            diagnosis.container = service.clone();
            services.push(diagnosis);
        }

        let findings = project_findings(&services, &all_ports);
        let unhealthy: Vec<&Diagnosis> = services
            .iter()
            .filter(|d| !matches!(d.status, Verdict::Healthy | Verdict::Stopped))
            .collect();

        let payload = serde_json::json!({
            "project": params.project,
            "services_total": services.len(),
            "services_unhealthy": unhealthy.len(),
            "verdict": if findings.is_empty() && unhealthy.is_empty() {
                "All services are healthy or cleanly stopped."
            } else {
                "One or more services need attention — see findings and per-service diagnoses."
            },
            "project_findings": findings,
            "services": services,
            "method": "Deterministic cross-service analysis. No LLM inference inside Bosun.",
        });

        bounded_json(
            &payload,
            "why_compose_failing",
            "Too many services to summarize at once — call diagnose_container per service.",
        )
    }
}

impl BosunServer {
    /// Diagnose one container. Split out so the batch path can fan out.
    async fn diagnose_one(&self, id: &str) -> Result<Diagnosis, String> {
        let inspect = self
            .engine()
            .docker()
            .inspect_container(id, None::<InspectContainerOptions>)
            .await
            .map_err(|e| format!("inspect failed for '{id}': {e}"))?;

        // Log signals are best-effort: a container with no logs is still
        // diagnosable from its exit code and restart count.
        let clusters = self.sample_log_clusters(id).await;
        Ok(diagnose(&inspect, &clusters))
    }

    /// Pull a bounded log window and cluster it, for diagnostic signal.
    ///
    /// Errors are swallowed on purpose: a container with unreadable logs is
    /// still diagnosable from its state, and failing the whole diagnosis over a
    /// missing log stream would be worse than diagnosing with less evidence.
    async fn sample_log_clusters(&self, id: &str) -> Vec<Cluster> {
        let options = LogsOptionsBuilder::new()
            .stdout(true)
            .stderr(true)
            .timestamps(true)
            .follow(false)
            .tail(&DIAGNOSTIC_TAIL.to_string())
            .build();

        let mut stream = self.engine().docker().logs(id, Some(options));
        let mut lines = Vec::new();

        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(output) => {
                    let stderr = matches!(output, bollard::container::LogOutput::StdErr { .. });
                    let text = String::from_utf8_lossy(output.as_ref()).into_owned();
                    lines.extend(logs::parse_lines(&text, stderr));
                }
                Err(e) => {
                    tracing::debug!(%e, id, "log sampling failed during diagnosis");
                    break;
                }
            }
        }

        // Keep only what a diagnosis can use: warn and above.
        let summary = logs::cluster(&lines, 5);
        summary
            .clusters
            .into_iter()
            .filter(|c| c.level >= Level::Warn)
            .collect()
    }
}

/// Whether a cluster's most recent occurrence is inside the recency window.
///
/// A cluster with no usable timestamp counts as current: unknown recency must
/// not silently downgrade a real error into history.
fn is_current(cluster: &Cluster, now: i64) -> bool {
    let Some(last_seen) = cluster.last_seen.as_deref() else {
        return true;
    };
    match chrono::DateTime::parse_from_rfc3339(last_seen) {
        Ok(ts) => now.saturating_sub(ts.timestamp()) <= RECENT_ERROR_WINDOW_SECS,
        Err(_) => true,
    }
}

/// Does this cluster carry enough weight to drive a verdict on its own?
fn is_significant(cluster: &Cluster) -> bool {
    cluster.level >= Level::Fatal
        || (cluster.level >= Level::Error && cluster.count >= ERROR_CLUSTER_THRESHOLD)
}

/// Render how long ago a cluster last fired, for the historical-errors note.
fn ago(cluster: &Cluster, now: i64) -> String {
    cluster
        .last_seen
        .as_deref()
        .and_then(|ts| chrono::DateTime::parse_from_rfc3339(ts).ok())
        .map(|ts| crate::bound::human_age(ts.timestamp(), now))
        .unwrap_or_else(|| "at an unknown time".into())
}

/// Seconds a container has been up, from `State.StartedAt`.
///
/// `None` when the timestamp is absent, unparseable, or Docker's "never started"
/// sentinel — the caller must treat unknown uptime as "don't conclude anything",
/// not as zero.
fn uptime_secs(started_at: Option<&str>, now: i64) -> Option<i64> {
    let started = started_at?;
    if started.starts_with("0001-01-01") {
        return None;
    }
    let parsed = chrono::DateTime::parse_from_rfc3339(started).ok()?;
    Some(now.saturating_sub(parsed.timestamp()).max(0))
}

/// The diagnostic core. Pure over its inputs, so it is directly testable
/// against fixtures without a daemon.
fn diagnose(inspect: &bollard::models::ContainerInspectResponse, clusters: &[Cluster]) -> Diagnosis {
    diagnose_at(inspect, clusters, crate::bound::now_epoch_secs())
}

/// [`diagnose`] with an injected clock, so uptime-dependent branches are testable.
fn diagnose_at(
    inspect: &bollard::models::ContainerInspectResponse,
    clusters: &[Cluster],
    now: i64,
) -> Diagnosis {
    let name = strip_leading_slash(inspect.name.as_deref().unwrap_or("unknown"));
    let state = inspect.state.as_ref();

    let running = state.and_then(|s| s.running).unwrap_or(false);
    let restarting = state.and_then(|s| s.restarting).unwrap_or(false);
    let oom_killed = state.and_then(|s| s.oom_killed).unwrap_or(false);
    let exit_code = state.and_then(|s| s.exit_code).unwrap_or(0);
    let restart_count = inspect.restart_count.unwrap_or(0);
    let daemon_error = state.and_then(|s| s.error.clone()).filter(|e| !e.is_empty());

    let health = state.and_then(|s| s.health.as_ref());
    let health_status = health
        .and_then(|h| h.status)
        .map(|s| format!("{s:?}").to_lowercase());
    let failing_streak = health.and_then(|h| h.failing_streak).unwrap_or(0);

    let uptime = uptime_secs(state.and_then(|s| s.started_at.as_deref()), now);

    // A container is actively crash-looping if the daemon says it's restarting,
    // or if it has a restart history and has only just come back up. The second
    // case is the one a naive check misses: sampled mid-loop, it looks running.
    let crash_looping = restarting
        || (restart_count >= CRASH_LOOP_THRESHOLD
            && (!running || uptime.is_some_and(|u| u < CRASH_LOOP_UPTIME_SECS)));

    let mut evidence: Vec<String> = Vec::new();
    let mut actions: Vec<String> = Vec::new();

    evidence.push(format!(
        "state.running={running}, state.exit_code={exit_code}, restart_count={restart_count}"
    ));
    if let Some(u) = uptime {
        evidence.push(format!("uptime={u}s (since state.started_at)"));
    }
    if restarting {
        evidence.push("state.restarting=true".into());
    }
    if let Some(err) = &daemon_error {
        evidence.push(format!("state.error='{err}'"));
    }
    if let Some(hs) = &health_status {
        evidence.push(format!(
            "healthcheck status='{hs}', failing_streak={failing_streak}"
        ));
    }
    for cluster in clusters {
        evidence.push(format!(
            "log cluster ({}, x{}): {}",
            cluster.level.as_str(),
            cluster.count,
            cluster.sample
        ));
    }

    // Ordered most-decisive first. OOM outranks everything because a 137 with
    // OOMKilled set has exactly one explanation, and treating it as a generic
    // SIGKILL sends the user looking in the wrong place.
    let (status, likely_cause) = if oom_killed {
        evidence.push("state.oom_killed=true — this is the decisive signal".into());
        actions.push(
            "Raise the container's memory limit (docker run -m / compose deploy.resources.limits.memory)"
                .into(),
        );
        actions.push("Call container_stats while it runs to see how close it gets to the limit".into());
        actions.push("Investigate the workload for a memory leak or an unbounded buffer".into());
        (
            Verdict::Failing,
            Some("Killed by the kernel OOM killer — the process exceeded its memory limit.".to_string()),
        )
    } else if crash_looping {
        actions.push(format!(
            "Call container_logs(id, level='error') — the container restarted {restart_count} times, \
             so the same failure is likely repeating"
        ));
        actions.push(format!("Call explain_exit_code({exit_code}) for what that code means"));
        actions.push("Check the restart policy — 'always' will mask a failure that never resolves".into());
        (
            Verdict::Failing,
            Some(format!(
                "Crash loop: restarted {restart_count} times and last exited with code {exit_code}.{}",
                match uptime {
                    Some(u) if running => format!(" Currently up, but only for {u}s."),
                    _ => String::new(),
                }
            )),
        )
    } else if !running && exit_code != 0 {
        let decoded = explain_exit_code(exit_code);
        actions.push(format!("Call explain_exit_code({exit_code}) for the full decode"));
        actions.push("Call container_logs(id, level='error') for the failure itself".into());
        if let Some(err) = &daemon_error {
            actions.push(format!("The daemon reported: {err}"));
        }
        (
            Verdict::Failing,
            Some(format!("Exited with code {exit_code}: {}", decoded.meaning)),
        )
    } else if !running {
        actions.push("Call container_start to bring it back up".into());
        (
            Verdict::Stopped,
            // Exit 0 is a clean stop, not a fault. Saying otherwise would send
            // the user hunting for a bug that isn't there.
            None,
        )
    } else if health_status.as_deref() == Some("unhealthy") {
        actions.push("Call inspect_container — state.health.recent_probes holds the probe output".into());
        actions.push("Verify the healthcheck command itself is correct and its timeout is realistic".into());
        actions.push("Check whether a dependency the probe reaches is itself down".into());
        (
            Verdict::Degraded,
            Some(format!(
                "Running, but the healthcheck is failing ({failing_streak} consecutive failures)."
            )),
        )
    } else if health_status.as_deref() == Some("starting") {
        actions.push("Wait for the healthcheck's start period to elapse, then diagnose again".into());
        (
            Verdict::Unknown,
            Some("Running, but still inside its healthcheck start period.".to_string()),
        )
    } else if let Some(worst) = clusters
        .iter()
        .find(|c| is_significant(c) && is_current(c, now))
    {
        actions.push("Call container_logs(id, level='error') for the full picture".into());
        (
            Verdict::Degraded,
            Some(format!(
                "Running, but emitting errors repeatedly: {} (seen {}x)",
                worst.sample, worst.count
            )),
        )
    } else if let Some(stale) = clusters.iter().find(|c| is_significant(c)) {
        // Errors exist but stopped. That is not a *live* fault, so it must not
        // read as one — but "stopped" has two explanations and Bosun cannot tell
        // them apart. Saying so beats implying the reassuring one: a reader who
        // takes silence for a fix stops looking, and the bug outlives the alarm.
        actions.push(format!(
            "Determine WHICH: was this fixed, or has the failing path simply not run since? \
             Bosun cannot tell. Check directly — the errors last fired {}",
            ago(stale, now)
        ));
        actions.push("Call container_logs(id, level='error') to see the full history".into());
        (
            Verdict::Healthy,
            Some(format!(
                "Container is healthy, but it logged errors earlier — most recently {}: {} (seen {}x). \
                 UNRESOLVED: absence of recent errors does not mean they were fixed; the code path \
                 may simply not have been exercised since. Verify before treating this as history.",
                ago(stale, now),
                stale.sample,
                stale.count
            )),
        )
    } else if restart_count > 0 {
        // Running now, but it has a history. Worth flagging without alarming.
        actions.push("Call container_logs to see what caused the earlier restarts".into());
        (
            Verdict::Healthy,
            Some(format!(
                "Currently healthy, but it has restarted {restart_count} time(s) — check whether that was expected."
            )),
        )
    } else {
        (Verdict::Healthy, None)
    };

    if actions.is_empty() {
        actions.push("No action needed.".into());
    }

    Diagnosis {
        container: name,
        status,
        likely_cause,
        evidence,
        suggested_actions: actions,
        log_signals: clusters.to_vec(),
        method: "Deterministic heuristics over exit code, OOM state, restart count, \
                 healthcheck history and clustered log signals. No LLM inference inside Bosun.",
    }
}

#[derive(Debug, Serialize)]
pub struct ExitCodeExplanation {
    pub code: i64,
    pub meaning: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signal: Option<String>,
    pub likely_causes: Vec<String>,
    pub what_to_check: Vec<String>,
}

/// Decode a container exit code.
///
/// Codes 128+N are "killed by signal N" — that mapping is what makes 137 and
/// 143 the two you actually see, and why they mean such different things.
pub fn explain_exit_code(code: i64) -> ExitCodeExplanation {
    let (meaning, signal, causes, checks): (&str, Option<&str>, Vec<&str>, Vec<&str>) = match code {
        0 => (
            "Success — the process exited cleanly.",
            None,
            vec!["The container finished its work, or was stopped deliberately."],
            vec!["Nothing to investigate. If you expected it to keep running, check the entrypoint — a foreground process may have daemonized."],
        ),
        1 => (
            "Generic application error — the process itself chose to fail.",
            None,
            vec![
                "An unhandled exception or explicit exit(1)",
                "Missing or invalid configuration",
                "A dependency the app needs was unreachable at startup",
            ],
            vec![
                "container_logs(id, level='error') — the app almost always logs why",
                "Verify required environment variables are set (inspect_container returns env keys)",
            ],
        ),
        125 => (
            "The Docker daemon itself failed — the container never started.",
            None,
            vec!["Invalid `docker run` flags", "A malformed option in the container config"],
            vec!["Check the run/compose invocation, not the application"],
        ),
        126 => (
            "Container command found but not executable.",
            None,
            vec![
                "The entrypoint or command lacks the execute bit",
                "A shell script with CRLF line endings",
                "Architecture mismatch between image and host",
            ],
            vec![
                "chmod +x the entrypoint in the Dockerfile",
                "Confirm the script's shebang and line endings",
                "Check the image's platform matches the host's",
            ],
        ),
        127 => (
            "Container command not found.",
            None,
            vec![
                "The binary named in CMD/ENTRYPOINT does not exist in the image",
                "A typo in the command",
                "A shared library is missing (a dynamically-linked binary reports 127 too)",
            ],
            vec![
                "Confirm the binary path exists in the image",
                "For a static/scratch image, check whether the binary is actually static",
            ],
        ),
        130 => (
            "Terminated by SIGINT (Ctrl-C).",
            Some("SIGINT (2)"),
            vec!["An interactive interrupt"],
            vec!["Usually deliberate — no action needed"],
        ),
        137 => (
            "Killed by SIGKILL. Usually an out-of-memory kill, sometimes a `docker stop` that timed out.",
            Some("SIGKILL (9)"),
            vec![
                "The kernel OOM killer terminated it for exceeding its memory limit",
                "`docker stop` grace period elapsed and the daemon escalated to SIGKILL",
                "Something else sent SIGKILL explicitly",
            ],
            vec![
                "inspect_container — State.OOMKilled distinguishes these two cases decisively",
                "If OOMKilled: raise the memory limit or fix the leak",
                "If not: the process ignored SIGTERM — check its shutdown handler, or raise the stop timeout",
            ],
        ),
        139 => (
            "Segmentation fault — the process crashed accessing invalid memory.",
            Some("SIGSEGV (11)"),
            vec![
                "A bug in native code",
                "An incompatible shared library version",
                "Architecture mismatch (an amd64 binary under emulation)",
            ],
            vec![
                "Check whether the image platform matches the host architecture",
                "Look for a stack trace or core dump in the logs",
            ],
        ),
        143 => (
            "Terminated by SIGTERM — a graceful shutdown request.",
            Some("SIGTERM (15)"),
            vec![
                "`docker stop` or `compose down`",
                "An orchestrator asked it to stop",
            ],
            vec![
                "This is normally expected. If unexpected, find who issued the stop",
                "An app that exits 143 rather than 0 may not be handling SIGTERM cleanly",
            ],
        ),
        c if (129..=192).contains(&c) => {
            // 128+N is the generic "killed by signal N" encoding.
            let signal_num = c - 128;
            return ExitCodeExplanation {
                code,
                meaning: format!(
                    "Terminated by signal {signal_num} (exit codes above 128 encode 128 + signal number)."
                ),
                signal: Some(format!("signal {signal_num}")),
                likely_causes: vec![
                    format!("The process received signal {signal_num} and did not handle it"),
                ],
                what_to_check: vec![
                    "container_logs for what the process was doing when it died".into(),
                    "inspect_container — State.Error may name the source".into(),
                ],
            };
        }
        _ => (
            "Application-specific exit code — its meaning is defined by the program, not by Docker.",
            None,
            vec!["The application chose this code deliberately"],
            vec![
                "Check the application's own documentation for its exit codes",
                "container_logs(id, level='error') for what it reported before exiting",
            ],
        ),
    };

    ExitCodeExplanation {
        code,
        meaning: meaning.to_string(),
        signal: signal.map(str::to_string),
        likely_causes: causes.into_iter().map(str::to_string).collect(),
        what_to_check: checks.into_iter().map(str::to_string).collect(),
    }
}

/// Findings that only exist when you look across services at once.
fn project_findings(services: &[Diagnosis], published_ports: &[(String, String)]) -> Vec<String> {
    let mut findings = Vec::new();

    // Host port conflicts. Two services publishing the same host port is a
    // configuration error the per-service view physically cannot see.
    let mut by_port: std::collections::HashMap<&str, Vec<&str>> = std::collections::HashMap::new();
    for (port, service) in published_ports {
        by_port.entry(port).or_default().push(service);
    }
    let mut conflicts: Vec<String> = by_port
        .iter()
        .filter(|(_, owners)| {
            // Distinct services only: one service with replicas is not a conflict.
            let mut distinct: Vec<&&str> = owners.iter().collect();
            distinct.sort_unstable();
            distinct.dedup();
            distinct.len() > 1
        })
        .map(|(port, owners)| {
            let mut names: Vec<&str> = owners.to_vec();
            names.sort_unstable();
            names.dedup();
            format!(
                "PORT CONFLICT: host port {port} is claimed by multiple services ({}). \
                 Only one can bind it; the others will fail to start.",
                names.join(", ")
            )
        })
        .collect();
    conflicts.sort();
    findings.append(&mut conflicts);

    let failing: Vec<&str> = services
        .iter()
        .filter(|d| d.status == Verdict::Failing)
        .map(|d| d.container.as_str())
        .collect();
    if !failing.is_empty() {
        findings.push(format!(
            "{} service(s) are failing: {}. Dependents of these will fail too — fix these first.",
            failing.len(),
            failing.join(", ")
        ));
    }

    let unhealthy: Vec<&str> = services
        .iter()
        .filter(|d| d.status == Verdict::Degraded)
        .map(|d| d.container.as_str())
        .collect();
    if !unhealthy.is_empty() {
        findings.push(format!(
            "{} service(s) are running but degraded: {}. \
             A depends_on with condition: service_healthy will block on these.",
            unhealthy.len(),
            unhealthy.join(", ")
        ));
    }

    let oom: Vec<&str> = services
        .iter()
        .filter(|d| {
            d.evidence
                .iter()
                .any(|e| e.contains("oom_killed=true"))
        })
        .map(|d| d.container.as_str())
        .collect();
    if !oom.is_empty() {
        findings.push(format!(
            "OOM KILLS: {} were killed for exceeding their memory limits. \
             If several died at once, the host itself may be under memory pressure.",
            oom.join(", ")
        ));
    }

    findings
}

#[cfg(test)]
mod tests {
    use bollard::models::{ContainerInspectResponse, ContainerState, Health, HealthStatusEnum};

    use super::*;

    fn container(state: ContainerState, restart_count: i64) -> ContainerInspectResponse {
        ContainerInspectResponse {
            name: Some("/app".into()),
            state: Some(state),
            restart_count: Some(restart_count),
            ..Default::default()
        }
    }

    #[test]
    fn an_oom_kill_outranks_every_other_signal() {
        // 137 + OOMKilled has exactly one explanation. Reporting it as a generic
        // SIGKILL would send the user looking in the wrong place.
        let d = diagnose(
            &container(
                ContainerState {
                    running: Some(false),
                    oom_killed: Some(true),
                    exit_code: Some(137),
                    ..Default::default()
                },
                5,
            ),
            &[],
        );

        assert_eq!(d.status, Verdict::Failing);
        assert!(d.likely_cause.as_ref().unwrap().contains("OOM"));
        assert!(d.evidence.iter().any(|e| e.contains("oom_killed=true")));
        assert!(d.suggested_actions.iter().any(|a| a.contains("memory limit")));
    }

    /// The gap this closes: sampling a crash-looping container in the instant it
    /// happens to be up. Before uptime was considered, this reported "healthy".
    #[test]
    fn a_container_caught_between_crashes_is_still_a_crash_loop() {
        const NOW: i64 = 1_800_000_000;
        let started = chrono::DateTime::from_timestamp(NOW - 3, 0)
            .unwrap()
            .to_rfc3339();

        let d = diagnose_at(
            &container(
                ContainerState {
                    running: Some(true),
                    restarting: Some(false),
                    exit_code: Some(1),
                    started_at: Some(started),
                    ..Default::default()
                },
                9,
            ),
            &[],
            NOW,
        );

        assert_eq!(d.status, Verdict::Failing);
        assert!(d.likely_cause.as_ref().unwrap().contains("Crash loop"));
        assert!(d.evidence.iter().any(|e| e.contains("uptime=3s")));
    }

    /// The contrast case: same restart count, but stable for weeks. Calling this
    /// a crash loop would be a false alarm.
    #[test]
    fn a_long_stable_container_with_restart_history_is_not_crash_looping() {
        const NOW: i64 = 1_800_000_000;
        let started = chrono::DateTime::from_timestamp(NOW - 1_000_000, 0)
            .unwrap()
            .to_rfc3339();

        let d = diagnose_at(
            &container(
                ContainerState {
                    running: Some(true),
                    restarting: Some(false),
                    started_at: Some(started),
                    ..Default::default()
                },
                9,
            ),
            &[],
            NOW,
        );

        assert_eq!(d.status, Verdict::Healthy);
        assert!(d.likely_cause.as_ref().unwrap().contains("restarted 9 time"));
    }

    #[test]
    fn the_daemons_restarting_flag_is_believed_on_its_own() {
        let d = diagnose_at(
            &container(
                ContainerState {
                    running: Some(true),
                    restarting: Some(true),
                    exit_code: Some(1),
                    ..Default::default()
                },
                1,
            ),
            &[],
            0,
        );
        assert_eq!(d.status, Verdict::Failing);
        assert!(d.evidence.iter().any(|e| e.contains("state.restarting=true")));
    }

    #[test]
    fn unparseable_or_sentinel_start_times_yield_no_uptime() {
        // Unknown uptime must not be silently treated as zero, which would make
        // every restart history look like an active loop.
        assert_eq!(uptime_secs(None, 1000), None);
        assert_eq!(uptime_secs(Some("0001-01-01T00:00:00Z"), 1000), None);
        assert_eq!(uptime_secs(Some("not a date"), 1000), None);
        assert_eq!(uptime_secs(Some("1970-01-01T00:00:10Z"), 100), Some(90));
    }

    #[test]
    fn repeated_restarts_are_reported_as_a_crash_loop() {
        let d = diagnose(
            &container(
                ContainerState {
                    running: Some(false),
                    exit_code: Some(1),
                    ..Default::default()
                },
                7,
            ),
            &[],
        );
        assert_eq!(d.status, Verdict::Failing);
        assert!(d.likely_cause.as_ref().unwrap().contains("Crash loop"));
    }

    #[test]
    fn a_clean_stop_is_not_reported_as_a_fault() {
        // Exit 0 means the user stopped it. Inventing a cause here would send
        // someone hunting for a bug that does not exist.
        let d = diagnose(
            &container(
                ContainerState {
                    running: Some(false),
                    exit_code: Some(0),
                    ..Default::default()
                },
                0,
            ),
            &[],
        );
        assert_eq!(d.status, Verdict::Stopped);
        assert!(d.likely_cause.is_none());
    }

    #[test]
    fn a_failing_healthcheck_degrades_a_running_container() {
        let d = diagnose(
            &container(
                ContainerState {
                    running: Some(true),
                    health: Some(Health {
                        status: Some(HealthStatusEnum::UNHEALTHY),
                        failing_streak: Some(4),
                        log: None,
                    }),
                    ..Default::default()
                },
                0,
            ),
            &[],
        );
        assert_eq!(d.status, Verdict::Degraded);
        assert!(d.likely_cause.as_ref().unwrap().contains("healthcheck"));
        assert!(d.evidence.iter().any(|e| e.contains("failing_streak=4")));
    }

    #[test]
    fn a_container_still_in_its_start_period_is_not_judged() {
        let d = diagnose(
            &container(
                ContainerState {
                    running: Some(true),
                    health: Some(Health {
                        status: Some(HealthStatusEnum::STARTING),
                        failing_streak: Some(0),
                        log: None,
                    }),
                    ..Default::default()
                },
                0,
            ),
            &[],
        );
        assert_eq!(d.status, Verdict::Unknown);
    }

    fn error_cluster(sample: &str, count: usize, level: Level) -> Cluster {
        Cluster {
            template: sample.to_string(),
            sample: sample.to_string(),
            count,
            level,
            first_seen: None,
            last_seen: None,
            stderr: true,
        }
    }

    fn running() -> ContainerState {
        ContainerState {
            running: Some(true),
            ..Default::default()
        }
    }

    #[test]
    fn repeated_errors_degrade_an_otherwise_healthy_container() {
        let d = diagnose(
            &container(running(), 0),
            &[error_cluster("ERROR connection refused to 10.0.0.5", 42, Level::Error)],
        );
        assert_eq!(d.status, Verdict::Degraded);
        assert!(d.likely_cause.as_ref().unwrap().contains("connection refused"));
        assert_eq!(d.log_signals.len(), 1);
    }

    /// Found by running the fleet-wide diagnosis for real: a single "canceling
    /// autovacuum task" line demoted a perfectly healthy Postgres. Long-lived
    /// services log routine self-recovering errors, and treating one of those as
    /// a verdict is how a status field stops meaning anything.
    #[test]
    fn a_single_stray_error_does_not_demote_a_healthy_container() {
        let d = diagnose(
            &container(running(), 0),
            &[error_cluster("ERROR:  canceling autovacuum task", 1, Level::Error)],
        );
        assert_eq!(d.status, Verdict::Healthy);
        assert!(d.likely_cause.is_none());
        // Still reported as evidence — suppressed from the verdict, not hidden.
        assert!(d.evidence.iter().any(|e| e.contains("autovacuum")));
    }

    /// Found reviewing a real session: a Postgres that failed at startup and had
    /// been fine for 20 hours was still reported "degraded", because the log
    /// window still held the errors. "This broke once" and "this is broken" are
    /// different claims, and a status field exists to make the second one.
    #[test]
    fn errors_that_stopped_hours_ago_are_history_not_a_live_fault() {
        const NOW: i64 = 1_800_000_000;
        let mut cluster = error_cluster("ERROR relation \"ocr_usage\" does not exist", 13, Level::Error);
        cluster.last_seen = Some(
            chrono::DateTime::from_timestamp(NOW - 20 * 3600, 0)
                .unwrap()
                .to_rfc3339(),
        );

        let d = diagnose_at(&container(running(), 0), &[cluster], NOW);

        assert_eq!(d.status, Verdict::Healthy);
        // Reported, not hidden — the agent still needs to see it.
        let cause = d.likely_cause.as_ref().unwrap();
        assert!(cause.contains("logged errors earlier"), "got: {cause}");
        assert!(cause.contains("ocr_usage"), "got: {cause}");

        // The regression this guards, observed in a real session: softening the
        // verdict to Healthy led the reading agent to conclude "the migration was
        // probably applied" — for a table that does not exist. The wording has to
        // state the unknown outright, or a reassuring tone gets read as an answer.
        assert!(
            cause.contains("UNRESOLVED"),
            "the verdict must name the unknown explicitly: {cause}"
        );
        assert!(
            cause.contains("does not mean they were fixed"),
            "the verdict must block the optimistic reading: {cause}"
        );
        assert!(
            d.suggested_actions
                .iter()
                .any(|a| a.contains("Bosun cannot tell")),
            "the action must admit the limit rather than implying resolution"
        );
    }

    #[test]
    fn the_same_errors_still_degrade_when_they_are_current() {
        const NOW: i64 = 1_800_000_000;
        let mut cluster = error_cluster("ERROR connection refused", 13, Level::Error);
        cluster.last_seen = Some(
            chrono::DateTime::from_timestamp(NOW - 120, 0)
                .unwrap()
                .to_rfc3339(),
        );

        let d = diagnose_at(&container(running(), 0), &[cluster], NOW);
        assert_eq!(d.status, Verdict::Degraded);
    }

    #[test]
    fn an_undatable_error_is_treated_as_current() {
        // Unknown recency must not silently downgrade a real error to history.
        let d = diagnose_at(
            &container(running(), 0),
            &[error_cluster("ERROR connection refused", 13, Level::Error)],
            1_800_000_000,
        );
        assert_eq!(d.status, Verdict::Degraded);
    }

    #[test]
    fn one_fatal_line_is_enough_on_its_own() {
        // A panic does not need to repeat to be worth reporting.
        let d = diagnose(
            &container(running(), 0),
            &[error_cluster("panic: nil map write", 1, Level::Fatal)],
        );
        assert_eq!(d.status, Verdict::Degraded);
    }

    #[test]
    fn a_clean_running_container_is_healthy_with_no_invented_cause() {
        let d = diagnose(
            &container(
                ContainerState {
                    running: Some(true),
                    ..Default::default()
                },
                0,
            ),
            &[],
        );
        assert_eq!(d.status, Verdict::Healthy);
        assert!(d.likely_cause.is_none());
        assert_eq!(d.suggested_actions, vec!["No action needed."]);
    }

    #[test]
    fn every_verdict_carries_its_evidence() {
        // The anti-hallucination contract: a verdict with no evidence would be
        // indistinguishable from a guess.
        let d = diagnose(
            &container(
                ContainerState {
                    running: Some(true),
                    ..Default::default()
                },
                0,
            ),
            &[],
        );
        assert!(!d.evidence.is_empty());
        assert!(d.method.contains("No LLM inference"));
    }

    #[test]
    fn exit_code_137_names_both_of_its_causes() {
        let e = explain_exit_code(137);
        assert_eq!(e.signal.as_deref(), Some("SIGKILL (9)"));
        assert!(e.meaning.contains("memory") || e.meaning.contains("OOM"));
        // The whole point of 137 is that it is ambiguous — both readings must appear.
        assert!(e.likely_causes.iter().any(|c| c.contains("OOM")));
        assert!(e.likely_causes.iter().any(|c| c.contains("docker stop")));
        assert!(e.what_to_check.iter().any(|c| c.contains("OOMKilled")));
    }

    #[test]
    fn the_well_known_codes_decode_correctly() {
        assert!(explain_exit_code(0).meaning.contains("Success"));
        assert!(explain_exit_code(126).meaning.contains("not executable"));
        assert!(explain_exit_code(127).meaning.contains("not found"));
        assert_eq!(explain_exit_code(139).signal.as_deref(), Some("SIGSEGV (11)"));
        assert_eq!(explain_exit_code(143).signal.as_deref(), Some("SIGTERM (15)"));
    }

    #[test]
    fn unnamed_signal_codes_decode_via_the_128_plus_n_rule() {
        let e = explain_exit_code(134); // SIGABRT
        assert!(e.meaning.contains("signal 6"), "got: {}", e.meaning);
        assert_eq!(e.signal.as_deref(), Some("signal 6"));
    }

    #[test]
    fn an_unknown_code_is_admitted_rather_than_guessed_at() {
        let e = explain_exit_code(42);
        assert!(e.meaning.contains("Application-specific"));
    }

    #[test]
    fn port_conflicts_are_found_across_services() {
        // The finding that only exists at project level — no single service's
        // diagnosis can see it.
        let ports = vec![
            ("8080/tcp".to_string(), "web".to_string()),
            ("8080/tcp".to_string(), "api".to_string()),
            ("5432/tcp".to_string(), "db".to_string()),
        ];
        let findings = project_findings(&[], &ports);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].contains("PORT CONFLICT"));
        assert!(findings[0].contains("8080/tcp"));
        assert!(findings[0].contains("api") && findings[0].contains("web"));
    }

    #[test]
    fn one_service_on_one_port_is_not_a_conflict() {
        let ports = vec![
            ("8080/tcp".to_string(), "web".to_string()),
            ("5432/tcp".to_string(), "db".to_string()),
        ];
        assert!(project_findings(&[], &ports).is_empty());
    }

    #[test]
    fn replicas_of_one_service_are_not_a_conflict() {
        // Same service name twice is scaling, not misconfiguration.
        let ports = vec![
            ("8080/tcp".to_string(), "web".to_string()),
            ("8080/tcp".to_string(), "web".to_string()),
        ];
        assert!(project_findings(&[], &ports).is_empty());
    }

    #[test]
    fn failing_and_degraded_services_are_summarized_separately() {
        let make = |name: &str, status: Verdict| Diagnosis {
            container: name.into(),
            status,
            likely_cause: None,
            evidence: vec![],
            suggested_actions: vec![],
            log_signals: vec![],
            method: "test",
        };
        let services = vec![
            make("db", Verdict::Failing),
            make("api", Verdict::Degraded),
            make("web", Verdict::Healthy),
        ];
        let findings = project_findings(&services, &[]);
        assert!(findings.iter().any(|f| f.contains("failing") && f.contains("db")));
        assert!(findings.iter().any(|f| f.contains("degraded") && f.contains("api")));
    }
}
