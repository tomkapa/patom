//! Inbound bridge: a decoded Lark event → a Patom thread message (+ an agent
//! trigger on a mention/DM).
//!
//! One single-consumer worker. The WS manager decodes + parses frames and hands
//! [`InboundWork`] here via a bounded mpsc; this worker does the slow path (DB
//! lookups, shadow-mint, append, enqueue) off the WS task that already ACKed.
//!
//! Per-message flow ([`process_event`] → `handle_message`):
//! 1. Drop the bot's own messages (`sender_type == "app"`).
//! 2. Resolve the app (`app_id → org_id, agent_id`).
//! 3. Shadow-mint the sender's colleague; mirror the chat to a Patom channel and
//!    add the sender as a member (so the append passes channel RLS).
//! 4. Classify: a DM, or a group message that `@`-mentions the bot, is a
//!    **trigger**; any other group message is **ambient** (ingest only).
//! 5. Resolve (or create + bind) the Patom thread; append the `posted` row with
//!    `idempotency_key = lark:{tenant}:{event_id}` (dedupes WS redelivery).
//! 6. Trigger only: resolve the agent's participation, enqueue a fresh-DAG
//!    trigger, and attach the outbound pump.

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, info, info_span, warn};

use crate::assets::{AssetContentType, SharedAssetStore};
use crate::auth::{Caller, UserId};
use crate::channels::ChannelId;
use crate::colleagues::{ColleagueId, SharedColleagueStore};
use crate::provider::ingest::ingest_attachment;
use crate::provider::limits::MAX_ATTACHMENTS_PER_MESSAGE;
use crate::provider::{Attachment, ChatMessage, UserContent};
use crate::runtime::{IdempotencyKey, NewTrigger, RequestKindPayload, SharedPromptQueue};
use crate::threads::{MessageKind, NewMessage, SharedThreadStore, ThreadId, ThreadMessageId};

use super::app_store::{LarkApp, SharedLarkAppStore};
use super::channel_map::SharedLarkChannelStore;
use super::directory::SharedLarkDirectory;
use super::error::LarkError;
use super::event::{InboundMessage, LarkEvent, LarkResource};
use super::limits::LARK_INBOUND_QUEUE;
use super::mention;
use super::resource::{LarkResourceKind, SharedResourceFetcher};
use super::roster;
use super::stream_pump::{AttachRequest, SharedLarkPumpHandle};
use super::thread_map::SharedLarkThreadStore;
use super::token::SharedTokenProvider;
use super::types::{LarkOpenId, LarkThreadId};

/// A decoded event plus the receiving bot's own `open_id` (so a group
/// bot-mention can be told from ambient chatter). The WS manager fills
/// `bot_open_id` from its per-connection resolution.
#[derive(Debug, Clone)]
pub struct InboundWork {
    pub event: LarkEvent,
    pub bot_open_id: Option<LarkOpenId>,
}

/// Dependencies for the bridge worker. Cloned per event so `process_event`
/// stays a free function for testing.
#[derive(Clone)]
pub struct BridgeDeps {
    pub apps: SharedLarkAppStore,
    pub directory: SharedLarkDirectory,
    pub channels: SharedLarkChannelStore,
    pub threads: SharedLarkThreadStore,
    pub thread_store: SharedThreadStore,
    pub colleagues: SharedColleagueStore,
    pub queue: SharedPromptQueue,
    pub stream_pump: SharedLarkPumpHandle,
    pub token_provider: SharedTokenProvider,
    pub http: reqwest::Client,
    pub api_base: String,
    /// Object store for re-hosting inbound resources as model input. `None` when
    /// no asset store is configured — resources are then dropped (text mirrors).
    pub assets: Option<SharedAssetStore>,
    /// Downloads an inbound resource's bytes from the Lark resource endpoint.
    pub resource_fetcher: SharedResourceFetcher,
}

impl std::fmt::Debug for BridgeDeps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
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

/// Spawn the bridge worker; returns its handle plus the sender the WS manager
/// hands work into.
pub fn spawn(
    deps: BridgeDeps,
    cancel: CancellationToken,
) -> (BridgeHandle, mpsc::Sender<InboundWork>) {
    let (tx, rx) = mpsc::channel::<InboundWork>(LARK_INBOUND_QUEUE);
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
    mut rx: mpsc::Receiver<InboundWork>,
    cancel: CancellationToken,
) {
    info!(event = "lark.bridge.start");
    loop {
        tokio::select! {
            () = cancel.cancelled() => {
                info!(event = "lark.bridge.shutdown");
                return;
            }
            maybe = rx.recv() => {
                let Some(work) = maybe else {
                    info!(event = "lark.bridge.tx_closed");
                    return;
                };
                let span = info_span!("lark.bridge.process");
                if let Err(e) = process_event(&deps, work).instrument(span).await {
                    warn!(error = ?e, event = "lark.bridge.process_failed");
                }
            }
        }
    }
}

/// Route one unit of work. Extracted so tests drive it directly.
pub async fn process_event(deps: &BridgeDeps, work: InboundWork) -> Result<(), LarkError> {
    match work.event {
        LarkEvent::Message(m) => handle_message(deps, *m, work.bot_open_id.as_ref()).await,
        LarkEvent::BotAdded(ev) | LarkEvent::UserAdded(ev) => roster::sync_on_join(deps, &ev).await,
        LarkEvent::UserRemoved(_) | LarkEvent::Other => Ok(()),
    }
}

async fn handle_message(
    deps: &BridgeDeps,
    m: InboundMessage,
    bot_open_id: Option<&LarkOpenId>,
) -> Result<(), LarkError> {
    if m.sender_type == "app" {
        return Ok(());
    }
    let app = deps.apps.read_by_app_id(&m.app_id).await?;
    let Some(user_id) = m.sender_user_id.clone() else {
        // No `user_id` means the contact scope is ungranted — we can't key a
        // stable identity, so drop (the scope is a hard setup gate).
        warn!(event = "lark.bridge.sender_missing_user_id_dropped");
        return Ok(());
    };
    let shadow = deps
        .directory
        .resolve_or_mint(app.org_id, &m.tenant_key, &user_id, &m.sender_open_id, None)
        .await?;
    let caller = Caller::new(shadow.user_id, app.org_id);
    let channel_id = deps
        .channels
        .ensure_channel(app.org_id, &m.tenant_key, &m.chat_id, shadow.user_id)
        .await?;

    let bot_oid = bot_open_id.map_or("", LarkOpenId::as_str);
    let is_trigger = m.chat_type == "p2p" || mention::mentions_bot(&m.mentions, bot_oid);

    let anchor = thread_anchor(&m)?;
    let thread_id = resolve_thread(
        deps,
        &caller,
        &app,
        &m,
        &anchor,
        channel_id,
        shadow.colleague_id,
    )
    .await?;

    // Render every mention placeholder to a readable `@Name` (and strip the
    // bot's own trigger mention) so the agent reads who is referenced and can
    // match them against its roster.
    let body_text = mention::render_inbound(&m.text, &m.mentions, bot_oid);
    let body = build_user_message(deps, &m, body_text).await;
    let idem = IdempotencyKey::try_from(format!(
        "lark:{}:{}",
        m.tenant_key.as_str(),
        m.event_id.as_str()
    ))?;
    let receiver = if is_trigger {
        Some(resolve_agent_colleague(deps, &app).await?)
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
                body: ChatMessage::User(body),
                request_id: None,
                idempotency_key: Some(idem.clone()),
            },
        )
        .await
        .map_err(|e| LarkError::Internal(format!("append: {e}")))?;

    if is_trigger {
        enqueue_and_attach(
            deps,
            &caller,
            &app,
            &m,
            thread_id,
            shadow.colleague_id,
            appended,
            idem,
        )
        .await?;
    }
    Ok(())
}

/// Assemble the user-message content: the rendered text (when non-empty)
/// followed by every supported resource re-hosted as model input. The feed must
/// never be empty (the agent turn asserts a non-empty body), so a message with
/// neither text nor a usable resource falls back to one empty text block.
async fn build_user_message(
    deps: &BridgeDeps,
    m: &InboundMessage,
    body_text: String,
) -> Vec<UserContent> {
    let mut content = Vec::new();
    if !body_text.is_empty() {
        content.push(UserContent::Text(body_text));
    }
    if !m.resources.is_empty() {
        append_resources(deps, m, &mut content).await;
    }
    if content.is_empty() {
        content.push(UserContent::Text(String::new()));
    }
    content
}

/// Fetch + re-host each supported resource, pushing it as a `UserContent` block.
/// Best-effort and bounded ([`MAX_ATTACHMENTS_PER_MESSAGE`], §5): an unsupported
/// type is skipped, a token/fetch/ingest failure is logged and skipped — neither
/// drops the message itself.
async fn append_resources(deps: &BridgeDeps, m: &InboundMessage, content: &mut Vec<UserContent>) {
    let Some(store) = deps.assets.as_ref() else {
        warn!(
            count = m.resources.len(),
            event = "lark.bridge.attachments_no_store",
        );
        return;
    };
    // One tenant token covers every resource download for this message.
    let token = match deps.token_provider.token(&m.app_id).await {
        Ok(t) => t,
        Err(e) => {
            warn!(error = ?e, event = "lark.bridge.attachment_token_failed");
            return;
        }
    };
    for res in m.resources.iter().take(MAX_ATTACHMENTS_PER_MESSAGE) {
        match ingest_lark_resource(deps, store, token.expose(), m, res).await {
            Ok(Some(block)) => content.push(block),
            Ok(None) => debug!(event = "lark.bridge.attachment_unsupported"),
            Err(e) => warn!(error = ?e, event = "lark.bridge.attachment_skipped"),
        }
    }
}

/// Resolve one Lark resource to a model-input block, or `Ok(None)` when the type
/// is unsupported. A `file` whose name has no model-input extension is skipped
/// *before* downloading; an `image`'s type is learned from the response
/// content-type.
async fn ingest_lark_resource(
    deps: &BridgeDeps,
    store: &SharedAssetStore,
    token: &str,
    m: &InboundMessage,
    res: &LarkResource,
) -> Result<Option<UserContent>, LarkError> {
    let pre = match res.kind {
        LarkResourceKind::File => {
            match res
                .filename
                .as_deref()
                .and_then(AssetContentType::from_attachment_extension)
            {
                Some(ct) => Some(ct),
                // Unsupported file (zip/video/…): skip without spending a download.
                None => return Ok(None),
            }
        }
        LarkResourceKind::Image => None,
    };
    let fetched = deps
        .resource_fetcher
        .fetch(token, m.message_id.as_str(), &res.file_key, res.kind)
        .await?;
    let content_type = pre
        .or_else(|| {
            fetched
                .content_type
                .as_deref()
                .and_then(AssetContentType::from_attachment_mime)
        })
        .or_else(|| {
            res.filename
                .as_deref()
                .and_then(AssetContentType::from_attachment_extension)
        });
    let Some(content_type) = content_type else {
        return Ok(None);
    };
    let filename = res
        .filename
        .clone()
        .unwrap_or_else(|| format!("attachment.{}", content_type.extension()));
    let attachment = ingest_attachment(store, &filename, content_type, fetched.bytes)
        .await
        .map_err(|e| LarkError::Internal(format!("attachment ingest: {e}")))?;
    Ok(Some(classify_attachment(attachment)))
}

/// An image rides as an `Image` block; everything else (PDF/Office/text) as a
/// `File` block — the provider converters materialise each at dispatch.
fn classify_attachment(att: Attachment) -> UserContent {
    if att.mime().is_image() {
        UserContent::Image(att)
    } else {
        UserContent::File(att)
    }
}

/// The Lark thread anchor: the message's `thread_id` if it is in a topic, else
/// the message's own id (so a top-level message roots a fresh thread).
fn thread_anchor(m: &InboundMessage) -> Result<LarkThreadId, LarkError> {
    match &m.thread_id {
        Some(t) => Ok(t.clone()),
        None => Ok(LarkThreadId::try_from(m.message_id.as_str())?),
    }
}

/// Resolve the bound Patom thread, or create + bind one on first sight.
async fn resolve_thread(
    deps: &BridgeDeps,
    caller: &Caller,
    app: &LarkApp,
    m: &InboundMessage,
    anchor: &LarkThreadId,
    channel_id: ChannelId,
    creator: ColleagueId,
) -> Result<ThreadId, LarkError> {
    if let Some(mapping) = deps
        .threads
        .lookup_by_thread(&m.tenant_key, &m.chat_id, anchor)
        .await?
    {
        return Ok(mapping.thread_id);
    }
    let thread = deps
        .thread_store
        .create_thread(caller, Some(channel_id), None, creator, None)
        .await
        .map_err(|e| LarkError::Internal(format!("create thread: {e}")))?;
    deps.threads
        .bind(
            app.org_id,
            &m.app_id,
            &m.tenant_key,
            &m.chat_id,
            anchor,
            thread,
        )
        .await?;
    Ok(thread)
}

/// Resolve the app's agent to its colleague id (the message receiver).
async fn resolve_agent_colleague(
    deps: &BridgeDeps,
    app: &LarkApp,
) -> Result<ColleagueId, LarkError> {
    deps.colleagues
        .resolve_agent(app.org_id, app.agent_id)
        .await
        .map_err(|e| LarkError::Internal(format!("resolve agent colleague: {e}")))
}

/// Resolve participation, enqueue a fresh-DAG trigger, and attach the pump.
#[allow(clippy::too_many_arguments)]
async fn enqueue_and_attach(
    deps: &BridgeDeps,
    caller: &Caller,
    app: &LarkApp,
    m: &InboundMessage,
    thread_id: ThreadId,
    sender_colleague: ColleagueId,
    trigger_msg: ThreadMessageId,
    idem: IdempotencyKey,
) -> Result<(), LarkError> {
    let state_id = deps
        .thread_store
        .resolve_participation(caller, thread_id, app.agent_id)
        .await
        .map_err(|e| LarkError::Internal(format!("resolve participation: {e}")))?;
    let acting_user_id: UserId = caller.user_id;
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
    deps.stream_pump
        .attach(AttachRequest {
            thread_id,
            org_id: app.org_id,
            app_id: m.app_id.clone(),
            chat_id: m.chat_id.clone(),
            reply_to: Some(m.message_id.clone()),
        })
        .await;
    info!(
        patom.thread.id = %thread_id.as_uuid(),
        patom.request.id = %request_id.as_uuid(),
        event = "lark.bridge.enqueued",
    );
    Ok(())
}
