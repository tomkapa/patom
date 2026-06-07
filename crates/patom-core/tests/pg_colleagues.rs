//! Integration tests for the colleagues directory (Stage 2). Each test gets a
//! freshly-migrated database via `#[sqlx::test]`; minting is driven by the
//! triggers in migration 57, so seeding a tenant alone populates the directory.

#![allow(clippy::expect_used)]

use std::sync::Arc;

use patom::auth::{UserId, begin_as_user};
use patom::clock::SystemClock;
use patom::colleagues::{
    COLLEAGUE_ROSTER_CACHE_CAP, COLLEAGUE_ROSTER_CACHE_TTL, ColleagueError, ColleagueKind,
    ColleagueRosterCache, ColleagueStore, PgColleagueStore, SharedColleagueStore,
};
use sqlx::PgPool;

mod common;
use common::pg::seed_tenant;

fn store(pool: &PgPool) -> PgColleagueStore {
    PgColleagueStore::new(pool.clone())
}

#[sqlx::test]
async fn seed_mints_one_human_and_one_agent_colleague(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let roster = store(&pool).list_for_org(seed.org_id).await.expect("list");
    assert_eq!(roster.len(), 2, "one human (member) + one agent (default)");
    let kinds: Vec<ColleagueKind> = roster.iter().map(|c| c.kind).collect();
    assert!(kinds.contains(&ColleagueKind::Human));
    assert!(kinds.contains(&ColleagueKind::Agent));
}

#[sqlx::test]
async fn roster_resolves_display_names_alpha_sorted(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let roster = store(&pool).list_for_org(seed.org_id).await.expect("list");
    // "Seeded Test User" < "test-default" by lower-case order → human first.
    assert_eq!(roster[0].kind, ColleagueKind::Human);
    assert_eq!(roster[0].display_name.as_str(), "Seeded Test User");
    assert_eq!(roster[1].kind, ColleagueKind::Agent);
    assert_eq!(roster[1].display_name.as_str(), "test-default");
}

#[sqlx::test]
async fn resolve_satellites_and_read(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let s = store(&pool);

    let agent_cid = s
        .resolve_agent(seed.org_id, seed.agent_id)
        .await
        .expect("resolve agent");
    let agent = s.read(agent_cid).await.expect("read agent colleague");
    assert_eq!(agent.kind(), ColleagueKind::Agent);
    assert_eq!(agent.agent_id(), Some(seed.agent_id));
    assert_eq!(agent.user_id(), None);
    assert_eq!(agent.org_id(), seed.org_id);

    let user_cid = s
        .resolve_user(seed.org_id, seed.user_id)
        .await
        .expect("resolve user");
    let human = s.read(user_cid).await.expect("read human colleague");
    assert_eq!(human.kind(), ColleagueKind::Human);
    assert_eq!(human.user_id(), Some(seed.user_id));
    assert_eq!(human.agent_id(), None);
}

#[sqlx::test]
async fn resolve_unknown_user_is_unmapped(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let err = store(&pool)
        .resolve_user(seed.org_id, UserId::new())
        .await
        .expect_err("unmapped satellite must error");
    assert!(matches!(
        err,
        ColleagueError::SatelliteUnmapped {
            kind: ColleagueKind::Human
        }
    ));
}

#[sqlx::test]
async fn read_unknown_colleague_is_not_found(pool: PgPool) {
    seed_tenant(&pool).await;
    let missing = patom::colleagues::ColleagueId::new();
    let err = store(&pool)
        .read(missing)
        .await
        .expect_err("missing id must error");
    assert!(matches!(err, ColleagueError::NotFound(_)));
}

#[sqlx::test]
async fn rls_isolates_colleagues_across_orgs(pool: PgPool) {
    let seed_a = seed_tenant(&pool).await;
    let seed_b = seed_tenant(&pool).await;

    // Under user B's tenant context (role patom_app), the RLS policy hides
    // org A's colleagues but shows org B's own.
    let mut tx = begin_as_user(&pool, seed_b.user_id)
        .await
        .expect("begin as user B");
    let seen_a: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM colleagues WHERE org_id = $1")
        .bind(seed_a.org_id)
        .fetch_one(&mut *tx)
        .await
        .expect("count org A");
    let seen_b: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM colleagues WHERE org_id = $1")
        .bind(seed_b.org_id)
        .fetch_one(&mut *tx)
        .await
        .expect("count org B");
    assert_eq!(seen_a.0, 0, "RLS must hide org A's colleagues from user B");
    assert!(seen_b.0 >= 1, "user B sees their own org's colleagues");
    drop(tx);
}

#[sqlx::test]
async fn roster_cache_serves_org_roster(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store: SharedColleagueStore = Arc::new(PgColleagueStore::new(pool.clone()));
    let cache = ColleagueRosterCache::new(
        COLLEAGUE_ROSTER_CACHE_CAP,
        COLLEAGUE_ROSTER_CACHE_TTL,
        SystemClock::shared(),
    );
    let roster = cache
        .get_or_load(seed.org_id, &store)
        .await
        .expect("cache load");
    assert_eq!(roster.len(), 2);
    // Second hit is served from cache (same content).
    let again = cache
        .get_or_load(seed.org_id, &store)
        .await
        .expect("cache hit");
    assert_eq!(again.len(), 2);
}
