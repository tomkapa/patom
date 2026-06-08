//! Bounds for the Lemon Squeezy subsystem (CLAUDE.md §5). Named, exported,
//! with a note on *why* each number.

/// Max byte length accepted for any Lemon Squeezy id.
///
/// Covers customer / subscription / order / variant / event ids. LS ids are
/// short numeric strings today; 128 bytes is generous headroom while still
/// capping anything that crosses the trust boundary into a `TEXT` column.
pub const MAX_LS_ID_BYTES: usize = 128;

/// Max accepted webhook body size. Lemon Squeezy payloads are a few KB of JSON;
/// 64 KiB is generous headroom and a hard ceiling on what we buffer + HMAC
/// before rejecting (CLAUDE.md §5).
pub const MAX_WEBHOOK_BODY_BYTES: usize = 64 * 1024;

/// Wall-clock budget for handling one webhook end to end (verify + parse +
/// store writes). Bounds the handler so a stuck DB can't pin a connection
/// indefinitely (CLAUDE.md §5).
pub const WEBHOOK_HANDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Per-call timeout for an outbound Lemon Squeezy REST request (checkout
/// creation, reconciliation fetch). Bounds every I/O await (CLAUDE.md §5)
/// independently of the shared client's default timeout.
pub const LS_API_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Per-statement timeout for a `cloud` schema query.
///
/// CLAUDE.md §5: every sqlx await is bounded. The pool's `acquire_timeout`
/// bounds connection checkout; this bounds the query itself.
pub const DB_QUERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Timeout for each startup migration I/O step (schema create, connect, run) so
/// a stalled DB can't hang boot indefinitely (CLAUDE.md §5).
pub const MIGRATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Grace window (days) after `current_period_end` for a `past_due` sub.
///
/// During it the subscription keeps its paid agent cap, giving Lemon Squeezy's
/// dunning time to recover the payment before we downgrade to the
/// no-subscription cap. Provisional — tune here without touching the gate
/// logic (#131).
pub const PAST_DUE_GRACE_DAYS: i64 = 3;

/// How often the reconciliation poll sweeps for stale subscriptions.
///
/// Each sweep re-fetches drifted subscriptions from Lemon Squeezy — the safety
/// net for webhooks we never received. Hourly is ample: webhooks are the
/// primary path.
pub const RECONCILE_INTERVAL: std::time::Duration = std::time::Duration::from_hours(1);

/// A subscription is "stale" (eligible for a reconcile fetch) once it hasn't
/// been updated for this long — long enough that a normal webhook would have
/// refreshed it.
pub const RECONCILE_STALE_AFTER_SECS: i64 = 6 * 60 * 60;

/// Max subscriptions refreshed per reconcile tick — bounds the outbound API
/// calls and DB writes one sweep can do (CLAUDE.md §5).
pub const RECONCILE_BATCH: i64 = 100;
