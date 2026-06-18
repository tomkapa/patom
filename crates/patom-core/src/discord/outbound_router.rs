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

use crate::approvals::{ActionSummary, ApprovalId, PlatformMessageId, PlatformTarget};
use crate::auth::OrgId;
use crate::outbound::limits::OUTBOUND_ENSURE_TIMEOUT;
use crate::outbound::{OutboundError, OutboundRouter};
use crate::threads::{SharedThreadStore, ThreadId};

use super::app_store::SharedDiscordAppStore;
use super::bridge::{AttachRequest, SharedOutboundAttach};
use super::channel_map::SharedDiscordChannelStore;
use super::directory::SharedDiscordDirectory;
use super::dm_map::SharedDiscordDmStore;
use super::poster::{
    ActionRow, AllowedMentions, Button, ButtonStyle, PostRequest, SharedDiscordPoster,
};
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
                // Proactive delivery has no triggering user; a connect link
                // here degrades to the web-UI pointer.
                user_id: None,
                application_id,
                container_id,
                // A proactive / continuation post is plain — no inline reply.
                reply_to: None,
            })
            .await;
    }

    async fn resolve_and_attach(&self, org: OrgId, thread: ThreadId) -> Result<(), OutboundError> {
        if let Some((app, container)) = self.resolve_container(org, thread).await? {
            self.attach(thread, org, app, container).await;
        }
        Ok(())
    }

    /// Resolve the Discord `(bot, container)` this thread posts to — the binding
    /// the pump attaches and the approval card posts to — or `None` when the
    /// thread is not Discord-backed. Shared by `ensure_delivery` (attach) and the
    /// approval seam (`resolve_target` / `post_approval`). Arm 3 opens + binds a
    /// DM channel as a side effect, idempotently (a re-call resolves arm 1b).
    async fn resolve_container(
        &self,
        org: OrgId,
        thread: ThreadId,
    ) -> Result<Option<(ApplicationId, ContainerId)>, OutboundError> {
        // Arm 1: a binding already exists (inbound-originated, re-fired, or
        // continued thread) — reuse its container.
        if let Some(b) = self
            .threads_map
            .lookup_by_patom_thread(thread)
            .await
            .map_err(backend)?
        {
            return Ok(Some((b.application_id, b.container_id)));
        }
        // Arm 1b: an existing DM binding — reuse its DM channel.
        if let Some(b) = self
            .dms
            .lookup_by_patom_thread(thread)
            .await
            .map_err(backend)?
        {
            return Ok(Some((b.application_id, b.dm_channel_id)));
        }

        // Arm 2: a channel thread whose Patom channel maps to a Discord channel.
        if let Some(channel_id) = self
            .thread_store
            .channel_of(thread)
            .await
            .map_err(backend)?
        {
            return self.channel_container(org, thread, channel_id).await;
        }

        // Arm 3: a DM thread whose counterpart is a Discord shadow.
        self.new_dm_container(org, thread).await
    }

    async fn channel_container(
        &self,
        org: OrgId,
        thread: ThreadId,
        channel_id: crate::channels::ChannelId,
    ) -> Result<Option<(ApplicationId, ContainerId)>, OutboundError> {
        let Some(cb) = self
            .channels
            .lookup_by_channel(channel_id)
            .await
            .map_err(backend)?
        else {
            return Ok(None); // Channel is not Discord-backed.
        };
        let Some(app) = self.bot_for_thread(org, thread).await? else {
            return Ok(None);
        };
        Ok(Some((app, cb.discord_channel_id)))
    }

    async fn new_dm_container(
        &self,
        org: OrgId,
        thread: ThreadId,
    ) -> Result<Option<(ApplicationId, ContainerId)>, OutboundError> {
        let Some(counterpart) = self
            .thread_store
            .dm_counterpart(thread)
            .await
            .map_err(backend)?
        else {
            return Ok(None); // Web-origin / not a DM.
        };
        let Some(snowflake) = self
            .directory
            .snowflake_for(org, counterpart)
            .await
            .map_err(backend)?
        else {
            return Ok(None); // Counterpart is not a Discord shadow — stays web-only.
        };
        let Some(app) = self.bot_for_thread(org, thread).await? else {
            return Ok(None);
        };
        // Open (or fetch) the DM channel, then bind so a re-call resolves arm 1b
        // and never opens a second channel (idempotent).
        let channel = self
            .poster
            .create_dm(&app, &snowflake)
            .await
            .map_err(backend)?;
        self.dms
            .bind(org, &app, thread, &channel)
            .await
            .map_err(backend)?;
        Ok(Some((app, channel)))
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

    /// Post the interactive approval card (Approve / Deny buttons) to `container`
    /// as `application_id`'s bot, returning the issued message id so the resolve
    /// path can edit it. The buttons' `custom_id` carries `apv:{id}:a|d`, parsed
    /// back on `INTERACTION_CREATE`.
    async fn post_card(
        &self,
        application_id: ApplicationId,
        container_id: ContainerId,
        approval_id: ApprovalId,
        action: &ActionSummary,
    ) -> Result<PlatformMessageId, OutboundError> {
        let content = format!(
            "🔔 Approval needed: {}. A decision is required before I proceed.",
            action.as_str()
        );
        let row = ActionRow::new(vec![
            Button::new(
                ButtonStyle::Success,
                "Approve",
                format!("apv:{}:a", approval_id.as_uuid()),
            ),
            Button::new(
                ButtonStyle::Danger,
                "Deny",
                format!("apv:{}:d", approval_id.as_uuid()),
            ),
        ]);
        let ids = self
            .poster
            .post(PostRequest {
                application_id,
                container_id,
                reply_to: None,
                content,
                allowed_mentions: AllowedMentions::none(),
                components: vec![row],
            })
            .await
            .map_err(backend)?;
        let id = ids.into_iter().next().ok_or_else(|| {
            OutboundError::Backend("discord: approval card returned no message id".to_owned())
        })?;
        PlatformMessageId::try_from(id.as_str().to_owned()).map_err(backend)
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

    async fn resolve_target(
        &self,
        org_id: OrgId,
        thread_id: ThreadId,
    ) -> Result<Option<PlatformTarget>, OutboundError> {
        let resolved = timeout(
            OUTBOUND_ENSURE_TIMEOUT,
            self.resolve_container(org_id, thread_id),
        )
        .await
        .unwrap_or(Err(OutboundError::Timeout))?;
        Ok(resolved.map(|(app, container)| PlatformTarget::Discord {
            application_id: app.as_str().to_owned(),
            container_id: container.as_str().to_owned(),
            // A fresh approval card, not an inline reply.
            reply_to: None,
        }))
    }

    async fn post_approval(
        &self,
        _org_id: OrgId,
        _thread_id: ThreadId,
        target: &PlatformTarget,
        approval_id: ApprovalId,
        action: &ActionSummary,
    ) -> Result<Option<PlatformMessageId>, OutboundError> {
        // Self-skip when the row is bound to another surface (the composite asks
        // every router; only the owning one posts).
        let PlatformTarget::Discord {
            application_id,
            container_id,
            ..
        } = target
        else {
            return Ok(None);
        };
        let app = ApplicationId::try_from(application_id.as_str()).map_err(backend)?;
        let container = ContainerId::try_from(container_id.as_str()).map_err(backend)?;
        timeout(
            OUTBOUND_ENSURE_TIMEOUT,
            self.post_card(app, container, approval_id, action),
        )
        .await
        .unwrap_or(Err(OutboundError::Timeout))
        .map(Some)
    }
}
