//! Behaviour-level tests for the `create_agent` tool.
//!
//! Exercises the tool through its public seam (`Tool::execute` with a wired
//! `ToolCallContext`) against a real Postgres-backed `AgentStore` so the
//! happy path, duplicate-name conflict, MCP allowlist passthrough, input
//! validation, and `is_default` lockdown all land on the same code path the
//! agent uses at runtime.

#![allow(clippy::expect_used)]

use patom::agents::{AgentName, AllowedMcpTools, SharedAgentStore, ToolScope};
use patom::clock::SystemClock;
use patom::mcp::McpCatalogId;
use patom::runtime::{ClaimKey, PromptRequestId, RequestKindPayload};
use patom::tools::system::CreateAgentTool;
use patom::tools::{Tool, ToolCallContext, ToolError};
use serde_json::{Value, json};
use uuid::Uuid;

mod common;
use common::pg::{seed_tenant, shared_agent_store};
use sqlx::PgPool;

struct Fixture {
    tool: CreateAgentTool,
    agents: SharedAgentStore,
    ctx: ToolCallContext,
    viewer_agent_id: patom::agents::AgentId,
    org_id: patom::auth::OrgId,
    user_id: patom::auth::UserId,
    user_colleague_id: patom::colleagues::ColleagueId,
}

async fn fixture(pool: &PgPool, seed: &common::pg::Seed) -> Fixture {
    let agents = shared_agent_store(pool.clone(), SystemClock::shared());
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
    let user_colleague_id =
        patom::colleagues::resolve_user_colleague(pool, seed.org_id, seed.user_id)
            .await
            .expect("user colleague");
    Fixture {
        tool: CreateAgentTool::new(agents.clone()),
        agents,
        ctx,
        viewer_agent_id: seed.agent_id,
        org_id: seed.org_id,
        user_id: seed.user_id,
        user_colleague_id,
    }
}

fn human_ctx(f: &Fixture) -> ToolCallContext {
    ToolCallContext {
        claim_key: f.ctx.claim_key,
        thread_id: None,
        state_id: None,
        viewer: patom::types::Participant::human(f.user_colleague_id, f.user_id),
        root_request_id: f.ctx.root_request_id,
        request_id: f.ctx.request_id,
        kind_payload: RequestKindPayload::Normal {},
        acting_user_id: f.ctx.acting_user_id,
        org_id: f.org_id,
    }
}

fn valid_input(name: &str) -> Value {
    json!({
        "name": name,
        "system_prompt": format!(
            "You are the {name}. Report to the human; escalate translation ambiguity to editor."
        ),
        "description": format!("{name} role for testing"),
    })
}

#[sqlx::test]
async fn happy_path_persists_record_with_is_default_false_and_empty_mcp(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let f = fixture(&pool, &seed).await;

    let out = f
        .tool
        .execute(valid_input("translator"), &f.ctx)
        .await
        .expect("create translator");
    let parsed: Value = serde_json::from_str(&out).expect("json");
    assert_eq!(parsed["name"], "translator");
    let agent_id_str = parsed["agent_id"].as_str().expect("agent_id string");
    let parsed_id: Uuid = agent_id_str.parse().expect("agent_id is uuid");

    let name = AgentName::try_from("translator").expect("name");
    let record = f
        .agents
        .read_by_name_for_viewer(f.viewer_agent_id, &name)
        .await
        .expect("read");
    assert_eq!(record.id.as_uuid(), parsed_id);
    assert!(!record.is_default);
    assert_eq!(record.allowed_mcp_tools, AllowedMcpTools::empty());
    assert!(
        record
            .system_prompt
            .as_str()
            .contains("escalate translation ambiguity")
    );
    assert_ne!(record.id, f.viewer_agent_id);
}

#[sqlx::test]
async fn allowlist_round_trips_on_persisted_record(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let f = fixture(&pool, &seed).await;

    // After the catalog rekey, allowlist keys are stable catalog ids
    // (notion / linear / …) rather than per-tenant server UUIDs.
    let cat_notion = McpCatalogId::try_from("notion").expect("catalog id");
    let cat_linear = McpCatalogId::try_from("linear").expect("catalog id");
    let input = json!({
        "name": "ops",
        "system_prompt": "You are the ops agent. Report to the human.",
        "description": "ops role for testing",
        // notion: every tool. linear: only `issues.create`.
        "allowed_mcp_tools": {
            "notion": null,
            "linear": ["issues.create"],
        },
    });

    let out = f.tool.execute(input, &f.ctx).await.expect("create ops");
    let parsed: Value = serde_json::from_str(&out).expect("json");
    let agent_id_str = parsed["agent_id"].as_str().expect("agent_id string");
    let agent_uuid: Uuid = agent_id_str.parse().expect("uuid");

    let name = AgentName::try_from("ops").expect("name");
    let record = f
        .agents
        .read_by_name_for_viewer(f.viewer_agent_id, &name)
        .await
        .expect("read");
    assert_eq!(record.id.as_uuid(), agent_uuid);
    assert_eq!(record.allowed_mcp_tools.len(), 2);
    assert!(matches!(
        record.allowed_mcp_tools.tools_for_catalog(&cat_notion),
        ToolScope::All
    ));
    let set_linear = match record.allowed_mcp_tools.tools_for_catalog(&cat_linear) {
        ToolScope::Some(set) => set,
        other => panic!("expected Some, got {other:?}"),
    };
    assert_eq!(set_linear.len(), 1);
    assert!(set_linear.iter().any(|n| n.as_str() == "issues.create"));
}

#[sqlx::test]
async fn duplicate_name_case_insensitive_returns_invalid_input(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let f = fixture(&pool, &seed).await;

    f.tool
        .execute(valid_input("translator"), &f.ctx)
        .await
        .expect("first create");

    let err = f
        .tool
        .execute(valid_input("Translator"), &f.ctx)
        .await
        .expect_err("duplicate name must fail");
    match err {
        ToolError::InvalidInput(msg) => assert!(msg.to_lowercase().contains("taken")),
        other => panic!("expected InvalidInput, got {other:?}"),
    }
}

#[sqlx::test]
async fn empty_name_is_rejected(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let f = fixture(&pool, &seed).await;
    let err = f
        .tool
        .execute(
            json!({
                "name": "",
                "system_prompt": "you are a thing",
                "description": "x",
            }),
            &f.ctx,
        )
        .await
        .expect_err("empty name");
    assert!(matches!(
        err,
        ToolError::InvalidInput(_) | ToolError::Json(_)
    ));
}

#[sqlx::test]
async fn empty_description_is_rejected(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let f = fixture(&pool, &seed).await;
    let err = f
        .tool
        .execute(
            json!({
                "name": "ghost",
                "system_prompt": "you are the ghost agent",
                "description": "   ",
            }),
            &f.ctx,
        )
        .await
        .expect_err("empty description");
    assert!(matches!(
        err,
        ToolError::InvalidInput(_) | ToolError::Json(_)
    ));
}

#[sqlx::test]
async fn oversize_system_prompt_is_rejected(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let f = fixture(&pool, &seed).await;
    let big = "x".repeat(70_000);
    let err = f
        .tool
        .execute(
            json!({
                "name": "huge",
                "system_prompt": big,
                "description": "huge role",
            }),
            &f.ctx,
        )
        .await
        .expect_err("oversize prompt");
    assert!(matches!(
        err,
        ToolError::InvalidInput(_) | ToolError::Json(_)
    ));
}

#[sqlx::test]
async fn non_agent_viewer_is_rejected(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let f = fixture(&pool, &seed).await;
    let err = f
        .tool
        .execute(valid_input("intern"), &human_ctx(&f))
        .await
        .expect_err("human cannot call create_agent");
    match err {
        ToolError::InvalidInput(msg) => assert!(msg.to_lowercase().contains("agent")),
        other => panic!("expected InvalidInput, got {other:?}"),
    }
}

#[sqlx::test]
async fn is_default_is_rejected_by_schema(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let f = fixture(&pool, &seed).await;
    let err = f
        .tool
        .execute(
            json!({
                "name": "usurper",
                "system_prompt": "you are the usurper agent",
                "description": "tries to become default",
                "is_default": true,
            }),
            &f.ctx,
        )
        .await
        .expect_err("deny_unknown_fields must reject is_default");
    assert!(matches!(
        err,
        ToolError::InvalidInput(_) | ToolError::Json(_)
    ));
    let name = AgentName::try_from("usurper").expect("name");
    assert!(
        f.agents
            .read_by_name_for_viewer(f.viewer_agent_id, &name)
            .await
            .is_err()
    );
    let default = f
        .agents
        .default_id_for(f.org_id)
        .await
        .expect("default present");
    assert_eq!(default, f.viewer_agent_id);
}
