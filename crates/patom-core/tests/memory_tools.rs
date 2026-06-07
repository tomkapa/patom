//! Trait-contract tests for the memory mutation tools (doc/memory.md
//! §1.5): `memory_write`, `memory_update`, `memory_forget`. They share a
//! per-turn counter so a single turn cannot exceed
//! [`MAX_MEMORY_MUTATIONS_PER_TURN`].
//!
//! `recall` lives in its own integration test alongside the embedding
//! provider stub.

#![allow(clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use patom::agents::{AgentNamesCache, AgentPromptCache, SharedAgentStore};
use patom::auth::{Language, SharedOrgLanguageResolver, SharedOrgRuleResolver};
use patom::clock::{SharedClock, SystemClock};
use patom::memory::{
    AgentMemory, MAX_MEMORY_MUTATIONS_PER_TURN, MemoryContent, MemoryHandle, MemoryKind,
    MemoryMutation, MemorySectionLoader, MemoryState, MutationSource, PgMemoryStore,
    SessionMemoryCache, SharedMemoryStore,
};
use patom::prompts::Prompts;
use patom::runtime::PromptRequestId;
use patom::session::{PgSessionStore, SharedSessionStore};
use patom::tools::system::{
    MemoryForgetTool, MemoryToolDeps, MemoryUpdateTool, MemoryValidateTool, MemoryWriteTool,
};
use patom::tools::{Tool, ToolCallContext, ToolError};
use patom::types::Participant;
use serde_json::json;

mod common;
use common::lang::StaticOrgLanguageResolver;
use common::pg::{human_to_agent_session, seed_prompt_request, seed_tenant};
use common::rule::StaticOrgRuleResolver;
use sqlx::PgPool;

struct Fixture {
    deps: MemoryToolDeps,
    loader: MemorySectionLoader,
    store: SharedMemoryStore,
    session: patom::session::SessionId,
    agent_id: patom::agents::AgentId,
    agent_colleague_id: patom::colleagues::ColleagueId,
    user_id: patom::auth::UserId,
    org_id: patom::auth::OrgId,
}

async fn fixture(pool: &PgPool, seed: &common::pg::Seed) -> Fixture {
    let clock: SharedClock = SystemClock::shared();
    let embeddings = common::embedding::FakeEmbeddingProvider::shared();
    let agents: SharedAgentStore = common::pg::shared_agent_store(pool.clone(), clock.clone());
    let sessions: SharedSessionStore = Arc::new(PgSessionStore::new(pool.clone(), clock.clone()));
    let prompt_cache = AgentPromptCache::new(8, Duration::from_mins(1), clock.clone());
    let names_cache = AgentNamesCache::new(16, Duration::from_mins(1), clock.clone());
    let store: SharedMemoryStore = Arc::new(PgMemoryStore::new(
        pool.clone(),
        clock.clone(),
        embeddings.clone(),
    ));
    let session_cache = SessionMemoryCache::new(8, Duration::from_mins(1), clock.clone());
    let colleagues: patom::colleagues::SharedColleagueStore =
        Arc::new(patom::colleagues::PgColleagueStore::new(pool.clone()));
    let loader = MemorySectionLoader::new(
        store.clone(),
        sessions.clone(),
        colleagues,
        embeddings,
        session_cache,
    );
    let prompts = Arc::new(Prompts::load());
    let language_resolver: SharedOrgLanguageResolver =
        Arc::new(StaticOrgLanguageResolver::new(Language::En));
    let rule_resolver: SharedOrgRuleResolver = Arc::new(StaticOrgRuleResolver::new(None));
    let _memory = AgentMemory::new(
        agents,
        prompt_cache,
        names_cache,
        loader.clone(),
        prompts,
        language_resolver,
        rule_resolver,
        clock,
    );
    let session =
        human_to_agent_session(pool, sessions.as_ref(), seed.agent_id, seed.org_id, seed.user_id).await;
    let agent_colleague_id =
        patom::colleagues::resolve_agent_colleague(pool, seed.org_id, seed.agent_id)
            .await
            .expect("seed agent colleague");
    Fixture {
        deps: MemoryToolDeps::new(loader.clone()),
        loader,
        store,
        session,
        agent_id: seed.agent_id,
        agent_colleague_id,
        user_id: seed.user_id,
        org_id: seed.org_id,
    }
}

fn ctx(f: &Fixture, request_id: PromptRequestId) -> ToolCallContext {
    ToolCallContext {
        session_id: f.session,
        viewer: Participant::agent(f.agent_colleague_id, f.agent_id),
        root_request_id: request_id,
        request_id,
        kind_payload: patom::runtime::RequestKindPayload::Normal {},
        acting_user_id: f.user_id,
        org_id: f.org_id,
    }
}

#[sqlx::test]
async fn memory_write_creates_tentative_row(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let f = fixture(&pool, &seed).await;
    let tool = MemoryWriteTool::new(f.deps.clone());
    let request = seed_prompt_request(&pool, f.session, f.agent_id, f.org_id).await;

    let out = tool
        .execute(
            json!({"kind": "self", "content": "I default to terse replies."}),
            &ctx(&f, request),
        )
        .await
        .expect("write");
    let parsed: serde_json::Value = serde_json::from_str(&out).expect("json");
    assert_eq!(parsed["state"], "tentative");
    assert_eq!(parsed["kind"], "self");

    let listed = f.store.list(f.agent_id).await.expect("list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].state, MemoryState::Tentative);
    assert_eq!(listed[0].kind, MemoryKind::Identity);
    assert!(!listed[0].pinned);
}

#[sqlx::test]
async fn memory_update_resolves_handle_and_resets_state(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let f = fixture(&pool, &seed).await;

    // Seed a memory at Held; the update tool should bring it back to
    // Tentative because the content changed.
    let written = f
        .store
        .apply(MemoryMutation::Write {
            agent: f.agent_id,
            kind: MemoryKind::Identity,
            content: MemoryContent::try_from("original").expect("c"),
            state: MemoryState::Held,
            pinned: false,
            subject: None,
            source: MutationSource::Operator,
        })
        .await
        .expect("seed");

    // Compose the section so the handle map is populated. We do this
    // through the same path the tool uses: a `resolve_handle` call.
    let request = seed_prompt_request(&pool, f.session, f.agent_id, f.org_id).await;
    let _ = MemoryWriteTool::new(f.deps.clone()); // ensure deps usable
    let update = MemoryUpdateTool::new(f.deps.clone());
    // The tool resolves M-1 via the session cache it shares.
    let out = update
        .execute(
            json!({"handle": "M-1", "content": "revised"}),
            &ctx(&f, request),
        )
        .await
        .expect("update");
    let parsed: serde_json::Value = serde_json::from_str(&out).expect("json");
    assert_eq!(parsed["memory_id"], written.memory_id.as_uuid().to_string());
    assert_eq!(parsed["state"], "tentative");

    let row = f
        .store
        .get(written.memory_id)
        .await
        .expect("get")
        .expect("present");
    assert_eq!(row.content.as_str(), "revised");
    assert_eq!(row.state, MemoryState::Tentative);
}

#[sqlx::test]
async fn memory_update_pinned_row_rejects_agent_call(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let f = fixture(&pool, &seed).await;
    let _written = f
        .store
        .apply(MemoryMutation::Write {
            agent: f.agent_id,
            kind: MemoryKind::Identity,
            content: MemoryContent::try_from("pinned").expect("c"),
            state: MemoryState::Core,
            pinned: true,
            subject: None,
            source: MutationSource::Operator,
        })
        .await
        .expect("seed pinned");

    let update = MemoryUpdateTool::new(f.deps.clone());
    let request = seed_prompt_request(&pool, f.session, f.agent_id, f.org_id).await;
    let err = update
        .execute(
            json!({"handle": "M-1", "content": "agent attempt"}),
            &ctx(&f, request),
        )
        .await
        .expect_err("agent edit blocked");
    assert!(matches!(err, ToolError::InvalidInput(_)), "got {err:?}");
}

#[sqlx::test]
async fn memory_validate_promotes_state_without_content_change(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let f = fixture(&pool, &seed).await;
    let written = f
        .store
        .apply(MemoryMutation::Write {
            agent: f.agent_id,
            kind: MemoryKind::Identity,
            content: MemoryContent::try_from("deploy mondays").expect("c"),
            state: MemoryState::Tentative,
            pinned: false,
            subject: None,
            source: MutationSource::Operator,
        })
        .await
        .expect("seed");

    let validate = MemoryValidateTool::new(f.deps.clone());
    let request = seed_prompt_request(&pool, f.session, f.agent_id, f.org_id).await;
    let out = validate
        .execute(
            json!({
                "handle": "M-1",
                "evidence": "web_search confirmed Monday deploy cadence on the team wiki"
            }),
            &ctx(&f, request),
        )
        .await
        .expect("validate");
    let parsed: serde_json::Value = serde_json::from_str(&out).expect("json");
    assert_eq!(parsed["memory_id"], written.memory_id.as_uuid().to_string());
    assert_eq!(parsed["state"], "held");

    let row = f
        .store
        .get(written.memory_id)
        .await
        .expect("get")
        .expect("present");
    assert_eq!(row.content.as_str(), "deploy mondays");
    assert_eq!(row.state, MemoryState::Held);
}

#[sqlx::test]
async fn memory_validate_rejects_pinned_row(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let f = fixture(&pool, &seed).await;
    let _ = f
        .store
        .apply(MemoryMutation::Write {
            agent: f.agent_id,
            kind: MemoryKind::Identity,
            content: MemoryContent::try_from("pinned belief").expect("c"),
            state: MemoryState::Core,
            pinned: true,
            subject: None,
            source: MutationSource::Operator,
        })
        .await
        .expect("seed pinned");

    let validate = MemoryValidateTool::new(f.deps.clone());
    let request = seed_prompt_request(&pool, f.session, f.agent_id, f.org_id).await;
    let err = validate
        .execute(
            json!({"handle": "M-1", "evidence": "external source agrees"}),
            &ctx(&f, request),
        )
        .await
        .expect_err("pinned validate blocked");
    assert!(matches!(err, ToolError::InvalidInput(_)), "got {err:?}");
}

#[sqlx::test]
async fn memory_forget_removes_row(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let f = fixture(&pool, &seed).await;
    let written = f
        .store
        .apply(MemoryMutation::Write {
            agent: f.agent_id,
            kind: MemoryKind::Identity,
            content: MemoryContent::try_from("disposable").expect("c"),
            state: MemoryState::Held,
            pinned: false,
            subject: None,
            source: MutationSource::Operator,
        })
        .await
        .expect("seed");

    let forget = MemoryForgetTool::new(f.deps.clone());
    let request = seed_prompt_request(&pool, f.session, f.agent_id, f.org_id).await;
    let out = forget
        .execute(json!({"handle": "M-1"}), &ctx(&f, request))
        .await
        .expect("forget");
    let parsed: serde_json::Value = serde_json::from_str(&out).expect("json");
    assert_eq!(parsed["status"], "forgotten");

    assert!(
        f.store.get(written.memory_id).await.expect("get").is_none(),
        "row should be gone"
    );
}

#[sqlx::test]
async fn unknown_handle_surfaces_invalid_input(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let f = fixture(&pool, &seed).await;
    let update = MemoryUpdateTool::new(f.deps.clone());
    let request = seed_prompt_request(&pool, f.session, f.agent_id, f.org_id).await;
    let err = update
        .execute(
            json!({"handle": "M-99", "content": "nope"}),
            &ctx(&f, request),
        )
        .await
        .expect_err("should fail");
    assert!(matches!(err, ToolError::InvalidInput(_)), "got {err:?}");
}

#[sqlx::test]
async fn per_turn_cap_blocks_overflow_within_one_request_id(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let f = fixture(&pool, &seed).await;
    let write = MemoryWriteTool::new(f.deps.clone());
    let request = seed_prompt_request(&pool, f.session, f.agent_id, f.org_id).await;

    // The first MAX_MEMORY_MUTATIONS_PER_TURN writes succeed.
    for i in 0..MAX_MEMORY_MUTATIONS_PER_TURN {
        write
            .execute(
                json!({"kind": "self", "content": format!("memory {i}")}),
                &ctx(&f, request),
            )
            .await
            .expect("under cap");
    }
    let err = write
        .execute(
            json!({"kind": "self", "content": "one too many"}),
            &ctx(&f, request),
        )
        .await
        .expect_err("over cap");
    assert!(
        matches!(&err, ToolError::InvalidInput(s) if s.contains("cap")),
        "got {err:?}"
    );

    // A different request id has its own quota.
    let new_req = seed_prompt_request(&pool, f.session, f.agent_id, f.org_id).await;
    write
        .execute(
            json!({"kind": "self", "content": "fresh turn"}),
            &ctx(&f, new_req),
        )
        .await
        .expect("fresh turn ok");
}

#[sqlx::test]
async fn handle_round_trips_through_session_cache(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let f = fixture(&pool, &seed).await;
    let store = f.store.clone();
    let written = store
        .apply(MemoryMutation::Write {
            agent: f.agent_id,
            kind: MemoryKind::Identity,
            content: MemoryContent::try_from("first").expect("c"),
            state: MemoryState::Held,
            pinned: false,
            subject: None,
            source: MutationSource::Operator,
        })
        .await
        .expect("seed");

    // Resolve via the same loader the tools use; M-1 should map to the
    // seeded memory id.
    let agents: SharedAgentStore =
        common::pg::shared_agent_store(pool.clone(), SystemClock::shared());
    let memory = AgentMemory::new(
        agents,
        AgentPromptCache::new(2, Duration::from_mins(1), SystemClock::shared()),
        AgentNamesCache::new(16, Duration::from_mins(1), SystemClock::shared()),
        f.loader.clone(),
        Arc::new(Prompts::load()),
        Arc::new(StaticOrgLanguageResolver::new(Language::En)),
        Arc::new(StaticOrgRuleResolver::new(None)),
        SystemClock::shared(),
    );
    let resolved = memory
        .resolve_handle(
            f.session,
            f.agent_id,
            &patom::runtime::RequestKindPayload::Normal {},
            MemoryHandle::try_from(1u32).expect("h"),
        )
        .await
        .expect("resolve");
    assert_eq!(resolved, Some(written.memory_id));
}

/// Build a ToolCallContext with a Resolution `kind_payload` attached.
/// Mirrors what the worker forwards for `RequestKind::Resolution` claims.
fn ctx_with_target(
    f: &Fixture,
    request_id: PromptRequestId,
    target: patom::memory::ContradictionEventId,
) -> ToolCallContext {
    ToolCallContext {
        session_id: f.session,
        viewer: Participant::agent(f.agent_colleague_id, f.agent_id),
        root_request_id: request_id,
        request_id,
        kind_payload: patom::runtime::RequestKindPayload::Resolution {
            contradiction_event_id: target,
        },
        acting_user_id: f.user_id,
        org_id: f.org_id,
    }
}

async fn seed_pair_and_contradiction(
    f: &Fixture,
) -> (
    patom::memory::MemoryId,
    patom::memory::MemoryId,
    patom::memory::ContradictionEventId,
) {
    let a = f
        .store
        .apply(MemoryMutation::Write {
            agent: f.agent_id,
            kind: MemoryKind::Identity,
            content: MemoryContent::try_from("ship on Friday").expect("c"),
            state: MemoryState::Held,
            pinned: false,
            subject: None,
            source: MutationSource::Operator,
        })
        .await
        .expect("a");
    let b = f
        .store
        .apply(MemoryMutation::Write {
            agent: f.agent_id,
            kind: MemoryKind::Identity,
            content: MemoryContent::try_from("don't ship on Friday").expect("c"),
            state: MemoryState::Held,
            pinned: false,
            subject: None,
            source: MutationSource::Operator,
        })
        .await
        .expect("b");
    let id = f
        .store
        .record_contradiction(f.agent_id, a.memory_id, b.memory_id, "test")
        .await
        .expect("contradiction");
    (a.memory_id, b.memory_id, id)
}

#[sqlx::test]
async fn memory_update_with_resolution_target_closes_contradiction(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let f = fixture(&pool, &seed).await;
    let (_, _, target) = seed_pair_and_contradiction(&f).await;

    let update = MemoryUpdateTool::new(f.deps.clone());
    let request = seed_prompt_request(&pool, f.session, f.agent_id, f.org_id).await;
    let _ = update
        .execute(
            json!({"handle": "M-1", "content": "ship on Friday after standup"}),
            &ctx_with_target(&f, request, target),
        )
        .await
        .expect("update");

    let row = f
        .store
        .read_contradiction(target)
        .await
        .expect("read")
        .expect("row");
    assert!(row.resolved_at.is_some(), "contradiction should be closed");
    assert!(
        row.resolution_event_id.is_some(),
        "mutation close points at the new event"
    );
    assert_eq!(row.resolution_reason, None);
}

#[sqlx::test]
async fn memory_forget_with_resolution_target_closes_contradiction(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let f = fixture(&pool, &seed).await;
    let (_, _, target) = seed_pair_and_contradiction(&f).await;

    let forget = MemoryForgetTool::new(f.deps.clone());
    let request = seed_prompt_request(&pool, f.session, f.agent_id, f.org_id).await;
    let _ = forget
        .execute(
            json!({"handle": "M-1"}),
            &ctx_with_target(&f, request, target),
        )
        .await
        .expect("forget");

    let row = f
        .store
        .read_contradiction(target)
        .await
        .expect("read")
        .expect("row");
    assert!(row.resolved_at.is_some());
    assert!(row.resolution_event_id.is_some());
    assert_eq!(row.resolution_reason, None);
}

#[sqlx::test]
async fn memory_update_without_resolution_target_leaves_contradiction_open(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let f = fixture(&pool, &seed).await;
    let (_, _, target) = seed_pair_and_contradiction(&f).await;

    let update = MemoryUpdateTool::new(f.deps.clone());
    let request = seed_prompt_request(&pool, f.session, f.agent_id, f.org_id).await;
    let _ = update
        .execute(
            json!({"handle": "M-1", "content": "unrelated tweak"}),
            &ctx(&f, request),
        )
        .await
        .expect("update");

    let row = f
        .store
        .read_contradiction(target)
        .await
        .expect("read")
        .expect("row");
    assert!(
        row.resolved_at.is_none(),
        "no resolution target -> contradiction stays pending"
    );
}
