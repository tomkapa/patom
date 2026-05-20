//! `request_user_wire_mcp` — ask the user to wire a missing MCP from
//! inside the chat thread.
//!
//! Emitted by the recruiter (or any other agent) once it decides a still-
//! unwired catalog entry would fit the role it's scoping. Publishes a
//! [`ResponseChunk::WireMcpRequest`] on the active claim's SSE stream so
//! the UI can render a click-to-wire card alongside the agent's other
//! turn output. The tool returns immediately (`{status:"requested"}`) and
//! does *not* block waiting for the wire to complete — the recruiter
//! resumes on the user's next turn, at which point a fresh `search_tools`
//! call will see the new wired state and the recruiter can include the
//! catalog id in `create_agent.allowed_mcp_tools`.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::warn;

use crate::mcp::{McpCatalogId, SharedMcpCatalogStore, SharedMcpServerStore};
use crate::runtime::{ResponseChunk, SharedResponseSink};
use crate::tools::{Tool, ToolCallContext, ToolError};
use crate::types::ToolName;

const TOOL_NAME: &str = "request_user_wire_mcp";

/// Cap on the recruiter's `reason` string. Sized for one to two short
/// sentences — enough room to cite use-case fit + ecosystem alignment +
/// pricing evidence, short enough that the wire card stays scannable.
const REASON_MAX_BYTES: usize = 512;

const TOOL_DESCRIPTION: &str = "Ask the user to wire an MCP integration from this chat. \
    Use after `search_tools` has confirmed the target catalog id exists but is not yet \
    wired in this workspace.\n\
    \n\
    Arguments:\n\
    - `catalog_id`: stable id from `search_tools` (e.g. \"notion\"). The catalog \
      entry must exist for this workspace (global built-in or tenant-custom) AND must \
      not already be wired — calling for an already-wired catalog is a mistake; just \
      include it in `create_agent.allowed_mcp_tools` instead.\n\
    - `reason`: a concrete justification the user will read on the connect card. State \
      (a) the use case it improves, (b) how it complements MCPs already wired in this \
      org, (c) confirmation it doesn't duplicate something already serving the same \
      role, and (d) pricing/fit evidence you gathered via `web_fetch` / `web_search`. \
      Capped at 512 bytes.\n\
    \n\
    Returns immediately with `{status:\"requested\"}`. The tool does not wait for the \
    wire to finish. Stop your turn after calling — the user wires the integration and \
    your next turn (triggered by the user's reply) will see the updated state via a \
    fresh `search_tools` call.";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    catalog_id: McpCatalogId,
    reason: String,
}

#[derive(Debug, Serialize)]
struct Output {
    status: &'static str,
    catalog_id: String,
}

pub struct RequestUserWireMcpTool {
    name: ToolName,
    description: &'static str,
    input_schema: Arc<Value>,
    catalog: SharedMcpCatalogStore,
    servers: SharedMcpServerStore,
    sink: SharedResponseSink,
}

impl std::fmt::Debug for RequestUserWireMcpTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RequestUserWireMcpTool")
            .finish_non_exhaustive()
    }
}

impl RequestUserWireMcpTool {
    #[must_use]
    pub fn new(
        catalog: SharedMcpCatalogStore,
        servers: SharedMcpServerStore,
        sink: SharedResponseSink,
    ) -> Self {
        let name =
            ToolName::try_from(TOOL_NAME).expect("invariant: request_user_wire_mcp valid name");
        let input_schema = Arc::new(json!({
            "type": "object",
            "required": ["catalog_id", "reason"],
            "properties": {
                "catalog_id": {
                    "type": "string",
                    "pattern": "^[a-z][a-z0-9_-]{0,39}$",
                },
                "reason": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": REASON_MAX_BYTES,
                },
            },
            "additionalProperties": false,
        }));
        Self {
            name,
            description: TOOL_DESCRIPTION,
            input_schema,
            catalog,
            servers,
            sink,
        }
    }
}

#[async_trait]
impl Tool for RequestUserWireMcpTool {
    fn name(&self) -> &ToolName {
        &self.name
    }
    fn description(&self) -> &str {
        self.description
    }
    fn input_schema(&self) -> Arc<Value> {
        self.input_schema.clone()
    }

    async fn execute(&self, input: Value, ctx: &ToolCallContext) -> Result<String, ToolError> {
        let parsed: Input = serde_json::from_value(input)?;
        if parsed.reason.trim().is_empty() {
            return Err(ToolError::InvalidInput(
                "request_user_wire_mcp: reason must not be empty".into(),
            ));
        }

        let viewer_agent_id = ctx.viewer.agent_id().ok_or_else(|| {
            ToolError::InvalidInput(
                "request_user_wire_mcp: caller must be an agent (not human)".into(),
            )
        })?;

        // Validate the catalog id resolves AND isn't already wired. The
        // two reads are independent — fan out so the model doesn't pay
        // sequential I/O on the validation path.
        let (entry, wired) = tokio::try_join!(
            async {
                self.catalog
                    .get_for_org(ctx.org_id, &parsed.catalog_id)
                    .await
                    .map_err(|e| ToolError::Backend(format!("request_user_wire_mcp catalog: {e}")))
            },
            async {
                self.servers
                    .list_for_org(ctx.org_id)
                    .await
                    .map_err(|e| ToolError::Backend(format!("request_user_wire_mcp servers: {e}")))
            },
        )?;
        let entry = entry.ok_or_else(|| {
            ToolError::InvalidInput(format!(
                "request_user_wire_mcp: catalog_id `{}` not found — check `search_tools`",
                parsed.catalog_id
            ))
        })?;
        if wired.iter().any(|s| s.catalog_id == parsed.catalog_id) {
            return Err(ToolError::InvalidInput(format!(
                "request_user_wire_mcp: `{}` is already wired — include it in \
                 create_agent.allowed_mcp_tools instead",
                parsed.catalog_id
            )));
        }

        let chunk = ResponseChunk::WireMcpRequest {
            from: viewer_agent_id,
            catalog_id: parsed.catalog_id.clone(),
            display_name: entry.display_name.as_str().to_owned(),
            reason: parsed.reason,
            auth_kind: entry.auth_kind,
            homepage_url: entry.homepage_url,
        };

        // Publish on the active claim's SSE stream so the UI sees the
        // request mid-turn (not waiting for `Done`). Uses the
        // tenant-scoped `publish_for_user` path so the
        // `prompt_response_chunks` insert is RLS-checked against the
        // session's owning user — same pattern as `send_message`.
        self.sink
            .publish_for_user(ctx.acting_user_id, ctx.request_id, chunk)
            .await
            .map_err(|e| {
                warn!(error = %e, "request_user_wire_mcp.publish.failed");
                ToolError::Backend(format!("request_user_wire_mcp publish: {e}"))
            })?;

        let out = Output {
            status: "requested",
            catalog_id: parsed.catalog_id.as_str().to_owned(),
        };
        Ok(serde_json::to_string(&out)?)
    }
}
