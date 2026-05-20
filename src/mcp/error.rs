use thiserror::Error;

use crate::crypto::CryptoError;
use crate::types::ParseError;

use super::types::{McpCatalogId, McpServerId};

/// Module error type for the MCP subsystem. CLAUDE.md §12: every public function in
/// `mcp::` returns `Result<T, McpError>` so callers can `match` exhaustively.
#[derive(Debug, Error)]
pub enum McpError {
    #[error("mcp server {0} not found")]
    NotFound(McpServerId),

    #[error("mcp server with catalog id `{0}` already wired in this org")]
    CatalogIdTaken(McpCatalogId),

    #[error("mcp catalog id `{0}` does not resolve to a known catalog entry")]
    CatalogIdUnknown(McpCatalogId),

    #[error("server cap reached (max {max})")]
    ServerCapExceeded { max: usize },

    #[error("parse: {0}")]
    Parse(#[from] ParseError),

    #[error("config rejected: {0}")]
    InvalidConfig(String),

    #[error("connect to mcp server failed: {0}")]
    Connect(String),

    #[error("list_tools on mcp server failed: {0}")]
    ListTools(String),

    #[error("mcp tool call failed: {0}")]
    Call(String),

    #[error("mcp tool call timed out")]
    CallTimeout,

    #[error("mcp store backend error: {0}")]
    Backend(String),

    #[error("mcp store db error: {0}")]
    Db(#[from] sqlx::Error),

    #[error("mcp credential crypto: {0}")]
    Crypto(#[from] CryptoError),
}
