//! [`Tool`] implementation that proxies to a remote MCP server via [`McpClient`].
//!
//! One `McpTool` ↔ one upstream tool ↔ one [`ToolSpec`] in the registry. The agent
//! never sees that the tool is remote — naming is the only externally visible
//! difference (`mcp_<alias>_<remote_name>`).

use std::fmt::Write;
use std::sync::Arc;

use async_trait::async_trait;
use rmcp::model::{CallToolResult, RawContent, RawEmbeddedResource};
use serde_json::Value;
use tracing::instrument;

use crate::mcp::error::McpError;
use crate::tools::{Tool, ToolCallContext, ToolError, truncate_to_char_boundary};
use crate::types::ToolName;

use super::client::McpClient;
use super::limits::MCP_RESULT_RENDER_CAP;

/// Built once per MCP tool exposed by a registered server.
pub struct McpTool {
    name: ToolName,
    remote_name: String,
    description: Arc<str>,
    schema: Arc<Value>,
    client: Arc<McpClient>,
}

impl std::fmt::Debug for McpTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpTool")
            .field("name", &self.name)
            .field("remote_name", &self.remote_name)
            .finish_non_exhaustive()
    }
}

impl McpTool {
    #[must_use]
    pub fn new(
        name: ToolName,
        remote_name: String,
        description: Arc<str>,
        schema: Arc<Value>,
        client: Arc<McpClient>,
    ) -> Self {
        Self {
            name,
            remote_name,
            description,
            schema,
            client,
        }
    }
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &ToolName {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> Arc<Value> {
        self.schema.clone()
    }

    #[instrument(name = "tool.mcp", skip_all, fields(patom.tool = %self.name))]
    async fn execute(&self, input: Value, _ctx: &ToolCallContext) -> Result<String, ToolError> {
        match self.client.call_tool(&self.remote_name, input).await {
            Ok(result) => {
                let rendered = render_result(&result);
                // The MCP protocol's `is_error: true` bit means the
                // upstream server ran the tool and chose to mark the
                // result as a failure (e.g. Google's MCP gateway sets
                // this on permission / quota errors). Propagate it
                // through the `Result<String, ToolError>` seam so the
                // agent_core layer records it in `tool_calls.is_error`
                // — otherwise `is_error=true` collapses into the Ok
                // branch and the audit row says success.
                if matches!(result.is_error, Some(true)) {
                    let err = ToolError::Upstream {
                        status: 0,
                        body: rendered,
                    };
                    // CLAUDE.md §2: emit `error!` inside the span so
                    // the OTel bridge sets span status to ERROR. The
                    // rendered body is already on the error variant —
                    // logging only the typed error avoids duplicating
                    // the truncated tool output in two places.
                    tracing::error!(
                        event = "mcp.tool.upstream_error",
                        patom.tool = %self.name,
                        error = ?err,
                    );
                    return Err(err);
                }
                Ok(rendered)
            }
            Err(McpError::CallTimeout) => Err(ToolError::Upstream {
                status: 0,
                body: format!("mcp tool `{}` timed out", self.remote_name),
            }),
            Err(e) => Err(ToolError::Upstream {
                status: 0,
                body: format!("mcp call error: {e}"),
            }),
        }
    }
}

/// Render a [`CallToolResult`] into the agent-side `String` body.
///
/// Image/audio: replaced with a `[image/audio: <mime>, <bytes>B]` placeholder.
/// Resources: text resources are inlined (with a URI prefix); blob resources are
/// summarised. Errors (`is_error == Some(true)`) are rendered identically to success
/// content — the agent's `ToolError::Upstream` carries the discriminator separately
/// at the call boundary.
fn render_result(result: &CallToolResult) -> String {
    let mut out = String::new();
    let prefix_error = matches!(result.is_error, Some(true));
    if prefix_error {
        out.push_str("[mcp tool returned isError=true]\n");
    }
    for block in &result.content {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        match &block.raw {
            RawContent::Text(t) => out.push_str(&t.text),
            RawContent::Image(img) => {
                let bytes = img.data.len();
                let _ = write!(out, "[image content blocked: {}, {bytes}B]", img.mime_type);
            }
            RawContent::Audio(a) => {
                let bytes = a.data.len();
                let _ = write!(out, "[audio content blocked: {}, {bytes}B]", a.mime_type);
            }
            RawContent::Resource(r) => render_resource_into(&mut out, r),
            RawContent::ResourceLink(link) => {
                let _ = write!(out, "[resource link: {}]", link.uri);
            }
        }
        if out.len() >= MCP_RESULT_RENDER_CAP {
            truncate_to_char_boundary(&mut out, MCP_RESULT_RENDER_CAP);
            out.push_str("\n[truncated]");
            break;
        }
    }
    if out.is_empty() {
        // The upstream server returned no content blocks — surface that explicitly so
        // the model doesn't see a silent empty string and assume success.
        out.push_str("[mcp tool returned no content]");
    }
    out
}

fn render_resource_into(out: &mut String, r: &RawEmbeddedResource) {
    use rmcp::model::ResourceContents;
    match &r.resource {
        ResourceContents::TextResourceContents { uri, text, .. } => {
            let _ = write!(out, "[resource {uri}]\n{text}");
        }
        ResourceContents::BlobResourceContents {
            uri,
            blob,
            mime_type,
            ..
        } => {
            let bytes = blob.len();
            let _ = write!(
                out,
                "[resource {uri}: {}, {bytes}B blocked]",
                mime_type.as_deref().unwrap_or("application/octet-stream")
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::{
        AnnotateAble, RawAudioContent, RawContent, RawImageContent, RawTextContent,
        ResourceContents,
    };

    fn text_block(s: &str) -> rmcp::model::Content {
        RawContent::Text(RawTextContent {
            text: s.to_owned(),
            meta: None,
        })
        .no_annotation()
    }

    fn image_block() -> rmcp::model::Content {
        RawContent::Image(RawImageContent {
            data: "ZmFrZQ==".to_owned(),
            mime_type: "image/png".to_owned(),
            meta: None,
        })
        .no_annotation()
    }

    fn audio_block() -> rmcp::model::Content {
        RawContent::Audio(RawAudioContent {
            data: "ZmFrZQ==".to_owned(),
            mime_type: "audio/wav".to_owned(),
        })
        .no_annotation()
    }

    fn text_resource(uri: &str, text: &str) -> rmcp::model::Content {
        RawContent::Resource(rmcp::model::RawEmbeddedResource {
            meta: None,
            resource: ResourceContents::TextResourceContents {
                uri: uri.to_owned(),
                text: text.to_owned(),
                mime_type: None,
                meta: None,
            },
        })
        .no_annotation()
    }

    fn make_result(content: Vec<rmcp::model::Content>, is_error: Option<bool>) -> CallToolResult {
        let mut result = if matches!(is_error, Some(true)) {
            CallToolResult::error(content)
        } else {
            CallToolResult::success(content)
        };
        result.is_error = is_error;
        result
    }

    #[test]
    fn text_blocks_are_concatenated() {
        let r = make_result(vec![text_block("hello"), text_block("world")], None);
        let s = render_result(&r);
        assert!(s.contains("hello"));
        assert!(s.contains("world"));
    }

    #[test]
    fn images_are_blocked_with_placeholder() {
        let r = make_result(vec![image_block()], None);
        let s = render_result(&r);
        assert!(s.contains("[image content blocked"));
        assert!(s.contains("image/png"));
    }

    #[test]
    fn audio_is_blocked_with_placeholder() {
        let r = make_result(vec![audio_block()], None);
        let s = render_result(&r);
        assert!(s.contains("[audio content blocked"));
    }

    #[test]
    fn text_resource_includes_uri_and_body() {
        let r = make_result(vec![text_resource("file:///x.txt", "body")], None);
        let s = render_result(&r);
        assert!(s.contains("file:///x.txt"));
        assert!(s.contains("body"));
    }

    #[test]
    fn empty_content_renders_explicit_message() {
        let r = make_result(vec![], None);
        let s = render_result(&r);
        assert!(s.contains("no content"));
    }

    #[test]
    fn is_error_marks_output() {
        let r = make_result(vec![text_block("oops")], Some(true));
        let s = render_result(&r);
        assert!(s.contains("isError=true"));
    }
}
