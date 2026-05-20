use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;
use thiserror::Error;

use crate::agents::AgentStoreError;
use crate::auth::AuthError;
use crate::mcp::McpError;
use crate::runtime::{PromptError, ResponseError};
use crate::session::SessionError;
use crate::types::ParseError;

/// One error type for the HTTP boundary. CLAUDE.md §12: `IntoResponse` lives next to
/// the variants so the HTTP mapping cannot drift from the variant set.
#[derive(Debug, Error)]
pub enum HttpError {
    #[error("invalid request: {0}")]
    BadRequest(String),

    #[error("not found")]
    NotFound,

    /// 403 with a fixed reason string. Use for role-gated routes (e.g.
    /// owner/admin-only mutations) where the failure mode is purely
    /// "your role isn't high enough", not an unknown resource.
    #[error("forbidden: {0}")]
    Forbidden(&'static str),

    #[error("conflict: {0}")]
    Conflict(String),

    #[error("payload too large")]
    PayloadTooLarge,

    #[error("too many requests")]
    TooManyRequests,

    #[error("parse: {0}")]
    Parse(#[from] ParseError),

    #[error("session: {0}")]
    Session(#[from] SessionError),

    #[error("agent: {0}")]
    Agent(#[from] AgentStoreError),

    #[error("prompt pipeline: {0}")]
    Prompt(#[from] PromptError),

    #[error("response stream: {0}")]
    Response(#[from] ResponseError),

    #[error("mcp: {0}")]
    Mcp(#[from] McpError),

    #[error("auth: {0}")]
    Auth(#[from] AuthError),

    #[error("internal error")]
    Internal,
}

impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        let (status, message) = match &self {
            Self::BadRequest(m) => (StatusCode::BAD_REQUEST, m.clone()),
            Self::NotFound => (StatusCode::NOT_FOUND, "not found".into()),
            Self::Forbidden(reason) => (StatusCode::FORBIDDEN, (*reason).into()),
            Self::Conflict(m) => (StatusCode::CONFLICT, m.clone()),
            Self::PayloadTooLarge => (StatusCode::PAYLOAD_TOO_LARGE, "too large".into()),
            Self::TooManyRequests => (StatusCode::TOO_MANY_REQUESTS, "too many requests".into()),
            Self::Parse(e) | Self::Auth(AuthError::Parse(e)) => {
                (StatusCode::BAD_REQUEST, e.to_string())
            }
            Self::Session(SessionError::NotFound(_)) => {
                (StatusCode::NOT_FOUND, "session not found".into())
            }
            Self::Session(SessionError::AgentNotFound(_))
            | Self::Agent(AgentStoreError::NotFound(_)) => {
                (StatusCode::BAD_REQUEST, "unknown agent_id".into())
            }
            Self::Agent(AgentStoreError::NameNotFound(_)) => {
                (StatusCode::NOT_FOUND, self.to_string())
            }
            Self::Session(SessionError::MessageCapExceeded { .. })
            | Self::Prompt(PromptError::PendingCapExceeded { .. })
            | Self::Mcp(McpError::ServerCapExceeded { .. }) => {
                (StatusCode::TOO_MANY_REQUESTS, self.to_string())
            }
            Self::Session(e) => (StatusCode::BAD_REQUEST, e.to_string()),
            Self::Agent(
                AgentStoreError::DefaultDeletionForbidden
                | AgentStoreError::InUse(_)
                | AgentStoreError::NameTaken(_),
            )
            | Self::Mcp(McpError::CatalogIdTaken(_)) => (StatusCode::CONFLICT, self.to_string()),
            Self::Agent(AgentStoreError::Parse(_))
            | Self::Mcp(
                McpError::Parse(_) | McpError::InvalidConfig(_) | McpError::CatalogIdUnknown(_),
            ) => (StatusCode::BAD_REQUEST, self.to_string()),
            Self::Agent(AgentStoreError::NoDefault) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "default agent not seeded".into(),
            ),
            Self::Agent(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "agent store error".into(),
            ),
            Self::Prompt(PromptError::RequestNotFound(_) | PromptError::SessionNotFound(_)) => {
                (StatusCode::NOT_FOUND, "not found".into())
            }
            Self::Prompt(e) => (StatusCode::BAD_REQUEST, e.to_string()),
            Self::Response(ResponseError::NotFound(_)) => {
                (StatusCode::NOT_FOUND, "stream not found".into())
            }
            Self::Response(e) => (StatusCode::BAD_GATEWAY, e.to_string()),
            Self::Mcp(McpError::NotFound(_)) => {
                (StatusCode::NOT_FOUND, "mcp server not found".into())
            }
            Self::Mcp(_) => (StatusCode::INTERNAL_SERVER_ERROR, "mcp store error".into()),
            Self::Auth(
                AuthError::Unauthenticated | AuthError::Jwt(_) | AuthError::OAuthStateInvalid,
            ) => (StatusCode::UNAUTHORIZED, "unauthorized".into()),
            Self::Auth(AuthError::EmailUnverified) => (
                StatusCode::FORBIDDEN,
                "email not verified by provider".into(),
            ),
            Self::Auth(AuthError::NotMember(_)) => (StatusCode::FORBIDDEN, self.to_string()),
            Self::Auth(AuthError::OAuthProvider(_)) => {
                (StatusCode::BAD_GATEWAY, "oauth provider unavailable".into())
            }
            Self::Auth(_) => (StatusCode::INTERNAL_SERVER_ERROR, "auth error".into()),
            Self::Internal => (StatusCode::INTERNAL_SERVER_ERROR, "internal error".into()),
        };
        // CLAUDE.md §2: every error response that maps to a 5xx is a
        // server-side fault — emit a tracing::error event so operators
        // see the underlying error variant rather than the bare wire
        // message. The handler span (via `TraceLayer`) gets the status.
        if status.is_server_error() {
            tracing::error!(
                event = "http.error.5xx",
                http.response.status_code = status.as_u16(),
                error = ?self,
            );
        }
        (status, Json(json!({ "error": message }))).into_response()
    }
}
