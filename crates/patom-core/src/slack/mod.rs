//! Slack adapter — bridges the existing multi-agent chat into Slack.
//!
//! Surface (Phase 1):
//! - `POST /slack/events` — public webhook; verifies HMAC, acks `200` in
//!   <3s, hands off to the bridge worker via a bounded mpsc.
//! - `POST /slack/commands` — public; receives `/patom` slash command
//!   invocations and opens the agent-picker modal via `views.open`.
//! - `POST /slack/interactions` — public; receives modal
//!   `view_submission`, enqueues the prompt, and binds the thread.
//! - `GET /slack/oauth/callback` — public; finishes the Slack v2 install.
//! - `POST /api/slack/install` — private; returns the authorize URL.
//!
//! Internals:
//! - `bridge.rs` resolves identity, parses mentions, mints a `Principal`,
//!   submits prompts through the existing queue.
//! - `stream_pump.rs` subscribes to `PgThreadStream` per Slack-rooted DAG
//!   and posts `Done` / `AgentMessage` / `Error` chunks back to Slack.
//! - `poster.rs` is the outbound `chat.postMessage` wrapper.
//!
//! Identity model: Phase 1 falls back to the installing user for every
//! Slack event in the workspace; per-user linking is GitHub issue #41.
//!
//! See `.claude/plans/plan-for-implementation-buzzing-lovelace.md` for the
//! full design and `CLAUDE.md` for the binding engineering rules.

pub mod bridge;
pub mod channel_map;
pub mod connect_link;
pub mod connection_card;
pub mod error;
pub mod events;
mod hex;
pub mod identity;
pub mod identity_routes;
pub mod interactions;
pub mod limits;
pub mod link_token;
pub mod mention;
pub mod modal;
pub mod oauth;
pub mod poster;
pub mod state;
pub mod stream_pump;
pub mod thread_map;
pub mod types;
pub mod verify;
pub mod workspace;

pub use state::SlackAppState;

pub use error::SlackError;
pub use types::{
    SLACK_ID_MAX_LEN, SLACK_SIGNATURE_LEN, SLACK_TOKEN_MAX_LEN, SLACK_TS_MAX_LEN, SlackBotToken,
    SlackChannelId, SlackEventTimestamp, SlackSignature, SlackTeamId, SlackThreadTs, SlackTs,
    SlackUserId,
};
pub use verify::{VerifyError, verify};
