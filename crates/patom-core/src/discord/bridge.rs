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
//! 3. Shadow-mint the sender's colleague.
//! 4. Classify: a DM, or a guild message that `@`-mentions the bot, is a
//!    **trigger**; any other guild message is **ambient** (ingest only). The
//!    trigger gate is `@`-mention-or-DM only — a message inside a thread does
//!    *not* auto-continue; a follow-up re-`@`-mentions the bot.
//! 5. Resolve the conversation container ([`resolve_conversation`]): a top-level
//!    guild @mention **opens a thread** on the triggering message and converses
//!    there (so the channel stays clean), degrading to an inline channel reply
//!    if the open fails; a message already inside a bot-owned thread continues
//!    there; a DM or ambient message stays in its own channel. Mirror that
//!    container as a Patom channel + add the sender as a member (channel RLS).
//! 6. Resolve (or create + bind) the Patom thread keyed on the conversation
//!    container; append the `posted` row with
//!    `idempotency_key = discord:{guild}:{message_id}` (dedupes redelivery /
//!    backfill overlap).
//! 7. Trigger only: resolve the agent's participation, enqueue a fresh-DAG
//!    trigger, and attach the outbound pump (routed to the conversation
//!    container, replying under the trigger only outside a thread).

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
use super::history::SharedHistoryReader;
use super::limits::{
    DISCORD_BACKFILL_MAX_MESSAGES, DISCORD_BACKFILL_MAX_PAGES, DISCORD_BACKFILL_PAGE_SIZE,
    DISCORD_DISPLAY_NAME_MAX, DISCORD_INBOUND_CONTENT_MAX, DISCORD_INBOUND_QUEUE,
    DISCORD_THREAD_NAME_MAX,
};
use super::roster;
use super::thread_opener::SharedThreadOpener;
use super::types::{ContainerId, DiscordMessageId, DiscordUserId, GuildId};

/// Where a freshly-triggered Patom thread's outbound chunks should be routed back
/// into Discord. The (D6) stream pump implements [`OutboundAttach`].
#[derive(Debug, Clone)]
pub struct AttachRequest {
    pub thread_id: ThreadId,
    pub org_id: OrgId,
    /// The bot that delivered the trigger — the fallback poster when the
    /// replying agent has no Discord bot of its own.
    pub application_id: super::types::ApplicationId,
    pub container_id: ContainerId,
    /// The triggering message to reply under (a Discord inline reply), or `None`
    /// to post plainly — inside a freshly-opened thread (whose root lives in the
    /// parent channel, not the thread) or a continuation in an owned thread.
    pub reply_to: Option<DiscordMessageId>,
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
    /// Reads pre-join channel history for the one-shot backfill on first access.
    pub history: SharedHistoryReader,
    /// Opens a Discord thread on a top-level channel @mention, so the agent
    /// converses in a thread instead of cluttering the channel.
    pub thread_opener: SharedThreadOpener,
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

    // 4. Trigger gate: @mention-or-DM only (a thread does not auto-continue).
    let is_trigger = m.guild_id.is_none() || m.mentions_bot(bot_user_id);
    info!(
        patom.discord.channel = %m.channel_id,
        has_guild = m.guild_id.is_some(),
        is_trigger,
        mentions = m.mentions.len(),
        content_len = m.content.len(),
        event = "discord.bridge.classified",
    );
    // 5. Where the conversation lives (may open a fresh thread).
    let conv = resolve_conversation(deps, app, &guild, &m, is_trigger).await?;

    let receiver = if is_trigger {
        Some(resolve_agent_colleague(deps, app).await?)
    } else {
        None
    };
    let (thread_id, appended) = mirror_into_thread(
        deps,
        app,
        &caller,
        &guild,
        &m,
        &conv,
        shadow.colleague_id,
        receiver,
    )
    .await?;

    if is_trigger {
        enqueue_and_attach(
            deps,
            &caller,
            app,
            &guild,
            &m,
            thread_id,
            &conv,
            shadow.colleague_id,
            appended,
        )
        .await?;
    }
    Ok(())
}

/// Mirror the message into its Patom thread: ensure the channel membership,
/// resolve (or create + bind) the thread, run the one-shot backfill on first
/// sight (skipped for a thread we just opened — its container differs from the
/// message's channel), and append the `posted` row. Returns the thread + the
/// appended row id for the trigger step.
#[allow(clippy::too_many_arguments)]
async fn mirror_into_thread(
    deps: &BridgeDeps,
    app: &DiscordApp,
    caller: &Caller,
    guild: &GuildId,
    m: &InboundMessage,
    conv: &Conversation,
    sender_colleague: ColleagueId,
    receiver: Option<ColleagueId>,
) -> Result<(ThreadId, ThreadMessageId), DiscordError> {
    let channel_id = deps
        .channels
        .ensure_channel(app.org_id, guild, &conv.container, caller.user_id)
        .await?;
    let (thread_id, needs_backfill) =
        resolve_thread(deps, caller, app, guild, conv, channel_id, sender_colleague).await?;
    if needs_backfill && conv.container == m.channel_id {
        maybe_backfill(deps, app, guild, &conv.container, thread_id, &m.message_id).await;
    }
    let appended = append_mirrored(
        deps,
        app,
        caller,
        thread_id,
        sender_colleague,
        receiver,
        guild,
        m,
    )
    .await?;
    Ok((thread_id, appended))
}

/// The resolved conversation target for a message: which Discord container the
/// agent converses in, the parent channel to bind (set only when we opened a
/// thread), whether that container is a thread (so we never re-attempt an open),
/// and the message to reply under (a Discord inline reply, or `None` to post
/// plainly inside a thread).
#[derive(Debug)]
struct Conversation {
    container: ContainerId,
    parent: Option<ContainerId>,
    is_thread: bool,
    reply_to: Option<DiscordMessageId>,
}

/// Decide where the conversation lives. A top-level guild @mention **opens a
/// fresh thread** on the triggering message (degrading to an inline channel
/// reply if the open fails); a message already inside a bot-owned thread
/// continues there (post plainly); a DM (or any non-trigger) stays in its own
/// channel — a DM trigger replies under the message, ambient posts plainly.
async fn resolve_conversation(
    deps: &BridgeDeps,
    app: &DiscordApp,
    guild: &GuildId,
    m: &InboundMessage,
    is_trigger: bool,
) -> Result<Conversation, DiscordError> {
    let in_owned_thread = deps
        .threads
        .lookup_by_container(guild, &m.channel_id)
        .await?
        .is_some_and(|b| b.is_thread);
    // `already_thread` ends true when the container is a thread (or otherwise
    // non-threadable): a known thread, or one a thread-open *permanently*
    // rejected. We persist it so a later mention never re-attempts the open.
    let mut already_thread = in_owned_thread;
    if m.guild_id.is_some() && is_trigger && !in_owned_thread {
        let name = thread_name(m);
        match deps
            .thread_opener
            .open_from_message(&app.application_id, &m.channel_id, &m.message_id, &name)
            .await
        {
            Ok(thread) => {
                info!(
                    patom.discord.thread = %thread,
                    patom.discord.channel = %m.channel_id,
                    event = "discord.bridge.thread_opened",
                );
                return Ok(Conversation {
                    container: thread,
                    parent: Some(m.channel_id.clone()),
                    is_thread: true,
                    reply_to: None,
                });
            }
            // Can't open a thread here. A 4xx is permanent (already a thread, a
            // forum, missing perms) → remember the container is non-threadable so
            // we never retry; a 5xx/transient failure degrades just this once.
            // Either way we fall back to a plain reply in the channel. A 4xx is an
            // expected degradation (info); a 5xx/unexpected one is worth a warn.
            Err(e) => {
                if is_permanent_open_failure(&e) {
                    already_thread = true;
                    info!(error = ?e, event = "discord.bridge.thread_open_fallback");
                } else {
                    warn!(error = ?e, event = "discord.bridge.thread_open_failed");
                }
            }
        }
    }
    // Continuation in a thread / a DM / an ambient ingest stays put. A trigger
    // outside a thread replies under the message; inside a thread (or ambient)
    // posts plainly.
    let reply_to = (is_trigger && !in_owned_thread).then(|| m.message_id.clone());
    Ok(Conversation {
        container: m.channel_id.clone(),
        parent: None,
        is_thread: already_thread,
        reply_to,
    })
}

/// Whether a failed thread-open will keep failing for this container — a 4xx
/// (e.g. 50024 "can't thread here": already a thread, a forum, missing perms).
/// A 5xx / rate-limit / transport error is transient and worth retrying later.
fn is_permanent_open_failure(e: &DiscordError) -> bool {
    matches!(e, DiscordError::PostFailed { status, .. } if (400..500).contains(status))
}

/// Derive a Discord thread name from the triggering message: rendered content,
/// whitespace-collapsed and truncated to [`DISCORD_THREAD_NAME_MAX`], with a
/// default when empty (Discord requires a 1–100 char thread name).
fn thread_name(m: &InboundMessage) -> String {
    let rendered = super::mention::render_inbound(&m.content, &m.mention_names());
    let body = strip_leading_mentions(&rendered);
    let truncated: String = body.chars().take(DISCORD_THREAD_NAME_MAX).collect();
    let name = if truncated.is_empty() {
        "conversation".to_owned()
    } else {
        truncated
    };
    assert!(!name.is_empty(), "invariant: thread name is non-empty");
    assert!(
        name.chars().count() <= DISCORD_THREAD_NAME_MAX,
        "invariant: thread name within the Discord cap"
    );
    name
}

/// Drop leading `@Name` / `<@id>` address tokens and collapse whitespace, so a
/// thread title reads as the request ("draft a JD") not the address
/// ("@Recruiter draft a JD"). Bounded by the token count.
fn strip_leading_mentions(s: &str) -> String {
    s.split_whitespace()
        .skip_while(|tok| tok.starts_with('@') || tok.starts_with("<@"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Idempotency key for a mirrored Discord message (and its trigger). Scoped to
/// the **app** — not just `guild:message` — because dedup is org-global on
/// `(org_id, idempotency_key)`: two bots that share a guild each mirror the
/// *same* Discord message into their own Patom thread, and a `guild:message`
/// key would let the first bot's row dedupe the second's. That left the
/// triggered bot's freshly-opened thread empty and tripped the non-empty-feed
/// assertion in the agent turn. Redelivery / backfill overlap for one bot still
/// dedups (same app + message).
fn message_idempotency_key(
    app: &DiscordApp,
    guild: &GuildId,
    message_id: &DiscordMessageId,
) -> Result<IdempotencyKey, DiscordError> {
    Ok(IdempotencyKey::try_from(format!(
        "discord:{}:{}:{}",
        app.application_id.as_str(),
        guild.as_str(),
        message_id.as_str()
    ))?)
}

/// Append one mirrored message as a `posted` row (the shared live + backfill
/// path). `<@id>` markers render to `@Name`; the idempotency key dedups
/// redelivery / backfill overlap. `receiver` is the addressed agent for a
/// trigger, else `None`.
#[allow(clippy::too_many_arguments)]
async fn append_mirrored(
    deps: &BridgeDeps,
    app: &DiscordApp,
    caller: &Caller,
    thread_id: ThreadId,
    sender_colleague: ColleagueId,
    receiver: Option<ColleagueId>,
    guild: &GuildId,
    m: &InboundMessage,
) -> Result<ThreadMessageId, DiscordError> {
    let rendered = super::mention::render_inbound(&m.content, &m.mention_names());
    let body_text: String = rendered.chars().take(DISCORD_INBOUND_CONTENT_MAX).collect();
    let idem = message_idempotency_key(app, guild, &m.message_id)?;
    deps.thread_store
        .append(
            caller,
            thread_id,
            NewMessage {
                kind: MessageKind::Posted,
                sender: Some(sender_colleague),
                owner_agent_id: None,
                receiver,
                body: ChatMessage::User(vec![UserContent::Text(body_text)]),
                request_id: None,
                idempotency_key: Some(idem),
            },
        )
        .await
        .map_err(|e| DiscordError::Internal(format!("append: {e}")))
}

/// Run the one-shot backfill and mark it complete on a definitive outcome (incl.
/// an empty / unreadable channel). A transient error is logged and left unmarked
/// so a later message retries.
async fn maybe_backfill(
    deps: &BridgeDeps,
    app: &DiscordApp,
    guild: &GuildId,
    channel: &ContainerId,
    thread_id: ThreadId,
    before: &DiscordMessageId,
) {
    match backfill_channel(deps, app, guild, channel, thread_id, before).await {
        Ok(()) => {
            if let Err(e) = deps.threads.mark_backfilled(guild, channel).await {
                warn!(error = ?e, event = "discord.backfill.mark_failed");
            }
        }
        Err(e) => warn!(error = ?e, event = "discord.backfill.failed"),
    }
}

/// Page channel history backward from `before`, then mirror it oldest-first so
/// the backfilled rows precede the live message in thread order. Bounded by
/// [`DISCORD_BACKFILL_MAX_PAGES`] and [`DISCORD_BACKFILL_MAX_MESSAGES`] (§5).
async fn backfill_channel(
    deps: &BridgeDeps,
    app: &DiscordApp,
    guild: &GuildId,
    channel: &ContainerId,
    thread_id: ThreadId,
    before: &DiscordMessageId,
) -> Result<(), DiscordError> {
    let mut cursor = before.clone();
    let mut collected: Vec<InboundMessage> = Vec::new();
    for _ in 0..DISCORD_BACKFILL_MAX_PAGES {
        if collected.len() >= DISCORD_BACKFILL_MAX_MESSAGES {
            break;
        }
        let page = deps
            .history
            .fetch_before(
                &app.application_id,
                channel,
                &cursor,
                DISCORD_BACKFILL_PAGE_SIZE,
            )
            .await?;
        let page_len = page.len();
        // The page is newest-first; its last (oldest) message is the next cursor.
        if let Some(oldest) = page.last() {
            cursor = oldest.message_id.clone();
        }
        collected.extend(page);
        if page_len < DISCORD_BACKFILL_PAGE_SIZE {
            break; // last page
        }
    }
    // Keep the most-recent window, then reverse to chronological (oldest-first).
    collected.truncate(DISCORD_BACKFILL_MAX_MESSAGES);
    collected.reverse();
    let mut mirrored = 0usize;
    for m in &collected {
        if mirror_backfilled(deps, app, guild, thread_id, m).await? {
            mirrored += 1;
        }
    }
    info!(channel = %channel, mirrored, event = "discord.backfill.complete");
    Ok(())
}

/// Mirror one backfilled message (shadow-mint its author + channel membership +
/// append, no trigger). Skips bot/webhook authors. Returns whether a row was
/// appended.
async fn mirror_backfilled(
    deps: &BridgeDeps,
    app: &DiscordApp,
    guild: &GuildId,
    thread_id: ThreadId,
    m: &InboundMessage,
) -> Result<bool, DiscordError> {
    if m.author.bot || m.webhook_id.is_some() {
        return Ok(false);
    }
    let display = m
        .author
        .display_name(m.member_nick.as_deref())
        .map(|d| d.chars().take(DISCORD_DISPLAY_NAME_MAX).collect::<String>());
    let shadow = deps
        .directory
        .resolve_or_mint(app.org_id, &m.author.id, display.as_deref())
        .await?;
    let caller = Caller::new(shadow.user_id, app.org_id);
    deps.channels
        .ensure_channel(app.org_id, guild, &m.channel_id, shadow.user_id)
        .await?;
    append_mirrored(
        deps,
        app,
        &caller,
        thread_id,
        shadow.colleague_id,
        None,
        guild,
        m,
    )
    .await?;
    Ok(true)
}

/// Resolve the bound Patom thread for the conversation container, or create +
/// bind one on first sight.
async fn resolve_thread(
    deps: &BridgeDeps,
    caller: &Caller,
    app: &DiscordApp,
    guild: &GuildId,
    conv: &Conversation,
    channel_id: ChannelId,
    creator: ColleagueId,
) -> Result<(ThreadId, bool), DiscordError> {
    // Returns `(thread_id, needs_backfill)`: an existing binding reports its
    // `backfill_complete` flag; a freshly-created one always needs backfill.
    if let Some(mapping) = deps
        .threads
        .lookup_by_container(guild, &conv.container)
        .await?
    {
        return Ok((mapping.thread_id, !mapping.backfill_complete));
    }
    let thread = deps
        .thread_store
        .create_thread(caller, Some(channel_id), None, creator, None)
        .await
        .map_err(|e| DiscordError::Internal(format!("create thread: {e}")))?;
    // `parent` is set only when we opened a thread (its parent channel); other
    // bindings record `parent = NULL`. `is_thread` is explicit (it can be true
    // with an unknown parent — a user-made thread we fell back into).
    deps.threads
        .bind(
            app.org_id,
            &app.application_id,
            guild,
            &conv.container,
            conv.parent.as_ref(),
            conv.is_thread,
            thread,
        )
        .await?;
    // A thread *we* opened has no pre-thread history (its parent channel's
    // backlog lives in the channel, not the thread), so mark its one-shot
    // backfill done now — otherwise the first *later* message inside the thread
    // would needlessly page the thread's own backlog. A user-made thread we fell
    // back into DOES have pre-mention history, so it still backfills on first
    // sight; channels / DMs likewise.
    let opened = conv.parent.is_some();
    if opened {
        deps.threads.mark_backfilled(guild, &conv.container).await?;
    }
    Ok((thread, !opened))
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

/// Resolve participation, enqueue a fresh-DAG trigger, and attach the pump
/// (routed to the conversation container, replying under the trigger only when
/// `conv.reply_to` is set — i.e. outside a thread).
#[allow(clippy::too_many_arguments)]
async fn enqueue_and_attach(
    deps: &BridgeDeps,
    caller: &Caller,
    app: &DiscordApp,
    guild: &GuildId,
    m: &InboundMessage,
    thread_id: ThreadId,
    conv: &Conversation,
    sender_colleague: ColleagueId,
    trigger_msg: ThreadMessageId,
) -> Result<(), DiscordError> {
    let state_id = deps
        .thread_store
        .resolve_participation(caller, thread_id, app.agent_id)
        .await
        .map_err(|e| DiscordError::Internal(format!("resolve participation: {e}")))?;
    let acting_user_id: UserId = caller.user_id;
    let idem = message_idempotency_key(app, guild, &m.message_id)?;
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
            application_id: app.application_id.clone(),
            container_id: conv.container.clone(),
            reply_to: conv.reply_to.clone(),
        })
        .await;
    info!(
        patom.thread.id = %thread_id.as_uuid(),
        patom.request.id = %request_id.as_uuid(),
        event = "discord.bridge.enqueued",
    );
    Ok(())
}
