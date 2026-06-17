//! `read_channel` — the agent's cross-thread history read (#199).
//!
//! Until now an agent saw only its own thread at run time (there was no
//! cross-session read tool). `read_channel` is the first: given a channel the
//! agent is a **member** of, it returns that channel's recent `posted` chat —
//! unioned across every thread bound to the channel (the ambient channel thread
//! plus every @mention sub-thread) — as a bounded, oldest-first transcript the
//! agent can summarise. It is the source for a scheduled digest ("summarise
//! unanswered questions / blockers since the last run") but is a general read:
//! the agent may call it on any turn for any channel it belongs to.
//!
//! The **membership gate** is the entire safety boundary — the same
//! `colleague_in_channel` check `send_message` uses to address a channel. An
//! agent cannot read a channel it does not belong to. The read is bounded per
//! CLAUDE.md §5: a capped row count, a `since` floor, an in-SQL per-message body
//! cap, and a final byte cap on the rendered transcript.
//!
//! Coverage note: ambient (non-mention) content is only present when the
//! platform's message-content grant is enabled; without it those rows carry no
//! text and render as "(no readable content)", so the digest degrades to the
//! messages the bot is always entitled to (mentions / DMs) rather than failing.

use std::fmt::Write as _;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::channels::ChannelId;
use crate::threads::{
    ChannelFeedRow, DEFAULT_READ_CHANNEL_MESSAGES, MAX_READ_CHANNEL_MESSAGES, SharedThreadStore,
};
use crate::types::{ParseError, ToolName};

use super::super::limits::{TOOL_RESULT_MAX_BYTES, truncate_to_char_boundary};
use super::super::traits::{Tool, ToolCallContext, ToolError};

const TOOL_NAME: &str = "read_channel";

const TOOL_DESCRIPTION: &str = "Read the recent message history of a channel you are a member \
    of, so you can summarise it (e.g. a standup digest, or unanswered questions and blockers \
    since a cutoff). Returns the channel's recent posts — across the channel and its reply \
    threads — oldest-first, as `<timestamp> <author>: <text>` lines.\n\
    \n\
    Arguments: `channel` (the channel id from your `<channels>` block — you must be a member; \
    reading a channel you don't belong to is refused), optional `since` (RFC-3339 / ISO-8601 \
    timestamp; only messages at or after it are returned — pass the start of your digest window), \
    optional `limit` (1..=200, default 50; the most recent messages are kept).\n\
    \n\
    Some lines may read `(no readable content)` — those are messages the bot ingested without \
    text (e.g. when the platform's message-content access isn't granted); summarise what you can.";

/// Row cap for one `read_channel` call. Parsed at the JSON boundary (CLAUDE.md
/// §1) — holding a `ReadChannelLimit` proves the value is in
/// `1..=MAX_READ_CHANNEL_MESSAGES`, so the core never sees an out-of-range cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ReadChannelLimit(i64);

impl ReadChannelLimit {
    fn get(self) -> i64 {
        self.0
    }
}

impl Default for ReadChannelLimit {
    fn default() -> Self {
        // The default is in range by construction (a small positive constant).
        Self(i64::from(DEFAULT_READ_CHANNEL_MESSAGES))
    }
}

impl TryFrom<i64> for ReadChannelLimit {
    type Error = ParseError;
    fn try_from(n: i64) -> Result<Self, Self::Error> {
        if (1..=MAX_READ_CHANNEL_MESSAGES).contains(&n) {
            Ok(Self(n))
        } else {
            Err(ParseError::OutOfRange {
                field: "read_channel_limit",
                detail: "1..=MAX_READ_CHANNEL_MESSAGES",
            })
        }
    }
}

impl<'de> serde::Deserialize<'de> for ReadChannelLimit {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let n = i64::deserialize(d)?;
        Self::try_from(n).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Deserialize)]
struct Input {
    /// The channel to read — a member channel from the `<channels>` block.
    channel: ChannelId,
    /// Only messages at or after this instant (RFC-3339). Omit for the most
    /// recent `limit` messages.
    #[serde(default)]
    since: Option<DateTime<Utc>>,
    /// Row cap, validated to `1..=MAX_READ_CHANNEL_MESSAGES` at parse time.
    #[serde(default)]
    limit: ReadChannelLimit,
}

/// Agent channel-history read tool. Holds only the thread store: the membership
/// gate and the channel read both live there.
pub struct ReadChannelTool {
    name: ToolName,
    description: &'static str,
    input_schema: Arc<Value>,
    threads: SharedThreadStore,
}

impl std::fmt::Debug for ReadChannelTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadChannelTool").finish_non_exhaustive()
    }
}

impl ReadChannelTool {
    #[must_use]
    pub fn new(threads: SharedThreadStore) -> Self {
        let name = ToolName::try_from(TOOL_NAME).expect("invariant: read_channel is a valid name");
        let input_schema = Arc::new(json!({
            "type": "object",
            "required": ["channel"],
            "properties": {
                "channel": { "type": "string", "format": "uuid" },
                "since": { "type": "string", "format": "date-time" },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_READ_CHANNEL_MESSAGES,
                },
            },
            "additionalProperties": false,
        }));
        Self {
            name,
            description: TOOL_DESCRIPTION,
            input_schema,
            threads,
        }
    }

    #[tracing::instrument(
        skip_all,
        name = "tool.read_channel",
        fields(
            patom.channel.id = tracing::field::Empty,
            patom.from.viewer = %ctx.viewer,
            patom.read_channel.outcome = tracing::field::Empty,
        ),
    )]
    async fn handle(&self, input: Input, ctx: &ToolCallContext) -> Result<String, ToolError> {
        tracing::Span::current().record("patom.channel.id", tracing::field::display(input.channel));
        // Caller must be an agent (humans don't run tool calls); its colleague id
        // is the membership-gate subject. `colleague_id` is `Some` for any
        // colleague, so the `agent_id` filter is what rejects a human / System.
        let viewer = ctx
            .viewer
            .colleague_id()
            .filter(|_| ctx.viewer.agent_id().is_some())
            .ok_or_else(|| {
                set_outcome("not_agent");
                ToolError::InvalidInput("read_channel: caller must be an agent".into())
            })?;

        // The membership gate is the safety boundary — no auto-add, mirrors the
        // `send_message` channel gate.
        let member = self
            .threads
            .colleague_in_channel(input.channel, viewer)
            .await
            .map_err(|e| {
                tracing::error!(error = ?e, event = "read_channel.membership_check.failed");
                set_outcome("backend_error");
                ToolError::Backend(format!("read_channel: membership check: {e}"))
            })?;
        if !member {
            set_outcome("not_member");
            return Err(ToolError::InvalidInput(
                "read_channel: you are not a member of that channel".into(),
            ));
        }

        // `limit` was bounds-validated at the JSON boundary (ReadChannelLimit).
        let rows = self
            .threads
            .channel_feed(input.channel, input.since, input.limit.get())
            .await
            .map_err(|e| {
                tracing::error!(error = ?e, event = "read_channel.channel_feed.failed");
                set_outcome("backend_error");
                ToolError::Backend(format!("read_channel: read: {e}"))
            })?;

        if rows.is_empty() {
            set_outcome("empty");
            return Ok("(no messages in that window)".to_owned());
        }
        set_outcome("ok");
        Ok(render(&rows))
    }
}

/// Render the channel rows as an oldest-first transcript, capped to
/// [`TOOL_RESULT_MAX_BYTES`] so a busy channel can't blow the context budget
/// (the row count is already bounded; this guards pathological line lengths).
fn render(rows: &[ChannelFeedRow]) -> String {
    // Rough per-line estimate (timestamp + author + preview) so the bounded
    // transcript doesn't reallocate as it grows.
    let mut out = String::with_capacity(rows.len().saturating_mul(96));
    for row in rows {
        let author = row.author.as_deref().unwrap_or("system");
        let text = if row.body_preview.is_empty() {
            "(no readable content)"
        } else {
            row.body_preview.as_str()
        };
        // Best-effort formatting into a String never fails; ignore the Result.
        let _ = writeln!(out, "{} {author}: {text}", row.created_at.to_rfc3339());
    }
    truncate_to_char_boundary(&mut out, TOOL_RESULT_MAX_BYTES);
    out
}

/// Record the `patom.read_channel.outcome` field on the enclosing span. Each
/// branch labels itself so dashboards can `GROUP BY` it. Variants: `not_agent`,
/// `not_member`, `empty`, `ok`, `backend_error`.
fn set_outcome(label: &'static str) {
    tracing::Span::current().record("patom.read_channel.outcome", label);
}

#[async_trait]
impl Tool for ReadChannelTool {
    fn name(&self) -> &ToolName {
        &self.name
    }

    fn description(&self) -> &str {
        self.description
    }

    fn input_schema(&self) -> Arc<Value> {
        self.input_schema.clone()
    }

    /// Read-only: no mutation of patom state, safe to run alongside other reads.
    fn concurrency_safe(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value, ctx: &ToolCallContext) -> Result<String, ToolError> {
        let parsed: Input = serde_json::from_value(input)?;
        self.handle(parsed, ctx).await
    }
}
