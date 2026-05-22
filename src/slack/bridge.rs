//! Inbound bridge: Slack `app_mention` event → Relay prompt enqueue.
//!
//! One single-consumer worker. The webhook handler hands events here via a
//! bounded mpsc and ack's Slack in <3 s; this worker does the slow path
//! (DB lookups, queue enqueue, stream-pump attach) off-line.
//!
//! Per-event flow (`process_event`):
//!
//! 1. Look up the workspace by `team_id` (privileged, decrypts the bot
//!    token). Unknown workspace → drop with a warn — Slack will retry,
//!    but the install was uninstalled.
//! 2. If the event's user is the bot itself, drop. The bot's own
//!    `chat.postMessage` posts fire `message` events; we are not
//!    subscribed to those, but defending here is cheap.
//! 3. Resolve identity: `slack_identities` lookup → linked user. Miss
//!    falls back to the workspace's `installed_by_user_id` (Phase 1
//!    simplification; Phase 2 turns the miss into an ephemeral "link
//!    your account" prompt — issue #41).
//! 4. Choose the anchor `thread_ts`. For replies Slack sets
//!    `event.thread_ts`; for a mention on a top-level message we use
//!    `event.ts` so the reply auto-creates a thread.
//! 5. `slack_threads` read-or-create.
//!    - **Existing row**: continuation. Receiver is the session's
//!      existing agent participant (HTTP path enforces the same rule —
//!      a user can't switch agents mid-thread).
//!    - **No row**: fresh session. Receiver is the mentioned agent, or
//!      the org's default if the mention parses to nothing /
//!      unresolvable.
//! 6. `queue.enqueue_for_user(...)` with
//!    `idempotency_key = "slack:<team>:<channel>:<event_ts>"`. Slack
//!    retries deliver the same key → enqueue returns
//!    `EnqueueOutcome::Existing` and we skip the bind.
//! 7. On `Inserted`, bind `(team, channel, thread_ts) → root_request_id`
//!    and attach a stream pump for the root.

use reqwest::Client;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, info, info_span, warn};

use crate::agents::{AgentId, AgentName, SharedAgentStore};
use crate::auth::OrgId;
use crate::runtime::{EnqueueOutcome, IdempotencyKey, NewPromptRequest, SharedPromptQueue};
use crate::session::SharedSessionStore;
use crate::types::{Participant, ParticipantKind, Prompt};

use super::error::SlackError;
use super::identity::SharedSlackIdentityStore;
use super::limits::SLACK_USERS_INFO_TIMEOUT;
use super::mention;
use super::poster::SharedSlackPoster;
use super::stream_pump::{AttachRequest, SharedStreamPumpHandle};
use super::thread_map::{SharedSlackThreadStore, ThreadMapping};
use super::types::{
    SlackBotToken, SlackChannelId, SlackTeamId, SlackThreadTs, SlackTs, SlackUserId,
};
use super::workspace::SharedSlackWorkspaceStore;

/// Where this event came from.
///
/// The bridge applies a different routing rule for each: an
/// `AppMention` always processes (falling back to the default agent
/// on an unknown name); a `ThreadMessage` only processes when the
/// thread is already bound, because we don't want arbitrary
/// in-channel chatter to mint fresh agent sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboundSource {
    /// The bot was `@`-mentioned in the message.
    AppMention,
    /// A non-mention message in a thread. Routed only if
    /// `slack_threads` already has a binding for the thread anchor.
    ThreadMessage,
}

/// Inbound event handed from the webhook handler to the bridge worker.
#[derive(Debug, Clone)]
pub struct InboundEvent {
    pub team_id: SlackTeamId,
    pub channel_id: SlackChannelId,
    pub user_id: SlackUserId,
    pub text: String,
    /// `thread_ts` when the mention is a reply inside an existing
    /// thread; `None` when the mention lands on a top-level message
    /// (caller uses `event_ts` as the anchor in that case).
    pub thread_ts: Option<SlackThreadTs>,
    pub event_ts: SlackTs,
    pub source: InboundSource,
}

/// Dependencies needed to process an event. Held by the worker; cloned
/// for each event so we can keep `process_event` a free function for
/// testing.
#[derive(Clone)]
pub struct BridgeDeps {
    pub queue: SharedPromptQueue,
    pub agents: SharedAgentStore,
    pub sessions: SharedSessionStore,
    pub workspaces: SharedSlackWorkspaceStore,
    pub identities: SharedSlackIdentityStore,
    pub threads: SharedSlackThreadStore,
    pub poster: SharedSlackPoster,
    pub stream_pump: SharedStreamPumpHandle,
    /// Shared HTTP client used to call `users.info` so the slash-command
    /// prompt mirror reads as the sender (correct workspace display name
    /// and avatar) instead of the bot/app default.
    pub http: Client,
}

impl std::fmt::Debug for BridgeDeps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BridgeDeps").finish_non_exhaustive()
    }
}

/// Handle for the spawned bridge worker. `shutdown` cancels and waits
/// for the worker to drain the in-flight event.
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

/// Spawn the bridge worker. Returns a handle for shutdown plus the
/// sender end of the mpsc the webhook hands events into.
pub fn spawn(
    deps: BridgeDeps,
    cancel: CancellationToken,
) -> (BridgeHandle, mpsc::Sender<InboundEvent>) {
    let (tx, rx) = mpsc::channel::<InboundEvent>(super::limits::SLACK_INBOUND_QUEUE);
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
    mut rx: mpsc::Receiver<InboundEvent>,
    cancel: CancellationToken,
) {
    info!(event = "slack.bridge.start");
    loop {
        tokio::select! {
            () = cancel.cancelled() => {
                info!(event = "slack.bridge.shutdown");
                return;
            }
            maybe = rx.recv() => {
                let Some(event) = maybe else {
                    info!(event = "slack.bridge.tx_closed");
                    return;
                };
                let span = info_span!(
                    "slack.bridge.process",
                    slack.team = %event.team_id,
                    slack.channel = %event.channel_id,
                    slack.event_ts = %event.event_ts,
                );
                if let Err(e) = process_event(&deps, event).instrument(span).await {
                    // Errors here are normal operating failures (unknown
                    // workspace, parse errors, transient DB hiccups);
                    // they must not crash the worker.
                    warn!(error = ?e, event = "slack.bridge.process_failed");
                }
            }
        }
    }
}

/// Single-event processing — extracted so unit tests can drive the
/// happy and error paths without spinning up the mpsc.
pub async fn process_event(deps: &BridgeDeps, event: InboundEvent) -> Result<(), SlackError> {
    let workspace = deps.workspaces.read_by_team(&event.team_id).await?;
    assert_eq!(workspace.team_id.as_str(), event.team_id.as_str());
    if workspace.bot_user_id.as_str() == event.user_id.as_str() {
        return Ok(());
    }
    let user_id = resolve_user_id(deps, &event, &workspace).await?;
    let anchor = match event.thread_ts.clone() {
        Some(t) => t,
        None => SlackThreadTs::try_from(event.event_ts.as_str())?,
    };
    let existing = deps
        .threads
        .lookup_by_thread(&event.team_id, &event.channel_id, &anchor)
        .await?;
    let receiver_agent_id = match (&existing, event.source) {
        (Some(mapping), _) => session_agent_participant(deps, mapping.session_id).await?,
        // Plain in-thread messages without a binding are dropped —
        // we never start a fresh agent session from random channel
        // chatter; that's the mention path's job.
        (None, InboundSource::ThreadMessage) => {
            info!(event = "slack.bridge.thread_message_unbound_dropped");
            return Ok(());
        }
        (None, InboundSource::AppMention) => {
            resolve_mention_or_default(deps, &event, &workspace.bot_user_id, workspace.org_id)
                .await?
        }
    };
    enqueue_and_bind(
        deps,
        &event,
        &workspace,
        &anchor,
        existing.as_ref(),
        receiver_agent_id,
        user_id,
    )
    .await
}

/// Slack user → Relay user. Phase 1 falls back to the workspace
/// installer when no explicit `slack_identities` row exists.
async fn resolve_user_id(
    deps: &BridgeDeps,
    event: &InboundEvent,
    workspace: &crate::slack::workspace::WorkspaceWithToken,
) -> Result<crate::auth::UserId, SlackError> {
    let linked = deps
        .identities
        .lookup(&event.team_id, &event.user_id)
        .await?;
    let Some(linked) = linked else {
        return Ok(workspace.installed_by_user_id);
    };
    assert_eq!(
        linked.org_id, workspace.org_id,
        "invariant: slack_identities + slack_workspaces FK keeps these aligned"
    );
    Ok(linked.user_id)
}

/// Enqueue a prompt for the resolved receiver and, on a fresh insert,
/// record the `(team, channel, thread_ts) → root_request_id` mapping.
/// Idempotency: a retried event with the same `event_ts` re-derives
/// the same `idempotency_key` and short-circuits at the queue.
///
/// `existing` carries the stored `ThreadMapping` for a continuing
/// thread. When `Some`, the pump attaches to its `root_request_id`
/// (the slot the PgThreadStream notifies on) rather than to this
/// turn's fresh `request_id`. Without this, threads resumed after a
/// backend restart silently drop outbound messages because the pump
/// is subscribed to the wrong broadcast slot.
async fn enqueue_and_bind(
    deps: &BridgeDeps,
    event: &InboundEvent,
    workspace: &crate::slack::workspace::WorkspaceWithToken,
    anchor: &SlackThreadTs,
    existing: Option<&ThreadMapping>,
    receiver_agent_id: AgentId,
    user_id: crate::auth::UserId,
) -> Result<(), SlackError> {
    let prompt = Prompt::try_from(strip_for_prompt(event, &workspace.bot_user_id))?;
    let idempotency_key = IdempotencyKey::try_from(format!(
        "slack:{team}:{channel}:{ts}",
        team = event.team_id.as_str(),
        channel = event.channel_id.as_str(),
        ts = event.event_ts.as_str(),
    ))?;
    let req = NewPromptRequest::normal(
        existing.map(|m| m.session_id),
        Participant::human(),
        receiver_agent_id,
        None,
        prompt,
        idempotency_key,
        workspace.org_id,
        user_id,
    );
    let outcome = deps.queue.enqueue_for_user(user_id, req).await?;
    if let EnqueueOutcome::Inserted {
        request_id,
        session,
        ..
    } = outcome
    {
        // For a continuing thread, the stored root_request_id is the
        // original first request (bind_root is ON CONFLICT DO NOTHING).
        // The PgThreadStream notifies on that original root, so the
        // pump must subscribe to it — not to this turn's request_id.
        let pump_root = existing.map_or(request_id, |m| m.root_request_id);
        deps.threads
            .bind_root(
                workspace.org_id,
                &event.team_id,
                &event.channel_id,
                anchor,
                session,
                pump_root,
            )
            .await?;
        deps.stream_pump
            .attach(AttachRequest {
                root: pump_root,
                org_id: workspace.org_id,
                team_id: event.team_id.clone(),
                channel_id: event.channel_id.clone(),
                thread_ts: anchor.clone(),
                slack_user_id: event.user_id.clone(),
                session_id: session,
            })
            .await;
        info!(
            relay.session.id = %session.as_uuid(),
            relay.request.id = %request_id.as_uuid(),
            event = "slack.bridge.enqueued",
        );
    }
    Ok(())
}

/// Read the agent participant of `session`. Mirrors `prompts.rs:126`.
async fn session_agent_participant(
    deps: &BridgeDeps,
    session: crate::session::SessionId,
) -> Result<AgentId, SlackError> {
    let (a, b) = deps
        .sessions
        .participants(session)
        .await
        .map_err(|e| SlackError::Internal(format!("session.participants: {e}")))?;
    match (a.kind(), b.kind()) {
        (ParticipantKind::Agent, _) => a
            .agent_id()
            .ok_or_else(|| SlackError::Internal("agent kind without id".to_owned())),
        (_, ParticipantKind::Agent) => b
            .agent_id()
            .ok_or_else(|| SlackError::Internal("agent kind without id".to_owned())),
        _ => Err(SlackError::Internal(
            "human-rooted session without an agent participant".to_owned(),
        )),
    }
}

/// Resolve `@AgentName` (if any) against the org. Falls back to the
/// org's default agent on miss.
async fn resolve_mention_or_default(
    deps: &BridgeDeps,
    event: &InboundEvent,
    bot: &SlackUserId,
    org_id: OrgId,
) -> Result<AgentId, SlackError> {
    let parsed = mention::parse(&event.text, bot);
    if let Some(name_raw) = parsed.agent_name
        && let Ok(name) = AgentName::try_from(name_raw.as_str())
    {
        match deps.agents.read_by_name_for_org(org_id, &name).await {
            Ok(record) => return Ok(record.id),
            Err(crate::agents::AgentStoreError::NameNotFound(_)) => {
                warn!(
                    relay.agent.name = %name_raw,
                    event = "slack.bridge.agent_not_found_falling_back_to_default",
                );
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(deps.agents.default_id_for(org_id).await?)
}

/// Strip the bot mention from the text and return the user's
/// remaining prompt content. If the message is just `@RelayBot` with
/// no follow-up, send an empty prompt — the prompt newtype rejects
/// empties so the caller will error out with a parse error and the
/// webhook handler logs + drops.
fn strip_for_prompt(event: &InboundEvent, bot: &SlackUserId) -> String {
    let parsed = mention::parse(&event.text, bot);
    parsed.stripped
}

/// Routing payload for a `/relay` slash command submission. Constructed
/// by the interactions handler after validating the `view_submission`
/// envelope; the bridge takes it from here.
#[derive(Debug, Clone)]
pub struct SlashCommandSubmit {
    pub team_id: SlackTeamId,
    pub channel_id: SlackChannelId,
    pub slack_user_id: SlackUserId,
    /// Slack `user_name` (e.g. `tomkapa`) — the human-readable handle,
    /// captured from the slash command form. Used as the `username`
    /// override on the synthetic prompt-mirror post so the message
    /// reads as that user rather than the raw `U…` id. (The Slack
    /// "APP" badge is unavoidable when posting via a bot token.)
    pub slack_user_name: String,
    pub agent_id: AgentId,
    pub prompt: Prompt,
    /// Stable per-modal identifier (Slack's `view.id`). Folded into the
    /// idempotency key so a re-submission of the same modal collapses
    /// into a single prompt at the queue.
    pub view_id: String,
}

/// Drive the slash command path.
///
/// Post the synthetic prompt mirror as a channel-top-level message
/// (becomes the thread root), enqueue the agent prompt, and bind the
/// resulting session to `(team, channel, thread_ts)` so future replies
/// via stickiness and the outbound stream pump land in the right thread.
///
/// Defence-in-depth: the caller is expected to have already re-checked
/// that `submit.agent_id` belongs to the workspace's `org_id`; this
/// function asserts the invariant via the agent lookup (`read` returns
/// only when the row exists, and the lookup is org-scoped through the
/// workspace).
pub async fn enqueue_from_slash(
    deps: &BridgeDeps,
    submit: SlashCommandSubmit,
) -> Result<(), SlackError> {
    let workspace = deps.workspaces.read_by_team(&submit.team_id).await?;
    // Identity lookup and agent re-read are independent — both only
    // need `workspace` to validate org alignment. Joining shaves one
    // serial round-trip off the slash command's 3 s ack window.
    let (identity, agent) = tokio::join!(
        deps.identities
            .lookup(&submit.team_id, &submit.slack_user_id),
        deps.agents.read(submit.agent_id),
    );
    let user_id = match identity? {
        Some(linked) => {
            assert_eq!(linked.org_id, workspace.org_id);
            linked.user_id
        }
        None => workspace.installed_by_user_id,
    };
    // Defence-in-depth: re-validate that the chosen agent belongs to
    // the workspace's org. The modal options carry agent ids the
    // client could in principle forge.
    let agent = agent?;
    if agent.org_id != workspace.org_id {
        return Err(SlackError::AgentNotFound(agent.name.as_str().to_owned()));
    }

    // Try the queue idempotency gate BEFORE posting the synthetic
    // prompt mirror to Slack. Slack retries `view_submission` on
    // upstream 5xx / timeouts; if we posted first, the retry would
    // produce duplicate top-level channel messages even though the
    // queue would collapse the duplicate enqueue.
    let idempotency_key = IdempotencyKey::try_from(format!(
        "slack-slash:{team}:{channel}:{view}",
        team = submit.team_id.as_str(),
        channel = submit.channel_id.as_str(),
        view = submit.view_id,
    ))?;
    let prompt_text = submit.prompt.as_str().to_owned();
    let req = NewPromptRequest::normal(
        None,
        Participant::human(),
        submit.agent_id,
        None,
        submit.prompt,
        idempotency_key,
        workspace.org_id,
        user_id,
    );
    let outcome = deps.queue.enqueue_for_user(user_id, req).await?;
    let request_id = outcome.request_id();
    let session = outcome.session();

    // Idempotency gate: use the *thread binding* (not the queue
    // outcome) as the signal that a previous attempt finished. An
    // `EnqueueOutcome::Existing` whose binding is missing means the
    // earlier attempt died between enqueue and bind — Slack retried
    // the `view_submission`, we hit the idempotency short-circuit at
    // the queue, but the agent's reply has no Slack thread to land
    // in. Re-posting the mirror and binding here closes that hole.
    //
    // Concurrent Slack retries can both pass this check before either
    // calls `bind_root`. The double-post race is bounded: Slack
    // retries are >=1 s apart and the mirror post + bind typically
    // complete in well under that. `bind_root` is ON CONFLICT DO
    // NOTHING, so the second `attach` will still land on the
    // correct (first-write-wins) root.
    if deps.threads.lookup_by_session(session).await?.is_some() {
        info!(event = "slack.bridge.slash_already_bound");
        return Ok(());
    }

    let prompt_post = post_prompt_mirror(
        deps,
        &workspace,
        &submit.channel_id,
        &submit.slack_user_id,
        &submit.slack_user_name,
        &prompt_text,
        &agent.name,
    )
    .await?;
    let anchor = SlackThreadTs::try_from(prompt_post.as_str())?;

    deps.threads
        .bind_root(
            workspace.org_id,
            &submit.team_id,
            &submit.channel_id,
            &anchor,
            session,
            request_id,
        )
        .await?;
    deps.stream_pump
        .attach(AttachRequest {
            root: request_id,
            org_id: workspace.org_id,
            team_id: submit.team_id.clone(),
            channel_id: submit.channel_id.clone(),
            thread_ts: anchor,
            slack_user_id: submit.slack_user_id.clone(),
            session_id: session,
        })
        .await;
    info!(
        relay.session.id = %session.as_uuid(),
        relay.request.id = %request_id.as_uuid(),
        relay.slack.source = "slash",
        event = "slack.bridge.enqueued",
    );
    Ok(())
}

/// Post the synthetic prompt-mirror message as a channel-top-level
/// post and return the Slack `ts` it lands under.
///
/// Body is a Block Kit envelope so the parent message renders the
/// prompt *and* a small "→ @agent" attribution line — without it the
/// reader can't tell which agent owns the thread without expanding it.
///
/// `users.info` is best-effort: a failure falls back to the slash
/// command's `user_name` form value and the app-default avatar. The
/// post still lands; only the attribution looks worse, which beats
/// failing the prompt over a profile lookup hiccup.
const SLACK_USERS_INFO_URL: &str = "https://slack.com/api/users.info";

async fn post_prompt_mirror(
    deps: &BridgeDeps,
    workspace: &crate::slack::workspace::WorkspaceWithToken,
    channel_id: &SlackChannelId,
    slack_user_id: &SlackUserId,
    slack_user_name: &str,
    prompt_text: &str,
    agent_name: &AgentName,
) -> Result<SlackTs, SlackError> {
    let profile =
        fetch_user_profile(&deps.http, &workspace.bot_token, slack_user_id.as_str()).await;
    let display_name = profile
        .as_ref()
        .and_then(|p| p.display_name.clone())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| slack_user_name.to_owned());
    let icon_url = profile.as_ref().and_then(|p| p.image_url.clone());
    let blocks = build_prompt_mirror_blocks(prompt_text, agent_name);
    deps.poster
        .post(super::poster::PostRequest {
            token: workspace.bot_token.clone(),
            channel: channel_id.clone(),
            thread_ts: None,
            body: super::poster::PostBody::Blocks {
                fallback_text: prompt_text.to_owned(),
                blocks,
            },
            // The `APP` badge next to the username is unavoidable on
            // bot-token posts. Using the sender's workspace display
            // name + avatar makes the mirror read as them at a glance.
            username: display_name,
            icon_url,
        })
        .await
}

/// Subset of a Slack `users.info` response we route on.
///
/// `display_name` is the user's customised workspace handle (`tomkapa`);
/// falls back through `real_name` to `name` when unset. `image_url` is
/// the `image_192` avatar URL, which Slack populates whenever the user
/// has any avatar at all (the smaller / larger variants would be
/// allocated unused).
#[derive(Debug)]
struct SlackUserProfile {
    display_name: Option<String>,
    image_url: Option<String>,
}

#[derive(serde::Deserialize)]
struct UsersInfoEnvelope {
    ok: bool,
    #[serde(default)]
    user: Option<UsersInfoUser>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(serde::Deserialize)]
struct UsersInfoUser {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    real_name: Option<String>,
    #[serde(default)]
    profile: Option<UsersInfoProfile>,
}

#[derive(Default, serde::Deserialize)]
struct UsersInfoProfile {
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    display_name_normalized: Option<String>,
    #[serde(default)]
    real_name: Option<String>,
    #[serde(default)]
    image_192: Option<String>,
}

/// Resolve the Slack user's workspace display name + avatar via
/// `users.info`. Best-effort: a timeout, transport error, or
/// `{ok: false}` body returns `None` and the caller falls back to the
/// slash-command form fields. One tight timeout covers send + body
/// together because the call runs inside Slack's 3 s `view_submission`
/// ack window — a generous Slack-side budget would burn the ack.
async fn fetch_user_profile(
    http: &Client,
    token: &SlackBotToken,
    user_id: &str,
) -> Option<SlackUserProfile> {
    let send = http
        .get(SLACK_USERS_INFO_URL)
        .bearer_auth(token.expose())
        .query(&[("user", user_id)])
        .send();
    let resp = match tokio::time::timeout(SLACK_USERS_INFO_TIMEOUT, send).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            warn!(error = %e, event = "slack.users_info.transport_failed");
            return None;
        }
        Err(_) => {
            warn!(event = "slack.users_info.timeout");
            return None;
        }
    };
    if !resp.status().is_success() {
        warn!(
            status = resp.status().as_u16(),
            event = "slack.users_info.http_error"
        );
        return None;
    }
    let parsed: UsersInfoEnvelope =
        match tokio::time::timeout(SLACK_USERS_INFO_TIMEOUT, resp.json()).await {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                warn!(error = %e, event = "slack.users_info.decode_failed");
                return None;
            }
            Err(_) => {
                warn!(event = "slack.users_info.body_timeout");
                return None;
            }
        };
    if !parsed.ok {
        warn!(
            error = parsed.error.as_deref().unwrap_or("unknown"),
            event = "slack.users_info.api_error",
        );
        return None;
    }
    let user = parsed.user?;
    let profile = user.profile.unwrap_or_default();
    let display_name = profile
        .display_name
        .filter(|s| !s.is_empty())
        .or_else(|| profile.display_name_normalized.filter(|s| !s.is_empty()))
        .or_else(|| profile.real_name.filter(|s| !s.is_empty()))
        .or_else(|| user.real_name.filter(|s| !s.is_empty()))
        .or_else(|| user.name.filter(|s| !s.is_empty()));
    Some(SlackUserProfile {
        display_name,
        image_url: profile.image_192,
    })
}

/// Build the Block Kit body for the slash-command prompt mirror.
///
/// Two blocks: a section with the user's prompt, then a small context
/// line attributing the routed agent. The context block is the
/// at-a-glance signal of which agent owns the thread — without it the
/// reader has to expand the thread to find out.
fn build_prompt_mirror_blocks(prompt: &str, agent_name: &AgentName) -> Value {
    json!([
        {
            "type": "section",
            "text": { "type": "mrkdwn", "text": prompt },
        },
        {
            "type": "context",
            "elements": [
                {
                    "type": "mrkdwn",
                    "text": format!("→ *@{}*", agent_name.as_str()),
                },
            ],
        },
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_mirror_renders_prompt_and_agent_context() {
        let agent = AgentName::try_from("recruiter").expect("valid name");
        let blocks = build_prompt_mirror_blocks("help me recruit", &agent);
        let arr = blocks.as_array().expect("array");
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["type"], "section");
        assert_eq!(arr[0]["text"]["text"], "help me recruit");
        assert_eq!(arr[1]["type"], "context");
        let ctx_text = arr[1]["elements"][0]["text"].as_str().expect("str");
        assert!(ctx_text.contains("recruiter"), "got: {ctx_text}");
        assert!(ctx_text.contains('@'), "should @-prefix agent name");
    }
}
