//! `search_agents` — semantic search over agents' descriptions
//! (doc/agent_discovery_plan.md §7).
//!
//! Used when the `<agents>` name index and Collaborator memory entries do
//! not settle which peer to delegate to — typically a genuinely new kind
//! of task. Returns top-K `{name, description}` cards. The caller is
//! excluded (§9.4 — caller-excluded consistently across `<agents>`,
//! `search_agents`, and `send_message`).
//!
//! Per design §7 we deliberately do NOT add `get_agent_card(name)` or
//! `list_agents` tools: the model has enough to decide from
//! `{name, description}`, and the `<agents>` block already exposes the
//! full bag of names.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::warn;

use crate::agents::{DEFAULT_SEARCH_AGENT_RESULTS, MAX_SEARCH_AGENT_RESULTS, SharedAgentStore};
use crate::provider::{SharedEmbeddingProvider, embed_one};
use crate::tools::{Tool, ToolCallContext, ToolError};
use crate::types::{ParseError, ToolName};

const TOOL_NAME: &str = "search_agents";

const TOOL_DESCRIPTION: &str = "Find an agent to delegate to when the names in your \
    `<agents>` block and your Collaborator memories don't obviously match the task. \
    Returns top-K agents (default 4, max 8) ranked by similarity between `query` and \
    each agent's operator-curated description.\n\
    \n\
    Use sparingly. Most delegations should hit a name your role prompt names, your \
    `<memory>` records, or your `<agents>` block recognises. Reach for `search_agents` \
    only when none of those settle the question.\n\
    \n\
    After a successful delegation that started with `search_agents`, write a \
    `memory_write(kind=\"collaborator\", ...)` recording who you used and why so \
    future-you skips the search.\n\
    \n\
    Arguments: `query` (free text describing the work that needs doing), optional \
    `limit` (1..=8, default 4). Results exclude you — `search_agents` never returns \
    the caller.";

/// Bounded top-K for the `search_agents` tool. Parsed at the JSON
/// boundary — holding a [`SearchAgentLimit`] proves the value is in
/// `1..=MAX_SEARCH_AGENT_RESULTS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SearchAgentLimit(u8);

impl SearchAgentLimit {
    const DEFAULT: Self = Self(DEFAULT_SEARCH_AGENT_RESULTS);

    fn get(self) -> u8 {
        self.0
    }
}

impl Default for SearchAgentLimit {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl TryFrom<u8> for SearchAgentLimit {
    type Error = ParseError;
    fn try_from(n: u8) -> Result<Self, Self::Error> {
        if n == 0 || n > MAX_SEARCH_AGENT_RESULTS {
            return Err(ParseError::OutOfRange {
                field: "search_agents_limit",
                detail: "1..=MAX_SEARCH_AGENT_RESULTS",
            });
        }
        Ok(Self(n))
    }
}

impl<'de> serde::Deserialize<'de> for SearchAgentLimit {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let n = u8::deserialize(d)?;
        Self::try_from(n).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Deserialize)]
struct Input {
    query: String,
    #[serde(default)]
    limit: SearchAgentLimit,
}

#[derive(Debug, Serialize)]
struct OutputItem {
    name: String,
    description: String,
}

#[derive(Debug, Serialize)]
struct Output {
    matches: Vec<OutputItem>,
}

pub struct SearchAgentsTool {
    name: ToolName,
    description: &'static str,
    input_schema: Arc<Value>,
    agents: SharedAgentStore,
    embeddings: SharedEmbeddingProvider,
}

impl std::fmt::Debug for SearchAgentsTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SearchAgentsTool").finish_non_exhaustive()
    }
}

impl SearchAgentsTool {
    #[must_use]
    pub fn new(agents: SharedAgentStore, embeddings: SharedEmbeddingProvider) -> Self {
        let name = ToolName::try_from(TOOL_NAME).expect("invariant: valid tool name");
        let input_schema = Arc::new(json!({
            "type": "object",
            "required": ["query"],
            "properties": {
                "query": { "type": "string", "minLength": 1, "maxLength": 1024 },
                "limit": { "type": "integer", "minimum": 1, "maximum": MAX_SEARCH_AGENT_RESULTS },
            },
            "additionalProperties": false,
        }));
        Self {
            name,
            description: TOOL_DESCRIPTION,
            input_schema,
            agents,
            embeddings,
        }
    }
}

#[async_trait]
impl Tool for SearchAgentsTool {
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

    async fn execute(&self, input: Value, ctx: &ToolCallContext) -> Result<String, ToolError> {
        let parsed: Input = serde_json::from_value(input)?;
        let viewer_id = ctx.viewer.agent_id().ok_or_else(|| {
            ToolError::Backend("search_agents invoked with non-agent viewer".into())
        })?;

        let embedding = embed_one(self.embeddings.as_ref(), &parsed.query)
            .await
            .map_err(|e| {
                warn!(error = %e, "search_agents.embed.error");
                ToolError::Backend(format!("embedding query failed: {e}"))
            })?;

        let k = usize::from(parsed.limit.get());
        let cards = self
            .agents
            .search_by_description(&embedding, viewer_id, k)
            .await
            .map_err(|e| ToolError::Backend(format!("search_agents store: {e}")))?;

        let matches = cards
            .into_iter()
            .map(|c| OutputItem {
                name: c.name.as_str().to_owned(),
                description: c.description.as_str().to_owned(),
            })
            .collect();
        Ok(serde_json::to_string(&Output { matches })?)
    }
}
