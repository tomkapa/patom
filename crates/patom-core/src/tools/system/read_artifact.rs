use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::instrument;

use crate::threads::{ArtifactHandle, ArtifactSelector, SharedThreadStore};
use crate::types::ToolName;

use super::super::limits::MAX_ARTIFACT_SLICE;
use super::super::traits::{Tool, ToolCallContext, ToolError};

/// Default cap on grep matches counted/windowed in one `read_artifact` call.
const DEFAULT_GREP_MATCHES: usize = 16;

/// Recover exact slices of a reduced + offloaded tool result (#185).
///
/// The companion to the dispatch-seam reducer: a heavy result leaves a bounded
/// preview/summary in the feed carrying an artifact `handle`; this tool reads
/// the full body back, by page or by grep, on demand.
///
/// Recursion fixpoint: its own output is bounded by `MAX_ARTIFACT_SLICE` (well
/// under the reduction threshold), so a `read_artifact` result is never itself
/// offloaded — chunk-by-chunk reads terminate.
#[derive(Debug)]
pub struct ReadArtifactTool {
    name: ToolName,
    schema: Arc<Value>,
    threads: SharedThreadStore,
}

impl ReadArtifactTool {
    #[must_use]
    pub fn new(threads: SharedThreadStore) -> Self {
        Self {
            name: ToolName::try_from("read_artifact").expect("static name is valid"),
            schema: Arc::new(json!({
                "type": "object",
                "properties": {
                    "handle": {
                        "type": "string",
                        "description": "The artifact handle shown in a reduced tool result."
                    },
                    "offset": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Character offset to start reading (paginate mode). Default 0."
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Max characters to return (paginate mode); capped server-side."
                    },
                    "grep": {
                        "type": "string",
                        "description": "Instead of paging, return the window starting at the first \
                            occurrence of this literal substring."
                    }
                },
                "required": ["handle"],
                "additionalProperties": false
            })),
            threads,
        }
    }
}

#[derive(Debug, Deserialize)]
struct Input {
    handle: String,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    grep: Option<String>,
}

#[async_trait]
impl Tool for ReadArtifactTool {
    fn name(&self) -> &ToolName {
        &self.name
    }

    fn description(&self) -> &str {
        "Read an exact slice of a large tool result that was offloaded (its body \
         was reduced to a preview/summary carrying an `artifact <handle>`). Page \
         with `offset`/`limit`, or jump to a match with `grep`. Use this to recover \
         any part of a result you only saw a preview of."
    }

    fn input_schema(&self) -> Arc<Value> {
        self.schema.clone()
    }

    fn concurrency_safe(&self) -> bool {
        true
    }

    // No `result_policy` override: a slice is always ≤ `MAX_ARTIFACT_SLICE`
    // (< the reduction threshold), so it never reaches the reduce seam — the
    // size gate is the recursion fixpoint, not the policy (#185).

    #[instrument(name = "tool.read_artifact", skip_all, fields(patom.tool = "read_artifact"))]
    async fn execute(&self, input: Value, ctx: &ToolCallContext) -> Result<String, ToolError> {
        let Input {
            handle,
            offset,
            limit,
            grep,
        } = serde_json::from_value(input)
            .map_err(|e| ToolError::InvalidInput(format!("read_artifact: {e}")))?;

        let handle = ArtifactHandle::try_from(handle.as_str())
            .map_err(|e| ToolError::InvalidInput(format!("read_artifact: bad handle: {e}")))?;

        let selector = match grep {
            Some(pattern) if !pattern.is_empty() => ArtifactSelector::Grep {
                pattern,
                max_matches: DEFAULT_GREP_MATCHES,
            },
            _ => ArtifactSelector::Page {
                offset: offset.unwrap_or(0),
                limit: limit.unwrap_or(MAX_ARTIFACT_SLICE),
            },
        };

        let slice = self
            .threads
            .load_tool_artifact_slice(ctx.org_id, &handle, selector)
            .await
            .map_err(|e| ToolError::Backend(format!("read_artifact: {e}")))?;

        slice.map_or_else(
            || {
                Err(ToolError::InvalidInput(format!(
                    "read_artifact: no artifact for handle {}",
                    handle.as_str()
                )))
            },
            |s| Ok(s.into_string()),
        )
    }
}
