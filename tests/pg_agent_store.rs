//! Trait-contract tests for [`patom_rs::agents::PgAgentStore`]: idempotent
//! seeding, default lookup, missing-agent error, and read round-trip.

#![allow(clippy::expect_used)]

use std::sync::Arc;

use std::collections::BTreeMap;

use patom_rs::agents::{
    AgentDescription, AgentId, AgentName, AgentStore, AgentStoreError, AgentSystemPrompt,
    AgentUpdate, AllowedMcpTools, DefaultAgentSeed, NewAgent, PgAgentStore,
};
use patom_rs::auth::OrgId;
use patom_rs::clock::SystemClock;
use patom_rs::mcp::McpCatalogId;
use patom_rs::session::PgSessionStore;
use sqlx::PgPool;

mod common;
use common::pg::{agent_store, human_to_agent_session, seed_tenant};

fn store(pool: &PgPool) -> Arc<PgAgentStore> {
    agent_store(pool.clone(), SystemClock::shared())
}

fn default_seed(name: &str, prompt: &str) -> DefaultAgentSeed {
    DefaultAgentSeed {
        name: AgentName::try_from(name).expect("valid name"),
        system_prompt: AgentSystemPrompt::try_from(prompt).expect("valid prompt"),
        description: AgentDescription::try_from("Default seed.").expect("valid desc"),
    }
}

fn new_agent(org_id: OrgId, name: &str, prompt: &str, is_default: bool) -> NewAgent {
    NewAgent {
        org_id,
        name: AgentName::try_from(name).expect("valid name"),
        system_prompt: AgentSystemPrompt::try_from(prompt).expect("valid prompt"),
        description: AgentDescription::try_from(format!("Role: {name}")).expect("valid desc"),
        is_default,
        allowed_mcp_tools: AllowedMcpTools::empty(),
        model: None,
        edited_by: None,
    }
}

/// Grant each catalog id full ("all tools") access. After the catalog
/// rekey the allowlist is keyed by stable catalog ids rather than wired-
/// server UUIDs.
fn allowed(ids: &[&str]) -> AllowedMcpTools {
    let mut raw: BTreeMap<String, Option<Vec<String>>> = BTreeMap::new();
    for id in ids {
        raw.insert((*id).to_owned(), None);
    }
    AllowedMcpTools::try_from(raw).expect("under cap")
}

fn cat(id: &str) -> McpCatalogId {
    McpCatalogId::try_from(id).expect("valid catalog id")
}

#[sqlx::test]
async fn seed_default_is_idempotent(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);

    // First seed: seed_tenant already inserted one. A second call must return
    // the same id rather than minting a new row.
    let again = store
        .seed_default(seed.org_id, default_seed("ignored", "ignored"))
        .await
        .expect("seed again");
    assert_eq!(again, seed.agent_id);

    // Third call from a totally fresh seed payload still resolves to the same row.
    let third = store
        .seed_default(seed.org_id, default_seed("also-ignored", "also-ignored"))
        .await
        .expect("seed third");
    assert_eq!(third, seed.agent_id);
}

#[sqlx::test]
async fn seed_default_does_not_overwrite_existing_prompt(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);

    // Re-seed with a different prompt; the existing row's prompt must be
    // preserved per the design conversation ("seed-only, no overwrite").
    let _ = store
        .seed_default(
            seed.org_id,
            default_seed("new-name", "this should be ignored"),
        )
        .await
        .expect("seed again");

    let record = store.read(seed.agent_id).await.expect("read");
    assert!(record.is_default);
    // Original prompt from seed_tenant wins.
    assert_eq!(record.system_prompt.as_str(), "test default prompt");
    assert_eq!(record.name.as_str(), "test-default");
}

#[sqlx::test]
async fn read_unknown_returns_not_found(pool: PgPool) {
    let _seed = seed_tenant(&pool).await;
    let store = store(&pool);

    let phantom = AgentId::new();
    let err = store.read(phantom).await.expect_err("not present");
    assert!(matches!(err, AgentStoreError::NotFound(_)));
}

#[sqlx::test]
async fn default_id_returns_seeded_row(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);

    let id = store.default_id_for(seed.org_id).await.expect("default");
    assert_eq!(id, seed.agent_id);
}

#[sqlx::test]
async fn create_then_list_round_trip(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);

    let a = store
        .create(new_agent(seed.org_id, "alpha", "you are alpha", false))
        .await
        .expect("create alpha");
    let b = store
        .create(new_agent(seed.org_id, "beta", "you are beta", false))
        .await
        .expect("create beta");
    assert!(!a.is_default);
    assert!(!b.is_default);

    let list = store.list().await.expect("list");
    // 1 seeded default + 2 new = 3 rows.
    assert_eq!(list.len(), 3);
    let names: Vec<&str> = list.iter().map(|r| r.name.as_str()).collect();
    assert!(names.contains(&"test-default"));
    assert!(names.contains(&"alpha"));
    assert!(names.contains(&"beta"));
}

#[sqlx::test]
async fn create_with_is_default_demotes_previous_default(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);

    let promoted = store
        .create(new_agent(
            seed.org_id,
            "new-default",
            "I am the new default",
            true,
        ))
        .await
        .expect("create promoted");
    assert!(promoted.is_default);

    // The previously-seeded default has been demoted in the same transaction.
    let old = store.read(seed.agent_id).await.expect("read old");
    assert!(!old.is_default);
    // And there is exactly one default now.
    let now_default = store.default_id_for(seed.org_id).await.expect("default");
    assert_eq!(now_default, promoted.id);
}

#[sqlx::test]
async fn update_promotes_to_default_atomically(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);

    let other = store
        .create(new_agent(seed.org_id, "other", "I am other", false))
        .await
        .expect("create other");
    assert!(!other.is_default);

    let promoted = store
        .update(
            other.id,
            AgentUpdate {
                is_default: Some(true),
                ..Default::default()
            },
        )
        .await
        .expect("promote");
    assert!(promoted.is_default);

    let old = store.read(seed.agent_id).await.expect("read old");
    assert!(!old.is_default);
    let now_default = store.default_id_for(seed.org_id).await.expect("default");
    assert_eq!(now_default, other.id);
}

#[sqlx::test]
async fn update_cannot_demote_only_default(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);

    let err = store
        .update(
            seed.agent_id,
            AgentUpdate {
                is_default: Some(false),
                ..Default::default()
            },
        )
        .await
        .expect_err("cannot demote");
    assert!(matches!(err, AgentStoreError::DefaultDeletionForbidden));
}

#[sqlx::test]
async fn update_changes_name_and_prompt(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);

    let agent = store
        .create(new_agent(seed.org_id, "orig", "orig prompt", false))
        .await
        .expect("create");
    let updated = store
        .update(
            agent.id,
            AgentUpdate {
                name: Some(AgentName::try_from("renamed").expect("name")),
                system_prompt: Some(AgentSystemPrompt::try_from("rolled-out v2").expect("prompt")),
                ..Default::default()
            },
        )
        .await
        .expect("update");

    assert_eq!(updated.name.as_str(), "renamed");
    assert_eq!(updated.system_prompt.as_str(), "rolled-out v2");
    assert_eq!(updated.id, agent.id);
}

#[sqlx::test]
async fn delete_removes_non_default_row(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);

    let agent = store
        .create(new_agent(seed.org_id, "disposable", "throwaway", false))
        .await
        .expect("create");
    store.delete(agent.id).await.expect("delete");

    let err = store.read(agent.id).await.expect_err("gone");
    assert!(matches!(err, AgentStoreError::NotFound(_)));
}

#[sqlx::test]
async fn delete_refuses_default(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);

    let err = store.delete(seed.agent_id).await.expect_err("forbidden");
    assert!(matches!(err, AgentStoreError::DefaultDeletionForbidden));
}

#[sqlx::test]
async fn create_default_allowed_mcp_tools_is_empty(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);

    // Operator opts in explicitly; absence of opt-in means no MCP tools.
    let agent = store
        .create(new_agent(seed.org_id, "scoped", "I have no MCP yet", false))
        .await
        .expect("create");
    assert!(agent.allowed_mcp_tools.is_empty());

    // The seeded default agent is also empty — the migration's column default
    // is `'{}'::jsonb` so existing rows round-trip into an empty allowlist.
    let default = store.read(seed.agent_id).await.expect("read default");
    assert!(default.allowed_mcp_tools.is_empty());
}

#[sqlx::test]
async fn create_with_explicit_allowed_mcp_tools_round_trips(pool: PgPool) {
    use patom_rs::agents::ToolScope;
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);

    let payload = NewAgent {
        org_id: seed.org_id,
        name: AgentName::try_from("scoped").expect("name"),
        system_prompt: AgentSystemPrompt::try_from("scoped agent").expect("prompt"),
        description: AgentDescription::try_from("Scoped agent.").expect("desc"),
        is_default: false,
        allowed_mcp_tools: allowed(&["notion", "linear"]),
        model: None,
        edited_by: None,
    };
    let created = store.create(payload).await.expect("create");
    assert_eq!(created.allowed_mcp_tools.len(), 2);
    assert!(matches!(
        created.allowed_mcp_tools.tools_for_catalog(&cat("notion")),
        ToolScope::All
    ));
    assert!(matches!(
        created.allowed_mcp_tools.tools_for_catalog(&cat("linear")),
        ToolScope::All
    ));

    let reread = store.read(created.id).await.expect("read");
    assert!(matches!(
        reread.allowed_mcp_tools.tools_for_catalog(&cat("notion")),
        ToolScope::All
    ));
    assert!(matches!(
        reread.allowed_mcp_tools.tools_for_catalog(&cat("linear")),
        ToolScope::All
    ));
}

#[sqlx::test]
async fn update_replaces_allowed_mcp_tools(pool: PgPool) {
    use patom_rs::agents::ToolScope;
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);

    let agent = store
        .create(NewAgent {
            org_id: seed.org_id,
            name: AgentName::try_from("rotates").expect("name"),
            system_prompt: AgentSystemPrompt::try_from("rotating MCP").expect("prompt"),
            description: AgentDescription::try_from("Rotating MCP agent.").expect("desc"),
            is_default: false,
            allowed_mcp_tools: allowed(&["notion", "linear"]),
            model: None,
            edited_by: None,
        })
        .await
        .expect("create");

    let updated = store
        .update(
            agent.id,
            AgentUpdate {
                allowed_mcp_tools: Some(allowed(&["jira"])),
                model: None,
                edited_by: None,
                ..Default::default()
            },
        )
        .await
        .expect("update");
    assert_eq!(updated.allowed_mcp_tools.len(), 1);
    assert!(matches!(
        updated.allowed_mcp_tools.tools_for_catalog(&cat("jira")),
        ToolScope::All
    ));

    // Empty map via Some(empty) is the explicit lockdown path.
    let locked = store
        .update(
            agent.id,
            AgentUpdate {
                allowed_mcp_tools: Some(AllowedMcpTools::empty()),
                model: None,
                edited_by: None,
                ..Default::default()
            },
        )
        .await
        .expect("update");
    assert!(locked.allowed_mcp_tools.is_empty());

    // Field omitted (None) leaves the column unchanged.
    let restored_first = store
        .update(
            agent.id,
            AgentUpdate {
                allowed_mcp_tools: Some(allowed(&["notion"])),
                model: None,
                edited_by: None,
                ..Default::default()
            },
        )
        .await
        .expect("update");
    let after_noop = store
        .update(
            agent.id,
            AgentUpdate {
                name: Some(AgentName::try_from("renamed-only").expect("name")),
                ..Default::default()
            },
        )
        .await
        .expect("update");
    assert_eq!(
        after_noop.allowed_mcp_tools,
        restored_first.allowed_mcp_tools
    );
}

#[sqlx::test]
async fn list_for_org_returns_alphabetised_pairs_scoped_to_org(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);

    let _zeta = store
        .create(new_agent(seed.org_id, "zeta", "z", false))
        .await
        .expect("create zeta");
    let _alpha = store
        .create(new_agent(seed.org_id, "alpha", "a", false))
        .await
        .expect("create alpha");
    let _mike = store
        .create(new_agent(seed.org_id, "mike", "m", false))
        .await
        .expect("create mike");

    let pairs = store.list_for_org(seed.org_id).await.expect("list_for_org");

    // Seeded default + 3 created = 4. Order is alphabetical by lower(name).
    let names: Vec<&str> = pairs.iter().map(|(_, n)| n.as_str()).collect();
    assert_eq!(names, vec!["alpha", "mike", "test-default", "zeta"]);
}

#[sqlx::test]
async fn list_for_org_excludes_other_orgs(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);

    let _local = store
        .create(new_agent(seed.org_id, "local", "in our org", false))
        .await
        .expect("create local");

    // A different org id should return zero rows. We don't bother
    // seeding that org — the only assertion is "different org_id ⇒
    // empty result", and SELECT against an absent org_id returns 0
    // regardless of RLS state.
    let other = OrgId::new();
    let pairs = store.list_for_org(other).await.expect("list_for_org other");
    assert!(pairs.is_empty(), "got: {pairs:?}");
}

#[sqlx::test]
async fn delete_refuses_when_referenced_by_a_session(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);

    let agent = store
        .create(new_agent(seed.org_id, "attached", "in use", false))
        .await
        .expect("create");
    let sessions = PgSessionStore::new(pool.clone(), SystemClock::shared());
    let _ = human_to_agent_session(&sessions, agent.id, seed.org_id, seed.user_id).await;

    let err = store.delete(agent.id).await.expect_err("in use");
    assert!(matches!(err, AgentStoreError::InUse(_)));
}
