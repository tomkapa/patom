//! Inbound bridge: a dispatched Gateway event → a Patom thread message (+ an
//! agent trigger on a mention/DM).
//!
//! One single-consumer worker. The connection loop decodes frames and hands
//! [`InboundDispatch`] here via a bounded mpsc; this worker does the slow path
//! (DB lookups, shadow-mint, append, enqueue) off the connection task.
//!
//! Per-message flow ([`process_event`] → `handle_message`):
//! 1. Drop the bot's own messages (`author.id == bot_user_id`).
//! 2. Skip peer-bot / webhook authors (no human shadow — attribution deferred).
//! 3. Shadow-mint the sender's colleague; mirror the channel and add the sender
//!    as a member (so the append passes channel RLS).
//! 4. Classify: a DM, or a guild message that `@`-mentions the bot, is a
//!    **trigger**; any other guild message is **ambient** (ingest only).
//! 5. Resolve (or create + bind) the Patom thread; append the `posted` row with
//!    `idempotency_key = discord:{guild}:{message_id}` (dedupes redelivery /
//!    backfill overlap).
//! 6. Trigger only: resolve the agent's participation, enqueue a fresh-DAG
//!    trigger, and attach the outbound pump.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, info, info_span, warn};

use crate::auth::{Caller, OrgId, UserId};
use crate::channels::ChannelId;
use crate::colleagues::{ColleagueId, SharedColleagueStore};
use crate::provider::{ChatMessage, UserContent};
use crate::runtime::{IdempotencyKey, NewTrigger, RequestKindPayload, SharedPromptQueue};
use crate::threads::{MessageKind, NewMessage, SharedThreadStore, ThreadId, ThreadMessageId};

use super::app_store::{DiscordApp, SharedDiscordAppStore};
use super::channel_map::SharedDiscordChannelStore;
use super::connection::InboundDispatch;
use super::directory::SharedDiscordDirectory;
use super::error::DiscordError;
use super::event::{self, DiscordEvent, InboundMessage};
use super::limits::{DISCORD_DISPLAY_NAME_MAX, DISCORD_INBOUND_CONTENT_MAX, DISCORD_INBOUND_QUEUE};
use super::roster;
use super::types::{ContainerId, DiscordUserId, GuildId};

/// Where a freshly-triggered Patom thread's outbound chunks should be routed back
/// into Discord. The (D6) stream pump implements [`OutboundAttach`].
#[derive(Debug, Clone)]
pub struct AttachRequest {
    pub thread_id: ThreadId,
    pub org_id: OrgId,
    pub container_id: ContainerId,
    /// The triggering message, so the reply can be a Discord reply.
    pub reply_to: super::types::DiscordMessageId,
}

/// The seam the bridge uses to attach the outbound pump for a thread. Kept a
/// trait so the bridge is testable without the pump and the pump is wired only
/// at the composition root.
#[async_trait]
pub trait OutboundAttach: fmt::Debug + Send + Sync {
    async fn attach(&self, req: AttachRequest);
}

pub type SharedOutboundAttach = Arc<dyn OutboundAttach>;

/// Dependencies for the bridge worker. Cloned per event so `process_event`
/// stays a free function for testing.
#[derive(Clone)]
pub struct BridgeDeps {
    pub apps: SharedDiscordAppStore,
    pub directory: SharedDiscordDirectory,
    pub channels: SharedDiscordChannelStore,
    pub threads: super::thread_map::SharedDiscordThreadStore,
    pub thread_store: SharedThreadStore,
    pub colleagues: SharedColleagueStore,
    pub queue: SharedPromptQueue,
    pub outbound: SharedOutboundAttach,
}

impl fmt::Debug for BridgeDeps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BridgeDeps").finish_non_exhaustive()
    }
}

/// Handle for the spawned bridge worker.
#[derive(Debug)]
pub struct BridgeHandle {
    cancel: CancellationToken,
    join: tokio::task::JoinHandle<()>,
}

impl BridgeHandle {
    pub async fn shutdown(self) {
        self.cancel.cancel();
        let _ = self.join.await;
    }
}

/// Spawn the bridge worker; returns its handle plus the sender the connection
/// loop hands dispatches into.
pub fn spawn(
    deps: BridgeDeps,
    cancel: CancellationToken,
) -> (BridgeHandle, mpsc::Sender<InboundDispatch>) {
    let (tx, rx) = mpsc::channel::<InboundDispatch>(DISCORD_INBOUND_QUEUE);
    let cancel_for_handle = cancel.clone();
    let join = tokio::spawn(run_loop(deps, rx, cancel));
    (
        BridgeHandle {
            cancel: cancel_for_handle,
            join,
        },
        tx,
    )
}

async fn run_loop(
    deps: BridgeDeps,
    mut rx: mpsc::Receiver<InboundDispatch>,
    cancel: CancellationToken,
) {
    info!(event = "discord.bridge.start");
    loop {
        tokio::select! {
            () = cancel.cancelled() => {
                info!(event = "discord.bridge.shutdown");
                return;
            }
            maybe = rx.recv() => {
                let Some(dispatch) = maybe else {
                    info!(event = "discord.bridge.tx_closed");
                    return;
                };
                let span = info_span!("discord.bridge.process");
                if let Err(e) = process_event(&deps, dispatch).instrument(span).await {
                    warn!(error = ?e, event = "discord.bridge.process_failed");
                }
            }
        }
    }
}

/// Route one dispatch. Extracted so tests drive it directly.
pub async fn process_event(
    deps: &BridgeDeps,
    dispatch: InboundDispatch,
) -> Result<(), DiscordError> {
    let event = event::parse(&dispatch.event_type, &dispatch.data)?;
    if matches!(event, DiscordEvent::Other) {
        return Ok(());
    }
    let app = deps.apps.read_by_app_id(&dispatch.application_id).await?;
    match event {
        DiscordEvent::Message(m) => handle_message(deps, &app, &dispatch.bot_user_id, *m).await,
        DiscordEvent::GuildCreate(gc) => roster::sync_guild(deps, app.org_id, &gc).await,
        DiscordEvent::MemberUpsert(ev) => roster::sync_member(deps, app.org_id, &ev).await,
        DiscordEvent::Other => Ok(()),
    }
}

async fn handle_message(
    deps: &BridgeDeps,
    app: &DiscordApp,
    bot_user_id: &DiscordUserId,
    m: InboundMessage,
) -> Result<(), DiscordError> {
    // 1. Drop the bot's own re-delivered messages (no mirror, no re-trigger).
    if m.author.id == *bot_user_id {
        return Ok(());
    }
    // 2. Peer bots and webhook authors are not human shadows; attribution to a
    //    shared "integration" colleague is deferred — skip them in the live path.
    if m.author.bot || m.webhook_id.is_some() {
        debug!(event = "discord.bridge.non_human_author_skipped");
        return Ok(());
    }
    // A DM carries no guild; anchor its thread/channel on the channel id itself.
    let guild = match &m.guild_id {
        Some(g) => g.clone(),
        None => GuildId::try_from(m.channel_id.as_str())?,
    };
    let display = m
        .author
        .display_name(m.member_nick.as_deref())
        .map(|d| d.chars().take(DISCORD_DISPLAY_NAME_MAX).collect::<String>());
    let shadow = deps
        .directory
        .resolve_or_mint(app.org_id, &m.author.id, display.as_deref())
        .await?;
    let caller = Caller::new(shadow.user_id, app.org_id);
    let channel_id = deps
        .channels
        .ensure_channel(app.org_id, &guild, &m.channel_id, shadow.user_id)
        .await?;

    let is_trigger = m.guild_id.is_none() || m.mentions_bot(bot_user_id);
    let thread_id = resolve_thread(
        deps,
        &caller,
        app,
        &guild,
        &m,
        channel_id,
        shadow.colleague_id,
    )
    .await?;

    let body_text: String = m
        .content
        .chars()
        .take(DISCORD_INBOUND_CONTENT_MAX)
        .collect();
    let idem = IdempotencyKey::try_from(format!(
        "discord:{}:{}",
        guild.as_str(),
        m.message_id.as_str()
    ))?;
    let receiver = if is_trigger {
        Some(resolve_agent_colleague(deps, app).await?)
    } else {
        None
    };
    let appended = deps
        .thread_store
        .append(
            &caller,
            thread_id,
            NewMessage {
                kind: MessageKind::Posted,
                sender: Some(shadow.colleague_id),
                owner_agent_id: None,
                receiver,
                body: ChatMessage::User(vec![UserContent::Text(body_text)]),
                request_id: None,
                idempotency_key: Some(idem.clone()),
            },
        )
        .await
        .map_err(|e| DiscordError::Internal(format!("append: {e}")))?;

    if is_trigger {
        enqueue_and_attach(
            deps,
            &caller,
            app,
            &guild,
            &m,
            thread_id,
            shadow.colleague_id,
            appended,
        )
        .await?;
    }
    Ok(())
}

/// Resolve the bound Patom thread, or create + bind one on first sight.
async fn resolve_thread(
    deps: &BridgeDeps,
    caller: &Caller,
    app: &DiscordApp,
    guild: &GuildId,
    m: &InboundMessage,
    channel_id: ChannelId,
    creator: ColleagueId,
) -> Result<ThreadId, DiscordError> {
    if let Some(mapping) = deps
        .threads
        .lookup_by_container(guild, &m.channel_id)
        .await?
    {
        return Ok(mapping.thread_id);
    }
    let thread = deps
        .thread_store
        .create_thread(caller, Some(channel_id), None, creator, None)
        .await
        .map_err(|e| DiscordError::Internal(format!("create thread: {e}")))?;
    // parent_id is unknown from a MESSAGE_CREATE; recorded later from THREAD_*.
    deps.threads
        .bind(
            app.org_id,
            &app.application_id,
            guild,
            &m.channel_id,
            None,
            thread,
        )
        .await?;
    Ok(thread)
}

/// Resolve the app's agent to its colleague id (the message receiver).
async fn resolve_agent_colleague(
    deps: &BridgeDeps,
    app: &DiscordApp,
) -> Result<ColleagueId, DiscordError> {
    deps.colleagues
        .resolve_agent(app.org_id, app.agent_id)
        .await
        .map_err(|e| DiscordError::Internal(format!("resolve agent colleague: {e}")))
}

/// Resolve participation, enqueue a fresh-DAG trigger, and attach the pump.
#[allow(clippy::too_many_arguments)]
async fn enqueue_and_attach(
    deps: &BridgeDeps,
    caller: &Caller,
    app: &DiscordApp,
    guild: &GuildId,
    m: &InboundMessage,
    thread_id: ThreadId,
    sender_colleague: ColleagueId,
    trigger_msg: ThreadMessageId,
) -> Result<(), DiscordError> {
    let state_id = deps
        .thread_store
        .resolve_participation(caller, thread_id, app.agent_id)
        .await
        .map_err(|e| DiscordError::Internal(format!("resolve participation: {e}")))?;
    let acting_user_id: UserId = caller.user_id;
    let idem = IdempotencyKey::try_from(format!(
        "discord:{}:{}",
        guild.as_str(),
        m.message_id.as_str()
    ))?;
    let request_id = deps
        .queue
        .enqueue_trigger(NewTrigger {
            org_id: app.org_id,
            acting_user_id,
            thread_id: Some(thread_id),
            state_id: Some(state_id),
            background_turn_id: None,
            sender_colleague_id: sender_colleague,
            receiver_agent_id: app.agent_id,
            root_request_id: None,
            trigger_message_id: Some(trigger_msg),
            idempotency_key: idem,
            kind_payload: RequestKindPayload::Normal {},
        })
        .await?;
    deps.outbound
        .attach(AttachRequest {
            thread_id,
            org_id: app.org_id,
            container_id: m.channel_id.clone(),
            reply_to: m.message_id.clone(),
        })
        .await;
    info!(
        patom.thread.id = %thread_id.as_uuid(),
        patom.request.id = %request_id.as_uuid(),
        event = "discord.bridge.enqueued",
    );
    Ok(())
}
