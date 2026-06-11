//! Outbound stream pumps: subscribe to one `PgThreadStream` slot per
//! Slack-bound Patom thread and forward `Done` / `AgentMessage` / `Error` /
//! `WireMcpRequest` chunks to Slack via `chat.postMessage`.
//!
//! Why this lives here, not on the SSE/HTTP path:
//! `PgThreadStream::subscribe(thread_id)` is an in-process
//! `broadcast::Receiver` — the Slack adapter takes the same primitive the web
//! UI does, no HTTP loop required.
//!
//! One task per active Slack-bound thread. Tasks are owned by a `JoinSet` capped
//! at `MAX_SLACK_STREAM_PUMPS`; new attaches over the cap evict the oldest idle
//! pump. Each task self-exits after `SLACK_PUMP_IDLE_TTL` of inactivity.
//!
//! Routing is trivial in the thread model: one Patom thread ↔ one Slack thread
//! (the inbound bridge binds it on creation, `slack_threads.thread_id`). Every
//! chunk a pump sees belongs to its thread, so every text-bearing chunk posts
//! into the single bound Slack `(channel, thread_ts)` — no per-session minting.
//!
//! Post rules:
//! - `ResponseChunk::Done { final_text }` → post `final_text` attributed to the
//!   chunk's `from_agent` (skipped when empty — quiescence `Done` is empty).
//! - `ResponseChunk::AgentMessage { from, content }` → post `content` attributed
//!   to `from` (the agent's posted egress; the human's view of the conversation).
//! - `ResponseChunk::Error { reason }` → post a short error note.
//! - `ResponseChunk::WireMcpRequest { .. }` → post a Block Kit connection-request
//!   card. For oauth2 catalogs the card carries a Connect button URL signed by
//!   `connect_link`; for static_headers / none it degrades to a "finish in the
//!   web UI" hint.
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
use crate::clock::SharedClock;
use crate::mcp::{McpAuthKind, McpCatalogId};
use crate::runtime::{ResponseChunk, SharedThreadStream, ThreadStreamEvent};
use crate::threads::ThreadId;
use crate::types::SecretString;

use super::connect_link::{SlackConnectClaims, sign_connect};
use super::connection_card::build_connection_request_card;
use super::error::SlackError;
use super::limits::{MAX_SLACK_STREAM_PUMPS, SLACK_MAX_POST_CHARS, SLACK_PUMP_IDLE_TTL};
use super::poster::{PostBody, PostRequest, SharedSlackPoster};
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

/// Request to attach a pump for a Slack-bound Patom thread.
#[derive(Debug, Clone)]
pub struct AttachRequest {
    /// The Patom thread this pump forwards. The pump subscribes the
    /// `PgThreadStream` slot for this id.
    pub thread_id: ThreadId,
    pub team_id: SlackTeamId,
    pub channel_id: SlackChannelId,
    /// The Slack thread anchor every chunk for `thread_id` posts under.
    pub thread_ts: SlackThreadTs,
    /// The Slack user who originated this thread. Threaded into any
    /// Connect-button URL minted for `WireMcpRequest` chunks so the
    /// `GET /slack/mcp/connect` handler can resolve the patom user
    /// the credential should write under.
    pub slack_user_id: SlackUserId,
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
    /// HMAC signing key for [`SlackConnectClaims`] tokens minted into
    /// Connect button URLs. Shared with `oauth.rs`'s state-token
    /// signing — same secret, same TTL.
    pub signing_secret: SecretString,
    /// Public-facing patom base URL (no trailing slash) the Connect
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
    let live: Arc<Mutex<HashMap<ThreadId, JoinHandle<()>>>> = Arc::new(Mutex::new(HashMap::new()));

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
                let thread_id = req.thread_id;
                let deps_clone = deps.clone();
                let cancel_clone = cancel.clone();
                let live_for_task = Arc::clone(&live);
                let span = info_span!(
                    "slack.stream_pump",
                    patom.thread.id = %thread_id,
                    slack.team = %req.team_id,
                    slack.channel = %req.channel_id,
                );
                let mut guard = live
                    .lock()
                    .expect("invariant: slack stream-pump live map poisoned");
                if guard.contains_key(&thread_id) {
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
                        guard.remove(&thread_id);
                    }
                    .instrument(span),
                );
                guard.insert(thread_id, handle);
            }
        }
    }
}

/// Per-thread pump body. Reads broadcast items until the stream closes, the
/// cancel token fires, or `SLACK_PUMP_IDLE_TTL` elapses with no chunks.
///
/// Ordering: `WireMcpRequest` cards are *deferred* into a small buffer and
/// posted after the next text-bearing chunk (Done / AgentMessage / Error).
/// Without this, the card would land in the thread *before* the agent's
/// narrative — out of order with the web UI's stacked bubble layout (text
/// above, card below).
async fn run_pump(
    deps: &PumpDeps,
    req: &AttachRequest,
    cancel: CancellationToken,
) -> Result<(), SlackError> {
    let mut stream = deps.thread_stream.subscribe(req.thread_id);
    let workspace = deps.workspaces.read_by_team(&req.team_id).await?;
    let mut idle_deadline = Instant::now() + SLACK_PUMP_IDLE_TTL;
    let mut deferred: Vec<DeferredPost> = Vec::new();

    loop {
        tokio::select! {
            () = cancel.cancelled() => return Ok(()),
            () = sleep_until(idle_deadline) => return Ok(()),
            next = stream.next() => {
                let Some(result) = next else {
                    return Ok(());
                };
                idle_deadline = Instant::now() + SLACK_PUMP_IDLE_TTL;
                handle_stream_event(deps, req, &workspace.bot_token, &mut deferred, result).await;
            }
        }
    }
}

/// One pending Slack post: the body + the username/avatar to attribute it to.
/// Used by the deferred-card buffer in [`run_pump`].
struct DeferredPost {
    body: PostBody,
    username: String,
    /// Avatar URL for the attributed agent, passed through as Slack
    /// `icon_url` (issue #43); `None` → default bot avatar.
    icon_url: Option<String>,
}

async fn handle_stream_event(
    deps: &PumpDeps,
    req: &AttachRequest,
    bot_token: &super::types::SlackBotToken,
    deferred: &mut Vec<DeferredPost>,
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
            let identity = resolve_agent_identity(deps, item.from_agent).await;
            let body = clip_body(body, SLACK_MAX_POST_CHARS);
            if matches!(&item.chunk, ResponseChunk::WireMcpRequest { .. }) {
                if deferred.len() >= MAX_DEFERRED_WIRE_CARDS {
                    deferred.remove(0);
                }
                deferred.push(DeferredPost {
                    body,
                    username: identity.username,
                    icon_url: identity.icon_url,
                });
                return;
            }
            post_to_thread(
                deps.poster.as_ref(),
                req,
                bot_token,
                body,
                identity.username,
                identity.icon_url,
            )
            .await;
            // Flush pending cards into the same Slack thread, after the
            // text-bearing chunk above (web-UI stacked-bubble order).
            for pending in deferred.drain(..) {
                post_to_thread(
                    deps.poster.as_ref(),
                    req,
                    bot_token,
                    pending.body,
                    pending.username,
                    pending.icon_url,
                )
                .await;
            }
        }
    }
}

/// Post one user-visible body into the thread's bound Slack thread. One Patom
/// thread ↔ one Slack thread (the binding the bridge recorded), so the target
/// is always `req.(channel_id, thread_ts)` — no lookup, no minting.
async fn post_to_thread(
    poster: &dyn super::poster::SlackPoster,
    req: &AttachRequest,
    token: &super::types::SlackBotToken,
    body: PostBody,
    username: String,
    icon_url: Option<String>,
) {
    if let Err(e) = poster
        .post(PostRequest {
            token: token.clone(),
            channel: req.channel_id.clone(),
            thread_ts: Some(req.thread_ts.clone()),
            body,
            username,
            // Per-agent avatar (issue #43): `Some` when the agent has
            // `avatar_url` set, else `None` → Slack's default bot avatar.
            icon_url,
            ephemeral_to: None,
        })
        .await
    {
        warn!(error = ?e, event = "slack.stream_pump.post_failed");
    }
}

/// Map a [`ResponseChunk`] to the body we post into Slack, or `None`
/// if this variant is not user-visible. Pure: caller is responsible for
/// minting the `connect_url` (signed token) when applicable.
fn payload_for_post(chunk: &ResponseChunk, connect_url: Option<&str>) -> Option<PostBody> {
    match chunk {
        // Empty text drops the post: Slack's `chat.postMessage` rejects
        // an empty `text` with `error: "no_text"` on a 200 response. An
        // agent that closes a turn with no user-visible final text — or
        // a cross-agent handoff with an empty content — has nothing to
        // surface, so we skip rather than emit an empty bubble.
        ResponseChunk::Done { final_text } if !final_text.is_empty() => {
            Some(PostBody::Text(final_text.clone()))
        }
        ResponseChunk::AgentMessage { content, .. } if !content.is_empty() => {
            Some(PostBody::Text(content.clone()))
        }
        ResponseChunk::Error { reason, .. } => {
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
                fallback_text: format!("Patom agent requested {display_name} connection"),
                blocks,
            })
        }
        // Dropped: empty-text Done/AgentMessage (would be `no_text`) and
        // streaming-only chunks under the "post once on Final" rule.
        ResponseChunk::Done { .. }
        | ResponseChunk::AgentMessage { .. }
        | ResponseChunk::Text { .. }
        | ResponseChunk::Reasoning { .. }
        | ResponseChunk::ToolCall(_)
        | ResponseChunk::ToolResult(_)
        | ResponseChunk::Stalled => None,
    }
}

/// Mint a signed `GET /slack/mcp/connect?token=...` URL for the
/// Connect button. The token binds the catalog being wired, the Slack
/// thread context (so the OAuth callback can post the "✓ Connected"
/// ping back), and the patom thread + originating agent (so the
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
        thread_id: req.thread_id,
        agent_id,
    };
    let token = sign_connect(deps.signing_secret.expose().as_bytes(), &claims, exp);
    let base = deps.connect_url_base.trim_end_matches('/');
    format!("{base}/slack/mcp/connect?token={token}")
}

/// The Slack identity (`username` + optional `icon_url`) an outbound
/// agent post is attributed to.
struct AgentIdentity {
    username: String,
    /// The agent's avatar URL, passed through as the Slack `icon_url`
    /// (issue #43). `None` → Slack renders the app's default bot avatar.
    icon_url: Option<String>,
}

/// Best-effort `from_agent` → Slack identity. On lookup failure (deleted
/// agent, transient DB), surface a stable placeholder username with no
/// avatar so the post still lands.
async fn resolve_agent_identity(deps: &PumpDeps, from: crate::agents::AgentId) -> AgentIdentity {
    match deps.agents.read(from).await {
        Ok(record) => AgentIdentity {
            username: record.name.as_str().to_owned(),
            icon_url: record.avatar_url.as_ref().map(|a| a.as_str().to_owned()),
        },
        Err(e) => {
            warn!(error = ?e, agent.id = %from, event = "slack.stream_pump.agent_read_failed");
            AgentIdentity {
                username: "agent".to_owned(),
                icon_url: None,
            }
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

    use crate::slack::poster::FakeSlackPoster;
    use crate::slack::types::{SlackBotToken, SlackChannelId, SlackTeamId, SlackThreadTs};

    fn attach_fixture() -> (AttachRequest, SlackBotToken) {
        let req = AttachRequest {
            thread_id: ThreadId::new(),
            team_id: SlackTeamId::try_from("T01ROOT").expect("team id"),
            channel_id: SlackChannelId::try_from("C01CHAN").expect("channel id"),
            thread_ts: SlackThreadTs::try_from("1700000000.111111").expect("thread ts"),
            slack_user_id: SlackUserId::try_from("U01USR").expect("user id"),
        };
        let token = SlackBotToken::try_from("xoxb-fake-token".to_owned()).expect("bot token");
        (req, token)
    }

    fn text_body(s: &str) -> PostBody {
        PostBody::Text(s.to_owned())
    }

    #[tokio::test]
    async fn post_to_thread_posts_under_bound_anchor() {
        let (req, token) = attach_fixture();
        let poster = FakeSlackPoster::new();

        post_to_thread(
            &poster,
            &req,
            &token,
            text_body("hello from recruiter"),
            "recruiter".to_owned(),
            Some("https://cdn.example/atlas.png".to_owned()),
        )
        .await;

        let captured = poster.captured();
        assert_eq!(captured.len(), 1, "exactly one post emitted");
        assert_eq!(
            captured[0].thread_ts.as_ref().map(SlackThreadTs::as_str),
            Some(req.thread_ts.as_str()),
            "posts under the bound Slack thread anchor",
        );
        assert_eq!(captured[0].channel.as_str(), req.channel_id.as_str());
        assert_eq!(captured[0].username, "recruiter");
        assert_eq!(
            captured[0].icon_url.as_deref(),
            Some("https://cdn.example/atlas.png"),
            "avatar_url passes through as icon_url",
        );
        match &captured[0].body {
            PostBody::Text(s) => assert_eq!(s, "hello from recruiter"),
            PostBody::Blocks { .. } => panic!("text body expected"),
        }
    }

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
            to_thread: ThreadId::new(),
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
            code: "timeout".to_owned(),
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
    fn payload_for_post_drops_done_with_empty_final_text() {
        // Slack would reject an empty `text` body with `no_text`; the
        // pump must drop the post rather than send it.
        let c = ResponseChunk::Done {
            final_text: String::new(),
        };
        assert!(payload_for_post(&c, None).is_none());
    }

    #[test]
    fn payload_for_post_drops_agent_message_with_empty_content() {
        let c = ResponseChunk::AgentMessage {
            from: crate::agents::AgentId::new(),
            to_thread: ThreadId::new(),
            content: String::new(),
        };
        assert!(payload_for_post(&c, None).is_none());
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
            Some("https://patom.example/slack/mcp/connect?token=abc"),
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
        let body = payload_for_post(&chunk, Some("https://patom.example/x")).expect("some");
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
}
