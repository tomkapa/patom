//! Per CLAUDE.md §5: every trust-boundary length cap lives here, not
//! buried in route handlers.

/// Outer request-body ceiling shared by every route. Sized for the
/// largest legitimate upload (`MAX_AVATAR_BYTES` = 2 MiB + multipart
/// envelope). The per-route extractors (DefaultBodyLimit on upload
/// routes, Json's own default elsewhere) clamp tighter — this is the
/// last-line guard that protects every endpoint, including misrouted
/// requests, from a runaway body. Three orders of magnitude below any
/// plausible OOM vector even when an upload route is in flight.
pub(super) const REQUEST_BODY_LIMIT_BYTES: usize = 4 * 1024 * 1024;

/// Short on purpose: `/readyz` must trip off this pod the moment
/// Postgres connectivity is lost, not after a multi-second pause that
/// lets a broken pod keep receiving traffic.
pub(super) const READYZ_DB_TIMEOUT_MS: u64 = 500;
