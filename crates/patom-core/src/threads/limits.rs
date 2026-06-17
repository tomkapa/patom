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

/// Upper bound on the channels listed in an agent's `<channels>` context block.
///
/// A roster this long is already past what fits inline in a turn prompt (#178);
/// the renderer degrades to a "use a tool to list" pointer past it, mirroring
/// `<colleagues>`. Bounds the `channels_for_colleague` scan too.
pub const MAX_CHANNELS_FOR_COLLEAGUE: i64 = 100;

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

/// Hard cap on the verbatim message rows `context_tail` returns for one turn.
///
/// The **windowing floor** that bounds the model prompt with no LLM in the loop
/// (#182, CLAUDE.md §5). Even a cold thread with no compaction summary yet, or a
/// summarizer that is failing, cannot push an unbounded history at the provider:
/// `context_tail` `LIMIT`s the read to this many most-recent rows. 200 is a deep
/// working window (well past what fits a single coherent reply) while keeping the
/// worst-case `thread_messages` scan and prompt size bounded; the rolling summary
/// carries everything older.
pub const MAX_CONTEXT_MESSAGES: i64 = 200;

/// Prompt-render cap, in characters, on a single `tool_result` body.
///
/// A fat tool result (web fetch, large file read, big MCP payload) is the single
/// biggest source of prompt bloat (#182). This caps only the **rendered** body —
/// the full row always stays in the immutable `thread_messages` feed; an over-cap
/// body is shown as `head + […omitted…] + tail` so it stays recoverable, never
/// deleted. ~32k chars ≈ 8k tokens at the `chars/4` heuristic: room for a
/// substantial result while a pathological one can't dominate the window.
/// Semantic produce-time reduction is a companion feature; this is only the
/// lossless safety net.
pub const MAX_TOOL_RESULT_CHARS: usize = 32_000;
