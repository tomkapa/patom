//! Shared Lark adapter state held on `AppState`.
//!
//! Cheap-clone (all fields are `Arc`). Holds the app-registration store (used by
//! the admin route) plus the pump + WS-manager handles (used by `run_server` for
//! ordered shutdown). The inbound bridge handle is returned separately from
//! `build_server` (like the Slack bridge), since it is consumed by `shutdown`.

use crate::approvals::SharedApprovalDecider;
use crate::clock::SharedClock;
use crate::types::SecretString;

use super::app_store::SharedLarkAppStore;
use super::directory::SharedLarkDirectory;
use super::poster::SharedLarkPoster;
use super::stream_pump::SharedLarkPumpHandle;
use super::token::SharedTokenProvider;
use super::ws_manager::SharedWsManagerHandle;

/// Lark adapter handles exposed on `AppState`.
#[derive(Clone)]
pub struct LarkAppState {
    pub apps: SharedLarkAppStore,
    pub stream_pump: SharedLarkPumpHandle,
    pub ws_manager: SharedWsManagerHandle,
    /// HMAC key verifying `GET /lark/mcp/connect` tokens (derived from
    /// `master_kek`; the same key the stream pump signs with).
    pub connect_secret: SecretString,
    /// Clock for the connect-token expiry check.
    pub clock: SharedClock,
    /// Posts the "✓ Connected" ping back into the originating Lark chat after
    /// the OAuth callback succeeds.
    pub poster: SharedLarkPoster,
    /// Mints the `tenant_access_token` the ping posts with.
    pub token_provider: SharedTokenProvider,
    /// People directory — the card-action route reverse-looks-up the clicking
    /// `open_id` to a colleague (#214).
    pub directory: SharedLarkDirectory,
    /// Resolves an approval card click: authorize → decide → resume (#214). The
    /// one seam every chat surface shares.
    pub decider: SharedApprovalDecider,
}

impl std::fmt::Debug for LarkAppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LarkAppState").finish_non_exhaustive()
    }
}
