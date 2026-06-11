//! Trait-contract tests for [`patom::agents::PgAgentStore`]: idempotent
//! seeding, default lookup, missing-agent error, and read round-trip.

#![allow(clippy::expect_used)]

use std::sync::Arc;

use std::collections::BTreeMap;

use patom::agents::{
    AgentDescription, AgentId, AgentName, AgentSeed, AgentStore, AgentStoreError,
    AgentSystemPrompt, AgentUpdate, AllowedMcpTools, AvatarIndex, NewAgent, PgAgentStore,
    preset_agent_avatar_url,
};
use patom::auth::OrgId;
use patom::clock::SystemClock;
use patom::mcp::McpCatalogId;
use patom::types::AvatarUrl;
use sqlx::PgPool;

mod common;
use common::pg::{agent_store, seed_tenant};

fn store(pool: &PgPool) -> Arc<PgAgentStore> {
    agent_store(pool.clone(), SystemClock::shared())
}

fn default_seed(name: &str, prompt: &str) -> AgentSeed {
    AgentSeed {
        name: AgentName::try_from(name).expect("valid name"),
        system_prompt: AgentSystemPrompt::try_from(prompt).expect("valid prompt"),
        description: AgentDescription::try_from("Default seed.").expect("valid desc"),
        avatar_url: None,
    }
}

fn new_agent(org_id: OrgId, name: &str, prompt: &str) -> NewAgent {
    NewAgent {
        org_id,
        name: AgentName::try_from(name).expect("valid name"),
        system_prompt: AgentSystemPrompt::try_from(prompt).expect("valid prompt"),
        description: AgentDescription::try_from(format!("Role: {name}")).expect("valid desc"),
        allowed_mcp_tools: AllowedMcpTools::empty(),
        model: None,
        avatar_url: None,
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

    // First seed: seed_tenant already inserted one ("test-default"). A second
    // call with the same name must return the same id rather than minting a
    // new row — preset idempotency is by (org, name).
    let again = store
        .seed_preset(seed.org_id, default_seed("test-default", "ignored"))
        .await
        .expect("seed again");
    assert_eq!(again, seed.agent_id);

    // Case-insensitive: the name-uniqueness index is on lower(name).
    let third = store
        .seed_preset(seed.org_id, default_seed("TEST-DEFAULT", "also-ignored"))
        .await
        .expect("seed third");
    assert_eq!(third, seed.agent_id);
}

#[sqlx::test]
async fn seed_default_does_not_overwrite_existing_prompt(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);

    // Re-seed (same name) with a different prompt; the existing row's prompt
    // must be preserved per the design conversation ("seed-only, no
    // overwrite").
    let _ = store
        .seed_preset(
            seed.org_id,
            default_seed("test-default", "this should be ignored"),
        )
        .await
        .expect("seed again");

    let record = store.read(seed.agent_id).await.expect("read");
    // Original prompt from seed_tenant wins.
    assert_eq!(record.system_prompt.as_str(), "test default prompt");
    assert_eq!(record.name.as_str(), "test-default");
}

#[sqlx::test]
async fn seed_default_agent_has_no_avatar(pool: PgPool) {
    // Issue #43: the migration adds avatar_url as nullable with no
    // default, so a seeded agent reads back `None`.
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);
    let record = store.read(seed.agent_id).await.expect("read");
    assert!(record.avatar_url.is_none(), "seed agent has no avatar");
}

#[sqlx::test]
async fn seed_preset_persists_default_avatar(pool: PgPool) {
    // When the asset CDN is configured the recruiter seed carries
    // `agent-1.png`; the URL must round-trip through the seed INSERT and the
    // SELECT/hydrate path unchanged.
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);

    let avatar = preset_agent_avatar_url("https://cdn.test", AvatarIndex::RECRUITER);
    let mut s = default_seed("avatared", "role prompt");
    s.avatar_url = Some(avatar.clone());
    let id = store
        .seed_preset(seed.org_id, s)
        .await
        .expect("seed avatared");

    let record = store.read(id).await.expect("read");
    assert_eq!(
        record.avatar_url.as_ref().map(AvatarUrl::as_str),
        Some("https://cdn.test/agents/agent-1.png"),
    );
}

#[sqlx::test]
async fn create_persists_avatar_url_and_reads_back(pool: PgPool) {
    // Issue #43: a `NewAgent.avatar_url` round-trips through INSERT and
    // the SELECT/hydrate path.
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);
    let url = AvatarUrl::try_from("https://cdn.example/atlas.png").expect("valid url");
    let mut payload = new_agent(seed.org_id, "with-avatar", "be helpful");
    payload.avatar_url = Some(url.clone());

    let created = store.create(payload).await.expect("create");
    assert_eq!(
        created.avatar_url.as_ref().map(AvatarUrl::as_str),
        Some(url.as_str()),
        "create returns the avatar it persisted",
    );
    let read = store.read(created.id).await.expect("read");
    assert_eq!(
        read.avatar_url.as_ref().map(AvatarUrl::as_str),
        Some(url.as_str()),
        "avatar survives the SELECT/hydrate round-trip",
    );
}

#[sqlx::test]
async fn update_sets_then_clears_avatar_url(pool: PgPool) {
    // Issue #43: the tri-state PATCH — `Some(Some(_))` sets,
    // `Some(None)` clears to NULL, and omitting it (`None`) leaves the
    // column untouched.
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);
    let created = store
        .create(new_agent(seed.org_id, "patchable", "be helpful"))
        .await
        .expect("create");
    assert!(created.avatar_url.is_none());

    let url = AvatarUrl::try_from("https://cdn.example/patchable.png").expect("valid url");
    let set = store
        .update(
            created.id,
            AgentUpdate {
                avatar_url: Some(Some(url.clone())),
                ..Default::default()
            },
        )
        .await
        .expect("set avatar");
    assert_eq!(
        set.avatar_url.as_ref().map(AvatarUrl::as_str),
        Some(url.as_str()),
    );

    // Omitting the field leaves the avatar untouched.
    let untouched = store
        .update(
            created.id,
            AgentUpdate {
                name: Some(AgentName::try_from("patchable2").expect("name")),
                ..Default::default()
            },
        )
        .await
        .expect("rename only");
    assert_eq!(
        untouched.avatar_url.as_ref().map(AvatarUrl::as_str),
        Some(url.as_str()),
        "omitted avatar_url is left untouched",
    );

    // `Some(None)` clears it back to NULL.
    let cleared = store
        .update(
            created.id,
            AgentUpdate {
                avatar_url: Some(None),
                ..Default::default()
            },
        )
        .await
        .expect("clear avatar");
    assert!(cleared.avatar_url.is_none(), "Some(None) clears to NULL");
    let reread = store.read(created.id).await.expect("read");
    assert!(
        reread.avatar_url.is_none(),
        "cleared avatar persists as NULL"
    );
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
async fn create_then_list_round_trip(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);

    store
        .create(new_agent(seed.org_id, "alpha", "you are alpha"))
        .await
        .expect("create alpha");
    store
        .create(new_agent(seed.org_id, "beta", "you are beta"))
        .await
        .expect("create beta");

    let list = store.list().await.expect("list");
    // 1 seeded default + 2 new = 3 rows.
    assert_eq!(list.len(), 3);
    let names: Vec<&str> = list.iter().map(|r| r.name.as_str()).collect();
    assert!(names.contains(&"test-default"));
    assert!(names.contains(&"alpha"));
    assert!(names.contains(&"beta"));
}

#[sqlx::test]
async fn update_changes_name_and_prompt(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);

    let agent = store
        .create(new_agent(seed.org_id, "orig", "orig prompt"))
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
async fn delete_removes_row(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);

    let agent = store
        .create(new_agent(seed.org_id, "disposable", "throwaway"))
        .await
        .expect("create");
    store.delete(agent.id).await.expect("delete");

    let err = store.read(agent.id).await.expect_err("gone");
    assert!(matches!(err, AgentStoreError::NotFound(_)));
}

#[sqlx::test]
async fn create_default_allowed_mcp_tools_is_empty(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);

    // Operator opts in explicitly; absence of opt-in means no MCP tools.
    let agent = store
        .create(new_agent(seed.org_id, "scoped", "I have no MCP yet"))
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
    use patom::agents::ToolScope;
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);

    let payload = NewAgent {
        org_id: seed.org_id,
        name: AgentName::try_from("scoped").expect("name"),
        system_prompt: AgentSystemPrompt::try_from("scoped agent").expect("prompt"),
        description: AgentDescription::try_from("Scoped agent.").expect("desc"),
        allowed_mcp_tools: allowed(&["notion", "linear"]),
        model: None,
        avatar_url: None,
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
    use patom::agents::ToolScope;
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);

    let agent = store
        .create(NewAgent {
            org_id: seed.org_id,
            name: AgentName::try_from("rotates").expect("name"),
            system_prompt: AgentSystemPrompt::try_from("rotating MCP").expect("prompt"),
            description: AgentDescription::try_from("Rotating MCP agent.").expect("desc"),
            allowed_mcp_tools: allowed(&["notion", "linear"]),
            model: None,
            avatar_url: None,
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
                avatar_url: None,
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
                avatar_url: None,
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
                avatar_url: None,
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
        .create(new_agent(seed.org_id, "zeta", "z"))
        .await
        .expect("create zeta");
    let _alpha = store
        .create(new_agent(seed.org_id, "alpha", "a"))
        .await
        .expect("create alpha");
    let _mike = store
        .create(new_agent(seed.org_id, "mike", "m"))
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
        .create(new_agent(seed.org_id, "local", "in our org"))
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
