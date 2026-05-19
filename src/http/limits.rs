//! Per CLAUDE.md §5: every trust-boundary length cap lives here, not
//! buried in route handlers.

/// Sized for JSON payloads; absorbs base64-encoded MCP credentials
/// with slack, three orders of magnitude below any plausible OOM
/// vector.
pub(super) const REQUEST_BODY_LIMIT_BYTES: usize = 2 * 1024 * 1024;

/// Short on purpose: `/readyz` must trip off this pod the moment
/// Postgres connectivity is lost, not after a multi-second pause that
/// lets a broken pod keep receiving traffic.
pub(super) const READYZ_DB_TIMEOUT_MS: u64 = 500;
