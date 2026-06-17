//! Lark (Feishu) BYO-bot chat integration — the **long-connection (WebSocket)**
//! adapter.
//!
//! Patom's first-class chat integration. Each customer registers one self-built
//! Lark app per agent; Patom opens a `pbbp2` long-connection per bot, mirrors
//! inbound Lark messages into `thread_messages` as ordinary `posted` rows (so
//! the agent loop is reused unchanged), shadow-mints every observed sender into
//! a stable colleague, triggers an agent run on a mention/DM (ambient group
//! messages are ingested for context only), and posts replies back with
//! `@`-tags.
//!
//! **Module boundary (by discipline):** this module depends on `patom-core`'s
//! neutral ports (`ThreadStore`, the prompt queue, `ThreadDisplayNames`, the
//! colleague mint, `send_message` addressing); non-`lark` core code never
//! references `lark::`. See `docs/lark-byo-integration-plan.md`.
//!
//! The shape mirrors the Slack adapter (`crate::slack`); the two differences
//! are the transport (WS long-connection vs HMAC webhook) and identity
//! (shadow-mint-every-sender vs link-or-drop).

pub mod admin_routes;
pub mod app_store;
pub mod bridge;
pub mod channel_map;
pub mod codec;
pub mod connect_link;
pub mod directory;
pub mod dm_map;
pub mod error;
pub mod event;
pub mod handshake;
pub mod limits;
pub mod mention;
pub mod outbound_router;
pub mod pbbp2;
pub mod poster;
pub mod resource;
pub mod roster;
pub mod state;
pub mod stream_pump;
pub mod thread_map;
pub mod token;
pub mod transport;
pub mod types;
pub mod ws_manager;

pub use error::LarkError;
pub use state::LarkAppState;
