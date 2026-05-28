//! Thin wrapper around `rmcp` that hides the SDK's transport, handler and `Peer`
//! plumbing behind a small surface (`connect`, `list_tools`, `call_tool`).
//!
//! Holds the running service so the worker task and HTTP connection stay alive for as
//! long as `McpClient` does. Dropping the client cancels the worker (rmcp signals on
//! drop), which closes the HTTP/SSE connection cleanly.

use std::collections::HashMap;
use std::sync::Arc;

use http::header::{HeaderName, HeaderValue};
use rmcp::ServiceExt;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, JsonObject, PaginatedRequestParams, Tool,
};
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use serde_json::Value;
use tokio::time::timeout;

use super::credentials::CredentialPayload;
use super::error::McpError;
use super::limits::{MCP_CALL_TIMEOUT, MCP_CONNECT_TIMEOUT, MCP_LIST_TOOLS_TIMEOUT};
use super::oauth::PatomMcpHttpClient;
use super::types::McpTransport;

/// Connected MCP server. Cheap to clone (the running service lives behind an `Arc`)
/// so multiple `McpTool` instances can share one upstream connection.
pub struct McpClient {
    inner: Arc<RunningService<RoleClient, ()>>,
}

impl std::fmt::Debug for McpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpClient").finish_non_exhaustive()
    }
}

impl McpClient {
    /// Open a connection to `transport` and complete the MCP `initialize`
    /// handshake under [`MCP_CONNECT_TIMEOUT`]. Used for servers with
    /// no credentials or with static-headers credentials — the headers
    /// are attached verbatim to every outbound request via rmcp's
    /// `custom_headers`.
    ///
    /// OAuth-bearing servers go through [`Self::connect_with_adapter`]
    /// so refresh-on-acquire / refresh-on-401 are handled by
    /// [`PatomMcpHttpClient`] instead of baking a static Bearer here.
    pub async fn connect(
        transport: &McpTransport,
        credentials: Option<&CredentialPayload>,
    ) -> Result<Self, McpError> {
        if matches!(credentials, Some(CredentialPayload::Oauth2(_))) {
            return Err(McpError::InvalidConfig(
                "oauth2 credentials require connect_with_adapter for refresh handling".into(),
            ));
        }
        match transport {
            McpTransport::Http { url } => {
                let custom_headers = build_request_headers(credentials)?;
                let cfg = StreamableHttpClientTransportConfig::with_uri(url.as_str())
                    .custom_headers(custom_headers);
                let transport = StreamableHttpClientTransport::from_config(cfg);
                let connect = timeout(MCP_CONNECT_TIMEOUT, ().serve(transport))
                    .await
                    .map_err(|_| McpError::Connect("initialize timed out".into()))?
                    .map_err(|e| McpError::Connect(e.to_string()))?;
                Ok(Self {
                    inner: Arc::new(connect),
                })
            }
        }
    }

    /// Open an OAuth-aware connection. The adapter implements rmcp's
    /// `StreamableHttpClient` trait with the refresh-on-acquire +
    /// refresh-on-401 behaviour described in [`PatomMcpHttpClient`];
    /// no static Bearer is attached, so a rotated token takes effect
    /// on the next call without reconnecting.
    pub async fn connect_with_adapter(
        transport: &McpTransport,
        adapter: PatomMcpHttpClient,
    ) -> Result<Self, McpError> {
        match transport {
            McpTransport::Http { url } => {
                let cfg = StreamableHttpClientTransportConfig::with_uri(url.as_str());
                let transport = StreamableHttpClientTransport::with_client(adapter, cfg);
                let connect = timeout(MCP_CONNECT_TIMEOUT, ().serve(transport))
                    .await
                    .map_err(|_| McpError::Connect("initialize timed out".into()))?
                    .map_err(|e| McpError::Connect(e.to_string()))?;
                Ok(Self {
                    inner: Arc::new(connect),
                })
            }
        }
    }

    /// Fetch the full tool list from the server, paginating until `next_cursor` is
    /// `None`. Bounded by [`MCP_LIST_TOOLS_TIMEOUT`] across all pages.
    pub async fn list_tools(&self) -> Result<Vec<Tool>, McpError> {
        let peer = self.inner.peer().clone();
        let fut = async move {
            let mut tools = Vec::new();
            let mut cursor = None;
            loop {
                let result = peer
                    .list_tools(Some(PaginatedRequestParams::default().with_cursor(cursor)))
                    .await
                    .map_err(|e| McpError::ListTools(e.to_string()))?;
                tools.extend(result.tools);
                cursor = result.next_cursor;
                if cursor.is_none() {
                    break;
                }
            }
            Ok::<_, McpError>(tools)
        };
        timeout(MCP_LIST_TOOLS_TIMEOUT, fut)
            .await
            .map_err(|_| McpError::ListTools("timed out".into()))?
    }

    /// Invoke `tools/call`, capped by [`MCP_CALL_TIMEOUT`].
    pub async fn call_tool(&self, name: &str, input: Value) -> Result<CallToolResult, McpError> {
        let arguments = match input {
            Value::Null => None,
            Value::Object(map) => Some(map),
            other => Some(json_object_with_value("input", other)),
        };
        let mut params = CallToolRequestParams::new(name.to_owned());
        if let Some(args) = arguments {
            params = params.with_arguments(args);
        }
        let peer = self.inner.peer().clone();
        let call = async move {
            peer.call_tool(params)
                .await
                .map_err(|e| McpError::Call(e.to_string()))
        };
        timeout(MCP_CALL_TIMEOUT, call)
            .await
            .unwrap_or_else(|_elapsed| Err(McpError::CallTimeout))
    }
}

fn json_object_with_value(field: &str, value: Value) -> JsonObject {
    let mut map = serde_json::Map::with_capacity(1);
    map.insert(field.to_owned(), value);
    map
}

/// Render the request-headers map for `transport.custom_headers`. Static
/// headers go on verbatim; an OAuth payload becomes
/// `Authorization: Bearer <access_token>`. The values cross from
/// app-validated newtypes into the HTTP-crate's typed `HeaderName` /
/// `HeaderValue` here — failures collapse onto [`McpError::InvalidConfig`]
/// so the connect-time error path stays uniform.
fn build_request_headers(
    credentials: Option<&CredentialPayload>,
) -> Result<HashMap<HeaderName, HeaderValue>, McpError> {
    let mut out: HashMap<HeaderName, HeaderValue> = HashMap::new();
    let Some(credentials) = credentials else {
        return Ok(out);
    };
    match credentials {
        CredentialPayload::StaticHeaders { headers } => {
            for (name, value) in headers {
                let header_name = HeaderName::try_from(name.as_str()).map_err(|e| {
                    McpError::InvalidConfig(format!("header name `{}`: {e}", name.as_str()))
                })?;
                let header_value = HeaderValue::from_str(value.as_str())
                    .map_err(|e| McpError::InvalidConfig(format!("header value rejected: {e}")))?;
                out.insert(header_name, header_value);
            }
        }
        CredentialPayload::Oauth2(oauth) => {
            // Bearer-tokens may carry chars outside `HeaderValue`'s ASCII
            // visible set; the rejection lands as `InvalidConfig` rather
            // than a panic so a misformatted upstream response surfaces
            // cleanly. The Authorization header is hard-coded — never
            // overridden by static headers, which would defeat OAuth.
            let header_value = HeaderValue::from_str(&format!("Bearer {}", oauth.access_token))
                .map_err(|_| McpError::InvalidConfig("oauth access_token rejected".into()))?;
            out.insert(http::header::AUTHORIZATION, header_value);
        }
    }
    Ok(out)
}
