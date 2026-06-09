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

/// Upper bound on a single [`ThreadStore::feed`] page (the G2 flat-feed read).
///
/// Caps the rows one HTTP history read ships to the FE (CLAUDE.md §5),
/// independent of an agent's LLM context window. Sized for a deep scrollback in
/// one fetch while still bounding the worst-case `thread_messages` scan; the FE
/// pages further with the `before_seq` keyset cursor.
///
/// [`ThreadStore::feed`]: super::ThreadStore::feed
pub const MAX_THREAD_FEED: i64 = 500;

/// Default [`ThreadStore::feed`] page when the HTTP caller omits `limit`.
///
/// One screen of scrollback plus headroom; the FE requests more via the cursor.
///
/// [`ThreadStore::feed`]: super::ThreadStore::feed
pub const DEFAULT_THREAD_FEED: u32 = 100;
