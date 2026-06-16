//! Behaviour-level tests for the `search_colleague` tool: it returns the unified
//! `{ kind, id, name, snippet }` shape through `Tool::execute`, finds a profiled
//! human, excludes the viewer, and rejects an out-of-range limit at the boundary.

#![allow(clippy::expect_used)]

use std::sync::Arc;

use patom::clock::SystemClock;
use patom::colleagues::{
    ColleagueProfile, PgProfileStore, ProfileStore, Role, SharedProfileStore,
    resolve_agent_colleague, resolve_user_colleague,
};
use patom::runtime::{ClaimKey, PromptRequestId, RequestKindPayload};
use patom::tools::system::SearchColleagueTool;
use patom::tools::{Tool, ToolCallContext, ToolError};
use serde_json::{Value, json};
use sqlx::PgPool;

mod common;
use common::embedding::FakeEmbeddingProvider;
use common::pg::seed_tenant;

struct Fixture {
    tool: SearchColleagueTool,
    profiles: Arc<PgProfileStore>,
    ctx: ToolCallContext,
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
    let shared: SharedProfileStore = profiles.clone();
    Fixture {
        tool: SearchColleagueTool::new(shared, FakeEmbeddingProvider::shared()),
        profiles,
        ctx,
    }
}

#[sqlx::test]
async fn finds_profiled_human_and_excludes_viewer(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let f = fixture(&pool, &seed).await;
    let human = resolve_user_colleague(&pool, seed.org_id, seed.user_id)
        .await
        .expect("human colleague");
    let agent = resolve_agent_colleague(&pool, seed.org_id, seed.agent_id)
        .await
        .expect("agent colleague");

    f.profiles
        .upsert(
            seed.org_id,
            &ColleagueProfile::new(
                human,
                Some(Role::try_from("Product Manager").expect("role")),
                None,
                None,
                None,
            ),
        )
        .await
        .expect("profile the human");

    let out = f
        .tool
        .execute(json!({ "query": "who owns the product roadmap" }), &f.ctx)
        .await
        .expect("search");
    let parsed: Value = serde_json::from_str(&out).expect("json");
    let matches = parsed["matches"].as_array().expect("matches array");

    let human_id = human.as_uuid().to_string();
    let agent_id = agent.as_uuid().to_string();
    let human_hit = matches
        .iter()
        .find(|m| m["id"].as_str() == Some(human_id.as_str()))
        .expect("profiled human is returned");
    assert_eq!(human_hit["kind"], "human");
    assert!(
        !matches
            .iter()
            .any(|m| m["id"].as_str() == Some(agent_id.as_str())),
        "viewer (the agent) is excluded"
    );
}

#[sqlx::test]
async fn out_of_range_limit_is_rejected(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let f = fixture(&pool, &seed).await;

    let err = f
        .tool
        .execute(json!({ "query": "anyone", "limit": 99 }), &f.ctx)
        .await
        .expect_err("limit over cap must reject");
    assert!(matches!(
        err,
        ToolError::Json(_) | ToolError::InvalidInput(_)
    ));
}

#[sqlx::test]
async fn empty_org_returns_no_matches(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let f = fixture(&pool, &seed).await;

    // Only the seeded agent (the viewer) exists; it is excluded, and no human is
    // profiled — so the result set is empty, not an error.
    let out = f
        .tool
        .execute(json!({ "query": "anyone at all" }), &f.ctx)
        .await
        .expect("search");
    let parsed: Value = serde_json::from_str(&out).expect("json");
    assert!(
        parsed["matches"].as_array().expect("array").is_empty(),
        "no discoverable colleagues yet"
    );
}
