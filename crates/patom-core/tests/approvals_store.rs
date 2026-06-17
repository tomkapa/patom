//! Integration tests for the approval store (issue #200) against real Postgres
//! via `#[sqlx::test]`. Covers the idempotent create, the atomic + idempotent
//! decide, server-side authorization, the hard-gate query, expiry, and the
//! per-agent gated-tool config.

mod common;

use chrono::{Duration, Utc};
use patom::approvals::{
    ActionSummary, ApprovalId, ApprovalStatus, ApprovalStore, ApproverPolicy, CreateOutcome,
    DecideOutcome, Decision, GatedToolStore, NewApproval, PgApprovalStore, PlatformTarget,
};
use patom::auth::Caller;
use patom::clock::SystemClock;
use patom::colleagues::{ColleagueId, resolve_agent_colleague, resolve_user_colleague};
use patom::runtime::PromptRequestId;
use patom::threads::{AgentThreadId, ThreadId};
use patom::types::ToolName;
use sqlx::PgPool;
use uuid::Uuid;

use common::pg::{Seed, seed_agent_thread_state, seed_prompt_request, seed_tenant};

struct Fixture {
    seed: Seed,
    thread_id: ThreadId,
    root: PromptRequestId,
    agent_colleague: ColleagueId,
    human_colleague: ColleagueId,
}

async fn fixture(pool: &PgPool) -> Fixture {
    let seed = seed_tenant(pool).await;
    let state: AgentThreadId = seed_agent_thread_state(pool, seed.org_id, seed.agent_id).await;
    let root = seed_prompt_request(pool, state, seed.agent_id, seed.org_id, seed.user_id).await;
    let (thread_id,): (Uuid,) =
        sqlx::query_as("SELECT thread_id FROM agent_thread_state WHERE id = $1")
            .bind(state)
            .fetch_one(pool)
            .await
            .expect("thread id for state");
    let agent_colleague = resolve_agent_colleague(pool, seed.org_id, seed.agent_id)
        .await
        .expect("agent colleague");
    let human_colleague = resolve_user_colleague(pool, seed.org_id, seed.user_id)
        .await
        .expect("human colleague");
    Fixture {
        seed,
        thread_id: ThreadId::from(thread_id),
        root,
        agent_colleague,
        human_colleague,
    }
}

fn store(pool: &PgPool) -> PgApprovalStore {
    PgApprovalStore::new(pool.clone(), SystemClock::shared())
}

fn new_approval(fx: &Fixture, tool: &str, approvers: ApproverPolicy, key: &str) -> NewApproval {
    NewApproval {
        id: ApprovalId::new(),
        thread_id: fx.thread_id,
        requesting_agent_id: fx.seed.agent_id,
        requesting_colleague_id: fx.agent_colleague,
        root_request_id: fx.root,
        action_summary: ActionSummary::try_from("Refund $40 to customer #12").expect("summary"),
        gated_tool: ToolName::try_from(tool).expect("tool name"),
        approvers,
        target: PlatformTarget::Web,
        idempotency_key: key.to_owned(),
        expires_at: Utc::now() + Duration::hours(1),
    }
}

#[sqlx::test]
async fn create_is_idempotent_on_idempotency_key(pool: PgPool) {
    let fx = fixture(&pool).await;
    let store = store(&pool);
    let caller = Caller::new(fx.seed.user_id, fx.seed.org_id);

    let first = store
        .create(
            &caller,
            new_approval(&fx, "refund_customer", ApproverPolicy::Anyone, "apv:k1"),
        )
        .await
        .expect("create");
    let second = store
        .create(
            &caller,
            new_approval(&fx, "refund_customer", ApproverPolicy::Anyone, "apv:k1"),
        )
        .await
        .expect("create again");

    assert!(matches!(first, CreateOutcome::Created(_)));
    assert!(matches!(second, CreateOutcome::Existing(_)));
    assert_eq!(
        first.record().id,
        second.record().id,
        "dedupe returns the same row"
    );
}

#[sqlx::test]
async fn decide_approves_and_double_click_is_idempotent(pool: PgPool) {
    let fx = fixture(&pool).await;
    let store = store(&pool);
    let caller = Caller::new(fx.seed.user_id, fx.seed.org_id);
    let created = store
        .create(
            &caller,
            new_approval(&fx, "refund_customer", ApproverPolicy::Anyone, "apv:k2"),
        )
        .await
        .expect("create");
    let id = created.record().id;

    let now = Utc::now();
    let first = store
        .decide(
            fx.seed.org_id,
            id,
            Decision::Approved,
            fx.human_colleague,
            now,
        )
        .await
        .expect("decide");
    let second = store
        .decide(
            fx.seed.org_id,
            id,
            Decision::Approved,
            fx.human_colleague,
            now,
        )
        .await
        .expect("decide again");

    match first {
        DecideOutcome::Decided(r) => {
            assert_eq!(r.status, ApprovalStatus::Approved);
            assert_eq!(r.decided_by_colleague, Some(fx.human_colleague));
            assert!(r.decided_at.is_some());
        }
        DecideOutcome::AlreadyDecided(_) => panic!("first decide should flip"),
    }
    assert!(
        matches!(second, DecideOutcome::AlreadyDecided(_)),
        "second click is idempotent"
    );
}

#[sqlx::test]
async fn decide_rejects_unauthorized_clicker(pool: PgPool) {
    let fx = fixture(&pool).await;
    let store = store(&pool);
    let caller = Caller::new(fx.seed.user_id, fx.seed.org_id);
    // Whitelist only the human; an unrelated colleague (the agent's) cannot decide.
    let created = store
        .create(
            &caller,
            new_approval(
                &fx,
                "merge_pr",
                ApproverPolicy::OneOf(vec![fx.human_colleague]),
                "apv:k3",
            ),
        )
        .await
        .expect("create");
    let id = created.record().id;

    let err = store
        .decide(
            fx.seed.org_id,
            id,
            Decision::Approved,
            fx.agent_colleague,
            Utc::now(),
        )
        .await
        .expect_err("unauthorized clicker");
    assert!(matches!(err, patom::approvals::ApprovalError::Unauthorized));

    // The authorized human still succeeds.
    let ok = store
        .decide(
            fx.seed.org_id,
            id,
            Decision::Approved,
            fx.human_colleague,
            Utc::now(),
        )
        .await
        .expect("authorized decide");
    assert!(matches!(ok, DecideOutcome::Decided(_)));
}

#[sqlx::test]
async fn has_approved_for_dag_gates_on_tool_and_status(pool: PgPool) {
    let fx = fixture(&pool).await;
    let store = store(&pool);
    let caller = Caller::new(fx.seed.user_id, fx.seed.org_id);
    let tool = ToolName::try_from("refund_customer").expect("tool");
    let other = ToolName::try_from("merge_pr").expect("tool");

    let created = store
        .create(
            &caller,
            new_approval(&fx, "refund_customer", ApproverPolicy::Anyone, "apv:k4"),
        )
        .await
        .expect("create");

    // Pending → no approval yet.
    assert!(
        !store
            .has_approved_for_dag(fx.seed.org_id, fx.root, &tool)
            .await
            .expect("query")
    );

    store
        .decide(
            fx.seed.org_id,
            created.record().id,
            Decision::Approved,
            fx.human_colleague,
            Utc::now(),
        )
        .await
        .expect("decide");

    // Approved → true for the gated tool, false for a different tool.
    assert!(
        store
            .has_approved_for_dag(fx.seed.org_id, fx.root, &tool)
            .await
            .expect("query")
    );
    assert!(
        !store
            .has_approved_for_dag(fx.seed.org_id, fx.root, &other)
            .await
            .expect("query other")
    );
}

#[sqlx::test]
async fn denied_decision_does_not_satisfy_the_gate(pool: PgPool) {
    let fx = fixture(&pool).await;
    let store = store(&pool);
    let caller = Caller::new(fx.seed.user_id, fx.seed.org_id);
    let tool = ToolName::try_from("refund_customer").expect("tool");
    let created = store
        .create(
            &caller,
            new_approval(&fx, "refund_customer", ApproverPolicy::Anyone, "apv:k5"),
        )
        .await
        .expect("create");
    store
        .decide(
            fx.seed.org_id,
            created.record().id,
            Decision::Denied,
            fx.human_colleague,
            Utc::now(),
        )
        .await
        .expect("deny");

    assert!(
        !store
            .has_approved_for_dag(fx.seed.org_id, fx.root, &tool)
            .await
            .expect("query")
    );
}

#[sqlx::test]
async fn expire_due_marks_pending_and_blocks_decide(pool: PgPool) {
    let fx = fixture(&pool).await;
    let store = store(&pool);
    let caller = Caller::new(fx.seed.user_id, fx.seed.org_id);
    let mut payload = new_approval(&fx, "refund_customer", ApproverPolicy::Anyone, "apv:k6");
    payload.expires_at = Utc::now() - Duration::minutes(5);
    let created = store.create(&caller, payload).await.expect("create");
    let id = created.record().id;

    let swept = store.expire_due(Utc::now()).await.expect("sweep");
    assert_eq!(swept, 1, "the past-TTL row is flipped to expired");

    let read = store.read(fx.seed.org_id, id).await.expect("read");
    assert_eq!(read.status, ApprovalStatus::Expired);

    let err = store
        .decide(
            fx.seed.org_id,
            id,
            Decision::Approved,
            fx.human_colleague,
            Utc::now(),
        )
        .await
        .expect_err("cannot decide expired");
    assert!(matches!(err, patom::approvals::ApprovalError::Expired));
}

#[sqlx::test]
async fn gated_tool_config_roundtrips(pool: PgPool) {
    let fx = fixture(&pool).await;
    let store = store(&pool);
    let caller = Caller::new(fx.seed.user_id, fx.seed.org_id);
    let tool = ToolName::try_from("refund_customer").expect("tool");

    assert!(
        !store
            .is_gated(fx.seed.org_id, fx.seed.agent_id, &tool)
            .await
            .expect("is_gated")
    );

    store
        .set_gated(&caller, fx.seed.agent_id, &tool)
        .await
        .expect("set");
    // Idempotent.
    store
        .set_gated(&caller, fx.seed.agent_id, &tool)
        .await
        .expect("set again");

    assert!(
        store
            .is_gated(fx.seed.org_id, fx.seed.agent_id, &tool)
            .await
            .expect("is_gated")
    );
    let listed = store
        .gated_tools_for_agent(fx.seed.org_id, fx.seed.agent_id)
        .await
        .expect("list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].as_str(), "refund_customer");

    store
        .unset_gated(&caller, fx.seed.agent_id, &tool)
        .await
        .expect("unset");
    assert!(
        !store
            .is_gated(fx.seed.org_id, fx.seed.agent_id, &tool)
            .await
            .expect("is_gated")
    );
}

#[sqlx::test]
async fn attach_message_records_platform_id(pool: PgPool) {
    let fx = fixture(&pool).await;
    let store = store(&pool);
    let caller = Caller::new(fx.seed.user_id, fx.seed.org_id);
    let created = store
        .create(
            &caller,
            new_approval(&fx, "refund_customer", ApproverPolicy::Anyone, "apv:k7"),
        )
        .await
        .expect("create");
    let id = created.record().id;

    let msg = patom::approvals::PlatformMessageId::try_from("om-12345".to_owned()).expect("msg id");
    store
        .attach_message(&caller, id, msg)
        .await
        .expect("attach");

    let read = store.read(fx.seed.org_id, id).await.expect("read");
    assert_eq!(read.platform_message_id.expect("set").as_str(), "om-12345");
}

#[sqlx::test]
async fn read_unknown_id_is_not_found(pool: PgPool) {
    let fx = fixture(&pool).await;
    let store = store(&pool);
    let err = store
        .read(fx.seed.org_id, ApprovalId::new())
        .await
        .expect_err("unknown");
    assert!(matches!(err, patom::approvals::ApprovalError::NotFound));
}
