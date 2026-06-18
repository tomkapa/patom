//! Tool-subsystem invariants. Per CLAUDE.md §5: every magic number is named, exported,
//! and doc-commented with the *why*.

use std::time::Duration;

/// Per-call timeout for `web_fetch`.
///
/// Most useful pages return in under 5 s; 20 s tolerates the long tail without letting
/// one bad host stall an entire agent turn.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(20);

/// Hard ceiling on bytes returned to the model from a single fetch. Anthropic charges by
/// token; 200 KB is roughly 50 K tokens of plain English — already excessive context.
pub const FETCH_MAX_BODY_BYTES: usize = 200 * 1024;

/// Maximum HTTP redirect hops for `web_fetch`. Defends against redirect loops and against
/// SSRF via redirect to an internal target after the initial URL passes our guard.
pub const FETCH_MAX_REDIRECTS: usize = 5;

/// Per-call timeout for `web_search`. Brave usually responds well under 2 s; 15 s caps
/// pathological cases without locking up an agent turn.
pub const SEARCH_TIMEOUT: Duration = Duration::from_secs(15);

/// Default and maximum result counts for `web_search`. Brave supports up to 20; we cap
/// at 10 to keep tool output budget modest by default.
pub const SEARCH_DEFAULT_COUNT: u8 = 5;
pub const SEARCH_MAX_COUNT: u8 = 10;

/// Hard ceiling on bytes a tool may return as a single result. Stops a future poorly
/// behaved tool from filling the model context with megabytes.
pub const TOOL_RESULT_MAX_BYTES: usize = 256 * 1024;

/// Maximum bytes accepted in a `tool_calls.tool_name` insert.
///
/// Mirrors the migration-25 CHECK constraint so app and database disagree
/// at the compiler level if either drifts (the recorder rejects oversize
/// names rather than relying on the DB to do so — defence in depth,
/// §5/§10).
pub const MAX_TOOL_NAME_BYTES: usize = 128;

/// Saturation cap for `tool_calls.duration_ms`.
///
/// Stored as `i32` in Postgres; we saturate rather than narrowing-cast so
/// an absurd duration (clock skew, paused tokio runtime) cannot wrap.
/// `i32::MAX` is ~24 days; real tool calls are capped by
/// `agent_core::limits::TOOL_TIMEOUT`.
pub const MAX_TOOL_CALL_DURATION_MS: i32 = i32::MAX;

/// Maximum bytes accepted in a `tool_calls.error_message` insert.
///
/// Tool errors are short reason strings ("upstream 500", "schema mismatch",
/// "timeout"). 2 KiB tolerates a generous JSON snippet without letting a
/// pathological provider response bloat the audit table. Mirrors the
/// migration-27 CHECK so app and database refuse oversize text consistently
/// (CLAUDE.md §5/§10).
pub const MAX_TOOL_CALL_ERROR_MESSAGE_BYTES: usize = 2048;

/// Default page size for `GET /mcp-servers/{id}/tool-calls`.
///
/// Matches the UI "Last 50 calls" label so the first render fills the card
/// without a paginate-on-mount round trip.
pub const DEFAULT_TOOL_CALLS_PAGE: u16 = 50;

/// Hard upper bound on a single tool-calls page.
///
/// 100 keeps the cursor query cheap (partial index + LIMIT) while leaving
/// room for a future "Load more" affordance to grab 2 pages at once.
pub const MAX_TOOL_CALLS_PAGE: u16 = 100;

// §5: per-tool body caps must always fit within the global tool-result cap so the agent
// boundary doesn't have to truncate something we already truncated upstream.
const _: () = assert!(FETCH_MAX_BODY_BYTES <= TOOL_RESULT_MAX_BYTES);

/// Produce-time reduction trigger (#185), in characters.
///
/// A tool result at or below this size flows into the feed verbatim — no
/// offload, no LLM. Above it, the body is offloaded to `tool_artifacts` and the
/// visible result is reduced (paginate or summarize). Reuses #182's
/// prompt-render cap so "too big for the feed" is one number across compaction
/// and reduction.
pub const TOOL_RESULT_REDUCE_THRESHOLD: usize = crate::threads::MAX_TOOL_RESULT_CHARS;

/// Hard ceiling, in characters, on a single `read_artifact` slice (#185).
///
/// A retrieval slice must stay strictly below [`TOOL_RESULT_REDUCE_THRESHOLD`]
/// so the slice — itself a tool result — can never re-trigger offload. This is
/// the recursion fixpoint that bounds chunk-by-chunk reads; half the threshold
/// leaves headroom for the partial-marker wrapper.
pub const MAX_ARTIFACT_SLICE: usize = 16_000;

/// Characters kept from the head of an oversized body in a paginate preview.
///
/// (#185) Enough to show the shape — headers, the first records — so the agent
/// can decide whether to page the rest via `read_artifact`.
pub const PREVIEW_HEAD_CHARS: usize = 2_000;

/// Characters kept from the tail of an oversized body in a paginate preview.
///
/// (#185) The tail is what blind head-truncation drops (the rows after a match,
/// a CSV's last lines); keeping it is the lossless win over truncation.
pub const PREVIEW_TAIL_CHARS: usize = 2_000;

/// How far into an offloaded body a `read_artifact` `grep` scans, in characters.
///
/// (#185) Grep loads a bounded prefix (CLAUDE.md §5 — no unbounded TEXT read)
/// and matches app-side; a body larger than this is grep-scanned only up to
/// here, and the agent pages the remainder by offset. 1M chars (~4 MB / ~250k
/// tokens of raw text) covers any realistic payload.
pub const MAX_ARTIFACT_GREP_SCAN: usize = 1_000_000;

/// Cap on grep matches counted/windowed in one `read_artifact` call (#185).
///
/// Bounds the match-counting scan and the number of windows considered; 16 is
/// well past what a single recovery step needs while keeping the grep output
/// (clamped to `MAX_ARTIFACT_SLICE`) cheap to assemble.
pub const DEFAULT_GREP_MATCHES: usize = 16;

/// Cap, in characters, on the intent string fed to the summarize fold (#185).
///
/// The intent is a prompt fragment (WebFetch `prompt`) or a bounded
/// serialization of the tool call input. Crosses a trust boundary (the model
/// supplies it), so it is capped (CLAUDE.md §5) before reaching the summarizer.
pub const MAX_REDUCTION_INTENT_CHARS: usize = 2_000;

// §5 recursion fixpoint: a retrieval slice is itself a tool result, so it must
// stay under the reduction trigger — otherwise `read_artifact` output would be
// offloaded again, ad infinitum.
const _: () = assert!(MAX_ARTIFACT_SLICE < TOOL_RESULT_REDUCE_THRESHOLD);
// §5: a paginate preview (head + tail + marker) must also stay under the trigger
// so a reduced result never itself needs reducing.
const _: () = assert!(PREVIEW_HEAD_CHARS + PREVIEW_TAIL_CHARS < TOOL_RESULT_REDUCE_THRESHOLD);

/// Truncate `s` to at most `target` bytes, on a UTF-8 boundary.
///
/// `String::truncate` panics if the cut lands mid-codepoint; this walks back to the
/// nearest boundary first. Used wherever we cap a tool result against a byte budget
/// (§5).
pub fn truncate_to_char_boundary(s: &mut String, target: usize) {
    let mut cut = target.min(s.len());
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.truncate(cut);
}

/// Head-trimming counterpart to [`truncate_to_char_boundary`].
///
/// Drops bytes from the START of `s` until the result fits in `max_bytes`,
/// stepping forward to the next UTF-8 boundary. Used by the reflection
/// scheduler so the most recent turns survive when a transcript exceeds
/// the prompt cap.
#[must_use]
pub fn truncate_from_start(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut idx = s.len() - max_bytes;
    while idx < s.len() && !s.is_char_boundary(idx) {
        idx += 1;
    }
    &s[idx..]
}

#[cfg(test)]
mod tests {
    use super::truncate_to_char_boundary;

    #[test]
    fn ascii_string_truncates_to_target() {
        let mut s = "hello world".to_owned();
        truncate_to_char_boundary(&mut s, 5);
        assert_eq!(s, "hello");
    }

    #[test]
    fn target_past_end_is_a_noop() {
        let mut s = "hi".to_owned();
        truncate_to_char_boundary(&mut s, 100);
        assert_eq!(s, "hi");
    }

    #[test]
    fn multi_byte_codepoint_is_not_split() {
        // "héllo" — 'é' is 2 bytes (0xC3 0xA9). Cutting at 2 lands mid-codepoint.
        let mut s = "héllo".to_owned();
        truncate_to_char_boundary(&mut s, 2);
        assert_eq!(s, "h");
    }
}
