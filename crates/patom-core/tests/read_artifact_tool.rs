//! #185 stages 14–15: the `read_artifact` system tool recovers offloaded tool
//! results on demand, is bounded (recursion fixpoint), and is org-isolated.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use patom::auth::OrgId;
use patom::clock::SystemClock;
use patom::colleagues::resolve_agent_colleague;
use patom::runtime::{ClaimKey, PromptRequestId, RequestKindPayload};
use patom::threads::{ArtifactHandle, NewToolArtifact, PgThreadStore, SharedThreadStore};
use patom::tools::limits::MAX_ARTIFACT_SLICE;
use patom::tools::system::ReadArtifactTool;
use patom::tools::{Tool, ToolCallContext, ToolError};
use patom::types::{Participant, ToolName};
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

mod common;
use common::pg::seed_tenant;

async fn write_big_artifact(threads: &SharedThreadStore, org: OrgId) -> (ArtifactHandle, String) {
    let body = format!("HEAD{}NEEDLE{}TAIL", "a".repeat(30_000), "b".repeat(30_000));
    let handle = ArtifactHandle::content_address(&body);
    threads
        .save_tool_artifact(NewToolArtifact {
            handle: handle.clone(),
            org_id: org,
            full_body: body.clone(),
            tokens: 15_000,
            tool_name: ToolName::try_from("big_result").expect("name"),
            agent_id: None,
            state_id: None,
            request_id: PromptRequestId::new(),
        })
        .await
        .expect("offload");
    (handle, body)
}

fn ctx_for(
    org: OrgId,
    agent_col: patom::colleagues::ColleagueId,
    agent: patom::agents::AgentId,
) -> ToolCallContext {
    let rid = PromptRequestId::new();
    ToolCallContext {
        claim_key: ClaimKey::from(Uuid::new_v4()),
        thread_id: None,
        state_id: None,
        viewer: Participant::agent(agent_col, agent),
        root_request_id: rid,
        request_id: rid,
        kind_payload: RequestKindPayload::Normal {},
        acting_user_id: patom::auth::UserId::new(),
        org_id: org,
    }
}

#[sqlx::test]
async fn read_artifact_pages_and_greps_within_bounds(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let threads: SharedThreadStore =
        Arc::new(PgThreadStore::new(pool.clone(), SystemClock::shared()));
    let agent_col = resolve_agent_colleague(&pool, seed.org_id, seed.agent_id)
        .await
        .expect("agent col");
    let (handle, _body) = write_big_artifact(&threads, seed.org_id).await;

    let tool = ReadArtifactTool::new(threads.clone());
    let ctx = ctx_for(seed.org_id, agent_col, seed.agent_id);

    // Page from the start.
    let head = tool
        .execute(
            json!({"handle": handle.as_str(), "offset": 0, "limit": 4}),
            &ctx,
        )
        .await
        .expect("page");
    assert_eq!(head, "HEAD");

    // Default (no offset/limit) is bounded to the recursion fixpoint, so feeding
    // it back through the dispatch seam would stay verbatim (never re-offloaded).
    let dflt = tool
        .execute(json!({"handle": handle.as_str()}), &ctx)
        .await
        .expect("default page");
    assert!(dflt.chars().count() <= MAX_ARTIFACT_SLICE);

    // Grep jumps to the match.
    let grep = tool
        .execute(json!({"handle": handle.as_str(), "grep": "NEEDLE"}), &ctx)
        .await
        .expect("grep");
    assert!(grep.contains("NEEDLE"));
    assert!(grep.chars().count() <= MAX_ARTIFACT_SLICE);
}

#[sqlx::test]
async fn read_artifact_is_org_isolated(pool: PgPool) {
    let owner = seed_tenant(&pool).await;
    let other = seed_tenant(&pool).await;
    let threads: SharedThreadStore =
        Arc::new(PgThreadStore::new(pool.clone(), SystemClock::shared()));
    let other_col = resolve_agent_colleague(&pool, other.org_id, other.agent_id)
        .await
        .expect("other col");
    let (handle, _body) = write_big_artifact(&threads, owner.org_id).await;

    let tool = ReadArtifactTool::new(threads.clone());
    // A caller in another org cannot read the handle — it surfaces as a
    // model-correctable invalid-input, not the bytes.
    let err = tool
        .execute(
            json!({"handle": handle.as_str(), "offset": 0, "limit": 10}),
            &ctx_for(other.org_id, other_col, other.agent_id),
        )
        .await
        .expect_err("must not leak across orgs");
    assert!(matches!(err, ToolError::InvalidInput(_)));
}
