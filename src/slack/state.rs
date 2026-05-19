//! Slack-side state container, embedded into [`AppState`] as an
//! `Option<SlackAppState>` so deployments without Slack stay clean
//! (handlers `404` instead of `500` when the feature is off).

use std::sync::Arc;

use reqwest::Client;
use tokio::sync::mpsc;

use crate::types::SecretString;

use super::bridge::InboundEvent;
use super::identity::SharedSlackIdentityStore;
use super::poster::SharedSlackPoster;
use super::stream_pump::SharedStreamPumpHandle;
use super::thread_map::SharedSlackThreadStore;
use super::workspace::SharedSlackWorkspaceStore;

/// Slack feature wiring shared by the public webhook handler, the
/// OAuth callback, and the private install endpoint. Cheap-clone.
#[derive(Clone)]
pub struct SlackAppState {
    pub signing_secret: SecretString,
    pub client_id: Arc<str>,
    pub client_secret: SecretString,
    /// E.g. `https://relay.example.com/slack/oauth/callback`.
    pub redirect_url: Arc<str>,
    pub workspaces: SharedSlackWorkspaceStore,
    pub identities: SharedSlackIdentityStore,
    pub threads: SharedSlackThreadStore,
    pub poster: SharedSlackPoster,
    /// Shared HTTP client used by the OAuth `oauth.v2.access` call.
    /// Held here (not built per-call) so the TLS pool is reused across
    /// installs.
    pub http: Client,
    /// Sender half of the bounded mpsc the webhook handler uses to
    /// hand events off to the bridge worker.
    pub bridge_tx: mpsc::Sender<InboundEvent>,
    /// Handle for attaching new stream pumps after a fresh bind.
    pub stream_pump: SharedStreamPumpHandle,
}

impl std::fmt::Debug for SlackAppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SlackAppState")
            .field("client_id", &self.client_id)
            .field("redirect_url", &self.redirect_url)
            .finish_non_exhaustive()
    }
}
