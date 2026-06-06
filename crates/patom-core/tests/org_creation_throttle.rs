//! Issue #121: the global org-creation rate limit. Once
//! [`MAX_ORGS_PER_WINDOW`] orgs have been created inside
//! [`ORG_CREATE_RATE_WINDOW`], `create_personal_org` (the cloud self-service
//! signup path) is refused with [`AuthError::OrgCreationThrottled`] — so a
//! launch-day spike or scripted signup loop can't mint orgs unbounded. The
//! window is driven by the `now` the OAuth callback threads in (CLAUDE.md §11),
//! so the test pins it deterministically rather than racing a real clock.

#![allow(clippy::expect_used)]

use chrono::{DateTime, Duration, Utc};
use patom::auth::limits::{MAX_ORGS_PER_WINDOW, ORG_CREATE_RATE_WINDOW};
use patom::auth::{AuthError, Language, PgUserStore, UserId, UserStore};
use sqlx::PgPool;
use uuid::Uuid;

/// Insert `n` bare org rows stamped at `created_at`, filling the throttle window
/// without going through the full create path.
async fn seed_orgs_at(pool: &PgPool, n: i64, created_at: DateTime<Utc>) {
    for _ in 0..n {
        let slug = format!("o-{}", &Uuid::new_v4().simple().to_string()[..10]);
        sqlx::query(
            "INSERT INTO organizations (id, name, slug, default_language, created_at, updated_at)
             VALUES ($1, $2, $3, 'en', $4, $4)",
        )
        .bind(patom::auth::OrgId::new())
        .bind("Org")
        .bind(&slug)
        .bind(created_at)
        .execute(pool)
        .await
        .expect("seed org");
    }
}

/// Insert a bare user so `create_personal_org` has an owner to attach.
async fn seed_user(pool: &PgPool) -> UserId {
    let id = UserId::new();
    let email = format!("u-{}@example.test", Uuid::new_v4().simple());
    sqlx::query(
        "INSERT INTO users (id, email, display_name, created_at, updated_at)
         VALUES ($1, $2, 'U', now(), now())",
    )
    .bind(id)
    .bind(&email)
    .execute(pool)
    .await
    .expect("seed user");
    id
}

#[sqlx::test]
async fn org_creation_is_throttled_at_the_window_cap(pool: PgPool) {
    let store = PgUserStore::new(pool.clone());
    let now: DateTime<Utc> = "2026-06-04T12:00:00Z".parse().expect("ts");
    let window = Duration::from_std(ORG_CREATE_RATE_WINDOW).expect("window fits");

    // Fill the window exactly to the cap.
    seed_orgs_at(&pool, MAX_ORGS_PER_WINDOW, now).await;

    // The next self-service create is refused.
    let user = seed_user(&pool).await;
    let err = store
        .create_personal_org(user, "late", "Late", Language::DEFAULT, now)
        .await
        .expect_err("at the cap, signup must be throttled");
    assert!(
        matches!(err, AuthError::OrgCreationThrottled),
        "expected OrgCreationThrottled, got {err:?}"
    );

    // Once the window drains (the seeded orgs fall outside it), creation works.
    let later = now + window + Duration::seconds(1);
    let user2 = seed_user(&pool).await;
    store
        .create_personal_org(user2, "fresh", "Fresh", Language::DEFAULT, later)
        .await
        .expect("after the window drains, signup succeeds again");
}

#[sqlx::test]
async fn self_host_creation_skips_throttle_and_default_cap(pool: PgPool) {
    // `with_beta_limits(false)` models a self-host deployment: the operator pays
    // their own provider bill, so neither the rate limit nor the default cap
    // applies (issue #121).
    let store = PgUserStore::new(pool.clone()).with_beta_limits(false);
    let now: DateTime<Utc> = "2026-06-04T12:00:00Z".parse().expect("ts");

    // Fill the window to (and past) the cap — a beta deployment would throttle.
    seed_orgs_at(&pool, MAX_ORGS_PER_WINDOW, now).await;

    let user = seed_user(&pool).await;
    let org = store
        .create_personal_org(user, "selfhost", "Self Host", Language::DEFAULT, now)
        .await
        .expect("self-host org creation is never throttled");

    // And the org is born uncapped — no org_budgets row.
    let budget: Option<(Option<i64>, i32)> = sqlx::query_as(
        "SELECT monthly_cap_micro_usd, warn_threshold_bps FROM org_budgets WHERE org_id = $1",
    )
    .bind(org.id)
    .fetch_optional(&pool)
    .await
    .expect("read budget");
    assert!(
        budget.is_none(),
        "self-host org gets no default cap — unlimited, the operator's own bill"
    );
}

#[sqlx::test]
async fn org_creation_under_the_cap_succeeds(pool: PgPool) {
    let store = PgUserStore::new(pool.clone());
    let now: DateTime<Utc> = "2026-06-04T12:00:00Z".parse().expect("ts");

    // One short of the cap → the next create is allowed.
    seed_orgs_at(&pool, MAX_ORGS_PER_WINDOW - 1, now).await;
    let user = seed_user(&pool).await;
    store
        .create_personal_org(user, "ok", "Ok", Language::DEFAULT, now)
        .await
        .expect("under the cap, signup succeeds");
}
