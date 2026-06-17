//! Integration test for the approval decision → fresh-trigger resume (issue
//! #200). Proves that approving a pending request seeds a private decision note
//! and enqueues a `Normal` trigger that REUSES the original DAG root, and that a
//! double-click resumes exactly once (idempotent).

mod common;

use std::sync::Arc;

use chrono::{Duration, Utc};
use patom::approvals::{
    ActionSummary, ApprovalDecider, ApprovalId, ApprovalResumer, ApprovalStore, ApproverPolicy,
    Decision, NewApproval, PgApprovalStore, PlatformTarget,
};
use patom::auth::Caller;
use patom::clock::SystemClock;
use patom::colleagues::{
    PgColleagueStore, SharedColleagueStore, resolve_agent_colleague, resolve_user_colleague,
};
use patom::runtime::{PgDagBudget, PgPromptQueue, SharedDagBudget, SharedPromptQueue};
use patom::threads::{AgentThreadId, PgThreadStore, SharedThreadStore, ThreadId};
use patom::types::ToolName;
use sqlx::PgPool;
use uuid::Uuid;

use common::pg::{seed_agent_thread_state, seed_prompt_request, seed_tenant};

#[sqlx::test]
async fn approve_seeds_note_and_resume_trigger_reusing_root(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let state: AgentThreadId = seed_agent_thread_state(&pool, seed.org_id, seed.agent_id).await;
    let root = seed_prompt_request(&pool, state, seed.agent_id, seed.org_id, seed.user_id).await;
    let (thread_uuid,): (Uuid,) =
        sqlx::query_as("SELECT thread_id FROM agent_thread_state WHERE id = $1")
            .bind(state)
            .fetch_one(&pool)
            .await
            .expect("thread id");
    let thread = ThreadId::from(thread_uuid);
    let agent_colleague = resolve_agent_colleague(&pool, seed.org_id, seed.agent_id)
        .await
        .expect("agent colleague");
    let human_colleague = resolve_user_colleague(&pool, seed.org_id, seed.user_id)
        .await
        .expect("human colleague");

    let clock = SystemClock::shared();
    let store: Arc<PgApprovalStore> = Arc::new(PgApprovalStore::new(pool.clone(), clock.clone()));
    let threads: SharedThreadStore = Arc::new(PgThreadStore::new(pool.clone(), clock.clone()));
    let queue: SharedPromptQueue = Arc::new(PgPromptQueue::new(pool.clone(), clock.clone()));
    let dag: SharedDagBudget = Arc::new(PgDagBudget::new(pool.clone()));
    let colleagues: SharedColleagueStore = Arc::new(PgColleagueStore::new(pool.clone()));
    let resumer = Arc::new(ApprovalResumer::new(threads, queue, dag, colleagues));
    let decider = ApprovalDecider::new(store.clone(), resumer, clock);

    // Record a pending approval, then approve it as the human.
    let caller = Caller::new(seed.user_id, seed.org_id);
    let created = store
        .create(
            &caller,
            NewApproval {
                id: ApprovalId::new(),
                thread_id: thread,
                requesting_agent_id: seed.agent_id,
                requesting_colleague_id: agent_colleague,
                root_request_id: root,
                action_summary: ActionSummary::try_from("Refund $40 to customer #12")
                    .expect("summary"),
                gated_tool: ToolName::try_from("refund_customer").expect("tool"),
                approvers: ApproverPolicy::Anyone,
                target: PlatformTarget::Web,
                idempotency_key: "apv:resume-test".to_owned(),
                expires_at: Utc::now() + Duration::hours(1),
            },
        )
        .await
        .expect("create");
    let approval_id = created.record().id;

    decider
        .decide(
            seed.org_id,
            approval_id,
            Decision::Approved,
            human_colleague,
        )
        .await
        .expect("decide");

    // A resume trigger was enqueued, reusing the original DAG root.
    let trig_roots: Vec<(Uuid,)> =
        sqlx::query_as("SELECT root_request_id FROM prompt_requests WHERE idempotency_key = $1")
            .bind(format!("apv-resume-{approval_id}"))
            .fetch_all(&pool)
            .await
            .expect("trigger query");
    assert_eq!(trig_roots.len(), 1, "exactly one resume trigger");
    assert_eq!(
        trig_roots[0].0,
        root.as_uuid(),
        "resume reuses the original DAG root"
    );

    // A private decision note was appended to the thread for the agent.
    let (note_count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM thread_messages \
         WHERE thread_id = $1 AND kind = 'system_note' AND owner_agent_id = $2",
    )
    .bind(thread)
    .bind(seed.agent_id)
    .fetch_one(&pool)
    .await
    .expect("note query");
    assert_eq!(note_count, 1, "one decision note seeded for the agent");

    // Double-click is idempotent: no second resume trigger, no second note.
    let again = decider
        .decide(
            seed.org_id,
            approval_id,
            Decision::Approved,
            human_colleague,
        )
        .await
        .expect("decide again");
    assert!(matches!(
        again,
        patom::approvals::DecideOutcome::AlreadyDecided(_)
    ));
    let (trig_count_after,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM prompt_requests WHERE idempotency_key = $1")
            .bind(format!("apv-resume-{approval_id}"))
            .fetch_one(&pool)
            .await
            .expect("trigger recount");
    assert_eq!(
        trig_count_after, 1,
        "double-click does not enqueue a second resume"
    );
}
