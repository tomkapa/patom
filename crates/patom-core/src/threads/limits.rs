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

/// Cap on the root-message snippet `list_threads` extracts in SQL (`LEFT`).
///
/// Two timeline lines of preview text; the full body ships on the G2 feed
/// read. Applied in SQL so an oversized body never crosses the wire
/// (CLAUDE.md §5: unbounded TEXT reads are banned without a cap).
pub const ROOT_SNIPPET_MAX_CHARS: i32 = 160;

/// Cap on `@`-tags one `POST /prompts` message may carry.
///
/// Each agent tag enqueues a trigger + mints a DAG, so this bounds the fan-out
/// a single message can cause. Eight is far past any real "ping the team"
/// message; a saturation counter is unnecessary because the HTTP boundary
/// rejects, not truncates.
pub const MAX_TAGS_PER_MESSAGE: usize = 8;
