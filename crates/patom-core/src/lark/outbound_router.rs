//! Lark implementation of the core [`OutboundRouter`] seam (issue #178).
//!
//! Symmetric to `discord::outbound_router`. Given only `(org_id, thread_id)`,
//! decide whether the thread belongs to Lark and attach the stream pump:
//!
//! 1. **Already bound** — an inbound-originated thread bound in `lark_threads`;
//!    reuse its `(app_id, chat_id)`.
//! 2. **Channel thread** — a Patom channel that reverse-maps to a Lark chat
//!    (`lark_channels`); post top-level to that chat as the thread's owning
//!    agent's bot. No `lark_threads` write — a top-level channel post has no
//!    `lark_thread_id` to key on, and the chat is re-derived from the channel
//!    map on every call (the pump is idempotent per thread).
//! 3. **DM thread** — added in Stage E (arm 3); a no-op here.
//! 4. **Else** — web-origin or another surface; no-op.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::time::timeout;

use crate::auth::OrgId;
use crate::outbound::limits::OUTBOUND_ENSURE_TIMEOUT;
use crate::outbound::{OutboundError, OutboundRouter};
use crate::threads::{SharedThreadStore, ThreadId};

use super::app_store::SharedLarkAppStore;
use super::channel_map::SharedLarkChannelStore;
use super::directory::SharedLarkDirectory;
use super::dm_map::SharedLarkDmStore;
use super::stream_pump::{AttachRequest, LarkPumpHandle, LarkRecipient};
use super::thread_map::SharedLarkThreadStore;
use super::types::{LarkAppId, LarkChatId, LarkOpenId};

/// The attach seam the Lark router depends on.
///
/// Kept a trait so the router is testable with a recording fake (the Discord
/// adapter has the equivalent `OutboundAttach`). [`LarkPumpHandle`] is the
/// production implementation.
#[async_trait]
pub trait LarkOutboundAttach: fmt::Debug + Send + Sync {
    async fn attach(&self, req: AttachRequest);
}

pub type SharedLarkOutboundAttach = Arc<dyn LarkOutboundAttach>;

#[async_trait]
impl LarkOutboundAttach for LarkPumpHandle {
    async fn attach(&self, req: AttachRequest) {
        // Defer to the handle's inherent attach (also used by the inbound bridge).
        Self::attach(self, req).await;
    }
}

pub struct LarkOutboundRouter {
    threads_map: SharedLarkThreadStore,
    channels: SharedLarkChannelStore,
    apps: SharedLarkAppStore,
    directory: SharedLarkDirectory,
    dms: SharedLarkDmStore,
    thread_store: SharedThreadStore,
    pump: SharedLarkOutboundAttach,
}

impl fmt::Debug for LarkOutboundRouter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LarkOutboundRouter").finish_non_exhaustive()
    }
}

fn backend(e: impl fmt::Display) -> OutboundError {
    OutboundError::Backend(format!("lark: {e}"))
}

impl LarkOutboundRouter {
    #[must_use]
    pub fn new(
        threads_map: SharedLarkThreadStore,
        channels: SharedLarkChannelStore,
        apps: SharedLarkAppStore,
        directory: SharedLarkDirectory,
        dms: SharedLarkDmStore,
        thread_store: SharedThreadStore,
        pump: SharedLarkOutboundAttach,
    ) -> Self {
        Self {
            threads_map,
            channels,
            apps,
            directory,
            dms,
            thread_store,
            pump,
        }
    }

    async fn attach_chat(
        &self,
        thread: ThreadId,
        org: OrgId,
        app_id: LarkAppId,
        chat_id: LarkChatId,
    ) {
        self.pump
            .attach(AttachRequest {
                thread_id: thread,
                org_id: org,
                app_id,
                // A proactive / continuation post is top-level — no threaded reply.
                recipient: LarkRecipient::Chat {
                    chat_id,
                    reply_to: None,
                },
            })
            .await;
    }

    async fn attach_dm(
        &self,
        thread: ThreadId,
        org: OrgId,
        app_id: LarkAppId,
        open_id: LarkOpenId,
    ) {
        self.pump
            .attach(AttachRequest {
                thread_id: thread,
                org_id: org,
                app_id,
                recipient: LarkRecipient::Dm { open_id },
            })
            .await;
    }

    async fn resolve_and_attach(&self, org: OrgId, thread: ThreadId) -> Result<(), OutboundError> {
        // Arm 1: a binding already exists — reuse its chat.
        if let Some(b) = self
            .threads_map
            .lookup_by_patom_thread(thread)
            .await
            .map_err(backend)?
        {
            self.attach_chat(thread, org, b.app_id, b.chat_id).await;
            return Ok(());
        }
        // Arm 1b: an existing DM binding — reuse its recipient open_id.
        if let Some(b) = self
            .dms
            .lookup_by_patom_thread(thread)
            .await
            .map_err(backend)?
        {
            self.attach_dm(thread, org, b.app_id, b.open_id).await;
            return Ok(());
        }

        // Arm 2: a channel thread whose Patom channel maps to a Lark chat.
        if let Some(channel_id) = self
            .thread_store
            .channel_of(thread)
            .await
            .map_err(backend)?
        {
            return self.attach_channel(org, thread, channel_id).await;
        }

        // Arm 3: a DM thread whose counterpart is a Lark shadow.
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
            return Ok(()); // Channel is not Lark-backed.
        };
        let Some(app) = self.bot_for_thread(org, thread).await? else {
            return Ok(());
        };
        self.attach_chat(thread, org, app, cb.chat_id).await;
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
        let Some(open_id) = self
            .directory
            .open_id_for(org, counterpart)
            .await
            .map_err(backend)?
        else {
            return Ok(()); // Counterpart is not a Lark shadow — stays web-only.
        };
        let Some(app) = self.bot_for_thread(org, thread).await? else {
            return Ok(());
        };
        // Bind before attaching so a re-fire resolves arm 1b (idempotent).
        self.dms
            .bind(org, &app, thread, &open_id)
            .await
            .map_err(backend)?;
        self.attach_dm(thread, org, app, open_id).await;
        Ok(())
    }

    /// The Lark bot the thread's owning agent speaks as, or `None` when the agent
    /// has no Lark app (nothing to post as).
    // `map_or` can't host the `await` in the `Some` arm — the match is the
    // idiomatic async form.
    #[allow(clippy::option_if_let_else)]
    async fn bot_for_thread(
        &self,
        org: OrgId,
        thread: ThreadId,
    ) -> Result<Option<LarkAppId>, OutboundError> {
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
impl OutboundRouter for LarkOutboundRouter {
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
