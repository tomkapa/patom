//! `search_colleague` — unified semantic search over the org's colleagues:
//! agents (by their operator-curated `description`) and profiled humans (by
//! their shared `colleague_profiles` board), ranked together by cosine
//! similarity (issue #183, doc/colleague-profiles-and-search-plan.md §3.5).
//!
//! Supersedes the agent-only `search_agents`: one path, one ranked list. Used
//! when the names in the `<colleagues>` / `<participants>` blocks and the
//! agent's Collaborator memories don't settle who to involve. Each hit carries
//! the colleague's id, so the model can act on it directly — e.g. pull a
//! non-thread colleague in via `send_message { to }`. The caller is excluded
//! (§9.4 — caller-excluded consistently across `<colleagues>`, `search_colleague`,
//! and `send_message`); humans without a profile are invisible until profiled.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::warn;

use crate::colleagues::{
    ColleagueId, DEFAULT_SEARCH_COLLEAGUE_K, SEARCH_COLLEAGUE_K, SharedProfileStore,
};
use crate::provider::{SharedEmbeddingProvider, embed_one};
use crate::tools::{Tool, ToolCallContext, ToolError};
use crate::types::{ParseError, ToolName};

const TOOL_NAME: &str = "search_colleague";

const TOOL_DESCRIPTION: &str = "Find a colleague — an agent OR a person — to involve when the \
    names in your `<colleagues>` / `<participants>` blocks and your Collaborator memories don't \
    obviously match the task. Returns top-K colleagues (default 4, max 8) ranked by similarity \
    between `query` and each one's profile: an agent's operator-curated description, or a \
    person's shared role/expertise/preferences board.\n\
    \n\
    Each result includes the colleague's `id`, so you can address them directly with \
    `send_message` (by id) even if they are not already in this thread.\n\
    \n\
    Use sparingly. Most of the time a name your role prompt, `<memory>`, or `<colleagues>` block \
    already names will do. People who have no profile yet won't appear here — that's expected.\n\
    \n\
    Arguments: `query` (free text describing the work or the kind of person you need), optional \
    `limit` (1..=8, default 4). Results exclude you.";

/// Bounded top-K for `search_colleague`. Parsed at the JSON boundary — holding a
/// [`SearchColleagueLimit`] proves the value is in `1..=SEARCH_COLLEAGUE_K`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SearchColleagueLimit(u8);

impl SearchColleagueLimit {
    const DEFAULT: Self = Self(DEFAULT_SEARCH_COLLEAGUE_K);

    fn get(self) -> u8 {
        self.0
    }
}

impl Default for SearchColleagueLimit {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl TryFrom<u8> for SearchColleagueLimit {
    type Error = ParseError;
    fn try_from(n: u8) -> Result<Self, Self::Error> {
        if n == 0 || n > SEARCH_COLLEAGUE_K {
            return Err(ParseError::OutOfRange {
                field: "search_colleague_limit",
                detail: "1..=SEARCH_COLLEAGUE_K",
            });
        }
        Ok(Self(n))
    }
}

impl<'de> serde::Deserialize<'de> for SearchColleagueLimit {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let n = u8::deserialize(d)?;
        Self::try_from(n).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Deserialize)]
struct Input {
    query: String,
    #[serde(default)]
    limit: SearchColleagueLimit,
}

#[derive(Debug, Serialize)]
struct OutputItem {
    /// `"agent"` or `"human"` — what backs this colleague.
    kind: &'static str,
    /// The colleague id to address via `send_message`.
    id: ColleagueId,
    name: String,
    snippet: String,
}

#[derive(Debug, Serialize)]
struct Output {
    matches: Vec<OutputItem>,
}

pub struct SearchColleagueTool {
    name: ToolName,
    description: &'static str,
    input_schema: Arc<Value>,
    profiles: SharedProfileStore,
    embeddings: SharedEmbeddingProvider,
}

impl std::fmt::Debug for SearchColleagueTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SearchColleagueTool")
            .finish_non_exhaustive()
    }
}

impl SearchColleagueTool {
    #[must_use]
    pub fn new(profiles: SharedProfileStore, embeddings: SharedEmbeddingProvider) -> Self {
        let name = ToolName::try_from(TOOL_NAME).expect("invariant: valid tool name");
        let input_schema = Arc::new(json!({
            "type": "object",
            "required": ["query"],
            "properties": {
                "query": { "type": "string", "minLength": 1, "maxLength": 1024 },
                "limit": { "type": "integer", "minimum": 1, "maximum": SEARCH_COLLEAGUE_K },
            },
            "additionalProperties": false,
        }));
        Self {
            name,
            description: TOOL_DESCRIPTION,
            input_schema,
            profiles,
            embeddings,
        }
    }
}

#[async_trait]
impl Tool for SearchColleagueTool {
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
        let viewer = ctx.viewer.colleague_id().ok_or_else(|| {
            ToolError::Backend("search_colleague invoked with non-colleague viewer".into())
        })?;

        let embedding = embed_one(self.embeddings.as_ref(), &parsed.query)
            .await
            .map_err(|e| {
                warn!(error = %e, "search_colleague.embed.error");
                ToolError::Backend(format!("embedding query failed: {e}"))
            })?;

        let k = usize::from(parsed.limit.get());
        let cards = self
            .profiles
            .search_colleagues(&embedding, viewer, k)
            .await
            .map_err(|e| ToolError::Backend(format!("search_colleague store: {e}")))?;

        let matches = cards
            .into_iter()
            .map(|c| OutputItem {
                kind: c.kind.as_str(),
                id: c.colleague_id,
                name: c.name.as_str().to_owned(),
                snippet: c.snippet,
            })
            .collect();
        Ok(serde_json::to_string(&Output { matches })?)
    }
}
