//! Outcome of a single `Agent::reply` invocation.
//!
//! Multi-agent communication splits "what the model emitted as final text"
//! from "did the model actually deliver a message via `send_message`". The
//! worker pool consumes both: `final_text` for legacy logs and trace context;
//! `send_message_calls` for the ping-pong defence (worker retries the turn if
//! the agent produced text without ever calling `send_message`).
//!
//! Today the only `Agent::reply` impl returns text-only, so `send_message_calls`
//! is always 0; the field exists ahead of the worker-side guard so the type
//! signature is stable across the build steps.

use std::fmt;

/// Result of one full reply turn — what to log, plus a counter the worker
/// uses to decide whether the agent actually communicated.
///
/// Fields are private (CLAUDE.md §1) — accessors below are the only way in.
#[derive(Clone, PartialEq, Eq)]
pub struct AgentReply {
    final_text: String,
    send_message_calls: usize,
}

impl AgentReply {
    /// Construct a reply from the two fields. `text-only` legacy paths use
    /// [`Self::text_only`] which makes the zero-count explicit.
    #[must_use]
    pub fn new(final_text: String, send_message_calls: usize) -> Self {
        Self {
            final_text,
            send_message_calls,
        }
    }

    /// Reply that produced final text but called no tools — used by every
    /// legacy single-agent path until `send_message` is wired in.
    #[must_use]
    pub fn text_only(final_text: String) -> Self {
        Self {
            final_text,
            send_message_calls: 0,
        }
    }

    /// The assistant's last text block in the turn. Kept for trace/observability
    /// only; agent-to-agent and agent-to-human delivery happens through
    /// `send_message`, not through this field.
    #[must_use]
    pub fn final_text(&self) -> &str {
        &self.final_text
    }

    /// Number of `send_message` tool calls the agent made during the turn.
    /// Zero means the worker treats the turn as ping-pong (text without
    /// delivery) and either nudges or fails per `MAX_PINGPONG_RETRIES`.
    #[must_use]
    pub const fn send_message_calls(&self) -> usize {
        self.send_message_calls
    }

    /// Did the agent communicate at least once this turn? False values trigger
    /// the worker-level ping-pong retry guard.
    #[must_use]
    pub const fn delivered(&self) -> bool {
        self.send_message_calls > 0
    }
}

impl fmt::Debug for AgentReply {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Length-only Debug to keep large model outputs out of logs (CLAUDE.md §2).
        f.debug_struct("AgentReply")
            .field("final_text.len", &self.final_text.len())
            .field("send_message_calls", &self.send_message_calls)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_only_has_zero_delivery() {
        let r = AgentReply::text_only("hi".into());
        assert_eq!(r.send_message_calls(), 0);
        assert!(!r.delivered());
        assert_eq!(r.final_text(), "hi");
    }

    #[test]
    fn new_records_call_count() {
        let r = AgentReply::new("done".into(), 2);
        assert_eq!(r.send_message_calls(), 2);
        assert!(r.delivered());
    }

    #[test]
    fn debug_omits_full_text() {
        // Sanity — the Debug impl reports the length, not the content. Long or
        // PII-bearing strings must not leak into trace output.
        let r = AgentReply::text_only("the quick brown fox".into());
        let dbg = format!("{r:?}");
        assert!(dbg.contains("final_text.len"));
        assert!(!dbg.contains("the quick brown fox"));
    }
}
