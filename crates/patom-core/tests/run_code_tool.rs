//! #218 acceptance tests for the `run_code` tool, exercised through the public
//! tool boundary with the test-catalog doubles (FakeSandbox + InMemoryAssetStore
//! + InMemoryOrgEgressStore + TestClock):
//!
//! 1a. stdout flows back through `Tool::execute`.
//! 1b. a wall-clock kill surfaces as a typed `is_error` result.
//! 2.  an empty org allowlist resolves to `EgressPolicy::DenyAll`; a denied host
//!     surfaces as an error.
//! 3.  a harvested artifact is stored under a tenant-private `sandbox/{org}/…`
//!     key and its URL appears in the result.
//! 4.  with `run_code` marked gated and no approval, the real `HardApprovalGate`
//!     blocks it (and allows it once approved) — the gate that the worker
//!     consults for every tool.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use chrono::Utc;
use patom::approvals::{
    ActionSummary, ApprovalGate, ApprovalId, ApprovalStore, ApproverPolicy, Decision, GateOutcome,
    GatedToolStore, HardApprovalGate, NewApproval, PgApprovalStore, PlatformTarget,
};
use patom::assets::{InMemoryAssetStore, SharedAssetStore};
use patom::auth::{Caller, OrgId, UserId};
use patom::clock::{SystemClock, TestClock};
use patom::runtime::{ClaimKey, PromptRequestId, RequestKindPayload};
use patom::sandbox::{
    EgressHost, EgressPolicy, ExitCode, FakeSandbox, InMemoryOrgEgressStore, OutputFile, RunOutput,
    SandboxError, ScratchFileName, SharedOrgEgressStore, SharedSandbox,
};
use patom::threads::ThreadId;
use patom::tools::system::RunCodeTool;
use patom::tools::{Tool, ToolCallContext, ToolError};
use patom::types::{Participant, ToolName};
use serde_json::json;
use sqlx::PgPool;

mod common;
use common::pg::{seed_agent_thread_state, seed_prompt_request, seed_tenant};

fn ctx(org: OrgId, thread: Option<ThreadId>) -> ToolCallContext {
    let rid = PromptRequestId::new();
    ToolCallContext {
        claim_key: ClaimKey::new(),
        thread_id: thread,
        state_id: None,
        viewer: Participant::system(),
        root_request_id: rid,
        request_id: rid,
        kind_payload: RequestKindPayload::Normal {},
        acting_user_id: UserId::new(),
        org_id: org,
    }
}

fn tool_with(sandbox: Arc<FakeSandbox>) -> (RunCodeTool, Arc<InMemoryAssetStore>) {
    let assets = Arc::new(InMemoryAssetStore::new("https://assets.test.invalid"));
    let store: SharedAssetStore = assets.clone();
    let egress: SharedOrgEgressStore = Arc::new(InMemoryOrgEgressStore::new());
    let clock = Arc::new(TestClock::default());
    let sandbox: SharedSandbox = sandbox;
    let tool = RunCodeTool::new(sandbox, store, egress, clock);
    (tool, assets)
}

#[tokio::test]
async fn stdout_returns_through_the_tool_boundary() {
    let out = RunOutput::new(
        ExitCode::new(0),
        "ok-42\n".to_owned(),
        String::new(),
        Vec::new(),
    );
    let fake = Arc::new(FakeSandbox::new().push_output(out));
    let (tool, _) = tool_with(fake);
    let res = tool
        .execute(
            json!({ "language": "python", "code": "print('ok', 42)" }),
            &ctx(OrgId::new(), None),
        )
        .await
        .expect("ok");
    assert!(res.contains("ok-42"), "stdout missing: {res}");
}

#[tokio::test(start_paused = true)]
async fn wall_clock_kill_is_a_typed_error() {
    let fake = Arc::new(FakeSandbox::new().push_timeout());
    let (tool, _) = tool_with(fake);
    let err = tool
        .execute(
            json!({ "language": "python", "code": "while True:\n  pass", "timeout_secs": 5 }),
            &ctx(OrgId::new(), None),
        )
        .await
        .expect_err("timeout is an error");
    assert!(matches!(err, ToolError::InvalidInput(_)), "got {err:?}");
}

#[tokio::test]
async fn empty_allowlist_runs_with_deny_all() {
    let fake = Arc::new(FakeSandbox::new());
    let (tool, _) = tool_with(fake.clone());
    let _ = tool
        .execute(
            json!({ "language": "shell", "code": "echo hi" }),
            &ctx(OrgId::new(), None),
        )
        .await
        .expect("ok");
    let reqs = fake.recorded_requests();
    assert_eq!(reqs.len(), 1);
    assert_eq!(*reqs[0].egress(), EgressPolicy::DenyAll);
}

#[tokio::test]
async fn denied_host_surfaces_as_error() {
    let host = EgressHost::try_from("api.example.com").expect("host");
    let fake = Arc::new(FakeSandbox::new().push_error(SandboxError::EgressDenied { host }));
    let (tool, _) = tool_with(fake);
    let err = tool
        .execute(
            json!({ "language": "python", "code": "import urllib.request" }),
            &ctx(OrgId::new(), None),
        )
        .await
        .expect_err("egress denied is an error");
    assert!(matches!(err, ToolError::InvalidInput(_)), "got {err:?}");
}

#[tokio::test]
async fn artifact_is_stored_under_tenant_private_key() {
    let name = ScratchFileName::try_from("report.csv").expect("name");
    let artifact =
        OutputFile::new(name, bytes::Bytes::from_static(b"a,b\n1,2\n")).expect("artifact");
    let out = RunOutput::new(
        ExitCode::new(0),
        "wrote report\n".to_owned(),
        String::new(),
        vec![artifact],
    );
    let fake = Arc::new(FakeSandbox::new().push_output(out));
    let (tool, assets) = tool_with(fake);
    let org = OrgId::new();
    let thread = ThreadId::new();
    let res = tool
        .execute(
            json!({ "language": "python", "code": "open('report.csv','w')" }),
            &ctx(org, Some(thread)),
        )
        .await
        .expect("ok");
    assert!(
        res.contains(&format!("sandbox/{org}/{thread}")),
        "tenant-private key missing: {res}"
    );
    assert_eq!(assets.len().await, 1, "artifact not stored");
}

#[sqlx::test]
async fn gated_run_code_is_blocked_until_approved(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let state = seed_agent_thread_state(&pool, seed.org_id, seed.agent_id).await;
    let root = seed_prompt_request(&pool, state, seed.agent_id, seed.org_id, seed.user_id).await;
    let (thread_id,): (uuid::Uuid,) =
        sqlx::query_as("SELECT thread_id FROM agent_thread_state WHERE id = $1")
            .bind(state)
            .fetch_one(&pool)
            .await
            .expect("thread id");
    let human = patom::colleagues::resolve_user_colleague(&pool, seed.org_id, seed.user_id)
        .await
        .expect("human colleague");

    let store = Arc::new(PgApprovalStore::new(pool.clone(), SystemClock::shared()));
    let gate = HardApprovalGate::new(store.clone(), store.clone());
    let caller = Caller::new(seed.user_id, seed.org_id);
    let run_code = ToolName::try_from("run_code").expect("name");

    // Mark run_code gated for this agent, then check the real gate: no approval
    // exists, so it must be blocked.
    store
        .set_gated(&caller, seed.agent_id, &run_code)
        .await
        .expect("set gated");
    let verdict = gate
        .check(seed.org_id, seed.agent_id, root, &run_code)
        .await;
    assert!(
        matches!(verdict, GateOutcome::Blocked(_)),
        "gated run_code with no approval must be blocked, got {verdict:?}"
    );

    // Record + approve a decision for run_code in this DAG; the gate now allows.
    let created = store
        .create(
            &caller,
            NewApproval {
                id: ApprovalId::new(),
                thread_id: ThreadId::from(thread_id),
                requesting_agent_id: seed.agent_id,
                requesting_colleague_id: patom::colleagues::resolve_agent_colleague(
                    &pool,
                    seed.org_id,
                    seed.agent_id,
                )
                .await
                .expect("agent colleague"),
                root_request_id: root,
                action_summary: ActionSummary::try_from("Run a chart-building script")
                    .expect("summary"),
                gated_tool: run_code.clone(),
                approvers: ApproverPolicy::Anyone,
                target: PlatformTarget::Web,
                idempotency_key: "apv:run_code".to_owned(),
                expires_at: Utc::now() + chrono::Duration::hours(1),
            },
        )
        .await
        .expect("create approval");
    store
        .decide(
            seed.org_id,
            created.record().id,
            Decision::Approved,
            human,
            Utc::now(),
        )
        .await
        .expect("approve");

    let verdict = gate
        .check(seed.org_id, seed.agent_id, root, &run_code)
        .await;
    assert_eq!(
        verdict,
        GateOutcome::Allowed,
        "an approved run_code must be allowed"
    );
}
