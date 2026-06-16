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

use crate::colleagues::ColleagueId;
use crate::runtime::{ResponseChunk, SharedThreadStream, ThreadStreamError, ThreadStreamEvent};
use crate::threads::ThreadId;

use super::app_store::SharedDiscordAppStore;
use super::bridge::{AttachRequest, OutboundAttach};
use super::directory::SharedDiscordDirectory;
use super::limits::{DISCORD_PUMP_IDLE_TTL, MAX_DISCORD_STREAM_PUMPS};
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
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            () = sleep_until(idle_deadline) => return,
            next = stream.next() => {
                let Some(result) = next else { return; };
                idle_deadline = Instant::now() + DISCORD_PUMP_IDLE_TTL;
                handle_stream_event(deps, req, result).await;
            }
        }
    }
}

/// Forward one stream event to Discord, if it carries user-visible text.
async fn handle_stream_event(
    deps: &PumpDeps,
    req: &AttachRequest,
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
    let Some((text, to)) = render_payload(&item.chunk) else {
        return;
    };
    // Post via the REPLYING agent's own bot; fall back to the attaching bot.
    let application_id = match deps
        .apps
        .app_id_for_agent(req.org_id, item.from_agent)
        .await
    {
        Ok(Some(a)) => a,
        Ok(None) => req.application_id.clone(),
        Err(e) => {
            warn!(error = ?e, event = "discord.stream_pump.agent_bot_resolve_failed");
            req.application_id.clone()
        }
    };
    let (content, pinged) = render_outbound(deps, req, &text, to).await;
    post_reply(deps, application_id, req, content, pinged).await;
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
}
