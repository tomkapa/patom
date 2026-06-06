//! `recall` — embedding-driven memory retrieval (doc/memory.md §1.4).
//!
//! Lets the agent ask for additional memories beyond what the contextual
//! layer surfaced at session start. Embedding-driven similarity search;
//! per-turn rate limit so a stuck loop cannot spam.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::warn;

use crate::memory::{
    MAX_RECALL_CALLS_PER_TURN, MemoryId, MemoryKind, RECALL_MAX_RESULTS, RecallLimit, SearchFilter,
};
use crate::provider::{SharedEmbeddingProvider, embed_one};
use crate::tools::{Tool, ToolCallContext, ToolError};
use crate::types::ToolName;

use super::{MemoryToolDeps, expect_agent, store_to_tool_err};

const TOOL_NAME: &str = "recall";

const TOOL_DESCRIPTION: &str = "Search your private memory by similarity to a free-text query. Returns up to \
     `limit` matching memories with their stable handles, kinds, and states. Use when \
     the conversation hits a topic you suspect you've remembered something about but \
     the `## Memory` section in your system prompt didn't surface it.\n\
     \n\
     Arguments: `query` (free text); optional `kind` filter (\"self\", \"other\", \
     \"procedure\", \"open\"); optional `limit` (1..=8, default 4).\n\
     \n\
     Recall is read-only: surfacing a memory does not validate it. Promotion needs \
     an independent signal (cross-session re-write, operator endorsement).";

#[derive(Debug, Deserialize)]
struct Input {
    query: String,
    #[serde(default)]
    kind: Option<MemoryKind>,
    /// 1..=`RECALL_MAX_RESULTS`. Defaults to `RecallLimit::DEFAULT` when
    /// the field is absent. Out-of-range values fail at parse time.
    #[serde(default)]
    limit: RecallLimit,
}

#[derive(Debug, Serialize)]
struct OutputItem {
    memory_id: MemoryId,
    kind: &'static str,
    state: &'static str,
    content: String,
    similarity: f32,
}

#[derive(Debug, Serialize)]
struct Output {
    matches: Vec<OutputItem>,
}

pub struct RecallTool {
    name: ToolName,
    description: &'static str,
    input_schema: Arc<Value>,
    deps: MemoryToolDeps,
    embeddings: SharedEmbeddingProvider,
    /// Per-turn call counter. Recall reads, doesn't mutate, so spending
    /// the mutation budget on it would surprise the model — it has its
    /// own.
    call_counter: Arc<super::PerTurnCallCounter>,
}

impl std::fmt::Debug for RecallTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecallTool").finish_non_exhaustive()
    }
}

impl RecallTool {
    #[must_use]
    pub fn new(deps: MemoryToolDeps, embeddings: SharedEmbeddingProvider) -> Self {
        let name = ToolName::try_from(TOOL_NAME).expect("valid tool name");
        let input_schema = Arc::new(json!({
            "type": "object",
            "required": ["query"],
            "properties": {
                "query": { "type": "string", "minLength": 1, "maxLength": 1024 },
                "kind": { "type": "string", "enum": ["self", "other", "collaborator", "procedure", "open"] },
                "limit": { "type": "integer", "minimum": 1, "maximum": RECALL_MAX_RESULTS }
            },
            "additionalProperties": false,
        }));
        Self {
            name,
            description: TOOL_DESCRIPTION,
            input_schema,
            deps,
            embeddings,
            call_counter: Arc::new(super::PerTurnCallCounter::with_cap(
                MAX_RECALL_CALLS_PER_TURN,
            )),
        }
    }
}

#[async_trait]
impl Tool for RecallTool {
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
        let agent = expect_agent(ctx)?;

        if self.call_counter.try_increment(ctx.request_id).is_err() {
            return Err(ToolError::InvalidInput(format!(
                "recall call cap exceeded for this turn (max {MAX_RECALL_CALLS_PER_TURN} calls)"
            )));
        }

        let limit = parsed.limit.get();

        let query_embedding = embed_one(self.embeddings.as_ref(), &parsed.query)
            .await
            .map_err(|e| {
                warn!(error = %e, "recall.embed.error");
                ToolError::Backend(format!("embedding query failed: {e}"))
            })?;

        let filter = SearchFilter {
            kinds: parsed.kind.map(|k| vec![k]),
            min_state: None,
        };
        let results = self
            .deps
            .store()
            .search_by_embedding(agent, &query_embedding, usize::from(limit), filter)
            .await
            .map_err(store_to_tool_err)?;

        // Reading does NOT advance validation (doc/memory.md §1.7) — only
        // the access counter and last_accessed_at update.
        let ids: Vec<MemoryId> = results.iter().map(|s| s.row.id).collect();
        if let Err(e) = self
            .deps
            .store()
            .record_access_for_user(ctx.acting_user_id, &ids)
            .await
        {
            warn!(error = %e, "recall.record_access.error");
        }

        let items = results
            .into_iter()
            .map(|scored| OutputItem {
                memory_id: scored.row.id,
                kind: scored.row.kind.as_str(),
                state: scored.row.state.as_str(),
                content: scored.row.content.as_str().to_owned(),
                similarity: scored.similarity,
            })
            .collect();

        Ok(serde_json::to_string(&Output { matches: items })?)
    }
}
