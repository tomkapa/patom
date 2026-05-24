//! MCP integration: persisted server registry + dynamic tool catalogue.
//!
//! Operators register MCP servers via the HTTP API (`/mcp-servers`); rows live in
//! Postgres. [`McpRegistry`] reads the enabled rows, opens connections through
//! [`McpClient`] (rmcp wrapper), and exposes the union of remote tools as
//! [`McpTool`] instances. The agent sees them through [`crate::tools::ToolBox`].

mod catalog;
mod client;
pub mod credentials;
mod error;
mod limits;
pub mod oauth;
mod pg_credentials;
mod pg_store;
mod rate_limit;
mod refresher;
mod registry;
mod scoped;
mod store;
mod tool;
mod types;

pub use catalog::{
    CatalogUpsert, McpAuthKind, McpCatalogDescription, McpCatalogDisplayName, McpCatalogEntry,
    McpCatalogStore, OAuthAuthorizeExtra, OAuthAuthorizeExtras, PgMcpCatalogStore,
    SharedMcpCatalogStore,
};
pub use client::McpClient;
pub use credentials::{
    CredentialPayload, MAX_CREDENTIAL_PLAINTEXT, McpCredentialRecord, McpCredentialStore,
    McpCredentialWrite, OAUTH2_KIND_LABEL, OAuth2Payload, STATIC_HEADERS_KIND_LABEL,
    SharedMcpCredentialStore,
};
pub use error::McpError;
pub use limits::{
    MAX_MCP_SERVERS, MAX_TOOLS_PER_SERVER, MCP_ALIAS_MAX_LEN, MCP_CALL_TIMEOUT,
    MCP_CATALOG_DESCRIPTION_MAX_LEN, MCP_CATALOG_DISPLAY_NAME_MAX_LEN, MCP_CATALOG_ID_MAX_LEN,
    MCP_CONNECT_TIMEOUT, MCP_CREDENTIAL_READ_TIMEOUT, MCP_DESCRIPTION_MAX_LEN,
    MCP_HEADER_NAME_MAX_LEN, MCP_HEADER_VALUE_MAX_LEN, MCP_LIST_TOOLS_TIMEOUT, MCP_MAX_HEADERS,
    MCP_RESULT_RENDER_CAP, MCP_TEST_CONNECT_BUCKETS_MAX, MCP_TEST_CONNECT_PER_MIN,
    MCP_TOOL_REMOTE_NAME_MAX_LEN, MCP_URL_MAX_LEN,
};
pub use pg_credentials::PgMcpCredentialStore;
pub use pg_store::PgMcpServerStore;
pub use rate_limit::TestConnectRateLimiter;
pub use refresher::{McpRefreshTrigger, McpRefresher};
pub use registry::{McpRegistry, RegistrySnapshot, ToolOrigin};
pub use scoped::ScopedMcpSource;
pub use store::{
    McpHealthUpdate, McpServerCreate, McpServerStore, McpServerUpdate, SharedMcpServerStore,
};
pub use tool::McpTool;
pub use types::{
    ConnectionStatus, DiscoveredTool, McpCatalogId, McpDescription, McpHeaderName, McpHeaderValue,
    McpHttpUrl, McpServerAlias, McpServerId, McpServerRecord, McpToolRemoteName, McpTransport,
};
