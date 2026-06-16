//! Behaviour-level tests for the `profile_write` tool: it writes the org-shared
//! board through `Tool::execute` with a wired `ToolCallContext`, against a real
//! Postgres `ProfileStore` — happy path + provenance, the "at least one field"
//! rule, oversize-field + unknown-field rejection, cross-org subject rejection,
//! and the non-agent-viewer guard.

#![allow(clippy::expect_used)]

use std::sync::Arc;

use patom::clock::SystemClock;
use patom::colleagues::{
    ColleagueId, PgProfileStore, ProfileStore, SharedProfileStore, resolve_agent_colleague,
    resolve_user_colleague,
};
use patom::runtime::{ClaimKey, PromptRequestId, RequestKindPayload};
use patom::tools::system::ProfileWriteTool;
use patom::tools::{Tool, ToolCallContext, ToolError};
use serde_json::{Value, json};
use sqlx::PgPool;

mod common;
use common::embedding::FakeEmbeddingProvider;
use common::pg::seed_tenant;

struct Fixture {
    tool: ProfileWriteTool,
    profiles: Arc<PgProfileStore>,
    ctx: ToolCallContext,
    human_colleague: ColleagueId,
    agent_colleague: ColleagueId,
}

async fn fixture(pool: &PgPool, seed: &common::pg::Seed) -> Fixture {
    let profiles = Arc::new(PgProfileStore::new(
        pool.clone(),
        SystemClock::shared(),
        FakeEmbeddingProvider::shared(),
    ));
    let request_id = PromptRequestId::new();
    let ctx = ToolCallContext {
        claim_key: ClaimKey::new(),
        thread_id: None,
        state_id: None,
        viewer: common::pg::agent_participant(pool, seed.org_id, seed.agent_id).await,
        root_request_id: request_id,
        request_id,
        kind_payload: RequestKindPayload::Normal {},
        acting_user_id: seed.user_id,
        org_id: seed.org_id,
    };
    let human_colleague = resolve_user_colleague(pool, seed.org_id, seed.user_id)
        .await
        .expect("human colleague");
    let agent_colleague = resolve_agent_colleague(pool, seed.org_id, seed.agent_id)
        .await
        .expect("agent colleague");
    let shared: SharedProfileStore = profiles.clone();
    Fixture {
        tool: ProfileWriteTool::new(shared),
        profiles,
        ctx,
        human_colleague,
        agent_colleague,
    }
}

fn human_ctx(f: &Fixture, seed: &common::pg::Seed) -> ToolCallContext {
    ToolCallContext {
        claim_key: f.ctx.claim_key,
        thread_id: None,
        state_id: None,
        viewer: patom::types::Participant::human(f.human_colleague, seed.user_id),
        root_request_id: f.ctx.root_request_id,
        request_id: f.ctx.request_id,
        kind_payload: RequestKindPayload::Normal {},
        acting_user_id: f.ctx.acting_user_id,
        org_id: f.ctx.org_id,
    }
}

#[sqlx::test]
async fn happy_path_writes_board_with_provenance(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let f = fixture(&pool, &seed).await;

    let out = f
        .tool
        .execute(
            json!({
                "subject": f.human_colleague,
                "role": "Product Manager",
                "preferences": "async-first",
            }),
            &f.ctx,
        )
        .await
        .expect("write profile");
    let parsed: Value = serde_json::from_str(&out).expect("json");
    assert_eq!(
        parsed["subject"].as_str().expect("subject"),
        f.human_colleague.as_uuid().to_string()
    );

    let map = f
        .profiles
        .get_many(&[f.human_colleague])
        .await
        .expect("get_many");
    let got = map.get(&f.human_colleague).expect("board row written");
    assert_eq!(got.role().expect("role").as_str(), "Product Manager");
    assert_eq!(got.preferences().expect("prefs").as_str(), "async-first");
    assert!(got.expertise().is_none());
    assert_eq!(
        got.updated_by(),
        Some(f.agent_colleague),
        "provenance is the writing agent's colleague"
    );
}

#[sqlx::test]
async fn requires_at_least_one_field(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let f = fixture(&pool, &seed).await;

    let err = f
        .tool
        .execute(json!({ "subject": f.human_colleague }), &f.ctx)
        .await
        .expect_err("subject-only must reject");
    match err {
        ToolError::InvalidInput(msg) => assert!(msg.contains("at least one")),
        other => panic!("expected InvalidInput, got {other:?}"),
    }
}

#[sqlx::test]
async fn oversize_role_is_rejected(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let f = fixture(&pool, &seed).await;

    let err = f
        .tool
        .execute(
            json!({ "subject": f.human_colleague, "role": "x".repeat(5000) }),
            &f.ctx,
        )
        .await
        .expect_err("oversize role must reject");
    assert!(matches!(err, ToolError::InvalidInput(_)));
}

#[sqlx::test]
async fn unknown_field_is_rejected(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let f = fixture(&pool, &seed).await;

    let err = f
        .tool
        .execute(
            json!({ "subject": f.human_colleague, "role": "PM", "salary": 1 }),
            &f.ctx,
        )
        .await
        .expect_err("unknown field must reject");
    assert!(matches!(
        err,
        ToolError::InvalidInput(_) | ToolError::Json(_)
    ));
}

#[sqlx::test]
async fn subject_outside_org_is_invalid_input(pool: PgPool) {
    let org_a = seed_tenant(&pool).await;
    let org_b = seed_tenant(&pool).await;
    let f = fixture(&pool, &org_a).await;
    let human_b = resolve_user_colleague(&pool, org_b.org_id, org_b.user_id)
        .await
        .expect("human in B");

    let err = f
        .tool
        .execute(json!({ "subject": human_b, "role": "Spy" }), &f.ctx)
        .await
        .expect_err("cross-org subject must reject");
    match err {
        ToolError::InvalidInput(msg) => assert!(msg.contains("org")),
        other => panic!("expected InvalidInput, got {other:?}"),
    }
}

#[sqlx::test]
async fn non_agent_viewer_is_rejected(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let f = fixture(&pool, &seed).await;

    let err = f
        .tool
        .execute(
            json!({ "subject": f.human_colleague, "role": "PM" }),
            &human_ctx(&f, &seed),
        )
        .await
        .expect_err("human caller must reject");
    match err {
        ToolError::InvalidInput(msg) => assert!(msg.contains("agent")),
        other => panic!("expected InvalidInput, got {other:?}"),
    }
}
