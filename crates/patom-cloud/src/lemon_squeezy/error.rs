//! The single error type at the Lemon Squeezy module boundary (CLAUDE.md §12).

use patom::types::ParseError;
use thiserror::Error;

/// Every failure mode of the Lemon Squeezy subsystem.
///
/// Spans the store, webhook verification, and checkout. Callers match
/// exhaustively; the HTTP mapping lives next to the route handlers that surface
/// it (added with the routes).
#[derive(Debug, Error)]
pub enum LemonSqueezyError {
    /// The `X-Signature` HMAC did not match the signing secret. → 401.
    #[error("webhook signature mismatch")]
    SignatureMismatch,

    /// A webhook arrived without the org mapping (`meta.custom_data.org_id`)
    /// needed to attribute the subscription. Acked so Lemon Squeezy stops
    /// retrying; logged for reconciliation.
    #[error("webhook missing org mapping")]
    MissingOrgMapping,

    /// A value crossing the boundary failed its newtype constructor.
    #[error("parse: {0}")]
    Parse(#[from] ParseError),

    /// Postgres error from the subscription store / idempotency ledger.
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}
