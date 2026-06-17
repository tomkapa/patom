//! Integration tests for the L1 + L2 `<participants>` plumbing (issue #183):
//! the `ThreadStore::thread_participants` query (creator + distinct posters) and
//! `AgentMemory::participants_block` enrichment (profiles, viewer exclusion,
//! raiser flag).

#![allow(clippy::expect_used)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use patom::agents::{AgentId, AgentPromptCache, SharedAgentStore};
use patom::auth::{Language, OrgId, SharedOrgLanguageResolver, SharedOrgRuleResolver};
use patom::clock::{SharedClock, SystemClock};
use patom::colleagues::{
    ColleagueId, ColleagueProfile, ColleagueRosterCache, NoThreadDisplayNames, PgColleagueStore,
    PgProfileStore, ProfileStore, Role, SharedColleagueStore, SharedProfileStore,
    resolve_agent_colleague, resolve_user_colleague,
};
use patom::memory::{
    AgentMemory, Memory, MemoryContent, MemoryKind, MemoryMutation, MemorySectionLoader,
    MemoryState, MemoryStore, MutationSource, PgMemoryStore, SharedMemoryStore,
};
use patom::prompts::Prompts;
use patom::threads::{PgThreadStore, ThreadId, ThreadParticipants, ThreadStore};
use patom::types::Participant;
use sqlx::PgPool;
use uuid::Uuid;

mod common;
use common::embedding::FakeEmbeddingProvider;
use common::lang::StaticOrgLanguageResolver;
use common::pg::{seed_agent, seed_tenant};
use common::rule::StaticOrgRuleResolver;

fn agent_memory(pool: &PgPool) -> AgentMemory {
    let clock: SharedClock = SystemClock::shared();
    let embeddings = FakeEmbeddingProvider::shared();
    let agents: SharedAgentStore = common::pg::shared_agent_store(pool.clone(), clock.clone());
    let colleagues: SharedColleagueStore = Arc::new(PgColleagueStore::new(pool.clone()));
    let profiles: SharedProfileStore = Arc::new(PgProfileStore::new(
        pool.clone(),
        clock.clone(),
        embeddings.clone(),
    ));
    let roster_cache = ColleagueRosterCache::new(16, Duration::from_mins(1), clock.clone());
    let store: SharedMemoryStore = Arc::new(PgMemoryStore::new(
        pool.clone(),
        clock.clone(),
        embeddings.clone(),
    ));
    let loader =
        MemorySectionLoader::new(store, colleagues.clone(), roster_cache.clone(), embeddings);
    let language: SharedOrgLanguageResolver =
        Arc::new(StaticOrgLanguageResolver::new(Language::En));
    let rule: SharedOrgRuleResolver = Arc::new(StaticOrgRuleResolver::new(None));
    AgentMemory::new(
        agents,
        AgentPromptCache::new(8, Duration::from_mins(1), clock.clone()),
        colleagues,
        profiles,
        roster_cache,
        Arc::new(NoThreadDisplayNames),
        loader,
        Arc::new(Prompts::load()),
        language,
        rule,
        Arc::new(patom::threads::PgThreadStore::new(
            pool.clone(),
            clock.clone(),
        )),
        clock,
    )
}

fn profile_store(pool: &PgPool) -> PgProfileStore {
    PgProfileStore::new(
        pool.clone(),
        SystemClock::shared(),
        FakeEmbeddingProvider::shared(),
    )
}

fn memory_store(pool: &PgPool) -> PgMemoryStore {
    PgMemoryStore::new(
        pool.clone(),
        SystemClock::shared(),
        FakeEmbeddingProvider::shared(),
    )
}

/// Write one private `collaborator` note `agent` holds about `subject`.
async fn write_collaborator_note(
    store: &PgMemoryStore,
    agent: AgentId,
    subject: ColleagueId,
    body: &str,
    state: MemoryState,
) {
    store
        .apply(MemoryMutation::Write {
            agent,
            kind: MemoryKind::Collaborator,
            content: MemoryContent::try_from(body).expect("note content"),
            state,
            pinned: false,
            subject: Some(subject),
            source: MutationSource::Operator,
        })
        .await
        .expect("write collaborator note");
}

/// Insert a DM thread raised by `creator` (counterpart `agent`), plus one posted
/// message from each of `senders`, and return the thread id.
async fn seed_thread(
    pool: &PgPool,
    org_id: OrgId,
    creator: ColleagueId,
    counterpart: ColleagueId,
    senders: &[ColleagueId],
) -> ThreadId {
    let now = chrono::Utc::now();
    let thread_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO threads (id, org_id, channel_id, root_message_id, \
                              created_by_colleague_id, dm_counterpart_colleague_id, \
                              created_at, last_activity_at) \
         VALUES ($1, $2, NULL, NULL, $3, $4, $5, $5)",
    )
    .bind(thread_id)
    .bind(org_id)
    .bind(creator)
    .bind(counterpart)
    .bind(now)
    .execute(pool)
    .await
    .expect("seed thread");

    for (i, sender) in senders.iter().enumerate() {
        sqlx::query(
            "INSERT INTO thread_messages \
                 (thread_id, seq, kind, sender_colleague_id, body, org_id, created_at) \
             VALUES ($1, $2, 'posted', $3, '{}'::jsonb, $4, $5)",
        )
        .bind(thread_id)
        .bind(i64::try_from(i).expect("seq") + 1)
        .bind(sender)
        .bind(org_id)
        .bind(now)
        .execute(pool)
        .await
        .expect("seed posted message");
    }
    ThreadId::from(thread_id)
}

#[sqlx::test]
async fn thread_participants_returns_creator_and_senders(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let human = resolve_user_colleague(&pool, seed.org_id, seed.user_id)
        .await
        .expect("human");
    let agent = resolve_agent_colleague(&pool, seed.org_id, seed.agent_id)
        .await
        .expect("agent");
    let thread = seed_thread(&pool, seed.org_id, human, agent, &[human, agent]).await;

    let store = PgThreadStore::new(pool.clone(), SystemClock::shared());
    let participants = store
        .thread_participants(thread)
        .await
        .expect("participants");
    assert_eq!(participants.creator, Some(human), "human raised the thread");
    assert!(participants.senders.contains(&human));
    assert!(participants.senders.contains(&agent));
}

#[sqlx::test]
async fn participants_block_enriches_human_flags_raiser_excludes_viewer(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let human = resolve_user_colleague(&pool, seed.org_id, seed.user_id)
        .await
        .expect("human");
    let agent = resolve_agent_colleague(&pool, seed.org_id, seed.agent_id)
        .await
        .expect("agent");

    // Give the human a shared profile so the block can show L2.
    profile_store(&pool)
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

    let memory = agent_memory(&pool);
    // Viewer = the agent. Human raised the thread; both posted.
    let participants = ThreadParticipants {
        creator: Some(human),
        senders: vec![human, agent],
    };
    let block = memory
        .participants_block(
            &participants,
            Participant::agent(agent, seed.agent_id),
            &HashMap::new(),
        )
        .await;

    assert!(block.contains("<participants>"), "block rendered: {block}");
    assert!(block.contains("Seeded Test User"), "human named: {block}");
    assert!(
        block.contains("Product Manager"),
        "L2 profile snippet: {block}"
    );
    assert!(
        block.contains("raised this thread"),
        "raiser flagged: {block}"
    );
    assert!(
        !block.contains("test-default"),
        "viewer (agent) excluded: {block}"
    );
}

#[sqlx::test]
async fn participants_block_empty_when_only_viewer(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let agent = resolve_agent_colleague(&pool, seed.org_id, seed.agent_id)
        .await
        .expect("agent");
    let memory = agent_memory(&pool);

    // Only the viewer has posted → nobody else to name → empty block.
    let participants = ThreadParticipants {
        creator: Some(agent),
        senders: vec![agent],
    };
    let block = memory
        .participants_block(
            &participants,
            Participant::agent(agent, seed.agent_id),
            &HashMap::new(),
        )
        .await;
    assert!(block.is_empty(), "viewer-only yields no block: {block}");
}

#[sqlx::test]
async fn collaborator_query_scopes_to_agent_kind_and_subject(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let human = resolve_user_colleague(&pool, seed.org_id, seed.user_id)
        .await
        .expect("human");
    // A second agent in the same org, writing its own note about the same human.
    let other_agent_id = seed_agent(&pool, seed.org_id, "other-agent").await;

    let store = memory_store(&pool);
    write_collaborator_note(
        &store,
        seed.agent_id,
        human,
        "mine about human",
        MemoryState::Held,
    )
    .await;
    write_collaborator_note(
        &store,
        other_agent_id,
        human,
        "theirs about human",
        MemoryState::Held,
    )
    .await;
    // A non-collaborator memory (no subject) the viewer holds — must not match.
    store
        .apply(MemoryMutation::Write {
            agent: seed.agent_id,
            kind: MemoryKind::Identity,
            content: MemoryContent::try_from("I am terse").expect("content"),
            state: MemoryState::Held,
            pinned: false,
            subject: None,
            source: MutationSource::Operator,
        })
        .await
        .expect("write identity memory");

    let rows = store
        .collaborator_memories_for_subjects(seed.agent_id, &[human])
        .await
        .expect("query");
    assert_eq!(
        rows.len(),
        1,
        "only the viewer's collaborator note: {rows:?}"
    );
    assert_eq!(rows[0].content.as_str(), "mine about human");
    assert_eq!(rows[0].subject, Some(human));

    // Empty subject set short-circuits to no rows.
    let none = store
        .collaborator_memories_for_subjects(seed.agent_id, &[])
        .await
        .expect("empty");
    assert!(none.is_empty(), "empty subjects → no rows");
}

#[sqlx::test]
async fn participants_block_surfaces_private_notes_for_present_people(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let human = resolve_user_colleague(&pool, seed.org_id, seed.user_id)
        .await
        .expect("human");
    let agent = resolve_agent_colleague(&pool, seed.org_id, seed.agent_id)
        .await
        .expect("agent");
    // A peer agent that is *also* a thread participant and a note subject.
    let peer_id = seed_agent(&pool, seed.org_id, "peer-agent").await;
    let peer = resolve_agent_colleague(&pool, seed.org_id, peer_id)
        .await
        .expect("peer");

    let store = memory_store(&pool);
    // Four notes about the human (cap is 3) spanning states, plus one about the
    // peer agent — proving the overlay covers agent subjects too.
    write_collaborator_note(
        &store,
        seed.agent_id,
        human,
        "human-alpha",
        MemoryState::Validated,
    )
    .await;
    write_collaborator_note(
        &store,
        seed.agent_id,
        human,
        "human-beta",
        MemoryState::Held,
    )
    .await;
    write_collaborator_note(
        &store,
        seed.agent_id,
        human,
        "human-gamma",
        MemoryState::Held,
    )
    .await;
    write_collaborator_note(
        &store,
        seed.agent_id,
        human,
        "human-delta",
        MemoryState::Tentative,
    )
    .await;
    write_collaborator_note(&store, seed.agent_id, peer, "peer-note", MemoryState::Held).await;

    let memory = agent_memory(&pool);
    let participants = ThreadParticipants {
        creator: Some(human),
        senders: vec![human, peer],
    };
    let block = memory
        .participants_block(
            &participants,
            Participant::agent(agent, seed.agent_id),
            &HashMap::new(),
        )
        .await;

    assert!(
        block.contains("(you noted) human-alpha"),
        "validated human note kept: {block}"
    );
    assert!(
        block.contains("(you noted) peer-note"),
        "agent-subject note surfaced: {block}"
    );
    // The cap keeps the three highest-priority/freshest notes, dropping the
    // lone Tentative one.
    assert!(
        !block.contains("human-delta"),
        "cap drops the lowest-priority note: {block}"
    );
    // 3 kept for the human + 1 for the peer agent.
    assert_eq!(
        block.matches("(you noted)").count(),
        4,
        "per-person cap applied: {block}"
    );
}
