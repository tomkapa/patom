//! `search_tools` — enumerate the system's MCP catalog for the recruiter.
//!
//! Returns every catalog entry visible to the caller's org (global
//! built-ins + tenant-custom). For each entry, surfaces whether the tenant
//! has wired it; when wired, includes the cached `discovered_tools` list
//! so the recruiter can reason about exposed capability without a second
//! round-trip.
//!
//! No filter / query parameters: the catalog is small (<10 entries v1)
//! and the recruiter is expected to pick semantically.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Serialize;
use serde_json::{Value, json};
use tracing::{error, instrument};

use crate::auth::OrgId;
use crate::mcp::{
    McpAuthKind, McpCatalogEntry, McpCatalogId, McpServerRecord, SharedMcpCatalogStore,
    SharedMcpServerStore,
};
use crate::tools::{Tool, ToolCallContext, ToolError};
use crate::types::ToolName;

const TOOL_NAME: &str = "search_tools";

/// Belt-and-braces ceiling on the rows returned to the model — the
/// catalog is small today but the cap defends against an unbounded
/// future growth surprising the per-turn budget.
const MAX_CATALOG_RESULTS: usize = 64;

const TOOL_DESCRIPTION: &str = "List every MCP integration the system understands, with \
    whether it's already wired into this workspace. Returns each entry's `catalog_id` \
    (stable string id like \"notion\" — use this when populating `allowed_mcp_tools` on \
    `create_agent`), `display_name`, `description`, `auth_kind`, `wired` (bool), and — \
    when wired — `exposed_tools` (the cached `{name, description}` list from the last \
    successful refresh).\n\
    \n\
    Call this when scoping a new hire's MCP access. For each plausible match decide:\n\
    - already wired → include `catalog_id` in `create_agent.allowed_mcp_tools` with the \
      specific remote tool names that fit the use case.\n\
    - not wired but fits → do the homework (open `homepage_url` via `web_fetch`, run \
      `web_search` for current pricing model and exposed tool surface), then call \
      `request_user_wire_mcp` with a concrete `reason`. Stop and wait for the user.\n\
    \n\
    No arguments. Results are stable-ordered by `catalog_id`.";

#[derive(Debug, Serialize)]
struct ExposedTool {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
}

#[derive(Debug, Serialize)]
struct OutputItem {
    catalog_id: String,
    display_name: String,
    description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    homepage_url: Option<String>,
    auth_kind: McpAuthKind,
    wired: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    exposed_tools: Option<Vec<ExposedTool>>,
}

#[derive(Debug, Serialize)]
struct Output {
    catalog: Vec<OutputItem>,
}

pub struct SearchToolsTool {
    name: ToolName,
    description: &'static str,
    input_schema: Arc<Value>,
    catalog: SharedMcpCatalogStore,
    servers: SharedMcpServerStore,
}

impl std::fmt::Debug for SearchToolsTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SearchToolsTool").finish_non_exhaustive()
    }
}

impl SearchToolsTool {
    #[must_use]
    pub fn new(catalog: SharedMcpCatalogStore, servers: SharedMcpServerStore) -> Self {
        let name = ToolName::try_from(TOOL_NAME).expect("invariant: search_tools valid name");
        let input_schema = Arc::new(json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false,
        }));
        Self {
            name,
            description: TOOL_DESCRIPTION,
            input_schema,
            catalog,
            servers,
        }
    }

    async fn collect(&self, org_id: OrgId) -> Result<Output, ToolError> {
        // Catalog + wired-servers reads are independent — fan out to halve
        // the tool's per-call latency. RLS already scopes both to the
        // caller's slice.
        let (entries, wired) = tokio::try_join!(
            async {
                self.catalog.list_for_org(org_id).await.map_err(|e| {
                    error!(error = ?e, "search_tools.catalog.failed");
                    ToolError::Backend(format!("search_tools catalog: {e}"))
                })
            },
            async {
                self.servers.list_for_org(org_id).await.map_err(|e| {
                    error!(error = ?e, "search_tools.servers.failed");
                    ToolError::Backend(format!("search_tools servers: {e}"))
                })
            },
        )?;

        // Index wired servers by catalog_id for O(1) join. Use a `HashMap`
        // keyed by the cheap-clone `McpCatalogId` (Arc<str>).
        let mut wired_index: HashMap<McpCatalogId, &McpServerRecord> = HashMap::new();
        for row in &wired {
            wired_index.insert(row.catalog_id.clone(), row);
        }

        let mut out: Vec<OutputItem> = Vec::with_capacity(entries.len().min(MAX_CATALOG_RESULTS));
        for entry in entries.into_iter().take(MAX_CATALOG_RESULTS) {
            out.push(build_item(entry, &wired_index));
        }
        Ok(Output { catalog: out })
    }
}

fn build_item(
    entry: McpCatalogEntry,
    wired_index: &HashMap<McpCatalogId, &McpServerRecord>,
) -> OutputItem {
    let wired_row = wired_index.get(&entry.id).copied();
    let exposed_tools = wired_row.and_then(|row| {
        row.discovered_tools.as_ref().map(|tools| {
            tools
                .iter()
                .map(|t| ExposedTool {
                    name: t.remote_name.clone(),
                    description: t.description.clone(),
                })
                .collect()
        })
    });
    OutputItem {
        catalog_id: entry.id.as_str().to_owned(),
        display_name: entry.display_name.as_str().to_owned(),
        description: entry.description.as_str().to_owned(),
        homepage_url: entry.homepage_url,
        auth_kind: entry.auth_kind,
        wired: wired_row.is_some(),
        exposed_tools,
    }
}

#[async_trait]
impl Tool for SearchToolsTool {
    fn name(&self) -> &ToolName {
        &self.name
    }
    fn description(&self) -> &str {
        self.description
    }
    fn input_schema(&self) -> Arc<Value> {
        self.input_schema.clone()
    }

    fn concurrency_safe(&self) -> bool {
        true
    }

    #[instrument(
        name = "tool.search_tools",
        skip_all,
        fields(relay.org.id = %ctx.org_id),
        err,
    )]
    async fn execute(&self, input: Value, ctx: &ToolCallContext) -> Result<String, ToolError> {
        // Schema says no arguments — reject unexpected keys at runtime so
        // a non-HTTP caller can't slip past `additionalProperties: false`.
        let empty = match &input {
            Value::Null => true,
            Value::Object(o) => o.is_empty(),
            _ => false,
        };
        if !empty {
            return Err(ToolError::InvalidInput(
                "search_tools: no arguments accepted".into(),
            ));
        }
        let out = self.collect(ctx.org_id).await?;
        Ok(serde_json::to_string(&out)?)
    }
}
