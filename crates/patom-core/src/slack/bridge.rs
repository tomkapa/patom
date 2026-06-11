//! Inbound bridge: Slack `app_mention` event → Patom prompt enqueue.
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
//! 3. Resolve identity: `slack_identities` lookup → linked user. A miss
//!    drops the event and posts an ephemeral nudge to run `/patom` to
//!    connect the account — there is no installer fallback (issue #41).
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
use sqlx::PgPool;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, info, info_span, warn};

use crate::agents::{AgentId, AgentName, SharedAgentStore};
use crate::auth::{Caller, OrgId, UserId, run_privileged};
use crate::colleagues::ColleagueId;
use crate::provider::{ChatMessage, UserContent};
use crate::runtime::{IdempotencyKey, NewTrigger, RequestKindPayload, SharedPromptQueue};
use crate::threads::{MessageKind, NewMessage, SharedThreadStore, ThreadId};
use crate::types::Prompt;

use super::channel_map::SharedSlackChannelStore;
use super::error::SlackError;
use super::identity::SharedSlackIdentityStore;
use super::limits::SLACK_USERS_INFO_TIMEOUT;
use super::mention;
use super::poster::SharedSlackPoster;
use super::stream_pump::{AttachRequest, SharedStreamPumpHandle};
use super::thread_map::SharedSlackThreadStore;
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
    /// Patom thread feed — the bridge creates the thread, appends the human's
    /// posted row, and resolves the agent's participation (the thread model's
    /// replacement for the old pair-session store).
    pub thread_store: SharedThreadStore,
    /// Colleague directory — resolves linked-human `(org_id, user_id)`
    /// to a colleague id so Slack events enqueue colleague-backed senders.
    pub colleagues: crate::colleagues::SharedColleagueStore,
    pub workspaces: SharedSlackWorkspaceStore,
    pub identities: SharedSlackIdentityStore,
    /// Slack-channel ↔ Patom-channel mapping (`slack_channels`). Lets a
    /// Slack-rooted conversation be a multi-participant channel thread
    /// (issue #41) and ensures the acting human is a channel member.
    pub channels_map: SharedSlackChannelStore,
    /// Slack-thread ↔ Patom-thread binding (`slack_threads`).
    pub threads: SharedSlackThreadStore,
    pub poster: SharedSlackPoster,
    pub stream_pump: SharedStreamPumpHandle,
    /// Direct pool handle for the trigger-idempotency pre-check (a Slack
    /// re-delivery of the same `event_ts` must not double-post the human row).
    pub pool: PgPool,
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
///
/// Thread model: a Slack `(team, channel, thread_ts)` triple maps to one Patom
/// thread. A first mention creates a fresh Patom DM thread (created by the
/// linked human) and binds it; a reply continues it. Either way the human's
/// `posted` row is appended @tagging the chosen agent, the agent's
/// participation is resolved, and a fresh-DAG trigger is enqueued — then the
/// outbound pump is attached so the agent's reply lands back in this Slack
/// thread.
pub async fn process_event(deps: &BridgeDeps, event: InboundEvent) -> Result<(), SlackError> {
    let workspace = deps.workspaces.read_by_team(&event.team_id).await?;
    assert_eq!(workspace.team_id.as_str(), event.team_id.as_str());
    if workspace.bot_user_id.as_str() == event.user_id.as_str() {
        return Ok(());
    }
    let Some(user_id) = resolve_user_id(deps, &event, &workspace).await? else {
        // Unlinked Slack user — there is no installer fallback in Phase 2.
        // Nudge them to the `/patom` onboarding entry and drop the event.
        nudge_unlinked(deps, &workspace, &event).await;
        return Ok(());
    };
    let org_id = workspace.org_id;
    let caller = Caller::new(user_id, org_id);
    // Mirror the Slack channel to a Patom channel and make the acting human
    // a member — on every event, so a second human joining an existing
    // Slack thread can read + post (issue #41). Channel threads replace the
    // old two-party DM threads.
    let channel_id = deps
        .channels_map
        .ensure_channel(org_id, &event.team_id, &event.channel_id, user_id)
        .await?;
    let anchor = match event.thread_ts.clone() {
        Some(t) => t,
        None => SlackThreadTs::try_from(event.event_ts.as_str())?,
    };
    let existing = deps
        .threads
        .lookup_by_thread(&event.team_id, &event.channel_id, &anchor)
        .await?;

    let human_colleague = resolve_human_colleague(deps, org_id, user_id).await?;

    // Resolve the target Patom thread + the agent the message addresses.
    let (thread_id, agent_id) = match (existing, event.source) {
        (Some(mapping), InboundSource::AppMention) => (
            mapping.thread_id,
            resolve_mention_or_recruiter(deps, &event, &workspace.bot_user_id, org_id).await?,
        ),
        (Some(mapping), InboundSource::ThreadMessage) => {
            // Plain reply in a bound thread — route to the agent the
            // conversation is with (a Slack thread maps to one Patom thread).
            let agent = deps
                .thread_store
                .last_agent(mapping.thread_id)
                .await
                .map_err(|e| SlackError::Internal(format!("last_agent: {e}")))?
                .ok_or_else(|| {
                    SlackError::Internal("bound slack thread has no agent participant".to_owned())
                })?;
            (mapping.thread_id, agent)
        }
        // A plain in-thread message without a binding is dropped — we never
        // start a fresh conversation from random channel chatter; that's the
        // mention path's job.
        (None, InboundSource::ThreadMessage) => {
            info!(event = "slack.bridge.thread_message_unbound_dropped");
            return Ok(());
        }
        (None, InboundSource::AppMention) => {
            let agent =
                resolve_mention_or_recruiter(deps, &event, &workspace.bot_user_id, org_id).await?;
            // New Slack-originated conversation → a fresh Patom *channel*
            // thread (multi-human, multi-agent), bound to this Slack thread.
            // The mentioned agent joins via `resolve_participation` below.
            let thread = deps
                .thread_store
                .create_thread(&caller, Some(channel_id), None, human_colleague, None)
                .await
                .map_err(|e| SlackError::Internal(format!("create thread: {e}")))?;
            deps.threads
                .bind(org_id, &event.team_id, &event.channel_id, &anchor, thread)
                .await?;
            (thread, agent)
        }
    };

    let idempotency_key = IdempotencyKey::try_from(format!(
        "slack:{team}:{channel}:{ts}",
        team = event.team_id.as_str(),
        channel = event.channel_id.as_str(),
        ts = event.event_ts.as_str(),
    ))?;
    let prompt = Prompt::try_from(strip_for_prompt(&event, &workspace.bot_user_id))?;
    submit_to_thread(
        deps,
        SlackSubmit {
            caller,
            org_id,
            user_id,
            human_colleague,
            agent_id,
            thread_id,
            prompt,
            idempotency_key,
        },
        AttachRequest {
            thread_id,
            team_id: event.team_id.clone(),
            channel_id: event.channel_id.clone(),
            thread_ts: anchor,
            slack_user_id: event.user_id.clone(),
        },
        "mention",
    )
    .await
}

/// Inputs to [`submit_to_thread`] — the shared "append human post + enqueue a
/// fresh-DAG trigger" sequence used by both the mention and slash paths.
struct SlackSubmit {
    caller: Caller,
    org_id: OrgId,
    user_id: UserId,
    human_colleague: ColleagueId,
    agent_id: AgentId,
    thread_id: ThreadId,
    prompt: Prompt,
    idempotency_key: IdempotencyKey,
}

/// Append the human's posted row @tagging the agent, resolve the agent's
/// participation, enqueue a fresh-DAG trigger, and attach the outbound pump.
///
/// Idempotent: a Slack re-delivery of the same `event_ts` (or slash `view_id`)
/// re-derives the same `idempotency_key`; if a trigger already exists we skip
/// the append/enqueue and only (re)attach the pump (cheap, deduped by the
/// supervisor) so a binding that survived a restart still streams.
async fn submit_to_thread(
    deps: &BridgeDeps,
    submit: SlackSubmit,
    attach: AttachRequest,
    source: &'static str,
) -> Result<(), SlackError> {
    if trigger_exists(&deps.pool, submit.org_id, &submit.idempotency_key).await? {
        info!(
            patom.slack.source = source,
            event = "slack.bridge.duplicate_event_dropped"
        );
        deps.stream_pump.attach(attach).await;
        return Ok(());
    }

    let agent_colleague = deps
        .colleagues
        .resolve_agent(submit.org_id, submit.agent_id)
        .await
        .map_err(|e| SlackError::Internal(format!("resolve agent colleague: {e}")))?;

    let trigger_msg = deps
        .thread_store
        .append(
            &submit.caller,
            submit.thread_id,
            NewMessage {
                kind: MessageKind::Posted,
                sender: Some(submit.human_colleague),
                owner_agent_id: None,
                receiver: Some(agent_colleague),
                body: ChatMessage::User(vec![UserContent::Text(submit.prompt.as_str().to_owned())]),
                request_id: None,
                idempotency_key: None,
            },
        )
        .await
        .map_err(|e| SlackError::Internal(format!("append: {e}")))?;

    let state_id = deps
        .thread_store
        .resolve_participation(&submit.caller, submit.thread_id, submit.agent_id)
        .await
        .map_err(|e| SlackError::Internal(format!("resolve participation: {e}")))?;

    let request_id = deps
        .queue
        .enqueue_trigger(NewTrigger {
            org_id: submit.org_id,
            acting_user_id: submit.user_id,
            thread_id: Some(submit.thread_id),
            state_id: Some(state_id),
            background_turn_id: None,
            sender_colleague_id: submit.human_colleague,
            receiver_agent_id: submit.agent_id,
            root_request_id: None,
            trigger_message_id: Some(trigger_msg),
            idempotency_key: submit.idempotency_key,
            kind_payload: RequestKindPayload::Normal {},
        })
        .await?;

    deps.stream_pump.attach(attach).await;
    info!(
        patom.thread.id = %submit.thread_id.as_uuid(),
        patom.request.id = %request_id.as_uuid(),
        patom.slack.source = source,
        event = "slack.bridge.enqueued",
    );
    Ok(())
}

/// Whether a trigger with `idempotency_key` already exists for `org` — the
/// Slack-retry dedup guard. Privileged existence check (the bridge is
/// workspace-keyed infra; the key is fully qualified by org).
async fn trigger_exists(
    pool: &PgPool,
    org: OrgId,
    key: &IdempotencyKey,
) -> Result<bool, SlackError> {
    run_privileged::<bool, SlackError>(pool, async |tx| {
        Ok(sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM prompt_requests \
             WHERE org_id = $1 AND idempotency_key = $2)",
        )
        .bind(org)
        .bind(key.as_str())
        .fetch_one(&mut **tx)
        .await?)
    })
    .await
}

/// Resolve the linked human's colleague id within `org`.
async fn resolve_human_colleague(
    deps: &BridgeDeps,
    org: OrgId,
    user_id: UserId,
) -> Result<ColleagueId, SlackError> {
    deps.colleagues
        .resolve_user(org, user_id)
        .await
        .map_err(|e| SlackError::Internal(format!("resolve human colleague: {e}")))
}

/// Slack user → Patom user. `Ok(None)` for an unlinked Slack user: Phase
/// 2 has no installer fallback, so the caller nudges them to `/patom`
/// rather than silently bridging as the installer.
async fn resolve_user_id(
    deps: &BridgeDeps,
    event: &InboundEvent,
    workspace: &crate::slack::workspace::WorkspaceWithToken,
) -> Result<Option<crate::auth::UserId>, SlackError> {
    let linked = deps
        .identities
        .lookup(&event.team_id, &event.user_id)
        .await?;
    let Some(linked) = linked else {
        return Ok(None);
    };
    assert_eq!(
        linked.org_id, workspace.org_id,
        "invariant: slack_identities + slack_workspaces FK keeps these aligned"
    );
    Ok(Some(linked.user_id))
}

/// Post an ephemeral "connect your account" nudge to an unlinked Slack
/// user, routing them to the `/patom` onboarding entry. Best-effort: a
/// failed post is logged, not propagated — the event is dropped either
/// way. Ephemeral, so it never clutters the channel for anyone else.
async fn nudge_unlinked(
    deps: &BridgeDeps,
    workspace: &crate::slack::workspace::WorkspaceWithToken,
    event: &InboundEvent,
) {
    let req = super::poster::PostRequest {
        token: workspace.bot_token.clone(),
        channel: event.channel_id.clone(),
        thread_ts: event.thread_ts.clone(),
        body: super::poster::PostBody::Text(
            "👋 To chat with Patom agents here, connect your account first — \
             run `/patom` in this channel."
                .to_owned(),
        ),
        username: "Patom".to_owned(),
        icon_url: None,
        ephemeral_to: Some(event.user_id.clone()),
    };
    match deps.poster.post(req).await {
        Ok(_) => info!(event = "slack.bridge.unlinked_nudged"),
        Err(e) => warn!(error = ?e, event = "slack.bridge.nudge_post_failed"),
    }
}

/// Resolve `@AgentName` (if any) against the org. Falls back to the org's
/// preset recruiter on miss — there is no "default agent" at runtime; the
/// recruiter is simply the agent every workspace is seeded with. If the
/// operator renamed or deleted it, the mention fails loudly with
/// `NameNotFound` and the webhook handler logs + drops.
async fn resolve_mention_or_recruiter(
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
                    patom.agent.name = %name_raw,
                    event = "slack.bridge.agent_not_found_falling_back_to_recruiter",
                );
            }
            Err(e) => return Err(e.into()),
        }
    }
    let recruiter = AgentName::try_from(crate::app::RECRUITER_AGENT_NAME)
        .expect("invariant: recruiter preset name is a valid AgentName");
    Ok(deps
        .agents
        .read_by_name_for_org(org_id, &recruiter)
        .await?
        .id)
}

/// Strip the bot mention from the text and return the user's
/// remaining prompt content. If the message is just `@PatomBot` with
/// no follow-up, send an empty prompt — the prompt newtype rejects
/// empties so the caller will error out with a parse error and the
/// webhook handler logs + drops.
fn strip_for_prompt(event: &InboundEvent, bot: &SlackUserId) -> String {
    let parsed = mention::parse(&event.text, bot);
    parsed.stripped
}

/// Routing payload for a `/patom` slash command submission. Constructed
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
    /// `Some` when the compose modal was opened from a **message
    /// shortcut** inside an existing Slack thread — the message's
    /// `thread_ts`. The prompt mirror then posts into that thread and
    /// continues (or creates) its binding instead of starting a new
    /// top-level thread. `None` for the `/patom` slash entry.
    pub thread_ts: Option<SlackThreadTs>,
}

/// Drive the slash command path.
///
/// Post the synthetic prompt mirror as a channel-top-level message (the Slack
/// thread root), create a Patom thread bound to `(team, channel, thread_ts)`,
/// append the human's posted row @tagging the agent, and enqueue a fresh-DAG
/// trigger — then attach the outbound pump.
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
        // The slash gate only opens the modal for a linked user, so this
        // is an unlink-during-modal race. Reject rather than mis-attribute
        // as the installer (no Phase-2 fallback).
        None => return Err(SlackError::IdentityNotLinked(submit.slack_user_id.clone())),
    };
    // Defence-in-depth: re-validate that the chosen agent belongs to
    // the workspace's org. The modal options carry agent ids the
    // client could in principle forge.
    let agent = agent?;
    if agent.org_id != workspace.org_id {
        return Err(SlackError::AgentNotFound(agent.name.as_str().to_owned()));
    }
    let org_id = workspace.org_id;

    // Idempotency gate BEFORE posting the synthetic prompt mirror. Slack
    // retries `view_submission` on upstream 5xx / timeouts; posting first
    // would produce duplicate top-level channel messages.
    let idempotency_key = IdempotencyKey::try_from(format!(
        "slack-slash:{team}:{channel}:{view}",
        team = submit.team_id.as_str(),
        channel = submit.channel_id.as_str(),
        view = submit.view_id,
    ))?;
    if trigger_exists(&deps.pool, org_id, &idempotency_key).await? {
        info!(event = "slack.bridge.slash_already_submitted");
        return Ok(());
    }

    let prompt_text = submit.prompt.as_str().to_owned();
    let caller = Caller::new(user_id, org_id);
    let human_colleague = resolve_human_colleague(deps, org_id, user_id).await?;

    // Mirror the Slack channel + add the human, then resolve the target
    // thread (a fresh top-level one for `/patom`, or the existing thread
    // for a message shortcut). Extracted to keep this fn within the
    // length ceiling (§4).
    let channel_id = deps
        .channels_map
        .ensure_channel(org_id, &submit.team_id, &submit.channel_id, user_id)
        .await?;
    let (thread_id, anchor) = resolve_slash_thread(
        deps,
        &workspace,
        &submit,
        channel_id,
        &caller,
        human_colleague,
        &prompt_text,
        &agent.name,
    )
    .await?;

    submit_to_thread(
        deps,
        SlackSubmit {
            caller,
            org_id,
            user_id,
            human_colleague,
            agent_id: submit.agent_id,
            thread_id,
            prompt: submit.prompt,
            idempotency_key,
        },
        AttachRequest {
            thread_id,
            team_id: submit.team_id.clone(),
            channel_id: submit.channel_id.clone(),
            thread_ts: anchor,
            slack_user_id: submit.slack_user_id.clone(),
        },
        "slash",
    )
    .await
}

/// Resolve the Patom thread + Slack anchor for a slash / shortcut submit,
/// posting the prompt mirror in the right place.
///
/// - **`/patom` (no `thread_ts`):** post the mirror top-level; its `ts`
///   anchors a fresh channel thread.
/// - **message shortcut (`thread_ts` set):** post the mirror *into* that
///   thread and continue its existing binding, or create + bind a channel
///   thread anchored there if this is the first Patom touch on it.
#[allow(clippy::too_many_arguments)]
async fn resolve_slash_thread(
    deps: &BridgeDeps,
    workspace: &crate::slack::workspace::WorkspaceWithToken,
    submit: &SlashCommandSubmit,
    channel_id: crate::channels::ChannelId,
    caller: &Caller,
    human_colleague: ColleagueId,
    prompt_text: &str,
    agent_name: &AgentName,
) -> Result<(ThreadId, SlackThreadTs), SlackError> {
    let org_id = caller.org_id;
    if let Some(existing) = submit.thread_ts.clone() {
        let thread_id = if let Some(mapping) = deps
            .threads
            .lookup_by_thread(&submit.team_id, &submit.channel_id, &existing)
            .await?
        {
            mapping.thread_id
        } else {
            let t = deps
                .thread_store
                .create_thread(caller, Some(channel_id), None, human_colleague, None)
                .await
                .map_err(|e| SlackError::Internal(format!("create thread: {e}")))?;
            deps.threads
                .bind(org_id, &submit.team_id, &submit.channel_id, &existing, t)
                .await?;
            t
        };
        post_prompt_mirror(
            deps,
            workspace,
            &submit.channel_id,
            &submit.slack_user_id,
            &submit.slack_user_name,
            prompt_text,
            agent_name,
            Some(&existing),
        )
        .await?;
        return Ok((thread_id, existing));
    }
    let prompt_post = post_prompt_mirror(
        deps,
        workspace,
        &submit.channel_id,
        &submit.slack_user_id,
        &submit.slack_user_name,
        prompt_text,
        agent_name,
        None,
    )
    .await?;
    let anchor = SlackThreadTs::try_from(prompt_post.as_str())?;
    let thread_id = deps
        .thread_store
        .create_thread(caller, Some(channel_id), None, human_colleague, None)
        .await
        .map_err(|e| SlackError::Internal(format!("create thread: {e}")))?;
    deps.threads
        .bind(
            org_id,
            &submit.team_id,
            &submit.channel_id,
            &anchor,
            thread_id,
        )
        .await?;
    Ok((thread_id, anchor))
}

/// Post the synthetic prompt-mirror message and return the Slack `ts` it
/// lands under. `reply_in_thread` is `Some(anchor)` to post the mirror as
/// a reply inside an existing Slack thread (message-shortcut path), or
/// `None` to post it as a channel-top-level message (slash path).
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

#[allow(clippy::too_many_arguments)]
async fn post_prompt_mirror(
    deps: &BridgeDeps,
    workspace: &crate::slack::workspace::WorkspaceWithToken,
    channel_id: &SlackChannelId,
    slack_user_id: &SlackUserId,
    slack_user_name: &str,
    prompt_text: &str,
    agent_name: &AgentName,
    reply_in_thread: Option<&SlackThreadTs>,
) -> Result<SlackTs, SlackError> {
    let profile =
        fetch_user_profile(&deps.http, &workspace.bot_token, slack_user_id.as_str()).await;
    let real_slack_name = profile
        .as_ref()
        .and_then(|p| p.display_name.clone())
        .filter(|s| !s.is_empty());
    // Opportunistically refresh the stored per-platform name — free, since
    // we already fetched the profile for the mirror username. Best-effort
    // and keyed by (team, slack_user); never touches the Patom user.
    if let Some(name) = real_slack_name.as_deref()
        && let Err(e) = deps
            .identities
            .set_display_name(&workspace.team_id, slack_user_id, name)
            .await
    {
        warn!(error = ?e, event = "slack.bridge.display_name_refresh_failed");
    }
    let display_name = real_slack_name.unwrap_or_else(|| slack_user_name.to_owned());
    let icon_url = profile.as_ref().and_then(|p| p.image_url.clone());
    let blocks = build_prompt_mirror_blocks(prompt_text, agent_name);
    deps.poster
        .post(super::poster::PostRequest {
            token: workspace.bot_token.clone(),
            channel: channel_id.clone(),
            thread_ts: reply_in_thread.cloned(),
            body: super::poster::PostBody::Blocks {
                fallback_text: prompt_text.to_owned(),
                blocks,
            },
            // The `APP` badge next to the username is unavoidable on
            // bot-token posts. Using the sender's workspace display
            // name + avatar makes the mirror read as them at a glance.
            username: display_name,
            icon_url,
            ephemeral_to: None,
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

/// Fetch just a Slack user's workspace display name via `users.info`.
/// Used by the identity-link paths to store the per-platform name on
/// `slack_identities` (issue #41). Best-effort: `None` on any failure or
/// an empty handle.
pub(crate) async fn fetch_slack_display_name(
    http: &Client,
    token: &SlackBotToken,
    slack_user_id: &str,
) -> Option<String> {
    fetch_user_profile(http, token, slack_user_id)
        .await
        .and_then(|p| p.display_name)
        .filter(|s| !s.is_empty())
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
