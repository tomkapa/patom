//! Shared Lark adapter state held on `AppState`.
//!
//! Cheap-clone (all fields are `Arc`). Holds the app-registration store (used by
//! the admin route) plus the pump + WS-manager handles (used by `run_server` for
//! ordered shutdown). The inbound bridge handle is returned separately from
//! `build_server` (like the Slack bridge), since it is consumed by `shutdown`.

use super::app_store::SharedLarkAppStore;
use super::stream_pump::SharedLarkPumpHandle;
use super::ws_manager::SharedWsManagerHandle;

/// Lark adapter handles exposed on `AppState`.
#[derive(Clone)]
pub struct LarkAppState {
    pub apps: SharedLarkAppStore,
    pub stream_pump: SharedLarkPumpHandle,
    pub ws_manager: SharedWsManagerHandle,
}

impl std::fmt::Debug for LarkAppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LarkAppState").finish_non_exhaustive()
    }
}
