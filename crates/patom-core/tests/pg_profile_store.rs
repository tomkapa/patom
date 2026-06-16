//! Trait-contract tests for [`patom::colleagues::PgProfileStore`] (Stage 2):
//! upsert + get_many round-trip, org-scope rejection, the re-embed skip, and the
//! unified `search_colleagues` UNION (profiled humans in, unprofiled + viewer
//! out, org-scoped). Each test gets a freshly-migrated DB via `#[sqlx::test]`.

#![allow(clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use patom::auth::{OrgId, UserId};
use patom::clock::SystemClock;
use patom::colleagues::{
    ColleagueId, ColleagueKind, ColleagueProfile, Expertise, PgProfileStore, Preferences,
    ProfileError, ProfileStore, Role, resolve_agent_colleague, resolve_user_colleague,
};
use patom::provider::{EmbeddingProvider, ProviderError, SharedEmbeddingProvider, embed_one};
use sqlx::PgPool;
use uuid::Uuid;

mod common;
use common::embedding::FakeEmbeddingProvider;
use common::pg::seed_tenant;

fn store(pool: &PgPool, embeddings: SharedEmbeddingProvider) -> PgProfileStore {
    PgProfileStore::new(pool.clone(), SystemClock::shared(), embeddings)
}

fn role(s: &str) -> Role {
    Role::try_from(s).expect("valid role")
}

fn expertise(s: &str) -> Expertise {
    Expertise::try_from(s).expect("valid expertise")
}

fn preferences(s: &str) -> Preferences {
    Preferences::try_from(s).expect("valid preferences")
}

/// Add a second human member to `org` (the trigger mints its colleague), and
/// return the minted colleague id.
async fn add_human(pool: &PgPool, org_id: OrgId, name: &str) -> ColleagueId {
    let user_id = UserId::new();
    let now = chrono::Utc::now();
    let email = format!("u-{}@example.test", Uuid::new_v4().simple());
    sqlx::query(
        "INSERT INTO users (id, email, display_name, created_at, updated_at) \
         VALUES ($1, $2, $3, $4, $4)",
    )
    .bind(user_id)
    .bind(&email)
    .bind(name)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert user");
    sqlx::query(
        "INSERT INTO org_members (org_id, user_id, role, created_at) VALUES ($1, $2, 'member', $3)",
    )
    .bind(org_id)
    .bind(user_id)
    .bind(now)
    .execute(pool)
    .await
    .expect("insert membership");
    resolve_user_colleague(pool, org_id, user_id)
        .await
        .expect("trigger mints colleague")
}

/// Non-empty 1536-dim query vector for search presence assertions (order is not
/// under test, only membership of the result set).
async fn query_vec() -> Vec<f32> {
    embed_one(&FakeEmbeddingProvider::new(), "who can do this")
        .await
        .expect("fake embed")
}

#[sqlx::test]
async fn upsert_then_get_many_round_trips(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let human = resolve_user_colleague(&pool, seed.org_id, seed.user_id)
        .await
        .expect("human colleague");
    let agent = resolve_agent_colleague(&pool, seed.org_id, seed.agent_id)
        .await
        .expect("agent colleague");
    let store = store(&pool, FakeEmbeddingProvider::shared());

    let profile = ColleagueProfile::new(
        human,
        Some(role("Product Manager")),
        Some(expertise("billing, pricing")),
        Some(preferences("async-first")),
        Some(agent),
    );
    store.upsert(seed.org_id, &profile).await.expect("upsert");

    let map = store.get_many(&[human]).await.expect("get_many");
    let got = map.get(&human).expect("profile present");
    assert_eq!(got.role().expect("role").as_str(), "Product Manager");
    assert_eq!(got.expertise().expect("expertise").as_str(), "billing, pricing");
    assert_eq!(got.preferences().expect("prefs").as_str(), "async-first");
    assert_eq!(got.updated_by(), Some(agent), "provenance recorded");
}

#[sqlx::test]
async fn get_many_omits_unprofiled_colleagues(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let human = resolve_user_colleague(&pool, seed.org_id, seed.user_id)
        .await
        .expect("human colleague");
    let store = store(&pool, FakeEmbeddingProvider::shared());

    // Nothing written yet: a missing profile is absent, not an error.
    let map = store.get_many(&[human]).await.expect("get_many");
    assert!(map.is_empty(), "unprofiled colleague has no row");
}

#[sqlx::test]
async fn upsert_rejects_subject_outside_org(pool: PgPool) {
    let org_a = seed_tenant(&pool).await;
    let org_b = seed_tenant(&pool).await;
    let human_b = resolve_user_colleague(&pool, org_b.org_id, org_b.user_id)
        .await
        .expect("human colleague in B");
    let store = store(&pool, FakeEmbeddingProvider::shared());

    // Write under org A, but the subject belongs to org B.
    let profile = ColleagueProfile::new(human_b, Some(role("Spy")), None, None, None);
    let err = store
        .upsert(org_a.org_id, &profile)
        .await
        .expect_err("cross-org subject must reject");
    assert!(matches!(
        err,
        ProfileError::SubjectNotInOrg { subject } if subject == human_b
    ));
}

#[sqlx::test]
async fn upsert_skips_reembed_on_identical_text(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let human = resolve_user_colleague(&pool, seed.org_id, seed.user_id)
        .await
        .expect("human colleague");
    let counter = Arc::new(CountingProvider::new());
    let shared: SharedEmbeddingProvider = counter.clone();
    let store = store(&pool, shared);

    let first =
        ColleagueProfile::new(human, Some(role("PM")), Some(expertise("billing")), None, None);
    store.upsert(seed.org_id, &first).await.expect("first upsert");
    assert_eq!(counter.calls(), 1, "first write embeds");

    // Identical composed text → reuse the stored vector, no embed call.
    let same =
        ColleagueProfile::new(human, Some(role("PM")), Some(expertise("billing")), None, None);
    store.upsert(seed.org_id, &same).await.expect("identical upsert");
    assert_eq!(counter.calls(), 1, "identical text skips re-embed");

    // Changed field → text differs → re-embed.
    let changed =
        ColleagueProfile::new(human, Some(role("PM")), Some(expertise("pricing")), None, None);
    store.upsert(seed.org_id, &changed).await.expect("changed upsert");
    assert_eq!(counter.calls(), 2, "changed text re-embeds");
}

#[sqlx::test]
async fn search_includes_profiled_human_excludes_unprofiled_and_viewer(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let human = resolve_user_colleague(&pool, seed.org_id, seed.user_id)
        .await
        .expect("human colleague");
    let agent = resolve_agent_colleague(&pool, seed.org_id, seed.agent_id)
        .await
        .expect("agent colleague");
    let unprofiled = add_human(&pool, seed.org_id, "Ghost").await;
    let store = store(&pool, FakeEmbeddingProvider::shared());

    store
        .upsert(
            seed.org_id,
            &ColleagueProfile::new(human, Some(role("Product Manager")), None, None, None),
        )
        .await
        .expect("profile the human");

    // Viewer = the agent: it is excluded; the profiled human is found; the
    // unprofiled human (NULL embedding) is invisible.
    let matches = store
        .search_colleagues(&query_vec().await, agent, 8)
        .await
        .expect("search");
    let ids: Vec<ColleagueId> = matches.iter().map(|m| m.colleague_id).collect();
    assert!(ids.contains(&human), "profiled human is discoverable");
    assert!(!ids.contains(&unprofiled), "unprofiled human is invisible");
    assert!(!ids.contains(&agent), "viewer is excluded");
    let human_hit = matches
        .iter()
        .find(|m| m.colleague_id == human)
        .expect("human hit");
    assert_eq!(human_hit.kind, ColleagueKind::Human);
}

#[sqlx::test]
async fn search_finds_agent_for_human_viewer(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let human = resolve_user_colleague(&pool, seed.org_id, seed.user_id)
        .await
        .expect("human colleague");
    let agent = resolve_agent_colleague(&pool, seed.org_id, seed.agent_id)
        .await
        .expect("agent colleague");
    let store = store(&pool, FakeEmbeddingProvider::shared());

    // The seeded agent already carries a description embedding; a human viewer
    // should find it (the agent side of the UNION).
    let matches = store
        .search_colleagues(&query_vec().await, human, 8)
        .await
        .expect("search");
    let agent_hit = matches
        .iter()
        .find(|m| m.colleague_id == agent)
        .expect("agent is discoverable");
    assert_eq!(agent_hit.kind, ColleagueKind::Agent);
}

#[sqlx::test]
async fn search_is_org_scoped(pool: PgPool) {
    let org_a = seed_tenant(&pool).await;
    let org_b = seed_tenant(&pool).await;
    let human_b = resolve_user_colleague(&pool, org_b.org_id, org_b.user_id)
        .await
        .expect("human colleague in B");
    let agent_a = resolve_agent_colleague(&pool, org_a.org_id, org_a.agent_id)
        .await
        .expect("agent colleague in A");
    let store = store(&pool, FakeEmbeddingProvider::shared());

    store
        .upsert(
            org_b.org_id,
            &ColleagueProfile::new(human_b, Some(role("PM in B")), None, None, None),
        )
        .await
        .expect("profile human in B");

    // Viewer in A must never see org B's profiled human.
    let matches = store
        .search_colleagues(&query_vec().await, agent_a, 8)
        .await
        .expect("search");
    assert!(
        !matches.iter().any(|m| m.colleague_id == human_b),
        "cross-org colleague must not surface"
    );
}

/// Embedding provider that counts `embed` calls so the re-embed-skip path is
/// observable; delegates to the deterministic [`FakeEmbeddingProvider`].
#[derive(Debug)]
struct CountingProvider {
    inner: FakeEmbeddingProvider,
    calls: AtomicUsize,
}

impl CountingProvider {
    fn new() -> Self {
        Self {
            inner: FakeEmbeddingProvider::new(),
            calls: AtomicUsize::new(0),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl EmbeddingProvider for CountingProvider {
    fn name(&self) -> &'static str {
        "counting-fake"
    }

    fn dimensions(&self) -> usize {
        self.inner.dimensions()
    }

    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.embed(texts).await
    }
}
