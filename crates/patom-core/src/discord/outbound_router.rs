//! Discord implementation of the core [`OutboundRouter`] seam (issue #178).
//!
//! Given only `(org_id, thread_id)`, decide whether the thread belongs to
//! Discord and, if so, attach the stream pump so its feed chunks post to the
//! right container. Classifier arms:
//!
//! 1. **Already bound** — an inbound-originated thread the bridge bound in
//!    `discord_threads`; reuse that container.
//! 2. **Channel thread** — a Patom channel that reverse-maps to a Discord
//!    channel (`discord_channels`); post inline to that channel as the thread's
//!    owning agent's bot. No `discord_threads` write — binding a channel-level
//!    container to one proactive thread would hijack the channel's inbound
//!    routing, and the container is cheaply re-derived from the channel map on
//!    every call (the pump is idempotent per thread).
//! 3. **DM thread** — added in Stage E (arm 3); a no-op here.
//! 4. **Else** — web-origin or another surface; no-op.

use std::fmt;

use async_trait::async_trait;
use tokio::time::timeout;

use crate::auth::OrgId;
use crate::outbound::limits::OUTBOUND_ENSURE_TIMEOUT;
use crate::outbound::{OutboundError, OutboundRouter};
use crate::threads::{SharedThreadStore, ThreadId};

use super::app_store::SharedDiscordAppStore;
use super::bridge::{AttachRequest, SharedOutboundAttach};
use super::channel_map::SharedDiscordChannelStore;
use super::directory::SharedDiscordDirectory;
use super::dm_map::SharedDiscordDmStore;
use super::poster::SharedDiscordPoster;
use super::thread_map::SharedDiscordThreadStore;
use super::types::{ApplicationId, ContainerId};

pub struct DiscordOutboundRouter {
    threads_map: SharedDiscordThreadStore,
    channels: SharedDiscordChannelStore,
    apps: SharedDiscordAppStore,
    directory: SharedDiscordDirectory,
    dms: SharedDiscordDmStore,
    poster: SharedDiscordPoster,
    thread_store: SharedThreadStore,
    pump: SharedOutboundAttach,
}

impl fmt::Debug for DiscordOutboundRouter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DiscordOutboundRouter")
            .finish_non_exhaustive()
    }
}

/// Flatten a platform error into the seam's error (CLAUDE.md §12 — map at the edge).
fn backend(e: impl fmt::Display) -> OutboundError {
    OutboundError::Backend(format!("discord: {e}"))
}

impl DiscordOutboundRouter {
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        threads_map: SharedDiscordThreadStore,
        channels: SharedDiscordChannelStore,
        apps: SharedDiscordAppStore,
        directory: SharedDiscordDirectory,
        dms: SharedDiscordDmStore,
        poster: SharedDiscordPoster,
        thread_store: SharedThreadStore,
        pump: SharedOutboundAttach,
    ) -> Self {
        Self {
            threads_map,
            channels,
            apps,
            directory,
            dms,
            poster,
            thread_store,
            pump,
        }
    }

    async fn attach(
        &self,
        thread: ThreadId,
        org: OrgId,
        application_id: ApplicationId,
        container_id: ContainerId,
    ) {
        self.pump
            .attach(AttachRequest {
                thread_id: thread,
                org_id: org,
                application_id,
                container_id,
                // A proactive / continuation post is plain — no inline reply.
                reply_to: None,
            })
            .await;
    }

    async fn resolve_and_attach(&self, org: OrgId, thread: ThreadId) -> Result<(), OutboundError> {
        // Arm 1: a binding already exists (inbound-originated, re-fired, or
        // continued thread) — reuse its container.
        if let Some(b) = self
            .threads_map
            .lookup_by_patom_thread(thread)
            .await
            .map_err(backend)?
        {
            self.attach(thread, org, b.application_id, b.container_id)
                .await;
            return Ok(());
        }
        // Arm 1b: an existing DM binding — reuse its DM channel.
        if let Some(b) = self
            .dms
            .lookup_by_patom_thread(thread)
            .await
            .map_err(backend)?
        {
            self.attach(thread, org, b.application_id, b.dm_channel_id)
                .await;
            return Ok(());
        }

        // Arm 2: a channel thread whose Patom channel maps to a Discord channel.
        if let Some(channel_id) = self
            .thread_store
            .channel_of(thread)
            .await
            .map_err(backend)?
        {
            return self.attach_channel(org, thread, channel_id).await;
        }

        // Arm 3: a DM thread whose counterpart is a Discord shadow.
        self.attach_new_dm(org, thread).await
    }

    async fn attach_channel(
        &self,
        org: OrgId,
        thread: ThreadId,
        channel_id: crate::channels::ChannelId,
    ) -> Result<(), OutboundError> {
        let Some(cb) = self
            .channels
            .lookup_by_channel(channel_id)
            .await
            .map_err(backend)?
        else {
            return Ok(()); // Channel is not Discord-backed.
        };
        let Some(app) = self.bot_for_thread(org, thread).await? else {
            return Ok(());
        };
        self.attach(thread, org, app, cb.discord_channel_id).await;
        Ok(())
    }

    async fn attach_new_dm(&self, org: OrgId, thread: ThreadId) -> Result<(), OutboundError> {
        let Some(counterpart) = self
            .thread_store
            .dm_counterpart(thread)
            .await
            .map_err(backend)?
        else {
            return Ok(()); // Web-origin / not a DM.
        };
        let Some(snowflake) = self
            .directory
            .snowflake_for(org, counterpart)
            .await
            .map_err(backend)?
        else {
            return Ok(()); // Counterpart is not a Discord shadow — stays web-only.
        };
        let Some(app) = self.bot_for_thread(org, thread).await? else {
            return Ok(());
        };
        // Open (or fetch) the DM channel, then bind before attaching so a re-fire
        // resolves arm 1b and never opens a second channel (idempotent).
        let channel = self
            .poster
            .create_dm(&app, &snowflake)
            .await
            .map_err(backend)?;
        self.dms
            .bind(org, &app, thread, &channel)
            .await
            .map_err(backend)?;
        self.attach(thread, org, app, channel).await;
        Ok(())
    }

    /// The Discord bot the thread's owning agent speaks as, or `None` when the
    /// agent has no Discord app (nothing to post as).
    // `map_or` can't host the `await` in the `Some` arm — the match is the
    // idiomatic async form.
    #[allow(clippy::option_if_let_else)]
    async fn bot_for_thread(
        &self,
        org: OrgId,
        thread: ThreadId,
    ) -> Result<Option<ApplicationId>, OutboundError> {
        match self
            .thread_store
            .last_agent(thread)
            .await
            .map_err(backend)?
        {
            Some(agent) => self
                .apps
                .app_id_for_agent(org, agent)
                .await
                .map_err(backend),
            None => Ok(None),
        }
    }
}

#[async_trait]
impl OutboundRouter for DiscordOutboundRouter {
    async fn ensure_delivery(
        &self,
        org_id: OrgId,
        thread_id: ThreadId,
    ) -> Result<(), OutboundError> {
        timeout(
            OUTBOUND_ENSURE_TIMEOUT,
            self.resolve_and_attach(org_id, thread_id),
        )
        .await
        .unwrap_or(Err(OutboundError::Timeout))
    }
}
