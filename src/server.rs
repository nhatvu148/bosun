//! The rmcp server: tool router assembly, server info, and read-only resources.

use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::model::{
    CallToolResult, ErrorData, Implementation, ListResourceTemplatesResult, ListResourcesResult,
    PaginatedRequestParams, ProtocolVersion, ReadResourceRequestParams, ReadResourceResponse,
    ReadResourceResult, Resource, ResourceContents, ResourceTemplate, ServerCapabilities,
    ServerInfo,
};
use rmcp::service::RequestContext;
use rmcp::{RoleServer, ServerHandler, tool, tool_handler, tool_router};

use crate::bound::bounded_json;
use crate::engine::client::EngineClient;

/// Instructions handed to the client over the MCP handshake.
///
/// This is always-on context for every session, so it stays short and states the
/// two things an agent cannot infer from tool schemas alone: that reads are
/// bounded by design, and that destructive writes are gated.
const INSTRUCTIONS: &str = "\
Bosun drives local containers over the Docker Engine API (Docker, OrbStack, Colima, Podman).

Two rules govern this server:

1. READS ARE BOUNDED BY DEFAULT. Every read tool caps its output and says so in its
   description. container_logs returns a CLUSTERED digest — repeated lines are grouped
   with a count, not relayed individually. inspect_container returns a projected field
   set with environment variable NAMES ONLY. Each bounded tool has an explicit escape
   hatch (raw=true / full=true) when you genuinely need everything.

2. DESTRUCTIVE WRITES ARE GATED. container_rm, container_exec, and compose_down with
   volumes require either dry_run=true (preview only) or confirm=\"<exact-target-name>\".
   If this client supports elicitation, they instead ask the operator directly and no
   token is needed — call bosun_info to see which mode is in effect. Never pass
   force=true unless the user asked for it.

For a failing container, prefer diagnose_container over reading raw logs — it returns a
structured verdict from exit code, OOM state, restart count, healthcheck history and log
clusters, and tells you what evidence it used.

For a question about MORE THAN ONE container (\"how is everything\", \"anything wrong?\"),
call diagnose_container or container_stats ONCE with ids=[\"*\"] rather than looping over
containers. Fleet questions are one call, not N.";

/// The MCP server. Cheap to clone — the engine client is shared.
#[derive(Clone)]
pub struct BosunServer {
    engine: Arc<EngineClient>,
    tool_router: ToolRouter<Self>,
}

impl BosunServer {
    pub fn new(engine: EngineClient) -> Self {
        Self {
            engine: Arc::new(engine),
            // Each module contributes its own router; they merge into one surface.
            tool_router: Self::info_router()
                + Self::read_router()
                + Self::actions_router()
                + Self::diagnose_router()
                + Self::compose_router(),
        }
    }

    pub fn engine(&self) -> &EngineClient {
        &self.engine
    }
}

#[tool_router(router = info_router)]
impl BosunServer {
    /// Report which engine Bosun bound to and how it got there.
    #[tool(
        name = "bosun_info",
        description = "Report the container engine Bosun connected to: engine name, socket address, how that \
                       address was resolved, server and API versions, and current container counts. Call this \
                       first when container operations behave unexpectedly — it tells you whether Bosun is \
                       talking to the daemon you think it is.",
        annotations(title = "Bosun engine info", read_only_hint = true)
    )]
    pub async fn bosun_info(&self, context: RequestContext<RoleServer>) -> CallToolResult {
        let engine = self.engine();

        // Counts are a liveness check as much as a statistic: if this errors,
        // the socket resolved but the daemon isn't really answering.
        let (running, total, count_error) = match engine
            .docker()
            .list_containers(Some(
                bollard::query_parameters::ListContainersOptionsBuilder::new()
                    .all(true)
                    .build(),
            ))
            .await
        {
            Ok(containers) => {
                let running = containers
                    .iter()
                    .filter(|c| {
                        matches!(
                            c.state,
                            Some(bollard::models::ContainerSummaryStateEnum::RUNNING)
                        )
                    })
                    .count();
                (Some(running), Some(containers.len()), None)
            }
            Err(e) => (None, None, Some(e.to_string())),
        };

        // Naming the gated tools here means the agent learns the write-safety
        // contract from a live answer, not only from reading tool descriptions.
        let mut destructive: Vec<String> = self
            .tool_router
            .list_all()
            .iter()
            .filter(|t| crate::safety::risk_of(&t.name) == Some(crate::safety::Risk::Destructive))
            .map(|t| t.name.to_string())
            .collect();
        destructive.sort();

        // Which gate is actually in force depends on this client, so report it
        // rather than making the agent guess from the tool schemas.
        let elicitation = context
            .peer
            .peer_info()
            .and_then(|info| info.capabilities.elicitation.clone())
            .is_some();

        let (approval_mode, approval_note) = if elicitation {
            (
                "elicitation",
                "This client supports elicitation, so destructive tools ask the operator \
                 directly and no confirm token is needed.",
            )
        } else {
            (
                "confirm_token",
                "This client does not support elicitation, so destructive tools require \
                 dry_run=true or confirm=\"<target>\". All other tools run directly.",
            )
        };

        let payload = serde_json::json!({
            "bosun_version": env!("CARGO_PKG_VERSION"),
            "engine": engine.engine().as_str(),
            "socket_path": engine.endpoint().address,
            "resolved_from": engine.endpoint().source.as_str(),
            "server_version": engine.server_version(),
            "api_version": engine.api_version(),
            "container_count": total,
            "running_count": running,
            "engine_error": count_error,
            "destructive_tools": destructive,
            "approval_mode": approval_mode,
            "approval_note": approval_note,
        });

        bounded_json(&payload, "bosun_info", "Unexpectedly large — report this.")
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for BosunServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        )
        .with_protocol_version(ProtocolVersion::default())
        .with_server_info(
            Implementation::new("bosun", env!("CARGO_PKG_VERSION"))
                .with_title("Bosun — Docker MCP server"),
        )
        .with_instructions(INSTRUCTIONS)
    }

    /// Concrete resources the agent can pull without spending a tool call.
    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        Ok(ListResourcesResult::with_all_items(vec![
            Resource::new("docker://containers", "containers")
                .with_title("Container state snapshot")
                .with_description(
                    "Compact snapshot of all containers: id, name, image, state, status, health. \
                     Same bounded shape as list_containers(all=true).",
                )
                .with_mime_type("application/json"),
        ]))
    }

    /// Parameterized resources — one per container, one per compose project.
    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, ErrorData> {
        Ok(ListResourceTemplatesResult::with_all_items(vec![
            ResourceTemplate::new("docker://container/{id}", "container")
                .with_title("Projected container inspect")
                .with_description(
                    "Projected inspect for one container. Environment variable names only — \
                     no values.",
                )
                .with_mime_type("application/json"),
            ResourceTemplate::new("docker://compose/{project}", "compose-project")
                .with_title("Compose service map")
                .with_description("Per-service state and health for one Compose project.")
                .with_mime_type("application/json"),
        ]))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        let uri = request.uri.as_str();

        let json = if uri == "docker://containers" {
            self.resource_containers().await?
        } else if let Some(id) = uri.strip_prefix("docker://container/") {
            self.resource_container(id).await?
        } else if let Some(project) = uri.strip_prefix("docker://compose/") {
            self.resource_compose(project).await?
        } else {
            return Err(ErrorData::resource_not_found(
                format!(
                    "unknown resource '{uri}'. Known: docker://containers, \
                     docker://container/{{id}}, docker://compose/{{project}}"
                ),
                None,
            ));
        };

        Ok(ReadResourceResponse::Complete(ReadResourceResult::new(
            vec![ResourceContents::text(json, uri).with_mime_type("application/json")],
        )))
    }
}

/// The full tool surface, assembled exactly as `new` assembles it.
///
/// Split out so tests can inspect the surface without needing a live daemon —
/// building a `BosunServer` requires a connection, but the router does not.
#[cfg(test)]
pub(crate) fn all_tools() -> Vec<rmcp::model::Tool> {
    let router: ToolRouter<BosunServer> = BosunServer::info_router()
        + BosunServer::read_router()
        + BosunServer::actions_router()
        + BosunServer::diagnose_router()
        + BosunServer::compose_router();
    router.list_all()
}

/// Resource bodies. These mirror the equivalent tools but return plain JSON, so
/// the two paths can't drift into disagreeing about the same container.
impl BosunServer {
    async fn resource_containers(&self) -> Result<String, ErrorData> {
        let containers = self
            .engine()
            .docker()
            .list_containers(Some(
                bollard::query_parameters::ListContainersOptionsBuilder::new()
                    .all(true)
                    .build(),
            ))
            .await
            .map_err(|e| ErrorData::internal_error(format!("list_containers failed: {e}"), None))?;

        let rows: Vec<serde_json::Value> = containers
            .iter()
            .map(|c| {
                serde_json::json!({
                    "id": crate::bound::project::short_id(c.id.as_deref().unwrap_or_default()),
                    "name": c.names.as_deref().unwrap_or_default().first()
                        .map(|n| crate::bound::project::strip_leading_slash(n))
                        .unwrap_or_default(),
                    "image": c.image,
                    "state": c.state.map(|s| format!("{s:?}").to_lowercase()),
                    "status": c.status,
                    "health": c.health.as_ref().and_then(|h| h.status)
                        .map(|s| format!("{s:?}").to_lowercase()),
                })
            })
            .collect();

        serde_json::to_string_pretty(&serde_json::json!({ "containers": rows }))
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))
    }

    async fn resource_container(&self, id: &str) -> Result<String, ErrorData> {
        let inspect = self
            .engine()
            .docker()
            .inspect_container(
                id,
                None::<bollard::query_parameters::InspectContainerOptions>,
            )
            .await
            .map_err(|e| {
                ErrorData::resource_not_found(format!("inspect '{id}' failed: {e}"), None)
            })?;

        serde_json::to_string_pretty(&crate::bound::project::project(&inspect))
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))
    }

    async fn resource_compose(&self, project: &str) -> Result<String, ErrorData> {
        let services = crate::tools::compose::project_services(self.engine(), project)
            .await
            .map_err(|e| ErrorData::internal_error(e, None))?;

        serde_json::to_string_pretty(&serde_json::json!({
            "project": project,
            "services": services,
        }))
        .map_err(|e| ErrorData::internal_error(e.to_string(), None))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::safety::{Risk, risk_of};

    /// The §6 invariant, enforced rather than documented: a tool cannot join the
    /// surface without being classified safe or destructive. Without this, a new
    /// destructive tool would ship un-gated and nothing would notice.
    #[test]
    fn every_registered_tool_is_risk_classified() {
        let unclassified: Vec<String> = all_tools()
            .iter()
            .filter(|t| risk_of(&t.name).is_none())
            .map(|t| t.name.to_string())
            .collect();

        assert!(
            unclassified.is_empty(),
            "these tools are missing a safety::risk_of classification: {unclassified:?}"
        );
    }

    /// A destructive tool must advertise itself as such to the client, and must
    /// expose both halves of the §6 contract in its schema.
    #[test]
    fn destructive_tools_declare_themselves_and_offer_both_gates() {
        for tool in all_tools() {
            if risk_of(&tool.name) != Some(Risk::Destructive) {
                continue;
            }

            let annotations = tool
                .annotations
                .as_ref()
                .unwrap_or_else(|| panic!("{} has no annotations", tool.name));
            assert_eq!(
                annotations.destructive_hint,
                Some(true),
                "{} is destructive but does not set destructive_hint",
                tool.name
            );

            let properties = tool
                .input_schema
                .get("properties")
                .unwrap_or_else(|| panic!("{} has no schema properties", tool.name));
            for gate in ["dry_run", "confirm"] {
                assert!(
                    properties.get(gate).is_some(),
                    "{} is destructive but its schema has no '{gate}' parameter",
                    tool.name
                );
            }
        }
    }

    /// Read tools must not claim to be destructive, or a cautious client will
    /// prompt for confirmation on a plain listing and the signal stops meaning
    /// anything.
    #[test]
    fn read_tools_are_marked_read_only() {
        for name in [
            "list_containers",
            "inspect_container",
            "container_logs",
            "diagnose_container",
        ] {
            let tool = all_tools()
                .into_iter()
                .find(|t| t.name == name)
                .unwrap_or_else(|| panic!("{name} is not registered"));
            assert_eq!(
                tool.annotations.as_ref().and_then(|a| a.read_only_hint),
                Some(true),
                "{name} should be marked read_only"
            );
        }
    }

    /// `container_exec` was originally excluded per HANDOFF §11, then added once
    /// real use showed the omission just pushed agents to `Bash(docker exec …)` —
    /// unbounded, unaudited and ungated. It only earns its place while it stays
    /// gated, so that is what this asserts. If exec is ever reclassified `Safe`,
    /// this test should fail and the reasoning above should be revisited.
    #[test]
    fn exec_is_exposed_but_only_because_it_is_gated() {
        let exec = all_tools()
            .into_iter()
            .find(|t| t.name == "container_exec")
            .expect("container_exec should be registered");

        assert_eq!(
            risk_of("container_exec"),
            Some(Risk::Destructive),
            "exec is arbitrary code execution and must stay gated"
        );
        assert_eq!(
            exec.annotations.as_ref().and_then(|a| a.destructive_hint),
            Some(true)
        );

        // argv-only is the other half of the safety story: a shell string would
        // mean Bosun hands user input to a shell it does not control.
        let cmd = exec.input_schema["properties"]["cmd"].clone();
        assert_eq!(
            cmd["type"], "array",
            "cmd must be an argv array, never a shell string: {cmd}"
        );
    }

    /// Tool descriptions are always-on context, so every tool needs one and the
    /// bounded reads must actually state their caps.
    #[test]
    fn every_tool_has_a_description_and_bounded_reads_state_their_caps() {
        for tool in all_tools() {
            let description = tool
                .description
                .as_ref()
                .unwrap_or_else(|| panic!("{} has no description", tool.name));
            assert!(
                description.len() > 40,
                "{} has a uselessly short description",
                tool.name
            );
        }

        let logs = all_tools()
            .into_iter()
            .find(|t| t.name == "container_logs")
            .expect("container_logs is registered");
        let description = logs.description.unwrap();
        assert!(
            description.contains("200"),
            "container_logs must state its default tail"
        );
        assert!(
            description.contains("raw=true"),
            "container_logs must name its escape hatch"
        );
    }
}
