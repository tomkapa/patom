//! `create_agent` — hire a new agent into the registry.
//!
//! The recruiter (default agent) reaches for this tool after scoping the
//! role with the customer, ruling out the existing `<colleagues>` block + top
//! `search_agents` result, and getting an explicit "go" on the draft.
//!
//! Per design, the tool:
//! * Always writes `is_default = false` — promoting the default stays an
//!   operator action via `PUT /agents/{id}` (doc/agent_discovery_plan.md §6).
//! * Accepts the operator-curated `allowed_mcp_tools` allowlist; empty is
//!   the lockdown default. Dangling ids are inert at runtime — the
//!   `McpRegistry` filters live tools — so the tool does not pre-validate
//!   them.
//! * Funnels every input through the agent newtypes' `TryFrom` smart
//!   constructors (CLAUDE.md §1: parse, don't validate).
//! * Surfaces `NameTaken` from the store as `InvalidInput` so the model can
//!   self-correct on the next turn rather than retrying the same arguments
//!   (CLAUDE.md §12 boundary: typed error in, typed error out).

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::{error, info};

use crate::agents::{
    AGENT_DESCRIPTION_MAX_LEN, AGENT_NAME_MAX_LEN, AGENT_SYSTEM_PROMPT_MAX_LEN, AgentDescription,
    AgentId, AgentName, AgentStoreError, AgentSystemPrompt, AllowedMcpTools,
    MAX_ALLOWED_MCP_CATALOGS_PER_AGENT, MAX_ALLOWED_MCP_TOOLS_PER_CATALOG_PER_AGENT, NewAgent,
    SharedAgentStore,
};
use crate::mcp::{MCP_CATALOG_ID_MAX_LEN, MCP_TOOL_REMOTE_NAME_MAX_LEN};
use crate::tools::{RequestKindModes, Tool, ToolCallContext, ToolError};
use crate::types::ToolName;

const TOOL_NAME: &str = "create_agent";
const TOOL_DESCRIPTION: &str = "Hire a new agent into the team. Use this only after \
    you've ruled out the names in your `<colleagues>` block and the top `search_agents` \
    result, and only after the customer has explicitly approved the draft.\n\
    \n\
    Inputs:\n\
    - `name`: role-shaped, lowercase, 1–64 chars, unique case-insensitively.\n\
    - `system_prompt`: the role's voice and scope, **with** the onboarding section — \
      who the hire reports to, the named peers they should `send_message` for help and \
      what each peer is good at, the escalation order, and the kinds of things the \
      hire should pay attention to and remember as they work.\n\
    - `description`: one sentence other agents read when deciding whether to delegate \
      here. Operator-facing, model-readable; embedded for `search_agents`.\n\
    - `allowed_mcp_tools` (optional): object mapping MCP catalog id \
      (e.g. \"notion\", \"linear\" — see `search_tools`) to either \
      `null` (= every tool from that catalog's wired connection) or an \
      array of remote tool names (= only those tools). Default empty \
      `{}` = no MCP access. Catalog ids that the tenant hasn't wired \
      are inert; ask the user to wire via `request_user_wire_mcp` first.\n\
    \n\
    The created agent is never the default. After it's created, tell the customer to \
    open a new session with the returned name — do not `send_message` the new hire \
    from this session.";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    name: String,
    system_prompt: String,
    description: String,
    #[serde(default)]
    allowed_mcp_tools: AllowedMcpTools,
    // `model` is intentionally absent here. Per-agent model selection is an
    // operator decision and stays on the HTTP/web-UI surface; agents minted
    // by the `create_agent` tool always inherit the workspace default. This
    // keeps the recruiter from quietly escalating its hires onto an
    // expensive backend.
}

#[derive(Debug, Serialize)]
struct Output {
    agent_id: AgentId,
    name: AgentName,
}

pub struct CreateAgentTool {
    name: ToolName,
    description: &'static str,
    input_schema: Arc<Value>,
    agents: SharedAgentStore,
}

impl std::fmt::Debug for CreateAgentTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CreateAgentTool").finish_non_exhaustive()
    }
}

impl CreateAgentTool {
    #[must_use]
    pub fn new(agents: SharedAgentStore) -> Self {
        let name = ToolName::try_from(TOOL_NAME).expect("invariant: create_agent valid name");
        let input_schema = Arc::new(json!({
            "type": "object",
            "required": ["name", "system_prompt", "description"],
            "properties": {
                "name": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": AGENT_NAME_MAX_LEN,
                },
                "system_prompt": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": AGENT_SYSTEM_PROMPT_MAX_LEN,
                },
                "description": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": AGENT_DESCRIPTION_MAX_LEN,
                },
                "allowed_mcp_tools": {
                    "type": "object",
                    "description": "Map MCP catalog id to `null` (all tools) or array of remote tool names.",
                    "maxProperties": MAX_ALLOWED_MCP_CATALOGS_PER_AGENT,
                    "propertyNames": {
                        "type": "string",
                        "pattern": "^[a-z][a-z0-9_-]{0,39}$",
                        "maxLength": MCP_CATALOG_ID_MAX_LEN,
                    },
                    "additionalProperties": {
                        "oneOf": [
                            { "type": "null" },
                            {
                                "type": "array",
                                "maxItems": MAX_ALLOWED_MCP_TOOLS_PER_CATALOG_PER_AGENT,
                                "uniqueItems": true,
                                "items": {
                                    "type": "string",
                                    "minLength": 1,
                                    "maxLength": MCP_TOOL_REMOTE_NAME_MAX_LEN,
                                },
                            },
                        ],
                    },
                },
            },
            "additionalProperties": false,
        }));
        Self {
            name,
            description: TOOL_DESCRIPTION,
            input_schema,
            agents,
        }
    }

    #[tracing::instrument(
        skip_all,
        name = "tool.create_agent",
        fields(
            patom.from.viewer = %ctx.viewer,
            patom.create_agent.outcome = tracing::field::Empty,
            patom.create_agent.id = tracing::field::Empty,
        ),
    )]
    async fn handle(&self, input: Input, ctx: &ToolCallContext) -> Result<Output, ToolError> {
        let viewer_agent_id = ctx.viewer.agent_id().ok_or_else(|| {
            set_outcome(Outcome::InvalidInput);
            ToolError::InvalidInput("create_agent: caller must be an agent".into())
        })?;

        let parse = |field: &str, e: crate::types::ParseError| {
            set_outcome(Outcome::InvalidInput);
            ToolError::InvalidInput(format!("create_agent: {field}: {e}"))
        };
        let name = AgentName::try_from(input.name).map_err(|e| parse("name", e))?;
        let system_prompt = AgentSystemPrompt::try_from(input.system_prompt)
            .map_err(|e| parse("system_prompt", e))?;
        let description =
            AgentDescription::try_from(input.description).map_err(|e| parse("description", e))?;
        // `allowed_mcp_tools` is validated by `AllowedMcpTools`'s
        // `Deserialize` impl at the JSON boundary — a malformed payload
        // returns `ToolError::InvalidInput` via the `serde_json::from_value`
        // call site below, not here.
        let allowed_mcp_tools = input.allowed_mcp_tools;

        // Hire into the calling agent's org. The viewer is always an
        // agent (guarded above), and every agent row has a NOT NULL
        // `org_id`, so this read is the single source of truth for which
        // tenant the new hire belongs to.
        let viewer_record = self.agents.read(viewer_agent_id).await.map_err(|e| {
            set_outcome(Outcome::BackendError);
            error!(error = ?e, event = "create_agent.viewer_lookup_failed");
            ToolError::Backend(format!("create_agent: caller lookup: {e}"))
        })?;

        // `is_default` and `model` are intentionally not on the input schema
        // and not patched here — both stay operator decisions via
        // `PUT /agents/{id}`. The recruiter cannot pick an LLM backend for a
        // hire; new hires always inherit the workspace default model.
        let payload = NewAgent {
            org_id: viewer_record.org_id,
            name,
            system_prompt,
            description,
            allowed_mcp_tools,
            model: None,
            // Recruiter-minted hires start with no avatar; the operator
            // sets one later via `PUT /agents/{id}` (issue #43).
            avatar_url: None,
            edited_by: Some(ctx.acting_user_id),
        };

        // Every input field is pre-parsed via its newtype `TryFrom` above,
        // so `AgentStoreError::Parse` from `create` can only mean an
        // internal invariant violation — let it fall through to the
        // backend-error arm with full context.
        let record = match self
            .agents
            .create_for_user(ctx.acting_user_id, payload)
            .await
        {
            Ok(r) => r,
            Err(AgentStoreError::NameTaken(taken)) => {
                set_outcome(Outcome::NameTaken);
                return Err(ToolError::InvalidInput(format!(
                    "create_agent: name {taken} is already taken; pick a different role name"
                )));
            }
            Err(e) => {
                set_outcome(Outcome::BackendError);
                error!(error = ?e, event = "create_agent.create_failed");
                return Err(ToolError::Backend(format!("create_agent: {e}")));
            }
        };

        tracing::Span::current()
            .record("patom.create_agent.id", tracing::field::display(record.id));
        set_outcome(Outcome::Created);
        info!(
            patom.agent.id = %record.id,
            patom.agent.name = %record.name,
            "create_agent.created",
        );
        Ok(Output {
            agent_id: record.id,
            name: record.name,
        })
    }
}

#[async_trait]
impl Tool for CreateAgentTool {
    fn name(&self) -> &ToolName {
        &self.name
    }
    fn description(&self) -> &str {
        self.description
    }
    fn input_schema(&self) -> Arc<Value> {
        self.input_schema.clone()
    }
    fn modes(&self) -> RequestKindModes {
        // Hiring is a deliberate, customer-facing action. Reflection and
        // resolution turns reason over the agent's own memory and have no
        // legitimate use for adding new teammates — keep them off this seam.
        RequestKindModes::NORMAL
    }
    async fn execute(&self, input: Value, ctx: &ToolCallContext) -> Result<String, ToolError> {
        let parsed: Input = serde_json::from_value(input)?;
        let out = self.handle(parsed, ctx).await?;
        Ok(serde_json::to_string(&out)?)
    }
}

/// Outcome label recorded on the `tool.create_agent` span. Mirrors the
/// `TaskOutcome` shape in `schedule_task` so dashboards `GROUP BY outcome`
/// without per-tool casing; the enum + typed setter prevents drift between
/// call sites.
#[derive(Debug, Clone, Copy)]
enum Outcome {
    Created,
    InvalidInput,
    NameTaken,
    BackendError,
}

impl Outcome {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::InvalidInput => "invalid_input",
            Self::NameTaken => "name_taken",
            Self::BackendError => "backend_error",
        }
    }
}

fn set_outcome(outcome: Outcome) {
    tracing::Span::current().record("patom.create_agent.outcome", outcome.as_str());
}
