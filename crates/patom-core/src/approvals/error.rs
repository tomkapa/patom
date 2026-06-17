//! The approval subsystem's single error type (CLAUDE.md §12).
//!
//! One enum so every caller's exhaustive `match` handles every failure;
//! `IntoResponse` lives next to the variants so the Lark card-callback route's
//! HTTP mapping can't drift.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use thiserror::Error;

use crate::types::ParseError;

#[derive(Debug, Error)]
pub enum ApprovalError {
    /// No approval row with that id (in the caller's org).
    #[error("approval not found")]
    NotFound,

    /// The row exists but is no longer `pending` — a double-click or a race
    /// with the expiry sweep. The decide path treats this as idempotent.
    #[error("approval is not pending")]
    NotPending,

    /// The clicker is not an authorized approver for this request.
    #[error("not authorized to decide this approval")]
    Unauthorized,

    /// The request's TTL elapsed before a decision arrived.
    #[error("approval has expired")]
    Expired,

    /// A downstream subsystem failed in a way that is not the caller's fault.
    #[error("approval backend error: {0}")]
    Backend(String),

    #[error(transparent)]
    Parse(#[from] ParseError),

    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

impl IntoResponse for ApprovalError {
    fn into_response(self) -> Response {
        // The Lark card-callback route must answer within 3 s; these map the few
        // failure modes a callback can hit. Backend/DB/Parse collapse to 500 —
        // the platform retries — while the client-fault variants are terminal.
        let status = match self {
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::NotPending | Self::Expired => StatusCode::CONFLICT,
            Self::Unauthorized => StatusCode::FORBIDDEN,
            Self::Backend(_) | Self::Db(_) | Self::Parse(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        status.into_response()
    }
}
