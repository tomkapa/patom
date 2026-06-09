//! Contract tests for the Postgres `SubscriptionStore` (#131): upsert/read
//! round-trip, natural-key idempotency, the webhook event ledger, and the
//! per-org RLS policy on `cloud.subscriptions`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use chrono::Utc;
use patom::auth::{OrgId, UserId, begin_as_user};
use patom::clock::SystemClock;
use patom_cloud::lemon_squeezy::{
    LsCustomerId, LsEventId, LsSubscriptionId, LsVariantId, NewSubscription, PgSubscriptionStore,
    Plan, SubscriptionStatus, SubscriptionStore,
};
use sqlx::PgPool;
use uuid::Uuid;

fn store(pool: &PgPool) -> PgSubscriptionStore {
    PgSubscriptionStore::new(pool.clone(), SystemClock::shared())
}

fn sub(org: OrgId, ls_sub: &str, plan: Plan, status: SubscriptionStatus) -> NewSubscription {
    NewSubscription {
        org_id: org,
        ls_customer_id: Some(LsCustomerId::try_from("cus_1").expect("customer id")),
        ls_subscription_id: LsSubscriptionId::try_from(ls_sub).expect("subscription id"),
        ls_variant_id: Some(LsVariantId::try_from("var_1").expect("variant id")),
        plan,
        status,
        current_period_end: None,
    }
}

/// Seed a user + org + owner membership directly (patom-cloud tests have no
/// access to core's test seeding helpers). Returns the pair.
async fn seed_member(pool: &PgPool) -> (UserId, OrgId) {
    let user = UserId::new();
    let org = OrgId::new();
    let now = Utc::now();
    let email = format!("seed-{}@example.test", Uuid::new_v4().simple());
    let slug = format!("seed-{}", &Uuid::new_v4().simple().to_string()[..8]);
    sqlx::query(
        "INSERT INTO users (id, email, display_name, created_at, updated_at) VALUES ($1,$2,$3,$4,$4)",
    )
    .bind(user)
    .bind(&email)
    .bind("Seed User")
    .bind(now)
    .execute(pool)
    .await
    .expect("seed user");
    sqlx::query(
        "INSERT INTO organizations (id, name, slug, default_language, created_at, updated_at) \
         VALUES ($1,$2,$3,'en',$4,$4)",
    )
    .bind(org)
    .bind("Seed Org")
    .bind(&slug)
    .bind(now)
    .execute(pool)
    .await
    .expect("seed org");
    sqlx::query(
        "INSERT INTO org_members (org_id, user_id, role, created_at) VALUES ($1,$2,'owner',$3)",
    )
    .bind(org)
    .bind(user)
    .bind(now)
    .execute(pool)
    .await
    .expect("seed membership");
    (user, org)
}

#[sqlx::test(migrations = "../patom-core/migrations")]
async fn upsert_then_read_round_trips(pool: PgPool) {
    patom_cloud::run_migrations(&pool).await.expect("migrate");
    let store = store(&pool);
    let org = OrgId::new();

    store
        .upsert(sub(org, "sub_1", Plan::Starter, SubscriptionStatus::Active))
        .await
        .expect("upsert");

    let got = store
        .read_for_org(org)
        .await
        .expect("read")
        .expect("a subscription");
    assert_eq!(got.plan, Plan::Starter);
    assert_eq!(got.status, SubscriptionStatus::Active);
    assert_eq!(got.ls_subscription_id.as_str(), "sub_1");
    assert_eq!(got.org_id, org);
}

#[sqlx::test(migrations = "../patom-core/migrations")]
async fn upsert_is_idempotent_on_subscription_id(pool: PgPool) {
    patom_cloud::run_migrations(&pool).await.expect("migrate");
    let store = store(&pool);
    let org = OrgId::new();

    store
        .upsert(sub(org, "sub_1", Plan::Starter, SubscriptionStatus::Active))
        .await
        .expect("first upsert");
    // Same ls_subscription_id, new status → updates in place, no second row.
    store
        .upsert(sub(org, "sub_1", Plan::Growth, SubscriptionStatus::PastDue))
        .await
        .expect("second upsert");

    let got = store.read_for_org(org).await.expect("read").expect("some");
    assert_eq!(got.plan, Plan::Growth);
    assert_eq!(got.status, SubscriptionStatus::PastDue);

    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM cloud.subscriptions WHERE org_id = $1")
            .bind(org)
            .fetch_one(&pool)
            .await
            .expect("count");
    assert_eq!(count, 1, "upsert must not duplicate the natural key");
}

#[sqlx::test(migrations = "../patom-core/migrations")]
async fn record_event_once_dedupes_redeliveries(pool: PgPool) {
    patom_cloud::run_migrations(&pool).await.expect("migrate");
    let store = store(&pool);
    let org = OrgId::new();
    let event = LsEventId::try_from("evt_1").expect("event id");

    let first = store
        .record_event_once(&event, Some(org))
        .await
        .expect("first record");
    assert!(first, "first delivery is newly recorded");

    let second = store
        .record_event_once(&event, Some(org))
        .await
        .expect("second record");
    assert!(!second, "a redelivery is reported as already-applied");
}

#[sqlx::test(migrations = "../patom-core/migrations")]
async fn rls_hides_other_orgs_subscriptions(pool: PgPool) {
    patom_cloud::run_migrations(&pool).await.expect("migrate");
    let store = store(&pool);

    // Two orgs, each with a subscription; user is a member of `mine` only.
    let (user, mine) = seed_member(&pool).await;
    let (_other_user, theirs) = seed_member(&pool).await;
    store
        .upsert(sub(
            mine,
            "sub_mine",
            Plan::Starter,
            SubscriptionStatus::Active,
        ))
        .await
        .expect("mine");
    store
        .upsert(sub(
            theirs,
            "sub_theirs",
            Plan::Scale,
            SubscriptionStatus::Active,
        ))
        .await
        .expect("theirs");

    // Under the RLS role, an unfiltered read sees only the member's org.
    let mut tx = begin_as_user(&pool, user).await.expect("tenant tx");
    let visible: Vec<OrgId> =
        sqlx::query_scalar("SELECT org_id FROM cloud.subscriptions ORDER BY org_id")
            .fetch_all(&mut *tx)
            .await
            .expect("rls read");
    tx.commit().await.expect("commit");

    assert_eq!(
        visible,
        vec![mine],
        "RLS must expose only the caller's org subscription",
    );
}
