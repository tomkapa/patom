//! Outbound stream pumps: subscribe to one `PgThreadStream` slot per
//! Slack-rooted DAG and forward `Done` / `AgentMessage` / `Error` /
//! `WireMcpRequest` chunks to Slack via `chat.postMessage`.
//!
//! Why this lives here, not on the SSE/HTTP path:
//! `PgThreadStream::subscribe(root)` is an in-process
//! `broadcast::Receiver` — the Slack adapter takes the same primitive
//! the web UI does, no HTTP loop required.
//!
//! One task per active Slack-rooted DAG. Tasks are owned by a
//! `JoinSet` capped at `MAX_SLACK_STREAM_PUMPS`; new attaches over the
//! cap evict the oldest idle pump. Each task self-exits after
//! `SLACK_PUMP_IDLE_TTL` of inactivity.
//!
//! Per-session routing: each chunk carries the `session_id` of the
//! `prompt_requests` row that produced it. A DAG may carry multiple
//! `(agent, human)` sessions; this pump routes each chunk to the Slack
//! thread bound to its session — or, when no binding exists yet (a
//! descendant agent first reaching the human), mints a fresh
//! top-level channel post and records the binding so future chunks
//! and inbound user replies in that thread stick.
//!
//! Post rules:
//! - `ResponseChunk::Done { final_text }` → post `final_text` attributed
//!   to the chunk's `from_agent` (the agent whose turn produced it).
//! - `ResponseChunk::AgentMessage { from, content }` → post `content`
//!   attributed to `from` (cross-agent handoff visible to the human).
//! - `ResponseChunk::Error { reason }` → post a short error note.
//! - `ResponseChunk::WireMcpRequest { .. }` → post a Block Kit
//!   connection-request card. For oauth2 catalogs the card carries a
//!   Connect button URL signed by `connect_link`; for static_headers /
//!   none the card degrades to a "finish in the web UI" hint.
//! - All other variants (`Text`, `Reasoning`, `ToolCall`, `ToolResult`,
//!   `Stalled`) are dropped without a post.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use futures::StreamExt;
use tokio::sync::{Mutex as AsyncMutex, mpsc};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep_until};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, info_span, warn};

use crate::agents::SharedAgentStore;
use crate::auth::OrgId;
use crate::clock::SharedClock;
use crate::mcp::{McpAuthKind, McpCatalogId};
use crate::runtime::{PromptRequestId, ResponseChunk, SharedThreadStream, ThreadStreamEvent};
use crate::session::SessionId;
use crate::types::SecretString;

use super::connect_link::{SlackConnectClaims, sign_connect};
use super::connection_card::build_connection_request_card;
use super::error::SlackError;
use super::limits::{
    MAX_SLACK_STREAM_PUMPS, MAX_SLACK_THREADS_PER_DAG_ROOT, SLACK_MAX_POST_CHARS,
    SLACK_PUMP_IDLE_TTL,
};
use super::poster::{PostBody, PostRequest, SharedSlackPoster};
use super::thread_map::SharedSlackThreadStore;
use super::types::{SlackChannelId, SlackTeamId, SlackThreadTs, SlackUserId};
use super::workspace::SharedSlackWorkspaceStore;

/// Connect-link TTL — same 10 min as the Slack install state token.
/// Long enough to click within a few minutes of seeing the card; short
/// enough that a leaked URL becomes useless quickly.
const CONNECT_LINK_TTL_SECS: i64 = 60 * 10;

/// Cap on deferred `WireMcpRequest` cards held per pump.
///
/// Cards are deferred until the next text-bearing chunk (Done /
/// AgentMessage / Error) so the agent's narrative posts above the
/// Connect button — matches the web UI's stacked-bubble layout. The
/// cap is a defensive bound (CLAUDE.md §5): a runaway agent that
/// emits dozens of `WireMcpRequest` chunks before any text would
/// otherwise grow this buffer unboundedly; we drop the oldest at the
/// cap.
const MAX_DEFERRED_WIRE_CARDS: usize = 8;

/// Request to attach a pump for `root`.
#[derive(Debug, Clone)]
pub struct AttachRequest {
    pub root: PromptRequestId,
    pub org_id: OrgId,
    pub team_id: SlackTeamId,
    pub channel_id: SlackChannelId,
    pub thread_ts: SlackThreadTs,
    /// The Slack user who originated this thread. Threaded into any
    /// Connect-button URL minted for `WireMcpRequest` chunks so the
    /// `GET /slack/mcp/connect` handler can resolve the relay user
    /// the credential should write under.
    pub slack_user_id: SlackUserId,
    /// The session this pump is bound to. Threaded into the Connect
    /// button signed token so the OAuth callback's auto-continue can
    /// inject the resume prompt into the right session.
    pub session_id: SessionId,
}

/// Handle returned to the composition root.
#[derive(Debug)]
pub struct StreamPumpHandle {
    tx: mpsc::Sender<AttachRequest>,
    cancel: CancellationToken,
    /// Supervisor's `JoinHandle`, parked in an async-aware mutex so
    /// `shutdown` (which takes `&self` because callers hold an `Arc`)
    /// can `take()` it and `.await` for clean exit.
    supervisor: AsyncMutex<Option<JoinHandle<()>>>,
}

impl StreamPumpHandle {
    pub async fn attach(&self, req: AttachRequest) {
        if self.tx.send(req).await.is_err() {
            warn!(event = "slack.stream_pump.attach_after_shutdown");
        }
    }

    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// Cancel every pump task and await the supervisor's exit. Safe to
    /// call concurrently; only the first caller sees the supervisor
    /// `JoinHandle` — subsequent calls are no-ops.
    pub async fn shutdown(&self) {
        self.cancel.cancel();
        let handle = self.supervisor.lock().await.take();
        if let Some(h) = handle {
            let _ = h.await;
        }
    }
}

pub type SharedStreamPumpHandle = Arc<StreamPumpHandle>;

#[derive(Clone)]
pub struct PumpDeps {
    pub thread_stream: SharedThreadStream,
    pub workspaces: SharedSlackWorkspaceStore,
    pub agents: SharedAgentStore,
    pub poster: SharedSlackPoster,
    pub threads: SharedSlackThreadStore,
    /// HMAC signing key for [`SlackConnectClaims`] tokens minted into
    /// Connect button URLs. Shared with `oauth.rs`'s state-token
    /// signing — same secret, same TTL.
    pub signing_secret: SecretString,
    /// Public-facing relay base URL (no trailing slash) the Connect
    /// button URL is built against. Same value as `oauth_redirect_base`
    /// on `AppState`.
    pub connect_url_base: Arc<str>,
    /// Injected clock — read for the `exp` field on the signed Connect
    /// link claims so tests can drive token freshness deterministically.
    pub clock: SharedClock,
}

impl std::fmt::Debug for PumpDeps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PumpDeps")
            .field("connect_url_base", &self.connect_url_base)
            .finish_non_exhaustive()
    }
}

/// Spawn the supervisor.
pub fn spawn(deps: PumpDeps, cancel: CancellationToken) -> SharedStreamPumpHandle {
    let (tx, rx) = mpsc::channel::<AttachRequest>(MAX_SLACK_STREAM_PUMPS);
    let supervisor_cancel = cancel.clone();
    let supervisor_handle = tokio::spawn(supervisor(deps, rx, supervisor_cancel));
    Arc::new(StreamPumpHandle {
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
    let live: Arc<Mutex<HashMap<PromptRequestId, JoinHandle<()>>>> =
        Arc::new(Mutex::new(HashMap::new()));

    loop {
        tokio::select! {
            () = cancel.cancelled() => {
                let mut guard = live
                    .lock()
                    .expect("invariant: slack stream-pump live map poisoned");
                for (_, h) in guard.drain() {
                    h.abort();
                }
                return;
            }
            maybe = rx.recv() => {
                let Some(req) = maybe else { return; };
                let root = req.root;
                let deps_clone = deps.clone();
                let cancel_clone = cancel.clone();
                let live_for_task = Arc::clone(&live);
                let span = info_span!(
                    "slack.stream_pump",
                    relay.dag.root = %root,
                    slack.team = %req.team_id,
                    slack.channel = %req.channel_id,
                );
                let mut guard = live
                    .lock()
                    .expect("invariant: slack stream-pump live map poisoned");
                if guard.contains_key(&root) {
                    continue;
                }
                if guard.len() >= MAX_SLACK_STREAM_PUMPS {
                    let finished_key = guard
                        .iter()
                        .find_map(|(k, h)| h.is_finished().then_some(*k));
                    if let Some(k) = finished_key {
                        guard.remove(&k);
                    } else if let Some(k) = guard.keys().next().copied()
                        && let Some(h) = guard.remove(&k)
                    {
                        h.abort();
                    }
                }
                let handle = tokio::spawn(
                    async move {
                        if let Err(e) = run_pump(&deps_clone, &req, cancel_clone).await {
                            warn!(error = ?e, event = "slack.stream_pump.exit_with_error");
                        }
                        let mut guard = live_for_task
                            .lock()
                            .expect("invariant: slack stream-pump live map poisoned");
                        guard.remove(&root);
                    }
                    .instrument(span),
                );
                guard.insert(root, handle);
            }
        }
    }
}

/// Per-DAG pump body. Reads broadcast items until the stream
/// quiesces, the cancel token fires, or `SLACK_PUMP_IDLE_TTL` elapses
/// with no chunks.
///
/// Ordering: `WireMcpRequest` cards are *deferred* into a small buffer
/// and posted after the next text-bearing chunk (Done / AgentMessage /
/// Error). Without this, the card would land in the thread *before*
/// the agent's narrative — out of order with the web UI's stacked
/// bubble layout (text above, card below).
async fn run_pump(
    deps: &PumpDeps,
    req: &AttachRequest,
    cancel: CancellationToken,
) -> Result<(), SlackError> {
    let mut stream = deps.thread_stream.subscribe(req.root);
    let workspace = deps.workspaces.read_by_team(&req.team_id).await?;
    let mut idle_deadline = Instant::now() + SLACK_PUMP_IDLE_TTL;
    let mut deferred: Vec<DeferredPost> = Vec::new();
    let mut minted_threads: usize = 0;

    loop {
        tokio::select! {
            () = cancel.cancelled() => return Ok(()),
            () = sleep_until(idle_deadline) => return Ok(()),
            next = stream.next() => {
                let Some(result) = next else {
                    return Ok(());
                };
                idle_deadline = Instant::now() + SLACK_PUMP_IDLE_TTL;
                handle_stream_event(
                    deps,
                    req,
                    &workspace.bot_token,
                    &mut deferred,
                    &mut minted_threads,
                    result,
                )
                .await;
            }
        }
    }
}

/// One pending Slack post: the body, the username to attribute it to,
/// and the session it belongs to. Used by the deferred-card buffer in
/// [`run_pump`]; session is preserved so the deferred card lands in
/// the same per-session Slack thread the text-bearing chunk did.
struct DeferredPost {
    body: PostBody,
    username: String,
    session_id: SessionId,
}

async fn handle_stream_event(
    deps: &PumpDeps,
    req: &AttachRequest,
    bot_token: &super::types::SlackBotToken,
    deferred: &mut Vec<DeferredPost>,
    minted_threads: &mut usize,
    event: Result<ThreadStreamEvent, crate::runtime::ThreadStreamError>,
) {
    match event {
        Err(e) => {
            warn!(error = ?e, event = "slack.stream_pump.backend_error");
        }
        Ok(ThreadStreamEvent::Stalled) => {
            warn!(event = "slack.stream_pump.stalled");
        }
        Ok(ThreadStreamEvent::Item(item)) => {
            let connect_url = match &item.chunk {
                ResponseChunk::WireMcpRequest {
                    catalog_id,
                    auth_kind: McpAuthKind::OAuth2,
                    ..
                } => Some(build_connect_url(deps, req, catalog_id, item.from_agent)),
                _ => None,
            };
            let Some(body) = payload_for_post(&item.chunk, connect_url.as_deref()) else {
                return;
            };
            let username = resolve_agent_name(deps, item.from_agent).await;
            let body = clip_body(body, SLACK_MAX_POST_CHARS);
            let (route_session, allow_mint) = routing_for(&item.chunk, item.session_id);
            if matches!(&item.chunk, ResponseChunk::WireMcpRequest { .. }) {
                if deferred.len() >= MAX_DEFERRED_WIRE_CARDS {
                    deferred.remove(0);
                }
                deferred.push(DeferredPost {
                    body,
                    username,
                    session_id: route_session,
                });
                return;
            }
            dispatch_post(
                deps.poster.as_ref(),
                deps.threads.as_ref(),
                req,
                bot_token,
                route_session,
                body,
                username,
                allow_mint,
                minted_threads,
            )
            .await;
            // Flush pending cards into the binding the text-bearing
            // chunk just established (or pre-existing). Deferred cards
            // never mint — a card without surrounding narrative would
            // be a confusing standalone thread.
            for pending in deferred.drain(..) {
                dispatch_post(
                    deps.poster.as_ref(),
                    deps.threads.as_ref(),
                    req,
                    bot_token,
                    pending.session_id,
                    pending.body,
                    pending.username,
                    false,
                    minted_threads,
                )
                .await;
            }
        }
    }
}

/// Route a single user-visible post to Slack.
///
/// Looks up the session's existing Slack thread; on hit, posts under
/// that `thread_ts`. On miss, behaviour depends on `allow_mint`:
/// - `true` (an explicit agent→human `AgentMessage`) — mint a fresh
///   top-level channel post and record the binding so the next chunk
///   for the same session sticks.
/// - `false` (`Done` / `Error` / deferred `WireMcpRequest`) — drop the
///   post. Without a binding, these chunks belong to an agent↔agent
///   session and surfacing them would create a side thread the human
///   has no addressable agent for.
#[allow(clippy::too_many_arguments)] // straight-line dispatch with no useful grouping
async fn dispatch_post(
    poster: &dyn super::poster::SlackPoster,
    threads: &dyn super::thread_map::SlackThreadStore,
    req: &AttachRequest,
    token: &super::types::SlackBotToken,
    session_id: SessionId,
    body: PostBody,
    username: String,
    allow_mint: bool,
    minted_threads: &mut usize,
) {
    let existing = match threads.lookup_by_session(session_id).await {
        Ok(opt) => opt,
        Err(e) => {
            warn!(
                error = ?e,
                relay.session.id = %session_id,
                event = "slack.stream_pump.lookup_failed",
            );
            return;
        }
    };

    if let Some(binding) = existing {
        if let Err(e) = poster
            .post(PostRequest {
                token: token.clone(),
                channel: binding.channel_id,
                thread_ts: Some(binding.thread_ts),
                body,
                username,
            })
            .await
        {
            warn!(error = ?e, event = "slack.stream_pump.post_failed");
        }
        return;
    }

    if !allow_mint {
        // Internal agent↔agent emission (typically a `Done` in a
        // descendant-pair session). The human is not the receiver of
        // this turn, so surfacing it would leak agent-private content
        // into Slack — drop silently.
        return;
    }

    // Miss — descendant agent reaching the human for the first time in
    // this session via an explicit `send_message(human, …)`. Mint a
    // fresh top-level post in the channel the DAG is rooted in, then
    // bind the session to the returned `ts`.
    if *minted_threads >= MAX_SLACK_THREADS_PER_DAG_ROOT {
        warn!(
            relay.session.id = %session_id,
            slack.minted = *minted_threads,
            event = "slack.stream_pump.mint_capped",
        );
        return;
    }

    let new_ts = match poster
        .post(PostRequest {
            token: token.clone(),
            channel: req.channel_id.clone(),
            thread_ts: None,
            body,
            username,
        })
        .await
    {
        Ok(ts) => ts,
        Err(e) => {
            warn!(
                error = ?e,
                relay.session.id = %session_id,
                event = "slack.stream_pump.mint_post_failed",
            );
            return;
        }
    };

    let anchor = match SlackThreadTs::try_from(new_ts.as_str()) {
        Ok(t) => t,
        Err(e) => {
            warn!(
                error = ?e,
                relay.session.id = %session_id,
                event = "slack.stream_pump.mint_anchor_invalid",
            );
            return;
        }
    };

    if let Err(e) = threads
        .bind_root(
            req.org_id,
            &req.team_id,
            &req.channel_id,
            &anchor,
            session_id,
            req.root,
        )
        .await
    {
        warn!(
            error = ?e,
            relay.session.id = %session_id,
            event = "slack.stream_pump.bind_failed",
        );
        return;
    }

    *minted_threads = minted_threads.saturating_add(1);
}

/// Decide which session a chunk routes to in Slack, and whether the
/// pump may mint a fresh thread for it.
///
/// `AgentMessage` carries its own `to_session` — the `(from, human)`
/// session the message belongs to. That differs from the publishing
/// request's session whenever an agent calls `send_message(human, …)`
/// from inside an agent↔agent turn (e.g. writer running in a
/// `writer↔recruiter` session calls `send_message(human, …)`; the
/// chunk's `to_session` is `writer↔human`, the request's session is
/// `writer↔recruiter`). The pump must route by `to_session` so the
/// message lands in the existing `writer↔human` thread instead of
/// minting a sibling thread.
///
/// `AgentMessage` is also the only chunk type that may mint — it's
/// the explicit agent→human address. Done / Error / WireMcpRequest
/// only post if the chunk's session is already bound; otherwise
/// they'd leak internal agent↔agent traffic into new Slack threads.
fn routing_for(chunk: &ResponseChunk, request_session: SessionId) -> (SessionId, bool) {
    match chunk {
        ResponseChunk::AgentMessage { to_session, .. } => (*to_session, true),
        _ => (request_session, false),
    }
}

/// Map a [`ResponseChunk`] to the body we post into Slack, or `None`
/// if this variant is not user-visible. Pure: caller is responsible for
/// minting the `connect_url` (signed token) when applicable.
fn payload_for_post(chunk: &ResponseChunk, connect_url: Option<&str>) -> Option<PostBody> {
    match chunk {
        ResponseChunk::Done { final_text } => Some(PostBody::Text(final_text.clone())),
        ResponseChunk::AgentMessage { content, .. } => Some(PostBody::Text(content.clone())),
        ResponseChunk::Error { reason } => {
            Some(PostBody::Text(format!(":warning: Error: {reason}")))
        }
        ResponseChunk::WireMcpRequest {
            display_name,
            reason,
            auth_kind,
            ..
        } => {
            let blocks =
                build_connection_request_card(reason, display_name, *auth_kind, connect_url);
            Some(PostBody::Blocks {
                fallback_text: format!("Relay agent requested {display_name} connection"),
                blocks,
            })
        }
        // Streaming-only chunks — dropped under the "post once on Final"
        // user choice.
        ResponseChunk::Text { .. }
        | ResponseChunk::Reasoning { .. }
        | ResponseChunk::ToolCall(_)
        | ResponseChunk::ToolResult(_)
        | ResponseChunk::Stalled => None,
    }
}

/// Mint a signed `GET /slack/mcp/connect?token=...` URL for the
/// Connect button. The token binds the catalog being wired, the Slack
/// thread context (so the OAuth callback can post the "✓ Connected"
/// ping back), and the relay session + originating agent (so the
/// universal auto-continue can resume the right agent loop).
fn build_connect_url(
    deps: &PumpDeps,
    req: &AttachRequest,
    catalog_id: &McpCatalogId,
    agent_id: crate::agents::AgentId,
) -> String {
    let exp = deps
        .clock
        .now_unix_secs()
        .saturating_add(CONNECT_LINK_TTL_SECS);
    let claims = SlackConnectClaims {
        catalog_id: catalog_id.clone(),
        team_id: req.team_id.clone(),
        channel_id: req.channel_id.clone(),
        thread_ts: req.thread_ts.clone(),
        slack_user_id: req.slack_user_id.clone(),
        session_id: req.session_id,
        agent_id,
    };
    let token = sign_connect(deps.signing_secret.expose().as_bytes(), &claims, exp);
    let base = deps.connect_url_base.trim_end_matches('/');
    format!("{base}/slack/mcp/connect?token={token}")
}

/// Best-effort `from_agent` → username. On lookup failure (deleted
/// agent, transient DB), surface a stable placeholder so the post
/// still lands.
async fn resolve_agent_name(deps: &PumpDeps, from: crate::agents::AgentId) -> String {
    match deps.agents.read(from).await {
        Ok(record) => record.name.as_str().to_owned(),
        Err(e) => {
            warn!(error = ?e, agent.id = %from, event = "slack.stream_pump.agent_read_failed");
            "agent".to_owned()
        }
    }
}

/// Clip the body's textual surface to fit Slack's per-message length cap.
/// For `PostBody::Text` the whole string is clipped; for `PostBody::Blocks`
/// only the fallback text is — individual block builders own block-level
/// truncation (e.g. `connection_card` truncates the reason paragraph).
fn clip_body(body: PostBody, max_chars: usize) -> PostBody {
    match body {
        PostBody::Text(s) => PostBody::Text(clip(s, max_chars)),
        PostBody::Blocks {
            fallback_text,
            blocks,
        } => PostBody::Blocks {
            fallback_text: clip(fallback_text, max_chars),
            blocks,
        },
    }
}

/// Trim `text` to fit Slack's per-message length cap with a graceful
/// ellipsis. Slice on a char boundary so we don't split a UTF-8 codepoint.
fn clip(text: String, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    if text.chars().count() <= max_chars {
        return text;
    }
    let mut out: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_for_post_returns_done_text() {
        let c = ResponseChunk::Done {
            final_text: "answer".to_owned(),
        };
        match payload_for_post(&c, None) {
            Some(PostBody::Text(s)) => assert_eq!(s, "answer"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn payload_for_post_returns_agent_message_content() {
        let c = ResponseChunk::AgentMessage {
            from: crate::agents::AgentId::new(),
            to_session: SessionId::new(),
            content: "hello".to_owned(),
        };
        match payload_for_post(&c, None) {
            Some(PostBody::Text(s)) => assert_eq!(s, "hello"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn payload_for_post_returns_error_with_prefix() {
        let c = ResponseChunk::Error {
            reason: "timeout".to_owned(),
        };
        match payload_for_post(&c, None) {
            Some(PostBody::Text(s)) => {
                assert!(s.contains("timeout"));
                assert!(s.contains("Error"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn payload_for_post_drops_streaming_variants() {
        assert!(
            payload_for_post(
                &ResponseChunk::Text {
                    value: "x".to_owned()
                },
                None
            )
            .is_none()
        );
        assert!(
            payload_for_post(
                &ResponseChunk::Reasoning {
                    value: "x".to_owned()
                },
                None
            )
            .is_none()
        );
        assert!(payload_for_post(&ResponseChunk::Stalled, None).is_none());
    }

    #[test]
    fn payload_for_post_renders_wire_mcp_request_with_connect_button() {
        let chunk = ResponseChunk::WireMcpRequest {
            from: crate::agents::AgentId::new(),
            catalog_id: McpCatalogId::try_from("notion").expect("valid catalog id"),
            display_name: "Notion".to_owned(),
            reason: "I want to draft a brief.".to_owned(),
            auth_kind: McpAuthKind::OAuth2,
            homepage_url: None,
        };
        let body = payload_for_post(
            &chunk,
            Some("https://relay.example/slack/mcp/connect?token=abc"),
        )
        .expect("some");
        match body {
            PostBody::Blocks {
                fallback_text,
                blocks,
            } => {
                assert!(fallback_text.contains("Notion"));
                let url = blocks[1]["elements"][0]["url"].as_str().expect("url");
                assert!(url.ends_with("?token=abc"));
            }
            PostBody::Text(_) => panic!("expected Blocks for WireMcpRequest"),
        }
    }

    #[test]
    fn payload_for_post_wire_mcp_request_static_headers_drops_button() {
        let chunk = ResponseChunk::WireMcpRequest {
            from: crate::agents::AgentId::new(),
            catalog_id: McpCatalogId::try_from("linear").expect("valid catalog id"),
            display_name: "Linear".to_owned(),
            reason: "Need Linear access.".to_owned(),
            auth_kind: McpAuthKind::StaticHeaders,
            homepage_url: None,
        };
        let body = payload_for_post(&chunk, Some("https://relay.example/x")).expect("some");
        if let PostBody::Blocks { blocks, .. } = body {
            assert_eq!(blocks[1]["type"], "context");
        } else {
            panic!("expected Blocks");
        }
    }

    #[test]
    fn clip_short_strings_passthrough() {
        let s = "abc".to_owned();
        assert_eq!(clip(s.clone(), 10), s);
    }

    #[test]
    fn clip_trims_with_ellipsis() {
        let s = "a".repeat(100);
        let out = clip(s, 10);
        assert_eq!(out.chars().count(), 10);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn clip_zero_max_returns_empty() {
        assert_eq!(clip("anything".to_owned(), 0), "");
    }

    use crate::slack::poster::FakeSlackPoster;
    use crate::slack::thread_map::{FakeSlackThreadStore, SlackThreadStore as _};
    use crate::slack::types::{SlackBotToken, SlackChannelId, SlackTeamId, SlackThreadTs};

    fn dispatch_fixtures() -> (
        AttachRequest,
        SlackBotToken,
        FakeSlackPoster,
        FakeSlackThreadStore,
    ) {
        let req = AttachRequest {
            root: PromptRequestId::new(),
            org_id: OrgId::new(),
            team_id: SlackTeamId::try_from("T01ROOT").expect("team id"),
            channel_id: SlackChannelId::try_from("C01CHAN").expect("channel id"),
            thread_ts: SlackThreadTs::try_from("1700000000.111111").expect("thread ts"),
            slack_user_id: SlackUserId::try_from("U01USR").expect("user id"),
            session_id: SessionId::new(),
        };
        let token = SlackBotToken::try_from("xoxb-fake-token".to_owned()).expect("bot token");
        (
            req,
            token,
            FakeSlackPoster::new(),
            FakeSlackThreadStore::new(),
        )
    }

    fn text_body(s: &str) -> PostBody {
        PostBody::Text(s.to_owned())
    }

    #[tokio::test]
    async fn dispatch_first_chunk_for_new_session_mints_top_level_post() {
        let (req, token, poster, threads) = dispatch_fixtures();
        let mut minted = 0;
        let session = SessionId::new();

        dispatch_post(
            &poster,
            &threads,
            &req,
            &token,
            session,
            text_body("hello from recruiter"),
            "recruiter".to_owned(),
            true,
            &mut minted,
        )
        .await;

        let captured = poster.captured();
        assert_eq!(captured.len(), 1, "exactly one post emitted");
        assert!(
            captured[0].thread_ts.is_none(),
            "fresh session posts as top-level"
        );
        assert_eq!(captured[0].channel.as_str(), req.channel_id.as_str());
        assert_eq!(captured[0].username, "recruiter");
        match &captured[0].body {
            PostBody::Text(s) => assert_eq!(s, "hello from recruiter"),
            PostBody::Blocks { .. } => panic!("text body expected"),
        }
        assert_eq!(minted, 1, "mint counter bumped");
        assert_eq!(threads.len(), 1, "binding recorded");
    }

    #[tokio::test]
    async fn dispatch_second_chunk_same_session_threads_under_minted_anchor() {
        let (req, token, poster, threads) = dispatch_fixtures();
        let mut minted = 0;
        let session = SessionId::new();

        dispatch_post(
            &poster,
            &threads,
            &req,
            &token,
            session,
            text_body("first"),
            "recruiter".to_owned(),
            true,
            &mut minted,
        )
        .await;
        dispatch_post(
            &poster,
            &threads,
            &req,
            &token,
            session,
            text_body("second"),
            "recruiter".to_owned(),
            true,
            &mut minted,
        )
        .await;

        let captured = poster.captured();
        assert_eq!(captured.len(), 2);
        assert!(captured[0].thread_ts.is_none(), "mint = top-level");
        let lookup = threads
            .lookup_by_session(session)
            .await
            .expect("lookup ok")
            .expect("binding exists");
        let anchor = captured[1].thread_ts.as_ref().expect("second threaded");
        assert_eq!(anchor.as_str(), lookup.thread_ts.as_str());
        assert_eq!(minted, 1, "only one mint across the two chunks");
    }

    #[tokio::test]
    async fn dispatch_uses_existing_binding_without_minting() {
        let (req, token, poster, threads) = dispatch_fixtures();
        let mut minted = 0;
        let session = SessionId::new();
        let existing_ts = SlackThreadTs::try_from("1700000000.222222").expect("ts");
        threads
            .bind_root(
                req.org_id,
                &req.team_id,
                &req.channel_id,
                &existing_ts,
                session,
                req.root,
            )
            .await
            .expect("seed binding");

        dispatch_post(
            &poster,
            &threads,
            &req,
            &token,
            session,
            text_body("follow-up"),
            "writer".to_owned(),
            false,
            &mut minted,
        )
        .await;

        let captured = poster.captured();
        assert_eq!(captured.len(), 1);
        assert_eq!(
            captured[0].thread_ts.as_ref().expect("threaded").as_str(),
            existing_ts.as_str(),
        );
        assert_eq!(minted, 0, "hit branch does not bump mint counter");
    }

    #[tokio::test]
    async fn dispatch_caps_minting_at_per_dag_root_limit() {
        let (req, token, poster, threads) = dispatch_fixtures();
        let mut minted = MAX_SLACK_THREADS_PER_DAG_ROOT;
        let session = SessionId::new();

        dispatch_post(
            &poster,
            &threads,
            &req,
            &token,
            session,
            text_body("should-be-dropped"),
            "agent".to_owned(),
            true,
            &mut minted,
        )
        .await;

        assert_eq!(
            poster.count(),
            0,
            "cap exhaustion drops the chunk without posting"
        );
        assert_eq!(
            threads.len(),
            0,
            "cap exhaustion does not bind a new thread"
        );
        assert_eq!(minted, MAX_SLACK_THREADS_PER_DAG_ROOT);
    }

    #[tokio::test]
    async fn dispatch_drops_unbound_session_when_minting_disallowed() {
        // Done / Error / WireMcpRequest chunks from an agent↔agent
        // session must not mint a new Slack thread — that's where the
        // "writer posts in a new thread" bug came from.
        let (req, token, poster, threads) = dispatch_fixtures();
        let mut minted = 0;
        let session = SessionId::new();

        dispatch_post(
            &poster,
            &threads,
            &req,
            &token,
            session,
            text_body("internal turn final answer"),
            "writer".to_owned(),
            false,
            &mut minted,
        )
        .await;

        assert_eq!(
            poster.count(),
            0,
            "no binding + allow_mint=false drops the post"
        );
        assert_eq!(threads.len(), 0, "no thread minted");
        assert_eq!(minted, 0);
    }

    // ──────────────────────────────────────────────────────────────────
    // routing_for: AgentMessage → (to_session, allow_mint=true);
    // everything else → (request_session, allow_mint=false).
    // ──────────────────────────────────────────────────────────────────

    #[test]
    fn routing_for_agent_message_uses_to_session_and_allows_mint() {
        // Reproduces the writer→human bug: writer is running in a
        // sibling agent↔agent session, but its send_message(human)
        // chunk must route by `to_session` (writer↔human) — not by
        // the publishing request's session (writer↔recruiter).
        let writer_human = SessionId::new();
        let writer_recruiter = SessionId::new();
        let chunk = ResponseChunk::AgentMessage {
            from: crate::agents::AgentId::new(),
            to_session: writer_human,
            content: "follow-up question".to_owned(),
        };
        let (route_session, allow_mint) = routing_for(&chunk, writer_recruiter);
        assert_eq!(route_session, writer_human, "route by to_session");
        assert!(allow_mint, "AgentMessage may mint");
    }

    #[test]
    fn routing_for_done_uses_request_session_and_disallows_mint() {
        // The exact bug shape: writer's Done in (writer ↔ recruiter)
        // must route by the publishing request's session and must NOT
        // be allowed to mint — otherwise it leaks into a fresh thread.
        let writer_recruiter = SessionId::new();
        let chunk = ResponseChunk::Done {
            final_text: "internal reply to recruiter".to_owned(),
        };
        let (route_session, allow_mint) = routing_for(&chunk, writer_recruiter);
        assert_eq!(route_session, writer_recruiter);
        assert!(!allow_mint, "Done never mints");
    }

    #[test]
    fn routing_for_error_uses_request_session_and_disallows_mint() {
        let request_session = SessionId::new();
        let chunk = ResponseChunk::Error {
            reason: "timeout".to_owned(),
        };
        let (route_session, allow_mint) = routing_for(&chunk, request_session);
        assert_eq!(route_session, request_session);
        assert!(!allow_mint);
    }

    #[test]
    fn routing_for_wire_mcp_request_uses_request_session_and_disallows_mint() {
        let request_session = SessionId::new();
        let chunk = ResponseChunk::WireMcpRequest {
            from: crate::agents::AgentId::new(),
            catalog_id: McpCatalogId::try_from("notion").expect("valid catalog id"),
            display_name: "Notion".to_owned(),
            reason: "draft a brief".to_owned(),
            auth_kind: McpAuthKind::OAuth2,
            homepage_url: None,
        };
        let (route_session, allow_mint) = routing_for(&chunk, request_session);
        assert_eq!(route_session, request_session);
        assert!(!allow_mint, "WireMcpRequest piggybacks, never mints");
    }

    // ──────────────────────────────────────────────────────────────────
    // End-to-end: the (writer↔human, writer↔recruiter) bug scenario.
    // ──────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn agent_message_from_sibling_pair_lands_in_writer_human_thread() {
        // Setup:
        //   T1   = bound to writer↔human (from the inbound mention)
        //   T2   = bound to recruiter↔human (recruiter's earlier mint)
        //
        // Writer runs in writer↔recruiter (sibling pair). It calls
        // send_message(human, …). The chunk is published with
        //   to_session = writer↔human
        //   request session = writer↔recruiter
        // The pump must route by to_session → T1, NOT mint a new
        // thread and NOT post under T2.
        let (req, token, poster, threads) = dispatch_fixtures();
        let writer_human = SessionId::new();
        let recruiter_human = SessionId::new();
        let writer_recruiter = SessionId::new();
        let t1 = SlackThreadTs::try_from("1700000000.111111").expect("ts");
        let t2 = SlackThreadTs::try_from("1700000000.222222").expect("ts");
        threads
            .bind_root(
                req.org_id,
                &req.team_id,
                &req.channel_id,
                &t1,
                writer_human,
                req.root,
            )
            .await
            .expect("seed t1");
        threads
            .bind_root(
                req.org_id,
                &req.team_id,
                &req.channel_id,
                &t2,
                recruiter_human,
                req.root,
            )
            .await
            .expect("seed t2");

        let chunk = ResponseChunk::AgentMessage {
            from: crate::agents::AgentId::new(),
            to_session: writer_human,
            content: "here's the code example you asked for".to_owned(),
        };
        let (route_session, allow_mint) = routing_for(&chunk, writer_recruiter);

        let mut minted = 0;
        dispatch_post(
            &poster,
            &threads,
            &req,
            &token,
            route_session,
            text_body("here's the code example you asked for"),
            "writer".to_owned(),
            allow_mint,
            &mut minted,
        )
        .await;

        let captured = poster.captured();
        assert_eq!(captured.len(), 1, "exactly one post");
        let posted_thread = captured[0].thread_ts.as_ref().expect("threaded reply");
        assert_eq!(
            posted_thread.as_str(),
            t1.as_str(),
            "writer's send_message(human) must land in writer↔human's thread (T1), \
             not recruiter's thread (T2), and not a fresh mint",
        );
        assert_eq!(minted, 0, "no new thread minted — used existing binding");
        assert_eq!(threads.len(), 2, "no extra binding written");
    }

    #[tokio::test]
    async fn done_in_sibling_pair_does_not_leak_to_slack() {
        // The mirror case: writer's Done in (writer↔recruiter) must
        // not surface anywhere in Slack — it's internal turn output
        // addressed to recruiter, not the human.
        let (req, token, poster, threads) = dispatch_fixtures();
        let writer_human = SessionId::new();
        let writer_recruiter = SessionId::new();
        let t1 = SlackThreadTs::try_from("1700000000.111111").expect("ts");
        threads
            .bind_root(
                req.org_id,
                &req.team_id,
                &req.channel_id,
                &t1,
                writer_human,
                req.root,
            )
            .await
            .expect("seed t1");

        let chunk = ResponseChunk::Done {
            final_text: "internal answer back to recruiter".to_owned(),
        };
        let (route_session, allow_mint) = routing_for(&chunk, writer_recruiter);

        let mut minted = 0;
        dispatch_post(
            &poster,
            &threads,
            &req,
            &token,
            route_session,
            text_body("internal answer back to recruiter"),
            "writer".to_owned(),
            allow_mint,
            &mut minted,
        )
        .await;

        assert_eq!(
            poster.count(),
            0,
            "sibling-pair Done must not leak into any Slack thread",
        );
        assert_eq!(minted, 0);
        assert_eq!(threads.len(), 1, "no new binding");
    }
}
