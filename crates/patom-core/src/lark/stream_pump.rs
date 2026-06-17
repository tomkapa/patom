//! Outbound stream pumps: one per Lark-bound Patom thread.
//!
//! Subscribes the `PgThreadStream` slot for a thread and forwards `Done` /
//! `AgentMessage` / `Error` chunks back to Lark as plain text replies. Simpler
//! than the Slack pump: a Lark app *is* one bot == one agent, so the reply posts
//! as the bot (its `tenant_access_token`) with no per-agent username/avatar and
//! no Block Kit cards.
//!
//! One task per active thread, owned by a bounded map (cap
//! [`MAX_LARK_STREAM_PUMPS`]); new attaches over the cap evict the oldest. Each
//! task self-exits after [`LARK_PUMP_IDLE_TTL`] of inactivity.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use futures::StreamExt;
use tokio::sync::{Mutex as AsyncMutex, mpsc};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep_until};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, info_span, warn};

use crate::agents::AgentId;
use crate::auth::{OrgId, UserId};
use crate::clock::SharedClock;
use crate::colleagues::ColleagueId;
use crate::mcp::wire_connect::{render_connect_message, truncate_with_ellipsis};
use crate::mcp::{McpAuthKind, McpCatalogId};
use crate::runtime::{ResponseChunk, SharedThreadStream, ThreadStreamError, ThreadStreamEvent};
use crate::threads::ThreadId;
use crate::types::SecretString;

use super::app_store::SharedLarkAppStore;
use super::connect_link::{LarkConnectClaims, sign_connect};
use super::directory::SharedLarkDirectory;
use super::error::LarkError;
use super::limits::{
    LARK_CONNECT_LINK_TTL_SECS, LARK_CONNECTION_REASON_MAX_CHARS, LARK_MAX_POST_CHARS,
    LARK_PUMP_IDLE_TTL, MAX_LARK_DEFERRED_WIRE_LINKS, MAX_LARK_STREAM_PUMPS,
};
use super::mention;
use super::poster::{PostRequest, SharedLarkPoster};
use super::token::SharedTokenProvider;
use super::types::{LarkAppId, LarkChatId, LarkMessageId, LarkOpenId};

/// Where a thread's chunks land on Lark.
///
/// A known chat (channel or bound conversation), or a never-seen DM addressed by
/// the recipient's `open_id` (#178). A sum, not chat_id + `Option<open_id>`, so
/// an impossible "both / and neither" state can't be built (CLAUDE.md §1).
#[derive(Debug, Clone)]
pub enum LarkRecipient {
    /// A known chat. Threads under `reply_to` when continuing an existing
    /// message; `None` posts top-level.
    Chat {
        chat_id: LarkChatId,
        reply_to: Option<LarkMessageId>,
    },
    /// A DM: send by the recipient's `open_id` (Lark routes to the p2p chat).
    Dm { open_id: LarkOpenId },
}

/// Request to attach a pump for a Lark-bound Patom thread.
#[derive(Debug, Clone)]
pub struct AttachRequest {
    /// The Patom thread this pump forwards.
    pub thread_id: ThreadId,
    /// The org the thread belongs to (for resolving `@`-tag handles).
    pub org_id: OrgId,
    /// The Patom user who triggered this thread — the shadow-minted owner of
    /// any MCP wiring an agent requests here (embedded in the connect link so
    /// the OAuth flow installs against the right org/user).
    pub user_id: UserId,
    /// The bot whose `tenant_access_token` posts the reply.
    pub app_id: LarkAppId,
    /// Where the reply lands.
    pub recipient: LarkRecipient,
}

/// Dependencies for the pump supervisor.
#[derive(Clone)]
pub struct PumpDeps {
    pub thread_stream: SharedThreadStream,
    pub poster: SharedLarkPoster,
    pub token_provider: SharedTokenProvider,
    /// Resolves the org's `@`-tag handles so `@Name` in a reply renders as a
    /// Lark `<at>` (pings the colleague).
    pub directory: SharedLarkDirectory,
    /// Resolves the replying agent → its own bot, so a multi-bot thread
    /// attributes each reply to the correct bot (not the first-attached one).
    pub apps: SharedLarkAppStore,
    /// HMAC key for the MCP connect-link token (derived from `master_kek`).
    pub connect_secret: SecretString,
    /// Base URL the connect link is built against, e.g. `https://patom.example`
    /// (the deployment's `oauth_redirect_base`).
    pub connect_url_base: Arc<str>,
    /// Clock for stamping the connect-link expiry.
    pub clock: SharedClock,
}

impl std::fmt::Debug for PumpDeps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PumpDeps").finish_non_exhaustive()
    }
}

/// Handle returned to the composition root.
#[derive(Debug)]
pub struct LarkPumpHandle {
    tx: mpsc::Sender<AttachRequest>,
    cancel: CancellationToken,
    supervisor: AsyncMutex<Option<JoinHandle<()>>>,
}

impl LarkPumpHandle {
    /// Attach (or re-attach) a pump for the request's thread.
    pub async fn attach(&self, req: AttachRequest) {
        if self.tx.send(req).await.is_err() {
            warn!(event = "lark.stream_pump.attach_after_shutdown");
        }
    }

    /// Cancel every pump task and await the supervisor's exit.
    pub async fn shutdown(&self) {
        self.cancel.cancel();
        let handle = self.supervisor.lock().await.take();
        if let Some(h) = handle {
            let _ = h.await;
        }
    }
}

/// Shared handle to the pump supervisor.
pub type SharedLarkPumpHandle = Arc<LarkPumpHandle>;

/// Spawn the supervisor.
#[must_use]
pub fn spawn(deps: PumpDeps, cancel: CancellationToken) -> SharedLarkPumpHandle {
    let (tx, rx) = mpsc::channel::<AttachRequest>(MAX_LARK_STREAM_PUMPS);
    let supervisor_cancel = cancel.clone();
    let supervisor_handle = tokio::spawn(supervisor(deps, rx, supervisor_cancel));
    Arc::new(LarkPumpHandle {
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
                let mut guard = live.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
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

/// Spawn (or skip, if already live) one per-thread pump, evicting the oldest
/// when at the cap.
fn spawn_pump(
    deps: &PumpDeps,
    cancel: &CancellationToken,
    live: &Arc<Mutex<HashMap<ThreadId, JoinHandle<()>>>>,
    req: AttachRequest,
) {
    let thread_id = req.thread_id;
    let mut guard = live
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if guard.contains_key(&thread_id) {
        return;
    }
    if guard.len() >= MAX_LARK_STREAM_PUMPS {
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
    let span = info_span!("lark.stream_pump", patom.thread.id = %thread_id);
    let handle = tokio::spawn(
        async move {
            if let Err(e) = run_pump(&deps_clone, &req, cancel_clone).await {
                warn!(error = ?e, event = "lark.stream_pump.exit_with_error");
            }
            live_for_task
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&thread_id);
        }
        .instrument(span),
    );
    guard.insert(thread_id, handle);
}

/// Per-thread pump body. Reads broadcast items until the stream closes, the
/// cancel token fires, or [`LARK_PUMP_IDLE_TTL`] elapses with no chunks.
async fn run_pump(
    deps: &PumpDeps,
    req: &AttachRequest,
    cancel: CancellationToken,
) -> Result<(), LarkError> {
    let mut stream = deps.thread_stream.subscribe(req.thread_id);
    let mut idle_deadline = Instant::now() + LARK_PUMP_IDLE_TTL;
    // Connect links buffered until the agent's narrative text posts, so the
    // "connect this server" prompt lands after the explanation (web-UI
    // stacked-bubble order). Mirrors the Slack pump's deferred-card buffer.
    let mut deferred: Vec<DeferredLink> = Vec::new();
    loop {
        tokio::select! {
            // Shutdown: drop pending links (best-effort surface only).
            () = cancel.cancelled() => return Ok(()),
            // Idle or stream close: flush any unflushed link before exiting so
            // a request that never saw trailing text isn't silently lost.
            () = sleep_until(idle_deadline) => break,
            next = stream.next() => {
                let Some(result) = next else { break; };
                idle_deadline = Instant::now() + LARK_PUMP_IDLE_TTL;
                handle_stream_event(deps, req, &mut deferred, result).await;
            }
        }
    }
    flush_deferred(deps, req, &mut deferred).await;
    Ok(())
}

/// One pending MCP connect message: the rendered text plus the agent it is
/// attributed to (so the flush posts via that agent's own bot).
struct DeferredLink {
    message: String,
    from_agent: AgentId,
}

/// Forward one stream event to Lark, if it carries user-visible text.
///
/// A `WireMcpRequest` is rendered into a connect message and **buffered** in
/// `deferred`; it is flushed after the next narrative text post (or at a turn
/// boundary), so the "connect this server" prompt reads after the agent's
/// explanation rather than before it.
async fn handle_stream_event(
    deps: &PumpDeps,
    req: &AttachRequest,
    deferred: &mut Vec<DeferredLink>,
    event: Result<ThreadStreamEvent, ThreadStreamError>,
) {
    let item = match event {
        Err(e) => {
            warn!(error = ?e, event = "lark.stream_pump.backend_error");
            return;
        }
        Ok(ThreadStreamEvent::Stalled) => {
            warn!(event = "lark.stream_pump.stalled");
            return;
        }
        Ok(ThreadStreamEvent::Item(item)) => item,
    };

    // MCP connect request: render + buffer, don't post yet.
    if let ResponseChunk::WireMcpRequest { .. } = &item.chunk {
        if let Some(message) = build_connect_message(deps, req, &item.chunk) {
            if deferred.len() >= MAX_LARK_DEFERRED_WIRE_LINKS {
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
        // Post via the REPLYING agent's own bot, so a multi-bot thread
        // attributes each reply correctly. Fall back to the attaching bot
        // only if the agent has no Lark bot of its own.
        let app_id = resolve_agent_bot(deps, req, item.from_agent).await;
        let rendered = render_outbound(deps, req, &text, to).await;
        post_reply(
            deps,
            &app_id,
            req,
            truncate_with_ellipsis(rendered, LARK_MAX_POST_CHARS),
        )
        .await;
        true
    } else {
        false
    };

    // Flush buffered connect links after the narrative text, or at a turn
    // boundary even when the final chunk carried no text (so an empty `Done`
    // never strands the link in the buffer).
    let terminal = matches!(
        &item.chunk,
        ResponseChunk::Done { .. } | ResponseChunk::Error { .. }
    );
    if posted || terminal {
        flush_deferred(deps, req, deferred).await;
    }
}

/// Resolve the bot whose `tenant_access_token` should post for `agent` — the
/// agent's own Lark bot, falling back to the attaching bot.
async fn resolve_agent_bot(deps: &PumpDeps, req: &AttachRequest, agent: AgentId) -> LarkAppId {
    match deps.apps.app_id_for_agent(req.org_id, agent).await {
        Ok(Some(a)) => a,
        Ok(None) => req.app_id.clone(),
        Err(e) => {
            warn!(error = ?e, event = "lark.stream_pump.agent_bot_resolve_failed");
            req.app_id.clone()
        }
    }
}

/// Post every buffered connect link in order, each via its agent's own bot.
async fn flush_deferred(deps: &PumpDeps, req: &AttachRequest, deferred: &mut Vec<DeferredLink>) {
    for pending in deferred.drain(..) {
        let app_id = resolve_agent_bot(deps, req, pending.from_agent).await;
        post_reply(
            deps,
            &app_id,
            req,
            truncate_with_ellipsis(pending.message, LARK_MAX_POST_CHARS),
        )
        .await;
    }
}

/// Render a `WireMcpRequest` chunk into the plain-text connect message Lark
/// posts. For an OAuth2 catalog this carries a signed
/// `GET /lark/mcp/connect?token=…` URL; for static-headers / no-auth catalogs
/// it points the user at the Patom web UI (Lark can't host a secret-entry
/// form). Returns `None` for any non-`WireMcpRequest` chunk.
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
    // Only OAuth2 catalogs get a signed connect link; the rest point at the
    // web UI (Lark can't host a secret-entry form).
    let url = match auth_kind {
        McpAuthKind::OAuth2 => Some(build_connect_url(deps, req, catalog_id, *from)),
        McpAuthKind::StaticHeaders | McpAuthKind::None => None,
    };
    Some(render_connect_message(
        display_name,
        reason,
        *auth_kind,
        url.as_deref(),
        LARK_CONNECTION_REASON_MAX_CHARS,
    ))
}

/// Mint a signed `GET /lark/mcp/connect?token=…` URL. The token binds the
/// catalog, the resolved Patom `(org, user)` that owns the wiring, the Lark
/// chat (so the OAuth callback pings back), and the Patom thread + agent (so
/// the universal auto-continue resumes the right loop).
fn build_connect_url(
    deps: &PumpDeps,
    req: &AttachRequest,
    catalog_id: &McpCatalogId,
    agent_id: AgentId,
) -> String {
    let exp = deps
        .clock
        .now_unix_secs()
        .saturating_add(LARK_CONNECT_LINK_TTL_SECS);
    let claims = LarkConnectClaims {
        catalog_id: catalog_id.clone(),
        org_id: req.org_id,
        user_id: req.user_id,
        app_id: req.app_id.clone(),
        chat_id: req.chat_id.clone(),
        reply_to: req.reply_to.clone(),
        thread_id: req.thread_id,
        agent_id,
    };
    let token = sign_connect(deps.connect_secret.expose().as_bytes(), &claims, exp);
    let base = deps.connect_url_base.trim_end_matches('/');
    format!("{base}/lark/mcp/connect?token={token}")
}

/// Render a reply for Lark: rewrite inline `@Name` to `<at>`, then prepend an
/// addressed-to cue for the `send_message` receiver when they were not named in
/// the text. A Lark *human* gets a real `<at>` ping; an *agent* (which cannot be
/// `<at>`-pinged across BYO apps) gets a plain `@Name` text marker instead.
/// Best-effort: a directory failure logs and posts what it has.
async fn render_outbound(
    deps: &PumpDeps,
    req: &AttachRequest,
    text: &str,
    to: Option<ColleagueId>,
) -> String {
    let inline = render_inline_mentions(deps, req, text).await;
    let Some(receiver) = to else {
        return inline;
    };
    match deps.directory.tag_for(req.org_id, receiver).await {
        // Human shadow: a real `<at>` ping, unless the inline render already
        // tagged them (the agent wrote `@Name` → avoid a double `<at>`).
        Ok(Some((name, open_id))) if !inline.contains(open_id.as_str()) => {
            format!("{} {inline}", mention::render_at(&open_id, &name))
        }
        // Human, but inline already tagged them → nothing more to prepend.
        Ok(Some(_)) => inline,
        // Not a Lark human shadow: an agent recipient cannot be `<at>`-pinged
        // across BYO apps, so fall back to a plain `@Name` addressed-to marker.
        Ok(None) => prepend_agent_marker(deps, req, receiver, inline).await,
        Err(e) => {
            warn!(error = ?e, event = "lark.stream_pump.receiver_tag_failed");
            inline
        }
    }
}

/// Prepend a plain `@Name` addressed-to marker when the `send_message` receiver
/// is an *agent* colleague — a visible "to whom" cue, since a peer bot cannot be
/// `<at>`-pinged in the BYO multi-app model (its `open_id` is app-scoped and
/// undiscoverable across apps). Deduped: skipped when the reply already names
/// the agent inline. Best-effort — a directory failure logs and posts as-is.
async fn prepend_agent_marker(
    deps: &PumpDeps,
    req: &AttachRequest,
    receiver: ColleagueId,
    inline: String,
) -> String {
    match deps.directory.agent_name_for(req.org_id, receiver).await {
        Ok(Some(name)) => apply_agent_marker(inline, &name),
        Ok(None) => inline,
        Err(e) => {
            warn!(error = ?e, event = "lark.stream_pump.receiver_agent_name_failed");
            inline
        }
    }
}

/// Prepend `@name ` to `inline` unless `inline` already names the agent inline
/// (so an agent that typed `@Name` itself isn't double-marked).
fn apply_agent_marker(inline: String, name: &str) -> String {
    if already_names(&inline, name) {
        inline
    } else {
        format!("@{name} {inline}")
    }
}

/// Whether `text` already contains an inline `@name` mention at a word boundary
/// — start-of-text or after whitespace, with a non-name char (or end) right
/// after the name. Mirrors `mention::render_ats`'s boundary rule so the dedup
/// agrees with the inline rewrite (e.g. `a@Name` is *not* a match).
fn already_names(text: &str, name: &str) -> bool {
    let needle = format!("@{name}");
    for (pos, _) in text.match_indices(&needle) {
        let prev_is_boundary = text[..pos]
            .chars()
            .next_back()
            .is_none_or(char::is_whitespace);
        let after = &text[pos + needle.len()..];
        let next_is_boundary = after.chars().next().is_none_or(|c| !is_name_char(c));
        if prev_is_boundary && next_is_boundary {
            return true;
        }
    }
    false
}

/// A char that can appear inside a mentionable name run (`[A-Za-z0-9_-]`);
/// matches `mention::is_name_char`.
fn is_name_char(c: char) -> bool {
    c.is_alphanumeric() || c == '-' || c == '_'
}

/// Rewrite inline `@Name` mentions in the reply into Lark `<at>` markup.
/// Best-effort: a directory failure logs and returns the text unchanged.
async fn render_inline_mentions(deps: &PumpDeps, req: &AttachRequest, text: &str) -> String {
    if !text.contains('@') {
        return text.to_owned();
    }
    match deps.directory.taggable_handles(req.org_id).await {
        Ok(handles) => mention::render_ats(text, &handles),
        Err(e) => {
            warn!(error = ?e, event = "lark.stream_pump.tag_resolve_failed");
            text.to_owned()
        }
    }
}

/// Post one reply to Lark via `app_id`'s bot `tenant_access_token`. Best-effort
/// — a failure is logged, not propagated.
async fn post_reply(deps: &PumpDeps, app_id: &LarkAppId, req: &AttachRequest, text: String) {
    let token = match deps.token_provider.token(app_id).await {
        Ok(t) => t,
        Err(e) => {
            warn!(error = ?e, event = "lark.stream_pump.token_failed");
            return;
        }
    };
    let result = match &req.recipient {
        LarkRecipient::Chat { chat_id, reply_to } => {
            deps.poster
                .post(PostRequest {
                    token,
                    chat_id: chat_id.clone(),
                    reply_to: reply_to.clone(),
                    text,
                })
                .await
        }
        LarkRecipient::Dm { open_id } => deps.poster.post_dm(token, open_id, &text).await,
    };
    if let Err(e) = result {
        warn!(error = ?e, event = "lark.stream_pump.post_failed");
    }
}

/// The user-visible text for a chunk plus the addressed recipient (the
/// `send_message` receiver, for an `<at>` ping), or `None` for non-visible
/// variants.
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
        assert!(
            render_payload(&ResponseChunk::Text {
                value: "x".to_owned()
            })
            .is_none()
        );
    }

    #[test]
    fn agent_marker_prepended_when_absent() {
        let out = apply_agent_marker("Hey, can you take this one?".to_owned(), "Recruiter");
        assert_eq!(out, "@Recruiter Hey, can you take this one?");
    }

    #[test]
    fn agent_marker_deduped_when_named_inline() {
        // The agent already addressed @Recruiter in the body → no second marker.
        let out = apply_agent_marker("@Recruiter please review this".to_owned(), "Recruiter");
        assert_eq!(out, "@Recruiter please review this");
    }

    #[test]
    fn agent_marker_dedup_requires_word_boundary() {
        // `email@Recruiter` is not an inline mention → still prepend the marker.
        let out = apply_agent_marker("ping email@Recruiter now".to_owned(), "Recruiter");
        assert_eq!(out, "@Recruiter ping email@Recruiter now");
    }

    #[test]
    fn already_names_respects_boundaries() {
        assert!(already_names("@Recruiter hi", "Recruiter"));
        assert!(already_names("hi @Recruiter", "Recruiter"));
        assert!(already_names("ok @Recruiter, thanks", "Recruiter"));
        // Prefix of a longer name must not count as a match.
        assert!(!already_names("@Recruiters meeting", "Recruiter"));
        // Not at a word boundary.
        assert!(!already_names("a@Recruiter", "Recruiter"));
        // Absent entirely.
        assert!(!already_names("hello there", "Recruiter"));
    }
}
