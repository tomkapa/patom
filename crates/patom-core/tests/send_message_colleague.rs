//! Stage 7: `send_message` addresses any colleague by id.
//!
//! Drives [`SendMessageTool`] directly (workers shut down) so each case asserts
//! the resolution + delivery decision in isolation:
//!
//! - an **agent** colleague id → enqueues a turn (delivery "queued", a request
//!   id), exactly as the name-sugar path does;
//! - a **human** colleague id → notifies over the existing human-delivery seam
//!   (delivery "published", **no** `prompt_requests` row → null request id);
//! - **self**, an **unknown** id, and a **cross-org** id are all rejected.
//!
//! Authority is untouched (locked decision #4): every case still runs under the
//! agent's acting user; only *addressing* moves onto the colleague axis.

#![allow(clippy::expect_used)]

use std::sync::Arc;

use patom::agents::{
    AgentDescription, AgentId, AgentName, AgentSystemPrompt, AllowedMcpTools, NewAgent,
    SharedAgentStore,
};
use patom::auth::{OrgId, UserId};
use patom::clock::SystemClock;
use patom::colleagues::{ColleagueId, SharedColleagueStore};
use patom::runtime::{
    IdempotencyKey, NewPromptRequest, PromptRequestId, RequestKindPayload, SharedDagBudget,
    SharedPromptQueue, SharedResponseSink,
};
use patom::session::{SessionId, SharedSessionStore};
use patom::tools::system::SendMessageTool;
use patom::tools::{Tool, ToolCallContext};
use patom::types::{Participant, Prompt};
use serde_json::{Value, json};
use sqlx::PgPool;

mod common;
use common::harness::{ScriptedProvider, build_harness};

/// The harness collaborators a direct `send_message` test needs, captured by
/// value so the worker pool can be shut down (its handle is consumed) while the
/// stores stay live.
struct Setup {
    pool: PgPool,
    sessions: SharedSessionStore,
    queue: SharedPromptQueue,
    dag: SharedDagBudget,
    sink: SharedResponseSink,
    agent_id: AgentId,
    agent_colleague_id: ColleagueId,
    user_id: UserId,
    user_colleague_id: ColleagueId,
    org_id: OrgId,
}

impl Setup {
    /// Build the harness, capture its pieces, then stop the worker so the rows
    /// we seed and enqueue are never claimed underneath the assertions.
    async fn idle(pool: PgPool) -> Self {
        let h = build_harness(pool, Arc::new(ScriptedProvider::new(vec![]))).await;
        let setup = Self {
            pool: h.pool.clone(),
            sessions: h.sessions.clone(),
            queue: h.queue.clone(),
            dag: h.dag.clone(),
            sink: h.hub.clone(),
            agent_id: h.default_agent_id,
            agent_colleague_id: h.default_agent_colleague_id,
            user_id: h.default_user_id,
            user_colleague_id: h.default_user_colleague_id,
            org_id: h.default_org_id,
        };
        h.workers.shutdown().await;
        setup
    }

    fn agents(&self) -> SharedAgentStore {
        common::pg::shared_agent_store(self.pool.clone(), SystemClock::shared())
    }

    fn tool(&self) -> SendMessageTool {
        let colleagues: SharedColleagueStore =
            Arc::new(patom::colleagues::PgColleagueStore::new(self.pool.clone()));
        SendMessageTool::new(
            self.sessions.clone(),
            self.queue.clone(),
            self.dag.clone(),
            self.agents(),
            colleagues,
            self.sink.clone(),
        )
    }

    fn agent_viewer(&self) -> Participant {
        Participant::agent(self.agent_colleague_id, self.agent_id)
    }

    /// Enqueue a human→agent root so a real DAG root + session exist for the
    /// tool to parent against and bump. Workers are down, so it just sits there.
    async fn seed_root(&self, key: &str) -> (PromptRequestId, SessionId) {
        let outcome = self
            .queue
            .enqueue(NewPromptRequest {
                session: None,
                sender: Participant::human(self.user_colleague_id, self.user_id),
                receiver_agent_id: self.agent_id,
                parent_session: None,
                content: Prompt::try_from("root prompt").expect("prompt"),
                idempotency_key: IdempotencyKey::try_from(key).expect("key"),
                org_id: self.org_id,
                created_by_user_id: self.user_id,
                kind_payload: RequestKindPayload::Normal {},
            })
            .await
            .expect("enqueue root");
        (outcome.request_id(), outcome.session())
    }

    fn ctx(&self, session: SessionId, root: PromptRequestId) -> ToolCallContext {
        ToolCallContext {
            session_id: session,
            viewer: self.agent_viewer(),
            root_request_id: root,
            request_id: root,
            kind_payload: RequestKindPayload::Normal {},
            acting_user_id: self.user_id,
            org_id: self.org_id,
        }
    }
}

#[sqlx::test]
async fn colleague_id_human_publishes_without_prompt_request(pool: PgPool) {
    let s = Setup::idle(pool).await;
    let tool = s.tool();
    let (root, session) = s.seed_root("k-human-id").await;

    // Address the seeded human by *colleague id* (not the `{"kind":"human"}`
    // sugar) — the headline Stage 7 capability.
    let out = tool
        .execute(
            json!({
                "receiver": { "kind": "colleague", "id": s.user_colleague_id },
                "content": "hi Tom",
            }),
            &s.ctx(session, root),
        )
        .await
        .expect("human-by-id delivers");

    let parsed: Value = serde_json::from_str(&out).expect("json");
    assert_eq!(parsed["delivery"], "published", "human → notification path");
    assert!(
        parsed["request_id"].is_null(),
        "human delivery mints no prompt_request: {parsed}"
    );
}

#[sqlx::test]
async fn colleague_id_agent_enqueues_turn(pool: PgPool) {
    let s = Setup::idle(pool).await;

    // A second agent in the same org — its colleague is minted by the trigger.
    let peer = s
        .agents()
        .create_for_user(
            s.user_id,
            NewAgent {
                org_id: s.org_id,
                name: AgentName::try_from("designer").expect("name"),
                system_prompt: AgentSystemPrompt::try_from("you design").expect("prompt"),
                description: AgentDescription::try_from("Designs things.").expect("desc"),
                is_default: false,
                allowed_mcp_tools: AllowedMcpTools::empty(),
                model: None,
                avatar_url: None,
                edited_by: None,
            },
        )
        .await
        .expect("create peer agent");
    let peer_cid = patom::colleagues::resolve_agent_colleague(&s.pool, s.org_id, peer.id)
        .await
        .expect("peer colleague minted");

    let tool = s.tool();
    let (root, session) = s.seed_root("k-agent-id").await;

    let out = tool
        .execute(
            json!({
                "receiver": { "kind": "colleague", "id": peer_cid },
                "content": "take a look",
                "context_summary": "kicking off a design pass",
            }),
            &s.ctx(session, root),
        )
        .await
        .expect("agent-by-id enqueues");

    let parsed: Value = serde_json::from_str(&out).expect("json");
    assert_eq!(parsed["delivery"], "queued", "agent → queue path");
    assert!(
        !parsed["request_id"].is_null(),
        "agent delivery enqueues a prompt_request: {parsed}"
    );
}

#[sqlx::test]
async fn colleague_id_self_rejected(pool: PgPool) {
    let s = Setup::idle(pool).await;
    let tool = s.tool();
    let (root, session) = s.seed_root("k-self").await;

    let err = tool
        .execute(
            json!({
                "receiver": { "kind": "colleague", "id": s.agent_colleague_id },
                "content": "talking to myself",
            }),
            &s.ctx(session, root),
        )
        .await
        .expect_err("addressing self must be rejected");
    assert!(
        err.to_string().contains("receiver equals caller"),
        "got {err}"
    );
}

#[sqlx::test]
async fn colleague_id_unknown_rejected(pool: PgPool) {
    let s = Setup::idle(pool).await;
    let tool = s.tool();
    let (root, session) = s.seed_root("k-unknown").await;

    let err = tool
        .execute(
            json!({
                "receiver": { "kind": "colleague", "id": ColleagueId::new() },
                "content": "anyone there?",
            }),
            &s.ctx(session, root),
        )
        .await
        .expect_err("unknown colleague must be rejected");
    assert!(err.to_string().contains("unknown colleague"), "got {err}");
}

#[sqlx::test]
async fn colleague_id_cross_org_rejected(pool: PgPool) {
    let s = Setup::idle(pool).await;

    // A colleague in a *different* org must not be addressable — the privileged
    // directory read crosses tenants, so isolation is enforced in the tool.
    let foreign = common::pg::seed_tenant(&s.pool).await;
    let foreign_cid =
        patom::colleagues::resolve_user_colleague(&s.pool, foreign.org_id, foreign.user_id)
            .await
            .expect("foreign human colleague");

    let tool = s.tool();
    let (root, session) = s.seed_root("k-cross-org").await;

    let err = tool
        .execute(
            json!({
                "receiver": { "kind": "colleague", "id": foreign_cid },
                "content": "wrong tenant",
            }),
            &s.ctx(session, root),
        )
        .await
        .expect_err("cross-org colleague must be rejected");
    assert!(
        err.to_string().contains("unknown colleague"),
        "cross-org leaks as unknown, not a distinct error: {err}"
    );
}
