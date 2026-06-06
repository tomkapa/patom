//! Behaviour tests for [`patom::memory::AgentMemory`] + the underlying
//! caches and composer (doc/memory.md §1.3).
//!
//! Proves the assembled `system` prompt has the expected
//! `<core>...</core>` / `<role>...</role>` / `<memory>...</memory>`
//! structure, that an admin's edit to an agent row is visible to live
//! workers within the cache TTL, and that the per-session memory section
//! is frozen for the session's lifetime.

#![allow(clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use patom::agents::{
    AgentNamesCache, AgentPromptCache, AgentSystemPrompt, AgentUpdate, SharedAgentStore,
};
use patom::auth::OrganizationRule;
use patom::auth::{Language, SharedOrgLanguageResolver, SharedOrgRuleResolver};
use patom::clock::{SharedClock, SystemClock, TestClock};
use patom::memory::{
    AgentMemory, CORE_TAG_CLOSE, CORE_TAG_OPEN, DATE_TAG_CLOSE, DATE_TAG_OPEN, MEMORY_TAG_CLOSE,
    MEMORY_TAG_OPEN, Memory, MemoryContent, MemoryHandle, MemoryKind, MemoryMutation,
    MemorySectionLoader, MemoryState, MutationSource, ORG_RULE_TAG_CLOSE, ORG_RULE_TAG_OPEN,
    PgMemoryStore, ROLE_TAG_CLOSE, ROLE_TAG_OPEN, SessionMemoryCache, SharedMemoryStore,
};
use patom::prompts::Prompts;
use patom::session::{PgSessionStore, SharedSessionStore};
use patom::types::Participant;
use sqlx::PgPool;

mod common;
use common::lang::StaticOrgLanguageResolver;
use common::pg::{human_to_agent_session, seed_tenant};
use common::rule::StaticOrgRuleResolver;

/// Marker substring used by the original ordering tests to confirm the
/// `<core>` body is present. Matches a stable phrase in the real English
/// `<identity>` block (see `src/prompts/internal.toml`).
const CORE_MARKER: &str = "thoughtful, professional teammate";

struct Fixture {
    memory: AgentMemory,
    sessions: SharedSessionStore,
    store: SharedMemoryStore,
}

fn build_memory(pool: &PgPool, seed: &common::pg::Seed, clock: SharedClock) -> Fixture {
    build_memory_with_rule(pool, seed, clock, None)
}

fn build_memory_with_rule(
    pool: &PgPool,
    seed: &common::pg::Seed,
    clock: SharedClock,
    rule: Option<OrganizationRule>,
) -> Fixture {
    let _ = seed; // ids accessed via seed at call sites
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
    let session_cache = SessionMemoryCache::new(16, Duration::from_mins(1), clock.clone());
    let loader =
        MemorySectionLoader::new(store.clone(), sessions.clone(), embeddings, session_cache);
    let prompts = Arc::new(Prompts::load());
    let language_resolver: SharedOrgLanguageResolver =
        Arc::new(StaticOrgLanguageResolver::new(Language::En));
    let rule_resolver: SharedOrgRuleResolver = Arc::new(StaticOrgRuleResolver::new(rule));
    let memory = AgentMemory::new(
        agents.clone(),
        prompt_cache,
        names_cache,
        loader,
        prompts,
        language_resolver,
        rule_resolver,
        clock,
    );
    Fixture {
        memory,
        sessions,
        store,
    }
}

#[sqlx::test]
async fn assembles_core_then_role_in_order(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let clock: SharedClock = Arc::new(TestClock::new());
    let f = build_memory(&pool, &seed, clock);

    let session = human_to_agent_session(
        f.sessions.as_ref(),
        seed.agent_id,
        seed.org_id,
        seed.user_id,
    )
    .await;
    let viewer = Participant::agent(seed.agent_id);
    let prompt = f
        .memory
        .system_prompt(
            session,
            viewer,
            &patom::runtime::RequestKindPayload::Normal {},
        )
        .await
        .expect("system prompt");

    let s = prompt.as_ref();
    let core_open = s.find(CORE_TAG_OPEN).expect("has <core>");
    let core_close = s.find(CORE_TAG_CLOSE).expect("has </core>");
    let role_open = s.find(ROLE_TAG_OPEN).expect("has <role>");
    let role_close = s.find(ROLE_TAG_CLOSE).expect("has </role>");

    assert!(core_open < core_close, "core tags ordered");
    assert!(core_close < role_open, "core block precedes role block");
    assert!(role_open < role_close, "role tags ordered");
    assert!(s.contains(CORE_MARKER), "core text present");
    assert!(
        s.contains("test default prompt"),
        "role text from the seeded agent present"
    );
}

#[sqlx::test]
async fn date_section_sits_between_role_and_memory(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let clock: SharedClock = Arc::new(TestClock::new());
    let f = build_memory(&pool, &seed, clock);
    let agent_id = seed.agent_id;

    // Seed one memory so `<memory>` actually appears — the ordering check
    // requires both anchors to be present.
    f.store
        .apply(MemoryMutation::Write {
            agent: agent_id,
            kind: MemoryKind::Identity,
            content: MemoryContent::try_from("I default to terse replies.").expect("valid"),
            state: MemoryState::Validated,
            pinned: false,
            source: MutationSource::Operator,
        })
        .await
        .expect("write");

    let session =
        human_to_agent_session(f.sessions.as_ref(), agent_id, seed.org_id, seed.user_id).await;
    let prompt = f
        .memory
        .system_prompt(
            session,
            Participant::agent(agent_id),
            &patom::runtime::RequestKindPayload::Normal {},
        )
        .await
        .expect("system prompt");

    let s = prompt.as_ref();
    let role_close = s.find(ROLE_TAG_CLOSE).expect("has </role>");
    let date_open = s.find(DATE_TAG_OPEN).expect("has <date>");
    let date_close = s.find(DATE_TAG_CLOSE).expect("has </date>");
    let memory_open = s.find(MEMORY_TAG_OPEN).expect("has <memory>");

    assert!(role_close < date_open, "<date> opens after </role>");
    assert!(date_open < date_close, "date tags ordered");
    assert!(
        date_close < memory_open,
        "<date> closes before <memory> opens"
    );

    // Body of <date> must be the YYYY-MM-DD (Weekday, UTC) shape. We don't
    // pin the value (TestClock is wall-clock-relative); the format anchors
    // the contract.
    let body_start = date_open + DATE_TAG_OPEN.len();
    let body = &s[body_start..date_close];
    let iso_prefix = body
        .get(..10)
        .unwrap_or_else(|| panic!("date body too short to hold YYYY-MM-DD: {body:?}"));
    chrono::NaiveDate::parse_from_str(iso_prefix, "%Y-%m-%d")
        .unwrap_or_else(|e| panic!("date body must start with ISO 8601 ({body:?}): {e}"));
    assert!(body.ends_with(", UTC)"), "date body tagged UTC: {body:?}");
}

#[sqlx::test]
async fn empty_memory_skips_memory_section(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let clock: SharedClock = Arc::new(TestClock::new());
    let f = build_memory(&pool, &seed, clock);

    let session = human_to_agent_session(
        f.sessions.as_ref(),
        seed.agent_id,
        seed.org_id,
        seed.user_id,
    )
    .await;
    let prompt = f
        .memory
        .system_prompt(
            session,
            Participant::agent(seed.agent_id),
            &patom::runtime::RequestKindPayload::Normal {},
        )
        .await
        .expect("system prompt");

    assert!(
        !prompt.contains(MEMORY_TAG_OPEN),
        "no memory tag when no memories: {prompt}"
    );
}

#[sqlx::test]
async fn renders_memory_section_after_role(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let clock: SharedClock = Arc::new(TestClock::new());
    let f = build_memory(&pool, &seed, clock);
    let agent_id = seed.agent_id;

    f.store
        .apply(MemoryMutation::Write {
            agent: agent_id,
            kind: MemoryKind::Identity,
            content: MemoryContent::try_from("I default to terse replies.").expect("valid"),
            state: MemoryState::Validated,
            pinned: false,
            source: MutationSource::Operator,
        })
        .await
        .expect("write");

    let session =
        human_to_agent_session(f.sessions.as_ref(), agent_id, seed.org_id, seed.user_id).await;
    let prompt = f
        .memory
        .system_prompt(
            session,
            Participant::agent(agent_id),
            &patom::runtime::RequestKindPayload::Normal {},
        )
        .await
        .expect("system prompt");

    let s = prompt.as_ref();
    let role_close = s.find(ROLE_TAG_CLOSE).expect("</role>");
    let memory_open = s.find(MEMORY_TAG_OPEN).expect("<memory>");
    let memory_close = s.find(MEMORY_TAG_CLOSE).expect("</memory>");

    assert!(role_close < memory_open, "memory follows role: {s}");
    assert!(memory_open < memory_close, "memory tags ordered");
    assert!(
        s.contains("- [M-1, validated] I default to terse replies."),
        "memory line shape: {s}"
    );
    assert!(s.contains("### Self"));
}

#[sqlx::test]
async fn frozen_during_session_returns_identical_prompt(pool: PgPool) {
    // The composed memory section must be cached for the session's
    // lifetime so the prompt prefix stays stable across turns. Adding a
    // memory between two `system_prompt` calls in the same session must
    // not change the second call's output.
    let seed = seed_tenant(&pool).await;
    let clock: SharedClock = Arc::new(TestClock::new());
    let f = build_memory(&pool, &seed, clock);
    let agent_id = seed.agent_id;
    let session =
        human_to_agent_session(f.sessions.as_ref(), agent_id, seed.org_id, seed.user_id).await;
    let viewer = Participant::agent(agent_id);

    let first = f
        .memory
        .system_prompt(
            session,
            viewer,
            &patom::runtime::RequestKindPayload::Normal {},
        )
        .await
        .expect("first");

    f.store
        .apply(MemoryMutation::Write {
            agent: agent_id,
            kind: MemoryKind::Identity,
            content: MemoryContent::try_from("post-cache memory").expect("valid"),
            state: MemoryState::Tentative,
            pinned: false,
            source: MutationSource::Operator,
        })
        .await
        .expect("write");

    let second = f
        .memory
        .system_prompt(
            session,
            viewer,
            &patom::runtime::RequestKindPayload::Normal {},
        )
        .await
        .expect("second");

    assert_eq!(
        first.as_ref(),
        second.as_ref(),
        "prompt frozen for the session's lifetime"
    );
    assert!(
        !second.contains("post-cache memory"),
        "post-cache write must not leak into the cached section: {second}"
    );
}

#[sqlx::test]
async fn resolve_handle_round_trips_to_memory_id(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let clock: SharedClock = Arc::new(TestClock::new());
    let f = build_memory(&pool, &seed, clock);
    let agent_id = seed.agent_id;

    let outcome = f
        .store
        .apply(MemoryMutation::Write {
            agent: agent_id,
            kind: MemoryKind::Identity,
            content: MemoryContent::try_from("identity").expect("valid"),
            state: MemoryState::Held,
            pinned: false,
            source: MutationSource::Operator,
        })
        .await
        .expect("write");

    let session =
        human_to_agent_session(f.sessions.as_ref(), agent_id, seed.org_id, seed.user_id).await;
    // Compose the section so the handle map is populated.
    let _ = f
        .memory
        .system_prompt(
            session,
            Participant::agent(agent_id),
            &patom::runtime::RequestKindPayload::Normal {},
        )
        .await
        .expect("compose");

    let handle = MemoryHandle::try_from(1u32).expect("valid");
    let resolved = f
        .memory
        .resolve_handle(
            session,
            agent_id,
            &patom::runtime::RequestKindPayload::Normal {},
            handle,
        )
        .await
        .expect("resolve");
    assert_eq!(resolved, Some(outcome.memory_id));

    let stranger = MemoryHandle::try_from(999u32).expect("valid");
    let missing = f
        .memory
        .resolve_handle(
            session,
            agent_id,
            &patom::runtime::RequestKindPayload::Normal {},
            stranger,
        )
        .await
        .expect("resolve missing");
    assert_eq!(missing, None);
}

#[sqlx::test]
async fn cache_serves_within_ttl_then_refreshes_after_expiry(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let clock = Arc::new(TestClock::new());
    let shared: SharedClock = clock.clone();

    let agents: SharedAgentStore = common::pg::shared_agent_store(pool.clone(), shared.clone());
    let cache = AgentPromptCache::new(8, Duration::from_mins(1), shared.clone());

    let first = cache
        .get_or_load(seed.agent_id, &agents)
        .await
        .expect("load");
    assert_eq!(first.as_str(), "test default prompt");

    agents
        .update(
            seed.agent_id,
            AgentUpdate {
                system_prompt: Some(
                    AgentSystemPrompt::try_from("rolled-out v2").expect("valid prompt"),
                ),
                avatar_url: None,
                edited_by: Some(seed.user_id),
                ..AgentUpdate::default()
            },
        )
        .await
        .expect("update");

    clock.advance(Duration::from_secs(30));
    let still_cached = cache
        .get_or_load(seed.agent_id, &agents)
        .await
        .expect("cached");
    assert_eq!(still_cached.as_str(), "test default prompt");

    clock.advance(Duration::from_secs(31));
    let refreshed = cache
        .get_or_load(seed.agent_id, &agents)
        .await
        .expect("refreshed");
    assert_eq!(refreshed.as_str(), "rolled-out v2");
}

#[sqlx::test]
async fn org_rule_block_sits_between_core_and_role(pool: PgPool) {
    // When the org has a rule, `<organization-rule>` slots in between
    // `</core>` and `<role>` — the cache-friendly position the renderer
    // commits to (see `src/memory/agent.rs`'s module doc).
    let seed = seed_tenant(&pool).await;
    let clock: SharedClock = Arc::new(TestClock::new());
    let rule = OrganizationRule::try_from("Cite file:line on every claim.").expect("valid");
    let f = build_memory_with_rule(&pool, &seed, clock, Some(rule.clone()));

    let session = human_to_agent_session(
        f.sessions.as_ref(),
        seed.agent_id,
        seed.org_id,
        seed.user_id,
    )
    .await;
    let viewer = Participant::agent(seed.agent_id);
    let prompt = f
        .memory
        .system_prompt(
            session,
            viewer,
            &patom::runtime::RequestKindPayload::Normal {},
        )
        .await
        .expect("system prompt");

    let s = prompt.as_ref();
    let core_close = s.find(CORE_TAG_CLOSE).expect("has </core>");
    let org_open = s.find(ORG_RULE_TAG_OPEN).expect("has <organization-rule>");
    let org_close = s
        .find(ORG_RULE_TAG_CLOSE)
        .expect("has </organization-rule>");
    let role_open = s.find(ROLE_TAG_OPEN).expect("has <role>");

    assert!(core_close < org_open, "rule opens after </core>");
    assert!(org_open < org_close, "rule tags ordered");
    assert!(org_close < role_open, "rule closes before <role>");
    assert!(s.contains(rule.as_str()), "rule body present");
}

#[sqlx::test]
async fn org_rule_block_omitted_when_unset(pool: PgPool) {
    // When the org has no rule, the tag never appears — empty configs
    // don't waste prompt budget or hand the model an empty envelope.
    let seed = seed_tenant(&pool).await;
    let clock: SharedClock = Arc::new(TestClock::new());
    let f = build_memory_with_rule(&pool, &seed, clock, None);

    let session = human_to_agent_session(
        f.sessions.as_ref(),
        seed.agent_id,
        seed.org_id,
        seed.user_id,
    )
    .await;
    let viewer = Participant::agent(seed.agent_id);
    let prompt = f
        .memory
        .system_prompt(
            session,
            viewer,
            &patom::runtime::RequestKindPayload::Normal {},
        )
        .await
        .expect("system prompt");

    let s = prompt.as_ref();
    assert!(
        !s.contains(ORG_RULE_TAG_OPEN),
        "organization-rule tag must be absent when unset"
    );
    assert!(!s.contains(ORG_RULE_TAG_CLOSE), "no dangling close tag");
}

#[sqlx::test]
async fn pg_memory_store_underlying_constructs(pool: PgPool) {
    // Smoke: building the store + cache via the public types matches the
    // app.rs wiring. Catches an export regression more directly than
    // the integration tests do.
    let _seed = seed_tenant(&pool).await;
    let clock: SharedClock = SystemClock::shared();
    let _store: SharedMemoryStore = Arc::new(PgMemoryStore::new(
        pool.clone(),
        clock.clone(),
        common::embedding::FakeEmbeddingProvider::shared(),
    ));
    let _cache = SessionMemoryCache::new(4, Duration::from_secs(1), clock);
}
