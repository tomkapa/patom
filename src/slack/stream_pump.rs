//! Outbound stream pumps: subscribe to one `PgThreadStream` slot per
//! Slack-rooted DAG and forward `Done` / `AgentMessage` / `Error`
//! chunks to Slack via `chat.postMessage`.
//!
//! Why this lives here, not on the SSE/HTTP path:
//! `PgThreadStream::subscribe(root)` is an in-process
//! `broadcast::Receiver` — the Slack adapter takes the same primitive
//! the web UI does, no HTTP loop required.
//!
//! One task per active Slack-rooted thread. Tasks are owned by a
//! `JoinSet` capped at `MAX_SLACK_STREAM_PUMPS`; new attaches over the
//! cap evict the oldest idle pump. Each task self-exits after
//! `SLACK_PUMP_IDLE_TTL` of inactivity.
//!
//! Post rules (Phase 1):
//! - `ResponseChunk::Done { final_text }` → post `final_text` attributed
//!   to the chunk's `from_agent` (the agent whose turn produced it).
//! - `ResponseChunk::AgentMessage { from, content }` → post `content`
//!   attributed to `from` (cross-agent handoff visible to the human).
//! - `ResponseChunk::Error { reason }` → post a short error note.
//! - All other variants (`Text`, `Reasoning`, `ToolCall`, `ToolResult`,
//!   `Stalled`) are dropped without a post.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use tokio::sync::{Mutex as AsyncMutex, mpsc};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep_until};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, info_span, warn};

use crate::agents::SharedAgentStore;
use crate::auth::OrgId;
use crate::runtime::{PromptRequestId, ResponseChunk, SharedThreadStream, ThreadStreamEvent};

use super::error::SlackError;
use super::limits::{MAX_SLACK_STREAM_PUMPS, SLACK_MAX_POST_CHARS, SLACK_PUMP_IDLE_TTL};
use super::poster::{PostRequest, SharedSlackPoster};
use super::types::{SlackChannelId, SlackTeamId, SlackThreadTs};
use super::workspace::SharedSlackWorkspaceStore;

/// Request to attach a pump for `root`.
#[derive(Debug, Clone)]
pub struct AttachRequest {
    pub root: PromptRequestId,
    pub org_id: OrgId,
    pub team_id: SlackTeamId,
    pub channel_id: SlackChannelId,
    pub thread_ts: SlackThreadTs,
}

/// Handle returned to the composition root.
///
/// `attach` schedules a new pump (no-op if one is already running for
/// `root`); `shutdown` cancels every pump and awaits the supervisor
/// task so the runtime has no detached pump tasks left at process
/// exit.
#[derive(Debug)]
pub struct StreamPumpHandle {
    tx: mpsc::Sender<AttachRequest>,
    cancel: CancellationToken,
    /// Supervisor's `JoinHandle`, parked in an async-aware mutex so
    /// `shutdown` (which takes `&self` because callers hold an `Arc`)
    /// can `take()` it and `.await` for clean exit. Outside of
    /// `shutdown` the mutex is uncontended.
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
}

impl std::fmt::Debug for PumpDeps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PumpDeps").finish_non_exhaustive()
    }
}

/// Spawn the supervisor. Holds the `JoinSet` of per-thread pump tasks
/// and routes attach requests onto fresh tasks (or no-ops when one is
/// already running for the requested root).
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
    // We hold join handles keyed by root so a duplicate attach is a
    // no-op. A real `JoinSet` would require us to track which task
    // belongs to which root; the `HashMap` is simpler and the cap is
    // still enforced (eviction below).
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
                // Single critical section: dedup, enforce cap, insert.
                // The supervisor lock is held only across synchronous
                // map ops + a `tokio::spawn` (which doesn't await).
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

/// Per-thread pump body. Reads broadcast items until the stream
/// quiesces, the cancel token fires, or `SLACK_PUMP_IDLE_TTL` elapses
/// with no chunks.
async fn run_pump(
    deps: &PumpDeps,
    req: &AttachRequest,
    cancel: CancellationToken,
) -> Result<(), SlackError> {
    let mut stream = deps.thread_stream.subscribe(req.root);
    // Resolve the workspace once — bot tokens are stable for the
    // pump's lifetime; if the workspace is uninstalled mid-thread,
    // the next post fails and we exit.
    let workspace = deps.workspaces.read_by_team(&req.team_id).await?;
    let mut idle_deadline = Instant::now() + SLACK_PUMP_IDLE_TTL;

    loop {
        tokio::select! {
            () = cancel.cancelled() => return Ok(()),
            () = sleep_until(idle_deadline) => return Ok(()),
            next = stream.next() => {
                let Some(result) = next else {
                    return Ok(());
                };
                idle_deadline = Instant::now() + SLACK_PUMP_IDLE_TTL;
                match result {
                    Err(e) => {
                        warn!(error = ?e, event = "slack.stream_pump.backend_error");
                    }
                    Ok(ThreadStreamEvent::Stalled) => {
                        warn!(event = "slack.stream_pump.stalled");
                        // Phase 1 accepts the loss; Phase 2 (issue #45)
                        // replays the latest terminal chunk from
                        // `prompt_response_chunks`.
                    }
                    Ok(ThreadStreamEvent::Item(item)) => {
                        if let Some(payload) = payload_for_post(&item.chunk) {
                            let username = resolve_agent_name(deps, item.from_agent).await;
                            let text = clip(payload, SLACK_MAX_POST_CHARS);
                            if let Err(e) = deps
                                .poster
                                .post(PostRequest {
                                    token: workspace.bot_token.clone(),
                                    channel: req.channel_id.clone(),
                                    thread_ts: req.thread_ts.clone(),
                                    text,
                                    username,
                                })
                                .await
                            {
                                warn!(error = ?e, event = "slack.stream_pump.post_failed");
                            }
                        }
                    }
                }
                // If the chunk we just observed was terminal, the DAG
                // has produced its final answer — let the pump idle
                // out naturally (it will exit on the next idle
                // deadline). We don't return immediately because an
                // agent might Ask another agent right after Done in
                // some flows; the idle TTL is the unambiguous gate.
                let _ = Duration::from_millis(0);
            }
        }
    }
}

/// Map a [`ResponseChunk`] to the text we post into Slack, or `None`
/// if this variant is not user-visible.
fn payload_for_post(chunk: &ResponseChunk) -> Option<String> {
    match chunk {
        ResponseChunk::Done { final_text } => Some(final_text.clone()),
        ResponseChunk::AgentMessage { content, .. } => Some(content.clone()),
        ResponseChunk::Error { reason } => Some(format!(":warning: Error: {reason}")),
        // Streaming-only chunks — dropped under the "post once on Final"
        // user choice.
        ResponseChunk::Text { .. }
        | ResponseChunk::Reasoning { .. }
        | ResponseChunk::ToolCall(_)
        | ResponseChunk::ToolResult(_)
        | ResponseChunk::Stalled => None,
    }
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
        assert_eq!(payload_for_post(&c).as_deref(), Some("answer"));
    }

    #[test]
    fn payload_for_post_returns_agent_message_content() {
        let c = ResponseChunk::AgentMessage {
            from: crate::agents::AgentId::new(),
            content: "hello".to_owned(),
        };
        assert_eq!(payload_for_post(&c).as_deref(), Some("hello"));
    }

    #[test]
    fn payload_for_post_returns_error_with_prefix() {
        let c = ResponseChunk::Error {
            reason: "timeout".to_owned(),
        };
        let out = payload_for_post(&c).expect("some");
        assert!(out.contains("timeout"));
        assert!(out.contains("Error"));
    }

    #[test]
    fn payload_for_post_drops_streaming_variants() {
        assert!(
            payload_for_post(&ResponseChunk::Text {
                value: "x".to_owned()
            })
            .is_none()
        );
        assert!(
            payload_for_post(&ResponseChunk::Reasoning {
                value: "x".to_owned()
            })
            .is_none()
        );
        assert!(payload_for_post(&ResponseChunk::Stalled).is_none());
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
