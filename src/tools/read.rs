//! Bounded read tools (HANDOFF §4 "Read / state", M1).
//!
//! Every tool here caps its own output and documents the cap in its description,
//! and every one offers an explicit escape hatch (`full` / `raw`) so a
//! truncation is never something the agent can't undo.

use bollard::query_parameters::{
    InspectContainerOptions, ListContainersOptionsBuilder, ListImagesOptionsBuilder,
    LogsOptionsBuilder, StatsOptionsBuilder,
};
use futures_util::StreamExt;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::bound::logs::{self, Level};
use crate::bound::project::{self, clip, short_id, strip_leading_slash};
use crate::bound::{bounded_json, human_age, human_bytes, now_epoch_secs};
use crate::server::BosunServer;
use crate::tools::{engine_error, parse_since, tool_error};

/// Default number of log lines to pull before clustering.
const DEFAULT_TAIL: i32 = 200;
/// Ceiling on `tail`, even when the caller asks for more.
const MAX_TAIL: i32 = 5_000;
/// Default number of distinct log clusters to return.
const DEFAULT_MAX_CLUSTERS: usize = 12;
/// In `raw` mode, the hard line cap — raw is an escape hatch, not a firehose.
const MAX_RAW_LINES: usize = 500;
/// Per-line clip in raw mode; a single JSON blob line can be enormous.
const MAX_RAW_LINE_CHARS: usize = 2_000;
/// Default cap on rows returned by listing tools.
const DEFAULT_LIST_LIMIT: usize = 100;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListContainersParams {
    /// Include stopped containers. Default false (running only).
    #[serde(default)]
    pub all: bool,
    /// Case-insensitive substring match against name, image, or id.
    #[serde(default)]
    pub filter: Option<String>,
    /// Max rows to return. Default 100.
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize)]
struct ContainerRow {
    id: String,
    name: String,
    image: String,
    state: String,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    health: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    ports: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct InspectContainerParams {
    /// Container id or name.
    pub id: String,
    /// Return the complete raw inspect blob instead of the projected view.
    /// This includes environment variable **values** and can be very large.
    #[serde(default)]
    pub full: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ContainerLogsParams {
    /// Container id or name.
    pub id: String,
    /// Lines to pull from the end of the log before clustering. Default 200, max 5000.
    #[serde(default)]
    pub tail: Option<i32>,
    /// Only consider logs newer than this: '30s', '5m', '2h', '3d', or a unix timestamp.
    #[serde(default)]
    pub since: Option<String>,
    /// Case-insensitive substring filter applied before clustering.
    #[serde(default)]
    pub grep: Option<String>,
    /// Minimum inferred severity: trace, debug, info, warn, error, fatal.
    #[serde(default)]
    pub level: Option<String>,
    /// Return untrimmed log lines instead of the cluster summary.
    /// Still capped at 500 lines to protect the context window.
    #[serde(default)]
    pub raw: bool,
    /// Max distinct clusters to return. Default 12.
    #[serde(default)]
    pub max_clusters: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ContainerStatsParams {
    /// A single container id or name.
    #[serde(default)]
    pub id: Option<String>,
    /// Several containers in one call. Use ["*"] for every running container.
    #[serde(default)]
    pub ids: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListImagesParams {
    /// Only return dangling (untagged) images.
    #[serde(default)]
    pub dangling: bool,
    /// Max rows to return. Default 100.
    #[serde(default)]
    pub limit: Option<usize>,
}

#[tool_router(router = read_router, vis = "pub(crate)")]
impl BosunServer {
    /// List containers as compact rows: id, name, image, state, status, health, ports.
    ///
    /// Returns a bounded table, never raw inspect blobs. Defaults to running
    /// containers only and at most 100 rows; pass `all=true` for stopped ones.
    #[tool(
        name = "list_containers",
        description = "List containers as compact rows (id, name, image, state, status, health, ports). \
                       Bounded: running-only and max 100 rows by default. Use all=true to include stopped \
                       containers, filter to substring-match name/image/id, and limit to change the row cap. \
                       Returns no raw inspect blobs — call inspect_container for detail on one container.",
        annotations(title = "List containers", read_only_hint = true)
    )]
    pub async fn list_containers(
        &self,
        Parameters(params): Parameters<ListContainersParams>,
    ) -> CallToolResult {
        let options = ListContainersOptionsBuilder::new().all(params.all).build();

        let containers = match self.engine().docker().list_containers(Some(options)).await {
            Ok(c) => c,
            Err(e) => return engine_error("list_containers failed", "-", e),
        };

        let needle = params.filter.as_deref().map(str::to_lowercase);
        let limit = params.limit.unwrap_or(DEFAULT_LIST_LIMIT);

        let mut rows: Vec<ContainerRow> = containers
            .iter()
            .filter(|c| match &needle {
                None => true,
                Some(needle) => {
                    let name = c
                        .names
                        .as_deref()
                        .unwrap_or_default()
                        .join(" ")
                        .to_lowercase();
                    let image = c.image.as_deref().unwrap_or_default().to_lowercase();
                    let id = c.id.as_deref().unwrap_or_default().to_lowercase();
                    name.contains(needle) || image.contains(needle) || id.contains(needle)
                }
            })
            .map(|c| ContainerRow {
                id: short_id(c.id.as_deref().unwrap_or_default()),
                name: c
                    .names
                    .as_deref()
                    .unwrap_or_default()
                    .first()
                    .map(|n| strip_leading_slash(n))
                    .unwrap_or_default(),
                image: c.image.clone().unwrap_or_default(),
                state: c
                    .state
                    .map(|s| format!("{s:?}").to_lowercase())
                    .unwrap_or_else(|| "unknown".into()),
                status: c.status.clone().unwrap_or_default(),
                health: c
                    .health
                    .as_ref()
                    .and_then(|h| h.status)
                    .map(|s| format!("{s:?}").to_lowercase()),
                ports: summarize_ports(c),
            })
            .collect();

        let matched = rows.len();
        let omitted = matched.saturating_sub(limit);
        rows.truncate(limit);

        // Counts and hints are only emitted when they carry information: if
        // nothing was dropped, "returned: 3, omitted: null" is three facts the
        // caller can already see by counting the rows.
        let mut payload = serde_json::json!({ "containers": rows });
        if omitted > 0 {
            payload["matched"] = matched.into();
            payload["returned"] = matched.min(limit).into();
            payload["omitted"] = omitted.into();
            payload["note"] = "Row cap reached — narrow with filter, or raise limit.".into();
        }
        if !params.all {
            payload["showing"] = "running only (all=true includes stopped)".into();
        }

        bounded_json(&payload, "list_containers", "Narrow with filter, or lower limit.")
    }

    /// Inspect one container, projected to the fields that matter.
    ///
    /// Environment variables come back as **keys only** — values are withheld so
    /// secrets don't land in a context window by accident. `full=true` returns
    /// the complete blob including values.
    #[tool(
        name = "inspect_container",
        description = "Inspect one container, projected to a curated field set: state (including exit code, \
                       OOMKilled, health probes), restart count and policy, ports, mounts, networks, labels, \
                       and environment variable NAMES ONLY (values withheld to keep secrets out of context). \
                       Pass full=true for the complete raw inspect blob including env values — this is large.",
        annotations(title = "Inspect container", read_only_hint = true)
    )]
    pub async fn inspect_container(
        &self,
        Parameters(params): Parameters<InspectContainerParams>,
    ) -> CallToolResult {
        let inspect = match self
            .engine()
            .docker()
            .inspect_container(&params.id, None::<InspectContainerOptions>)
            .await
        {
            Ok(i) => i,
            Err(e) => return engine_error("inspect_container failed", &params.id, e),
        };

        if params.full {
            tracing::debug!(id = %params.id, "returning full inspect blob (env values included)");
            return bounded_json(
                &inspect,
                "inspect_container",
                "The raw blob exceeded the cap. Call without full=true for the projected view.",
            );
        }

        bounded_json(
            &project::project(&inspect),
            "inspect_container",
            "Unexpectedly large projection — report this.",
        )
    }

    /// Container logs, returned as a clustered digest rather than a firehose.
    ///
    /// Near-identical lines are grouped by a normalized skeleton, so 500 repeated
    /// stacktraces come back as one entry with a count and a first/last-seen
    /// window. `raw=true` returns individual lines (still capped).
    #[tool(
        name = "container_logs",
        description = "Container logs as a BOUNDED cluster digest, not a firehose. Pulls tail=200 lines by \
                       default (max 5000) and groups near-identical lines by a normalized skeleton — so 500 \
                       repeated stacktraces return as ONE cluster with count, first_seen and last_seen. \
                       Clusters are ranked severity-first, max 12 by default. Filter with since ('5m'), grep, \
                       or level ('error'). Pass raw=true for individual lines instead (capped at 500).",
        annotations(title = "Container logs (clustered)", read_only_hint = true)
    )]
    pub async fn container_logs(
        &self,
        Parameters(params): Parameters<ContainerLogsParams>,
    ) -> CallToolResult {
        let tail = params.tail.unwrap_or(DEFAULT_TAIL).clamp(1, MAX_TAIL);

        let mut builder = LogsOptionsBuilder::new()
            .stdout(true)
            .stderr(true)
            // Timestamps are what make first_seen/last_seen possible; we strip
            // them back off each line before clustering.
            .timestamps(true)
            .follow(false)
            .tail(&tail.to_string());

        if let Some(since) = &params.since {
            match parse_since(since, now_epoch_secs()) {
                Ok(ts) => builder = builder.since(ts as i32),
                Err(e) => return tool_error(e),
            }
        }

        let min_level = match params.level.as_deref().map(Level::parse) {
            Some(None) => {
                return tool_error(
                    "level must be one of: trace, debug, info, warn, error, fatal".to_string(),
                );
            }
            Some(Some(l)) => Some(l),
            None => None,
        };

        let mut stream = self
            .engine()
            .docker()
            .logs(&params.id, Some(builder.build()));

        let mut lines: Vec<logs::LogLine> = Vec::new();
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(output) => {
                    let stderr = matches!(output, bollard::container::LogOutput::StdErr { .. });
                    let text = String::from_utf8_lossy(output.as_ref()).into_owned();
                    lines.extend(logs::parse_lines(&text, stderr));
                }
                Err(e) => return engine_error("container_logs failed", &params.id, e),
            }
        }

        let total_pulled = lines.len();

        if let Some(needle) = params.grep.as_deref().map(str::to_lowercase) {
            lines.retain(|l| l.text.to_lowercase().contains(&needle));
        }
        if let Some(min) = min_level {
            lines.retain(|l| logs::infer_level(&l.text) >= min);
        }

        if params.raw {
            let shown: Vec<serde_json::Value> = lines
                .iter()
                .rev()
                .take(MAX_RAW_LINES)
                .rev()
                .map(|l| {
                    serde_json::json!({
                        "ts": l.timestamp,
                        "stderr": l.stderr,
                        "text": clip(&l.text, MAX_RAW_LINE_CHARS),
                    })
                })
                .collect();

            let omitted = lines.len().saturating_sub(shown.len());
            let payload = serde_json::json!({
                "container": params.id,
                "mode": "raw",
                "lines_pulled": total_pulled,
                "lines_after_filters": lines.len(),
                "lines_returned": shown.len(),
                "lines_omitted": (omitted > 0).then_some(omitted),
                "lines": shown,
                "note": "raw mode is still capped at 500 lines. Drop raw=true for the clustered digest.",
            });
            return bounded_json(
                &payload,
                "container_logs",
                "Lower tail, add grep/level filters, or drop raw=true for the clustered digest.",
            );
        }

        let summary = logs::cluster(&lines, params.max_clusters.unwrap_or(DEFAULT_MAX_CLUSTERS));

        let payload = serde_json::json!({
            "container": params.id,
            "mode": "clustered",
            "lines_pulled": total_pulled,
            "lines_after_filters": summary.lines_scanned,
            "distinct_clusters": summary.distinct_clusters,
            "clusters_omitted": summary.clusters_omitted,
            "clusters": summary.clusters,
            "note": "Clusters group near-identical lines by a normalized skeleton; \
                     'count' is occurrences within the scanned window. Levels are inferred \
                     from line text, not a structured field. Pass raw=true for individual lines.",
        });

        bounded_json(
            &payload,
            "container_logs",
            "Lower max_clusters or tail, or narrow with grep/level/since.",
        )
    }

    /// A single stats snapshot — CPU %, memory, network, block IO.
    ///
    /// Takes two samples internally because CPU percentage is a *delta* between
    /// consecutive readings; there is no honest way to report it from one sample.
    /// The caller still gets one object back, never a stream.
    #[tool(
        name = "container_stats",
        description = "Resource snapshot: CPU %, memory used/limit/percent, network rx/tx, block IO, PIDs. \
                       BATCH-CAPABLE — pass ids=[\"*\"] to snapshot EVERY running container in one call, or \
                       ids=[\"a\",\"b\"] for several. Prefer that over calling this once per container. \
                       Returns digest objects, never a stream; samples are taken concurrently, two per \
                       container ~1s apart because CPU % is a delta between readings.",
        annotations(title = "Container stats snapshot", read_only_hint = true)
    )]
    pub async fn container_stats(
        &self,
        Parameters(params): Parameters<ContainerStatsParams>,
    ) -> CallToolResult {
        let ids = match crate::tools::resolve_ids(
            self.engine(),
            params.id.as_deref(),
            &params.ids,
            // Stats only exist for running containers, so "*" means running.
            false,
        )
        .await
        {
            Ok(ids) => ids,
            Err(e) => return tool_error(e),
        };

        if ids.len() == 1 {
            return match self.stats_one(&ids[0]).await {
                Ok(value) => {
                    bounded_json(&value, "container_stats", "Unexpectedly large — report this.")
                }
                Err(e) => tool_error(e),
            };
        }

        // Concurrent: each snapshot costs ~1s of wall clock waiting for the
        // second sample, so serially this would scale linearly with the fleet.
        let results = futures_util::future::join_all(ids.iter().map(|id| self.stats_one(id))).await;

        let mut stats = Vec::new();
        let mut failed = Vec::new();
        for (id, result) in ids.iter().zip(results) {
            match result {
                Ok(value) => stats.push(value),
                // One unreadable container must not sink the whole batch.
                Err(e) => failed.push(serde_json::json!({ "container": id, "error": e })),
            }
        }

        let mut payload = serde_json::json!({ "stats": stats });
        if !failed.is_empty() {
            payload["unavailable"] = failed.into();
        }
        bounded_json(
            &payload,
            "container_stats",
            "Too many containers at once — pass a shorter ids list.",
        )
    }

    /// Snapshot one container. Split out so the batch path can run these
    /// concurrently — each call spends ~1s waiting for its second sample.
    async fn stats_one(&self, id: &str) -> Result<serde_json::Value, String> {
        // stream=true gives consecutive samples; we take exactly two and stop.
        let options = StatsOptionsBuilder::new().stream(true).one_shot(false).build();
        let mut stream = self.engine().docker().stats(id, Some(options));

        let mut samples = Vec::with_capacity(2);
        for _ in 0..2 {
            match stream.next().await {
                Some(Ok(s)) => samples.push(s),
                Some(Err(e)) => return Err(format!("stats failed for '{id}': {e}")),
                None => break,
            }
        }

        let Some(latest) = samples.last() else {
            return Err(format!(
                "no stats available for '{id}' — it is probably not running"
            ));
        };

        let mem_usage = latest.memory_stats.as_ref().and_then(|m| m.usage);
        let mem_limit = latest.memory_stats.as_ref().and_then(|m| m.limit);
        let mem_percent = match (mem_usage, mem_limit) {
            (Some(u), Some(l)) if l > 0 => Some((u as f64 / l as f64) * 100.0),
            _ => None,
        };

        let (rx, tx) = latest
            .networks
            .as_ref()
            .map(|nets| {
                nets.values().fold((0u64, 0u64), |(rx, tx), n| {
                    (rx + n.rx_bytes.unwrap_or(0), tx + n.tx_bytes.unwrap_or(0))
                })
            })
            .unwrap_or((0, 0));

        let (block_read, block_write) = block_io(latest);

        // Byte counts appear once each. Memory keeps its raw number because
        // "is it near the limit" is a numeric question; network and block IO are
        // cumulative counters where the magnitude is the whole answer, so the
        // human rendering alone carries it.
        let mut payload = serde_json::json!({
            "container": strip_leading_slash(latest.name.as_deref().unwrap_or(id)),
            "cpu": {
                "percent": cpu_percent(latest).map(round2),
                "online_cpus": latest.cpu_stats.as_ref().and_then(|c| c.online_cpus),
                "throttled_periods": latest
                    .cpu_stats
                    .as_ref()
                    .and_then(|c| c.throttling_data.as_ref())
                    .and_then(|t| t.throttled_periods),
            },
            "memory": {
                "used": mem_usage.map(human_bytes),
                "used_bytes": mem_usage,
                "limit": mem_limit.map(human_bytes),
                "percent": mem_percent.map(round2),
            },
            "network": { "rx": human_bytes(rx), "tx": human_bytes(tx) },
            "block_io": { "read": human_bytes(block_read), "write": human_bytes(block_write) },
            "pids": latest.pids_stats.as_ref().and_then(|p| p.current),
        });

        // Only say something when there is something to say. "This is a snapshot"
        // on every single call is boilerplate the tool description already covers.
        if samples.len() < 2 {
            payload["note"] =
                "Only one sample was available, so cpu.percent could not be computed.".into();
        }

        Ok(payload)
    }

    #[tool(
        name = "list_images",
        description = "List images as compact rows: id, tags, size, age, and how many containers use each. \
                       Bounded to 100 rows by default. Pass dangling=true to show only untagged images \
                       (the ones safe to reclaim).",
        annotations(title = "List images", read_only_hint = true)
    )]
    pub async fn list_images(
        &self,
        Parameters(params): Parameters<ListImagesParams>,
    ) -> CallToolResult {
        let mut builder = ListImagesOptionsBuilder::new().all(false);
        if params.dangling {
            let filters = std::collections::HashMap::from([("dangling", vec!["true"])]);
            builder = builder.filters(&filters);
        }

        let images = match self.engine().docker().list_images(Some(builder.build())).await {
            Ok(i) => i,
            Err(e) => return engine_error("list_images failed", "-", e),
        };

        let now = now_epoch_secs();
        let limit = params.limit.unwrap_or(DEFAULT_LIST_LIMIT);
        let total = images.len();

        let mut rows: Vec<serde_json::Value> = images
            .iter()
            .map(|i| {
                serde_json::json!({
                    "id": short_id(i.id.strip_prefix("sha256:").unwrap_or(&i.id)),
                    "tags": if i.repo_tags.is_empty() {
                        vec!["<none> (dangling)".to_string()]
                    } else {
                        i.repo_tags.clone()
                    },
                    "size": human_bytes(i.size.max(0) as u64),
                    "size_bytes": i.size,
                    "age": human_age(i.created, now),
                    "in_use_by": i.containers.max(0),
                })
            })
            .collect();

        // Biggest first: the useful ordering when you're looking for what to reclaim.
        rows.sort_by_key(|r| std::cmp::Reverse(r["size_bytes"].as_i64().unwrap_or(0)));
        let omitted = total.saturating_sub(limit);
        rows.truncate(limit);

        let payload = serde_json::json!({
            "images": rows,
            "total": total,
            "returned": total.min(limit),
            "omitted": (omitted > 0).then_some(omitted),
            "sorted_by": "size, largest first",
        });

        bounded_json(&payload, "list_images", "Lower limit, or pass dangling=true to narrow.")
    }
}

/// `docker ps`-style port rendering from a container summary row.
///
/// A published port almost always appears twice — once bound on `0.0.0.0` and
/// once on `::` — which is one fact rendered as two lines. We collapse the pair
/// to `8080->80/tcp`, and only name a host IP when it is a *specific* interface,
/// because that is the case where the address actually carries information.
fn summarize_ports(c: &bollard::models::ContainerSummary) -> Vec<String> {
    let mut out: Vec<String> = c
        .ports
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(|p| {
            let proto = p
                .typ
                .map(|t| format!("{t:?}").to_lowercase())
                .unwrap_or_else(|| "tcp".into());
            match p.public_port {
                Some(public) if is_wildcard_bind(p.ip.as_deref()) => {
                    format!("{public}->{}/{proto}", p.private_port)
                }
                Some(public) => {
                    let ip = p.ip.as_deref().unwrap_or_default();
                    format!("{ip}:{public}->{}/{proto}", p.private_port)
                }
                None => format!("{}/{proto}", p.private_port),
            }
        })
        .collect();
    out.sort();
    // Dedup now collapses the IPv4/IPv6 pair, since both rendered identically.
    out.dedup();
    out
}

/// `0.0.0.0`, `::`, and absent all mean "every interface".
fn is_wildcard_bind(ip: Option<&str>) -> bool {
    matches!(ip, None | Some("") | Some("0.0.0.0") | Some("::"))
}

/// CPU percentage the way `docker stats` computes it: the container's CPU-time
/// delta over the system's CPU-time delta, scaled by core count.
///
/// Returns `None` rather than 0.0 when the deltas aren't usable — a fabricated
/// zero would read as "idle", which is a different claim from "unknown".
fn cpu_percent(stats: &bollard::models::ContainerStatsResponse) -> Option<f64> {
    let cpu = stats.cpu_stats.as_ref()?;
    let precpu = stats.precpu_stats.as_ref()?;

    let total = cpu.cpu_usage.as_ref()?.total_usage?;
    let pre_total = precpu.cpu_usage.as_ref()?.total_usage?;
    let system = cpu.system_cpu_usage?;
    let pre_system = precpu.system_cpu_usage?;

    let cpu_delta = total.checked_sub(pre_total)? as f64;
    let system_delta = system.checked_sub(pre_system)? as f64;

    if system_delta <= 0.0 || cpu_delta < 0.0 {
        return None;
    }

    let cores = cpu
        .online_cpus
        .filter(|c| *c > 0)
        .map(|c| c as f64)
        .or_else(|| {
            cpu.cpu_usage
                .as_ref()
                .and_then(|u| u.percpu_usage.as_ref())
                .map(|p| p.len() as f64)
        })
        .unwrap_or(1.0);

    Some((cpu_delta / system_delta) * cores * 100.0)
}

/// Sum the recursive blkio entries into total bytes read and written.
fn block_io(stats: &bollard::models::ContainerStatsResponse) -> (u64, u64) {
    stats
        .blkio_stats
        .as_ref()
        .and_then(|b| b.io_service_bytes_recursive.as_ref())
        .map(|entries| {
            entries.iter().fold((0u64, 0u64), |(r, w), e| {
                match e.op.as_deref().map(str::to_ascii_lowercase).as_deref() {
                    Some("read") => (r + e.value.unwrap_or(0), w),
                    Some("write") => (r, w + e.value.unwrap_or(0)),
                    _ => (r, w),
                }
            })
        })
        .unwrap_or((0, 0))
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use bollard::models::{
        ContainerCpuStats, ContainerCpuUsage, ContainerStatsResponse, ContainerBlkioStatEntry,
        ContainerBlkioStats,
    };

    use super::*;

    fn cpu_sample(total: u64, system: u64, cores: u32) -> ContainerCpuStats {
        ContainerCpuStats {
            cpu_usage: Some(ContainerCpuUsage {
                total_usage: Some(total),
                ..Default::default()
            }),
            system_cpu_usage: Some(system),
            online_cpus: Some(cores),
            ..Default::default()
        }
    }

    #[test]
    fn cpu_percent_matches_the_docker_stats_formula() {
        // 10% of one core's worth of delta, across 4 cores => 40%.
        let stats = ContainerStatsResponse {
            cpu_stats: Some(cpu_sample(1_100, 11_000, 4)),
            precpu_stats: Some(cpu_sample(1_000, 10_000, 4)),
            ..Default::default()
        };
        let pct = cpu_percent(&stats).unwrap();
        assert!((pct - 40.0).abs() < 0.001, "got {pct}");
    }

    #[test]
    fn cpu_percent_is_none_rather_than_a_fabricated_zero() {
        // No previous sample => genuinely unknown, and must not read as "idle".
        let stats = ContainerStatsResponse {
            cpu_stats: Some(cpu_sample(1_000, 10_000, 2)),
            precpu_stats: None,
            ..Default::default()
        };
        assert!(cpu_percent(&stats).is_none());

        // A zero system delta can't yield a ratio either.
        let flat = ContainerStatsResponse {
            cpu_stats: Some(cpu_sample(1_000, 10_000, 2)),
            precpu_stats: Some(cpu_sample(1_000, 10_000, 2)),
            ..Default::default()
        };
        assert!(cpu_percent(&flat).is_none());
    }

    #[test]
    fn counter_resets_do_not_panic_or_produce_garbage() {
        // Daemon restarts can make the current reading smaller than the previous.
        let stats = ContainerStatsResponse {
            cpu_stats: Some(cpu_sample(500, 5_000, 2)),
            precpu_stats: Some(cpu_sample(1_000, 10_000, 2)),
            ..Default::default()
        };
        assert!(cpu_percent(&stats).is_none());
    }

    #[test]
    fn block_io_sums_read_and_write_separately() {
        let entry = |op: &str, value: u64| ContainerBlkioStatEntry {
            op: Some(op.to_string()),
            value: Some(value),
            ..Default::default()
        };
        let stats = ContainerStatsResponse {
            blkio_stats: Some(ContainerBlkioStats {
                io_service_bytes_recursive: Some(vec![
                    entry("read", 100),
                    entry("write", 250),
                    entry("Read", 50),
                    entry("sync", 999),
                ]),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(block_io(&stats), (150, 250));
    }

    #[test]
    fn block_io_defaults_to_zero_when_absent() {
        assert_eq!(block_io(&ContainerStatsResponse::default()), (0, 0));
    }

    fn port(ip: Option<&str>, public: Option<u16>, private: u16) -> bollard::models::PortSummary {
        bollard::models::PortSummary {
            ip: ip.map(str::to_string),
            public_port: public,
            private_port: private,
            typ: Some(bollard::models::PortSummaryTypeEnum::TCP),
        }
    }

    fn summary(ports: Vec<bollard::models::PortSummary>) -> bollard::models::ContainerSummary {
        bollard::models::ContainerSummary {
            ports: Some(ports),
            ..Default::default()
        }
    }

    #[test]
    fn dual_stack_bindings_collapse_to_one_line() {
        // Docker reports a published port twice, once on 0.0.0.0 and once on ::.
        // That is one fact; rendering it as two lines is pure duplication.
        let rendered = summarize_ports(&summary(vec![
            port(Some("0.0.0.0"), Some(8001), 8001),
            port(Some("::"), Some(8001), 8001),
        ]));
        assert_eq!(rendered, vec!["8001->8001/tcp"]);
    }

    #[test]
    fn a_specific_host_interface_is_still_named() {
        // Binding to one interface is information the caller needs; only the
        // wildcard case is noise.
        let rendered = summarize_ports(&summary(vec![port(Some("127.0.0.1"), Some(5432), 5432)]));
        assert_eq!(rendered, vec!["127.0.0.1:5432->5432/tcp"]);
    }

    #[test]
    fn unpublished_ports_show_only_the_container_port() {
        let rendered = summarize_ports(&summary(vec![port(None, None, 6379)]));
        assert_eq!(rendered, vec!["6379/tcp"]);
    }

    #[test]
    fn distinct_published_ports_are_all_kept() {
        let rendered = summarize_ports(&summary(vec![
            port(Some("0.0.0.0"), Some(8001), 8001),
            port(Some("::"), Some(8001), 8001),
            port(Some("0.0.0.0"), Some(9090), 9090),
        ]));
        assert_eq!(rendered, vec!["8001->8001/tcp", "9090->9090/tcp"]);
    }

    #[test]
    fn every_form_of_wildcard_bind_is_recognized() {
        assert!(is_wildcard_bind(None));
        assert!(is_wildcard_bind(Some("")));
        assert!(is_wildcard_bind(Some("0.0.0.0")));
        assert!(is_wildcard_bind(Some("::")));
        assert!(!is_wildcard_bind(Some("127.0.0.1")));
        assert!(!is_wildcard_bind(Some("192.168.1.5")));
    }
}
