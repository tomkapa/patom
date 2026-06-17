//! Outbound stream pumps: one per Discord-bound Patom thread.
//!
//! Subscribes the `PgThreadStream` slot for a thread and forwards `Done` /
//! `AgentMessage` / `Error` chunks back to Discord as replies. A reply posts as
//! the **replying agent's own bot** (so a multi-bot thread attributes each reply
//! correctly), with inline `@Name` rewritten to `<@id>` and a mandatory
//! `allowed_mentions` listing exactly the pinged ids.
//!
//! One task per active thread, owned by a bounded map (cap
//! [`MAX_DISCORD_STREAM_PUMPS`]); new attaches over the cap evict the oldest.
//! Each task self-exits after [`DISCORD_PUMP_IDLE_TTL`] of inactivity. The
//! [`DiscordPumpHandle`] implements the bridge's [`OutboundAttach`] seam.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use futures::StreamExt;
use tokio::sync::{Mutex as AsyncMutex, mpsc};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep_until};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, info_span, warn};

use crate::agents::AgentId;
use crate::clock::SharedClock;
use crate::colleagues::ColleagueId;
use crate::mcp::wire_connect::render_connect_message;
use crate::mcp::{McpAuthKind, McpCatalogId};
use crate::runtime::{ResponseChunk, SharedThreadStream, ThreadStreamError, ThreadStreamEvent};
use crate::threads::ThreadId;
use crate::types::SecretString;

use super::app_store::SharedDiscordAppStore;
use super::bridge::{AttachRequest, OutboundAttach};
use super::connect_link::{DiscordConnectClaims, sign_connect};
use super::directory::SharedDiscordDirectory;
use super::limits::{
    DISCORD_CONNECT_LINK_TTL_SECS, DISCORD_CONNECTION_REASON_MAX_CHARS, DISCORD_PUMP_IDLE_TTL,
    MAX_DISCORD_DEFERRED_WIRE_LINKS, MAX_DISCORD_STREAM_PUMPS,
};
use super::mention;
use super::poster::{AllowedMentions, PostRequest, SharedDiscordPoster};
use super::types::{ApplicationId, DiscordUserId};

/// Dependencies for the pump supervisor.
#[derive(Clone)]
pub struct PumpDeps {
    pub thread_stream: SharedThreadStream,
    pub poster: SharedDiscordPoster,
    /// Resolves the org's `@`-tag handles + addressed recipient for outbound
    /// `<@id>` rendering.
    pub directory: SharedDiscordDirectory,
    /// Resolves the replying agent → its own bot, so a multi-bot thread
    /// attributes each reply to the correct bot (not the first-attached one).
    pub apps: SharedDiscordAppStore,
    /// HMAC key for the MCP connect-link token (derived from `master_kek`).
    pub connect_secret: SecretString,
    /// Base URL the connect link is built against (the deployment's
    /// `oauth_redirect_base`).
    pub connect_url_base: Arc<str>,
    /// Clock for stamping the connect-link expiry.
    pub clock: SharedClock,
}

impl std::fmt::Debug for PumpDeps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PumpDeps").finish_non_exhaustive()
    }
}

/// Handle returned to the composition root; implements [`OutboundAttach`].
#[derive(Debug)]
pub struct DiscordPumpHandle {
    tx: mpsc::Sender<AttachRequest>,
    cancel: CancellationToken,
    supervisor: AsyncMutex<Option<JoinHandle<()>>>,
}

impl DiscordPumpHandle {
    /// Cancel every pump task and await the supervisor's exit.
    pub async fn shutdown(&self) {
        self.cancel.cancel();
        let handle = self.supervisor.lock().await.take();
        if let Some(h) = handle {
            let _ = h.await;
        }
    }
}

#[async_trait]
impl OutboundAttach for DiscordPumpHandle {
    async fn attach(&self, req: AttachRequest) {
        if self.tx.send(req).await.is_err() {
            warn!(event = "discord.stream_pump.attach_after_shutdown");
        }
    }
}

pub type SharedDiscordPumpHandle = Arc<DiscordPumpHandle>;

/// Spawn the supervisor.
#[must_use]
pub fn spawn(deps: PumpDeps, cancel: CancellationToken) -> SharedDiscordPumpHandle {
    let (tx, rx) = mpsc::channel::<AttachRequest>(MAX_DISCORD_STREAM_PUMPS);
    let supervisor_cancel = cancel.clone();
    let supervisor_handle = tokio::spawn(supervisor(deps, rx, supervisor_cancel));
    Arc::new(DiscordPumpHandle {
        tx,
        cancel,
        supervisor: AsyncMutex::new(Some(supervisor_handle)),
    })
}

/// Pump supervisor. Owns the set of live pump tasks; bounded.
async fn supervisor(
    deps: PumpDeps,
    mut rx: mpsc::Receiver<AttachRequest>,
    cancel: CancellationToken,
) {
    let live: Arc<Mutex<HashMap<ThreadId, JoinHandle<()>>>> = Arc::new(Mutex::new(HashMap::new()));
    loop {
        tokio::select! {
            () = cancel.cancelled() => {
                let mut guard = live.lock().unwrap_or_else(PoisonError::into_inner);
                for (_, h) in guard.drain() {
                    h.abort();
                }
                return;
            }
            maybe = rx.recv() => {
                let Some(req) = maybe else { return; };
                spawn_pump(&deps, &cancel, &live, req);
            }
        }
    }
}

/// Spawn (or skip, if already live) one per-thread pump, evicting the oldest when
/// at the cap.
fn spawn_pump(
    deps: &PumpDeps,
    cancel: &CancellationToken,
    live: &Arc<Mutex<HashMap<ThreadId, JoinHandle<()>>>>,
    req: AttachRequest,
) {
    let thread_id = req.thread_id;
    let mut guard = live.lock().unwrap_or_else(PoisonError::into_inner);
    if guard.contains_key(&thread_id) {
        return;
    }
    if guard.len() >= MAX_DISCORD_STREAM_PUMPS {
        let finished = guard
            .iter()
            .find_map(|(k, h)| h.is_finished().then_some(*k));
        if let Some(k) = finished {
            guard.remove(&k);
        } else if let Some(k) = guard.keys().next().copied()
            && let Some(h) = guard.remove(&k)
        {
            h.abort();
        }
    }
    let deps_clone = deps.clone();
    let cancel_clone = cancel.clone();
    let live_for_task = Arc::clone(live);
    let span = info_span!("discord.stream_pump", patom.thread.id = %thread_id);
    let handle = tokio::spawn(
        async move {
            run_pump(&deps_clone, &req, cancel_clone).await;
            live_for_task
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&thread_id);
        }
        .instrument(span),
    );
    guard.insert(thread_id, handle);
}

/// Per-thread pump body. Reads broadcast items until the stream closes, the
/// cancel token fires, or [`DISCORD_PUMP_IDLE_TTL`] elapses with no chunks.
async fn run_pump(deps: &PumpDeps, req: &AttachRequest, cancel: CancellationToken) {
    let mut stream = deps.thread_stream.subscribe(req.thread_id);
    let mut idle_deadline = Instant::now() + DISCORD_PUMP_IDLE_TTL;
    // Connect links buffered until the agent's narrative text posts, so the
    // "connect this server" prompt lands after the explanation. Mirrors the
    // Slack pump's deferred-card buffer.
    let mut deferred: Vec<DeferredLink> = Vec::new();
    loop {
        tokio::select! {
            // Shutdown: drop pending links (best-effort surface only).
            () = cancel.cancelled() => return,
            // Idle or stream close: flush any unflushed link before exiting.
            () = sleep_until(idle_deadline) => break,
            next = stream.next() => {
                let Some(result) = next else { break; };
                idle_deadline = Instant::now() + DISCORD_PUMP_IDLE_TTL;
                handle_stream_event(deps, req, &mut deferred, result).await;
            }
        }
    }
    flush_deferred(deps, req, &mut deferred).await;
}

/// One pending MCP connect message: the rendered text plus the agent it is
/// attributed to (so the flush posts via that agent's own bot).
struct DeferredLink {
    message: String,
    from_agent: AgentId,
}

/// Forward one stream event to Discord, if it carries user-visible text.
///
/// A `WireMcpRequest` is rendered into a connect message and **buffered** in
/// `deferred`; it is flushed after the next narrative text post (or at a turn
/// boundary), so the connect prompt reads after the agent's explanation.
async fn handle_stream_event(
    deps: &PumpDeps,
    req: &AttachRequest,
    deferred: &mut Vec<DeferredLink>,
    event: Result<ThreadStreamEvent, ThreadStreamError>,
) {
    let item = match event {
        Err(e) => {
            warn!(error = ?e, event = "discord.stream_pump.backend_error");
            return;
        }
        Ok(ThreadStreamEvent::Stalled) => {
            warn!(event = "discord.stream_pump.stalled");
            return;
        }
        Ok(ThreadStreamEvent::Item(item)) => item,
    };

    // MCP connect request: render + buffer, don't post yet.
    if let ResponseChunk::WireMcpRequest { .. } = &item.chunk {
        if let Some(message) = build_connect_message(deps, req, &item.chunk) {
            if deferred.len() >= MAX_DISCORD_DEFERRED_WIRE_LINKS {
                deferred.remove(0);
            }
            deferred.push(DeferredLink {
                message,
                from_agent: item.from_agent,
            });
        }
        return;
    }

    let posted = if let Some((text, to)) = render_payload(&item.chunk) {
        // Post via the REPLYING agent's own bot; fall back to the attaching bot.
        let application_id = resolve_agent_bot(deps, req, item.from_agent).await;
        let (content, pinged) = render_outbound(deps, req, &text, to).await;
        post_reply(deps, application_id, req, content, pinged).await;
        true
    } else {
        false
    };

    // Flush buffered connect links after the narrative text, or at a turn
    // boundary even when the final chunk carried no text.
    let terminal = matches!(
        &item.chunk,
        ResponseChunk::Done { .. } | ResponseChunk::Error { .. }
    );
    if posted || terminal {
        flush_deferred(deps, req, deferred).await;
    }
}

/// Resolve the bot that should post for `agent` — the agent's own Discord bot,
/// falling back to the attaching bot.
async fn resolve_agent_bot(deps: &PumpDeps, req: &AttachRequest, agent: AgentId) -> ApplicationId {
    match deps.apps.app_id_for_agent(req.org_id, agent).await {
        Ok(Some(a)) => a,
        Ok(None) => req.application_id.clone(),
        Err(e) => {
            warn!(error = ?e, event = "discord.stream_pump.agent_bot_resolve_failed");
            req.application_id.clone()
        }
    }
}

/// Post every buffered connect link in order, each via its agent's own bot. A
/// connect message is a system notice — no `@`-mentions, so `allowed_mentions`
/// is empty.
async fn flush_deferred(deps: &PumpDeps, req: &AttachRequest, deferred: &mut Vec<DeferredLink>) {
    for pending in deferred.drain(..) {
        let application_id = resolve_agent_bot(deps, req, pending.from_agent).await;
        // Posted as-is (not clipped): the reason is already capped low enough
        // that the whole message — link included — fits one Discord message,
        // so the poster never chunks and splits the signed URL.
        post_reply(deps, application_id, req, pending.message, Vec::new()).await;
    }
}

/// Render a `WireMcpRequest` chunk into the plain-text connect message Discord
/// posts. OAuth2 catalogs carry a signed `GET /discord/mcp/connect?token=…`
/// URL; static-headers / no-auth catalogs point at the web UI. Returns `None`
/// for any non-`WireMcpRequest` chunk.
fn build_connect_message(
    deps: &PumpDeps,
    req: &AttachRequest,
    chunk: &ResponseChunk,
) -> Option<String> {
    let ResponseChunk::WireMcpRequest {
        from,
        catalog_id,
        display_name,
        reason,
        auth_kind,
        ..
    } = chunk
    else {
        return None;
    };
    let url = match auth_kind {
        McpAuthKind::OAuth2 => Some(build_connect_url(deps, req, catalog_id, *from)),
        McpAuthKind::StaticHeaders | McpAuthKind::None => None,
    };
    Some(render_connect_message(
        display_name,
        reason,
        *auth_kind,
        url.as_deref(),
        DISCORD_CONNECTION_REASON_MAX_CHARS,
    ))
}

/// Mint a signed `GET /discord/mcp/connect?token=…` URL. The token binds the
/// catalog, the resolved Patom `(org, user)`, the Discord channel (so the
/// OAuth callback pings back), and the Patom thread + agent (so the universal
/// auto-continue resumes the right loop).
fn build_connect_url(
    deps: &PumpDeps,
    req: &AttachRequest,
    catalog_id: &McpCatalogId,
    agent_id: AgentId,
) -> String {
    let exp = deps
        .clock
        .now_unix_secs()
        .saturating_add(DISCORD_CONNECT_LINK_TTL_SECS);
    let claims = DiscordConnectClaims {
        catalog_id: catalog_id.clone(),
        org_id: req.org_id,
        user_id: req.user_id,
        application_id: req.application_id.clone(),
        container_id: req.container_id.clone(),
        reply_to: req.reply_to.clone(),
        thread_id: req.thread_id,
        agent_id,
    };
    let token = sign_connect(deps.connect_secret.expose().as_bytes(), &claims, exp);
    let base = deps.connect_url_base.trim_end_matches('/');
    format!("{base}/discord/mcp/connect?token={token}")
}

/// Render an outbound reply: rewrite inline `@Name` to `<@id>` (collecting the
/// pinged ids), then prepend an addressed-to cue for the `send_message`
/// receiver. A human gets a real `<@id>` ping; an agent gets a plain `@Name`
/// marker (its bot reads the thread, no ping needed). Best-effort.
async fn render_outbound(
    deps: &PumpDeps,
    req: &AttachRequest,
    text: &str,
    to: Option<ColleagueId>,
) -> (String, Vec<DiscordUserId>) {
    let (inline, mut pinged) = render_inline(deps, req, text).await;
    let Some(receiver) = to else {
        return (inline, pinged);
    };
    match deps.directory.tag_for(req.org_id, receiver).await {
        Ok(Some((_, snowflake))) if !inline.contains(&mention::render_mention(&snowflake)) => {
            let prefixed = format!("{} {inline}", mention::render_mention(&snowflake));
            pinged.push(snowflake);
            (prefixed, pinged)
        }
        Ok(Some(_)) => (inline, pinged),
        Ok(None) => (
            prepend_agent_marker(deps, req, receiver, inline).await,
            pinged,
        ),
        Err(e) => {
            warn!(error = ?e, event = "discord.stream_pump.receiver_tag_failed");
            (inline, pinged)
        }
    }
}

/// Rewrite inline `@Name` mentions into `<@id>` pings. Best-effort.
async fn render_inline(
    deps: &PumpDeps,
    req: &AttachRequest,
    text: &str,
) -> (String, Vec<DiscordUserId>) {
    if !text.contains('@') {
        return (text.to_owned(), Vec::new());
    }
    match deps.directory.taggable_handles(req.org_id).await {
        Ok(handles) => mention::render_outbound(text, &handles),
        Err(e) => {
            warn!(error = ?e, event = "discord.stream_pump.tag_resolve_failed");
            (text.to_owned(), Vec::new())
        }
    }
}

/// Prepend a plain `@Name` addressed-to marker for an *agent* receiver (deduped
/// against an inline mention). Best-effort.
async fn prepend_agent_marker(
    deps: &PumpDeps,
    req: &AttachRequest,
    receiver: ColleagueId,
    inline: String,
) -> String {
    match deps.directory.agent_name_for(req.org_id, receiver).await {
        Ok(Some(name)) if !mention::already_names(&inline, &name) => format!("@{name} {inline}"),
        Ok(_) => inline,
        Err(e) => {
            warn!(error = ?e, event = "discord.stream_pump.receiver_agent_name_failed");
            inline
        }
    }
}

/// Post one reply via `application_id`'s bot. Best-effort — a failure is logged.
async fn post_reply(
    deps: &PumpDeps,
    application_id: ApplicationId,
    req: &AttachRequest,
    content: String,
    pinged: Vec<DiscordUserId>,
) {
    if content.is_empty() {
        return;
    }
    if let Err(e) = deps
        .poster
        .post(PostRequest {
            application_id,
            container_id: req.container_id.clone(),
            reply_to: req.reply_to.clone(),
            content,
            allowed_mentions: AllowedMentions::users(&pinged),
        })
        .await
    {
        warn!(error = ?e, event = "discord.stream_pump.post_failed");
    }
}

/// The user-visible text for a chunk plus the addressed recipient, or `None` for
/// non-visible variants.
fn render_payload(chunk: &ResponseChunk) -> Option<(String, Option<ColleagueId>)> {
    match chunk {
        ResponseChunk::Done { final_text } if !final_text.is_empty() => {
            Some((final_text.clone(), None))
        }
        ResponseChunk::AgentMessage { content, to, .. } if !content.is_empty() => {
            Some((content.clone(), *to))
        }
        ResponseChunk::Error { reason, .. } => Some((format!("⚠️ Error: {reason}"), None)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_for_done_and_agent_message() {
        assert_eq!(
            render_payload(&ResponseChunk::Done {
                final_text: "answer".to_owned()
            }),
            Some(("answer".to_owned(), None))
        );
        let receiver = ColleagueId::new();
        assert_eq!(
            render_payload(&ResponseChunk::AgentMessage {
                from: crate::agents::AgentId::new(),
                to_thread: ThreadId::new(),
                content: "hi".to_owned(),
                to: Some(receiver),
            }),
            Some(("hi".to_owned(), Some(receiver)))
        );
    }

    #[test]
    fn payload_for_error_has_prefix() {
        let (t, to) = render_payload(&ResponseChunk::Error {
            reason: "boom".to_owned(),
            code: "boom".to_owned(),
        })
        .expect("some");
        assert!(t.contains("boom"));
        assert!(to.is_none());
    }

    #[test]
    fn empty_and_streaming_chunks_drop() {
        assert!(
            render_payload(&ResponseChunk::Done {
                final_text: String::new()
            })
            .is_none()
        );
        assert!(render_payload(&ResponseChunk::Stalled).is_none());
    }

    /// The renderer itself is unit-tested in `mcp::wire_connect`; here we guard
    /// the Discord-specific invariant: a worst-case reason (at the cap) plus a
    /// realistic max-length connect URL still fits ONE Discord message, so the
    /// poster never chunks and splits the signed link. This is the property
    /// that justifies Discord's lower `DISCORD_CONNECTION_REASON_MAX_CHARS`.
    #[test]
    fn connect_message_with_max_reason_and_url_fits_one_message() {
        let long = "a".repeat(DISCORD_CONNECTION_REASON_MAX_CHARS + 100);
        // A connect token is a ~330-char payload + 64-char sig; pad generously.
        let url = format!(
            "https://patom.example/discord/mcp/connect?token={}",
            "t".repeat(420)
        );
        let msg = render_connect_message(
            "Some Connector",
            &long,
            McpAuthKind::OAuth2,
            Some(&url),
            DISCORD_CONNECTION_REASON_MAX_CHARS,
        );
        assert!(msg.contains('…'), "long reason is truncated");
        assert!(
            msg.chars().count() <= super::super::limits::DISCORD_MESSAGE_MAX,
            "connect message fits one Discord message (no chunk → no URL split)"
        );
    }
}
