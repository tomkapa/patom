//! Behaviour-level tests for the `create_agent` tool.
//!
//! Exercises the tool through its public seam (`Tool::execute` with a wired
//! `ToolCallContext`) against a real Postgres-backed `AgentStore` so the
//! happy path, duplicate-name conflict, MCP allowlist passthrough, input
//! validation, and `is_default` lockdown all land on the same code path the
//! agent uses at runtime.

#![allow(clippy::expect_used)]

use relay_rs::agents::{AgentName, AllowedMcpTools, SharedAgentStore, ToolScope};
use relay_rs::clock::SystemClock;
use relay_rs::mcp::McpCatalogId;
use relay_rs::runtime::{PromptRequestId, RequestKindPayload};
use relay_rs::session::{PgSessionStore, SharedSessionStore};
use relay_rs::tools::system::CreateAgentTool;
use relay_rs::tools::{Tool, ToolCallContext, ToolError};
use relay_rs::types::Participant;
use serde_json::{Value, json};
use std::sync::Arc;
use uuid::Uuid;

mod common;
use common::pg::{TestDb, human_to_agent_session, shared_agent_store};

struct Fixture {
    tool: CreateAgentTool,
    agents: SharedAgentStore,
    ctx: ToolCallContext,
    viewer_agent_id: relay_rs::agents::AgentId,
    org_id: relay_rs::auth::OrgId,
}

async fn fixture(db: &TestDb) -> Fixture {
    let agents = shared_agent_store(db.pool.clone(), SystemClock::shared());
    let sessions: SharedSessionStore =
        Arc::new(PgSessionStore::new(db.pool.clone(), SystemClock::shared()));
    let session = human_to_agent_session(
        sessions.as_ref(),
        db.default_agent_id,
        db.default_org_id,
        db.default_user_id,
    )
    .await;
    let request_id = PromptRequestId::new();
    let ctx = ToolCallContext {
        session_id: session,
        viewer: Participant::agent(db.default_agent_id),
        root_request_id: request_id,
        request_id,
        kind_payload: RequestKindPayload::Normal {},
        acting_user_id: db.default_user_id,
        org_id: db.default_org_id,
    };
    Fixture {
        tool: CreateAgentTool::new(agents.clone()),
        agents,
        ctx,
        viewer_agent_id: db.default_agent_id,
        org_id: db.default_org_id,
    }
}

fn human_ctx(f: &Fixture) -> ToolCallContext {
    ToolCallContext {
        session_id: f.ctx.session_id,
        viewer: Participant::Human,
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

#[tokio::test(flavor = "multi_thread")]
async fn happy_path_persists_record_with_is_default_false_and_empty_mcp() {
    let db = TestDb::fresh().await;
    let f = fixture(&db).await;

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

#[tokio::test(flavor = "multi_thread")]
async fn allowlist_round_trips_on_persisted_record() {
    let db = TestDb::fresh().await;
    let f = fixture(&db).await;

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

#[tokio::test(flavor = "multi_thread")]
async fn duplicate_name_case_insensitive_returns_invalid_input() {
    let db = TestDb::fresh().await;
    let f = fixture(&db).await;

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

#[tokio::test(flavor = "multi_thread")]
async fn empty_name_is_rejected() {
    let db = TestDb::fresh().await;
    let f = fixture(&db).await;
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

#[tokio::test(flavor = "multi_thread")]
async fn empty_description_is_rejected() {
    let db = TestDb::fresh().await;
    let f = fixture(&db).await;
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

#[tokio::test(flavor = "multi_thread")]
async fn oversize_system_prompt_is_rejected() {
    let db = TestDb::fresh().await;
    let f = fixture(&db).await;
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

#[tokio::test(flavor = "multi_thread")]
async fn non_agent_viewer_is_rejected() {
    let db = TestDb::fresh().await;
    let f = fixture(&db).await;
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

#[tokio::test(flavor = "multi_thread")]
async fn is_default_is_rejected_by_schema() {
    let db = TestDb::fresh().await;
    let f = fixture(&db).await;
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
