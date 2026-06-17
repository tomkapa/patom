//! Shared Discord adapter state held on `AppState`.
//!
//! Cheap-clone (all fields are `Arc`). Holds the app-registration store (used by
//! the admin route) plus the pump + gateway-manager handles (used by
//! `run_server` for ordered shutdown). The inbound bridge handle is returned
//! separately from `build_server` (like Lark), since it is consumed by shutdown.

use crate::clock::SharedClock;
use crate::types::SecretString;

use super::app_store::SharedDiscordAppStore;
use super::poster::SharedDiscordPoster;
use super::stream_pump::SharedDiscordPumpHandle;
use super::ws_manager::SharedWsManagerHandle;

/// Discord adapter handles exposed on `AppState`.
#[derive(Clone)]
pub struct DiscordAppState {
    pub apps: SharedDiscordAppStore,
    pub stream_pump: SharedDiscordPumpHandle,
    pub ws_manager: SharedWsManagerHandle,
    /// HMAC key verifying `GET /discord/mcp/connect` tokens (derived from
    /// `master_kek`; the same key the stream pump signs with).
    pub connect_secret: SecretString,
    /// Clock for the connect-token expiry check.
    pub clock: SharedClock,
    /// Posts the "✓ Connected" ping back into the originating channel after the
    /// OAuth callback succeeds (the poster resolves the bot token internally).
    pub poster: SharedDiscordPoster,
}

impl std::fmt::Debug for DiscordAppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscordAppState").finish_non_exhaustive()
    }
}
