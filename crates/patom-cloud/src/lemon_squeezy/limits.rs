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
