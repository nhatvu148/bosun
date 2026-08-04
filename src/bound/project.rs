//! Inspect projection (docs/ORIGINAL-SPEC.md §5).
//!
//! `docker inspect` returns a blob measured in kilobytes, most of it graph-driver
//! paths and defaults nobody reads. This module projects it down to the fields
//! that answer real questions — is it running, why did it stop, what is it bound
//! to, what is mounted — and returns **env keys without values** so a `full: true`
//! is required before a secret can reach a context window.

use std::collections::BTreeMap;

use bollard::models::{ContainerInspectResponse, ContainerState, MountPoint};
use serde::Serialize;

/// The curated view of a container. Everything here earns its place by being
/// something you would actually ask about during troubleshooting.
#[derive(Debug, Serialize)]
pub struct ProjectedInspect {
    pub id: String,
    pub name: String,
    pub image: String,
    pub created: Option<String>,
    pub state: ProjectedState,
    pub restart_count: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restart_policy: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub mounts: Vec<ProjectedMount>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub networks: Vec<String>,
    /// Environment variable **names only**. Values are withheld by default so
    /// secrets don't leak into context; pass `full: true` for the raw blob.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub env_keys: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_limit_bytes: Option<i64>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    /// Constant reminder that this is a projection, not the whole truth.
    pub note: &'static str,
}

#[derive(Debug, Serialize)]
pub struct ProjectedState {
    pub status: String,
    pub running: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
    /// The single most load-bearing field in OOM diagnosis.
    pub oom_killed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health: Option<ProjectedHealth>,
}

#[derive(Debug, Serialize)]
pub struct ProjectedHealth {
    pub status: String,
    pub failing_streak: i64,
    /// Only the last few probe results — the full log is unbounded.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub recent_probes: Vec<ProjectedProbe>,
}

#[derive(Debug, Serialize)]
pub struct ProjectedProbe {
    pub exit_code: i64,
    /// Probe output, clipped — a failing healthcheck can print a lot.
    pub output: String,
}

#[derive(Debug, Serialize)]
pub struct ProjectedMount {
    pub kind: String,
    pub source: String,
    pub destination: String,
    pub rw: bool,
}

/// How many healthcheck probes to keep. Docker retains 5; the most recent
/// handful is what tells you whether a check is flapping or solidly broken.
const MAX_PROBES: usize = 3;

/// Healthcheck output is arbitrary program output and can be huge.
const MAX_PROBE_OUTPUT: usize = 300;

pub fn project(inspect: &ContainerInspectResponse) -> ProjectedInspect {
    let config = inspect.config.as_ref();

    ProjectedInspect {
        id: short_id(inspect.id.as_deref().unwrap_or_default()),
        name: strip_leading_slash(inspect.name.as_deref().unwrap_or_default()),
        image: config
            .and_then(|c| c.image.clone())
            .or_else(|| inspect.image.clone())
            .unwrap_or_default(),
        created: inspect.created.as_ref().map(ToString::to_string),
        state: project_state(inspect.state.as_ref()),
        restart_count: inspect.restart_count.unwrap_or(0),
        restart_policy: inspect
            .host_config
            .as_ref()
            .and_then(|h| h.restart_policy.as_ref())
            .and_then(|p| p.name.as_ref())
            .map(|n| format!("{n:?}").to_lowercase()),
        ports: project_ports(inspect),
        mounts: inspect
            .mounts
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(project_mount)
            .collect(),
        networks: inspect
            .network_settings
            .as_ref()
            .and_then(|n| n.networks.as_ref())
            .map(|n| n.keys().cloned().collect())
            .unwrap_or_default(),
        env_keys: config
            .and_then(|c| c.env.as_ref())
            .map(|env| env.iter().map(|e| env_key(e).to_string()).collect())
            .unwrap_or_default(),
        command: config.and_then(|c| c.cmd.as_ref()).map(|c| c.join(" ")),
        memory_limit_bytes: inspect
            .host_config
            .as_ref()
            .and_then(|h| h.memory)
            .filter(|m| *m > 0),
        labels: config
            .and_then(|c| c.labels.as_ref())
            .map(|l| l.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default(),
        note: "Projected view. Env values withheld; pass full=true for the raw inspect blob.",
    }
}

fn project_state(state: Option<&ContainerState>) -> ProjectedState {
    let Some(state) = state else {
        return ProjectedState {
            status: "unknown".into(),
            running: false,
            exit_code: None,
            oom_killed: false,
            error: None,
            started_at: None,
            finished_at: None,
            health: None,
        };
    };

    ProjectedState {
        status: state
            .status
            .map(|s| format!("{s:?}").to_lowercase())
            .unwrap_or_else(|| "unknown".into()),
        running: state.running.unwrap_or(false),
        exit_code: state.exit_code,
        oom_killed: state.oom_killed.unwrap_or(false),
        error: state.error.clone().filter(|e| !e.is_empty()),
        started_at: state.started_at.clone().filter(|s| !is_zero_time(s)),
        finished_at: state.finished_at.clone().filter(|s| !is_zero_time(s)),
        health: state.health.as_ref().map(|h| ProjectedHealth {
            status: h
                .status
                .map(|s| format!("{s:?}").to_lowercase())
                .unwrap_or_else(|| "unknown".into()),
            failing_streak: h.failing_streak.unwrap_or(0),
            recent_probes: h
                .log
                .as_deref()
                .unwrap_or_default()
                .iter()
                .rev()
                .take(MAX_PROBES)
                .map(|p| ProjectedProbe {
                    exit_code: p.exit_code.unwrap_or(0),
                    output: clip(p.output.as_deref().unwrap_or_default(), MAX_PROBE_OUTPUT),
                })
                .collect(),
        }),
    }
}

fn project_mount(m: &MountPoint) -> ProjectedMount {
    ProjectedMount {
        // MountPointType is a plain String in the Engine API ("bind", "volume", "tmpfs").
        kind: m.typ.clone().unwrap_or_else(|| "unknown".into()),
        // For named volumes the name is the useful identifier; for binds it's the path.
        source: m
            .name
            .clone()
            .filter(|n| !n.is_empty())
            .or_else(|| m.source.clone())
            .unwrap_or_default(),
        destination: m.destination.clone().unwrap_or_default(),
        rw: m.rw.unwrap_or(true),
    }
}

/// Render port bindings the way `docker ps` does: `0.0.0.0:8080->80/tcp`.
fn project_ports(inspect: &ContainerInspectResponse) -> Vec<String> {
    let Some(ports) = inspect
        .network_settings
        .as_ref()
        .and_then(|n| n.ports.as_ref())
    else {
        return Vec::new();
    };

    let mut out: Vec<String> = Vec::new();
    for (container_port, bindings) in ports {
        match bindings {
            Some(bindings) if !bindings.is_empty() => {
                for b in bindings {
                    let host_ip = b.host_ip.as_deref().unwrap_or("0.0.0.0");
                    let host_port = b.host_port.as_deref().unwrap_or("");
                    out.push(format!("{host_ip}:{host_port}->{container_port}"));
                }
            }
            // Exposed but unpublished — still worth knowing when debugging
            // "why can't I reach it".
            _ => out.push(format!("{container_port} (not published)")),
        }
    }
    out.sort();
    out
}

/// `KEY=value` → `KEY`. A bare entry with no `=` is already just a key.
pub fn env_key(entry: &str) -> &str {
    entry.split_once('=').map_or(entry, |(k, _)| k)
}

/// Docker's "never started" sentinel; rendering it is pure noise.
fn is_zero_time(s: &str) -> bool {
    s.starts_with("0001-01-01")
}

pub fn short_id(id: &str) -> String {
    id.chars().take(12).collect()
}

pub fn strip_leading_slash(name: &str) -> String {
    name.strip_prefix('/').unwrap_or(name).to_string()
}

/// Truncate to `max` chars, marking the cut so the caller knows it happened.
pub fn clip(s: &str, max: usize) -> String {
    let trimmed = s.trim();
    if trimmed.chars().count() <= max {
        return trimmed.to_string();
    }
    let kept: String = trimmed.chars().take(max).collect();
    format!("{kept}… (clipped)")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_values_are_never_exposed_by_the_key_extractor() {
        assert_eq!(
            env_key("DATABASE_URL=postgres://user:hunter2@db/app"),
            "DATABASE_URL"
        );
        assert_eq!(env_key("PATH=/usr/bin"), "PATH");
        assert_eq!(env_key("BARE_KEY"), "BARE_KEY");
    }

    #[test]
    fn env_key_handles_values_containing_equals() {
        assert_eq!(env_key("TOKEN=abc=def=ghi"), "TOKEN");
    }

    #[test]
    fn short_id_truncates_to_docker_ps_width() {
        let full = "3f4a1b2c9d8e7f6a5b4c3d2e1f0a9b8c7d6e5f4a3b2c1d0e";
        assert_eq!(short_id(full), "3f4a1b2c9d8e");
        assert_eq!(short_id("abc"), "abc");
    }

    #[test]
    fn container_names_lose_the_api_slash_prefix() {
        assert_eq!(strip_leading_slash("/web-1"), "web-1");
        assert_eq!(strip_leading_slash("web-1"), "web-1");
    }

    #[test]
    fn clip_marks_the_cut_but_leaves_short_input_alone() {
        assert_eq!(clip("short", 100), "short");
        let clipped = clip(&"x".repeat(500), 10);
        assert!(clipped.starts_with(&"x".repeat(10)));
        assert!(clipped.ends_with("(clipped)"));
    }

    #[test]
    fn zero_time_sentinel_is_treated_as_absent() {
        assert!(is_zero_time("0001-01-01T00:00:00Z"));
        assert!(!is_zero_time("2026-08-04T10:00:00Z"));
    }
}
