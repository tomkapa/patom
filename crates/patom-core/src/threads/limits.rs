//! Bounds for the thread-feed subsystem (CLAUDE.md §5 — every batch has a cap,
//! named and doc-commented with *why this number*).

/// Upper bound on a single [`ThreadStore::list_threads`] page.
///
/// A channel / DM list this long is already past what a human scans in one
/// view; the cap keeps an unbounded `threads` scan from materialising a giant
/// result on a busy org. Pagination (keyset on `last_activity_at`) is the
/// later refinement that lifts it.
///
/// [`ThreadStore::list_threads`]: super::ThreadStore::list_threads
pub const MAX_THREAD_LIST: i64 = 200;
