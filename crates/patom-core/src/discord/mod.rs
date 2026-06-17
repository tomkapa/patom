//! Discord BYO-bot chat integration — the **Gateway (WebSocket)** adapter.
//!
//! Patom's third chat integration (after Slack and Lark). Each customer
//! registers one self-built Discord application per agent; Patom opens one
//! Gateway connection per bot (JSON frames `{op,d,s,t}` over `wss`), mirrors
//! inbound Discord messages into `thread_messages` as ordinary `posted` rows (so
//! the agent loop is reused unchanged), shadow-mints every observed sender into
//! a stable colleague, triggers an agent run on a mention/DM (ambient channel
//! messages are ingested for context only), and posts replies back with
//! `<@id>` mentions guarded by a mandatory `allowed_mentions`.
//!
//! **Module boundary (by discipline):** this module depends on `patom-core`'s
//! neutral ports (`ThreadStore`, the prompt queue, `ThreadDisplayNames`, the
//! colleague mint, `send_message` addressing); non-`discord` core code never
//! references `discord::`. See `doc/discord-byo-integration-plan.md`.
//!
//! The shape mirrors the Lark adapter (`crate::lark`); Discord is the **"clean"**
//! platform — a stable global user snowflake, no email dependency, a static bot
//! token (no refresh loop), and self-attributing history — so the adapter is a
//! thinner mirror that exercises the *generic* core seams more than
//! adapter-specific machinery. The two genuinely new hazards versus Lark are the
//! mandatory `allowed_mentions` safety object and the Cloudflare invalid-request
//! ban that aggregates across a shared egress IP (both handled in `poster.rs`).

pub mod admin_routes;
pub mod app_store;
pub mod attachment;
pub mod bridge;
pub mod channel_map;
pub mod codec;
pub mod connection;
pub mod directory;
pub mod dm_map;
pub mod error;
pub mod event;
pub mod handshake;
pub mod history;
pub mod limits;
pub mod mention;
pub mod outbound_router;
pub mod poster;
pub mod ratelimit;
pub mod roster;
pub mod state;
pub mod stream_pump;
pub mod thread_map;
pub mod thread_opener;
pub mod transport;
pub mod types;
pub mod ws_manager;

pub use error::DiscordError;
pub use state::DiscordAppState;
