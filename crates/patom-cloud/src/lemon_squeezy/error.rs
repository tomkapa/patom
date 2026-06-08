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

    /// Checkout requested a variant this store doesn't sell (not in the
    /// configured variant→plan map). → 400.
    #[error("unknown checkout variant")]
    UnknownVariant,

    /// Transport error talking to the Lemon Squeezy REST API. → 502.
    #[error("lemon squeezy request failed: {0}")]
    Http(#[from] reqwest::Error),

    /// Lemon Squeezy returned a non-success status. → 502. Carries the typed
    /// [`reqwest::StatusCode`] rather than a bare `u16` so the value is always a
    /// valid status (CLAUDE.md §1).
    #[error("lemon squeezy returned status {status}")]
    Upstream { status: reqwest::StatusCode },

    /// Postgres error from the subscription store / idempotency ledger.
    #[error(transparent)]
    Db(#[from] sqlx::Error),

    /// A bounded DB I/O await exceeded its `tokio::time::timeout` (CLAUDE.md
    /// §5) — the query/connection stalled. → 500 (transient; retriable).
    #[error("database operation timed out")]
    Timeout,
}
