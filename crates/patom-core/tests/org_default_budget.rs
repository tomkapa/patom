//! Issue #121: a freshly-created *cloud* org starts with the default monthly
//! budget cap (not unlimited), while the self-host bootstrap org stays
//! unlimited. The cap is what stops an uncapped beta org from draining the
//! provider budget — see `src/budget/limits.rs`.
//!
//! Each test runs against a fresh, fully-migrated database minted by
//! `#[sqlx::test]`, so `organizations` starts empty — the precondition the
//! bootstrap path keys off.

#![allow(clippy::expect_used)]

use patom::auth::{Language, PgUserStore, UserId, UserStore};
use patom::budget::limits::{DEFAULT_ORG_MONTHLY_CAP_MICROS, DEFAULT_WARN_BPS};
use sqlx::PgPool;
use uuid::Uuid;

/// Read the configured cap + warn threshold for an org, RLS-bypassing (the test
/// owns the pool as the privileged role). `None` = no `org_budgets` row at all
/// (unlimited).
async fn read_budget_row(pool: &PgPool, org: patom::auth::OrgId) -> Option<(Option<i64>, i32)> {
    sqlx::query_as(
        "SELECT monthly_cap_micro_usd, warn_threshold_bps FROM org_budgets WHERE org_id = $1",
    )
    .bind(org)
    .fetch_optional(pool)
    .await
    .expect("read org_budgets")
}

/// Insert a bare user so org creation has an owner to attach. Returns its id.
async fn seed_user(pool: &PgPool) -> UserId {
    let id = UserId::new();
    let email = format!("u-{}@example.test", Uuid::new_v4().simple());
    sqlx::query(
        "INSERT INTO users (id, email, display_name, created_at, updated_at)
         VALUES ($1, $2, $3, now(), now())",
    )
    .bind(id)
    .bind(&email)
    .bind("Test User")
    .execute(pool)
    .await
    .expect("seed user");
    id
}

/// The exact migration-62 backfill statements, so the test exercises the SQL
/// that actually ships rather than a paraphrase.
const BACKFILL_UP: &str =
    include_str!("../migrations/00000000000062_org_default_budget_backfill.up.sql");
const BACKFILL_DOWN: &str =
    include_str!("../migrations/00000000000062_org_default_budget_backfill.down.sql");

/// Insert a bare org (no owner) and return its id. `budget` of `Some((cap, bps))`
/// also seeds an `org_budgets` row — `cap = None` is the admin-chosen-unlimited
/// case (a row with NULL cap), while a `budget` of `None` leaves the org with no
/// budget row at all. Pairing cap with bps makes the "cap without a threshold"
/// state unrepresentable.
async fn seed_bare_org(pool: &PgPool, budget: Option<(Option<i64>, i32)>) -> patom::auth::OrgId {
    let id = patom::auth::OrgId::new();
    let slug = format!("o-{}", &Uuid::new_v4().simple().to_string()[..8]);
    sqlx::query(
        "INSERT INTO organizations (id, name, slug, default_language, created_at, updated_at)
         VALUES ($1, $2, $3, 'en', now(), now())",
    )
    .bind(id)
    .bind("Org")
    .bind(&slug)
    .execute(pool)
    .await
    .expect("seed org");
    if let Some((cap, bps)) = budget {
        sqlx::query(
            "INSERT INTO org_budgets
                 (org_id, monthly_cap_micro_usd, warn_threshold_bps, created_at, updated_at)
             VALUES ($1, $2, $3, now(), now())",
        )
        .bind(id)
        .bind(cap)
        .bind(bps)
        .execute(pool)
        .await
        .expect("seed budget");
    }
    id
}

#[sqlx::test]
async fn backfill_caps_only_capless_orgs_and_is_reversible(pool: PgPool) {
    // A: pre-existing org with no budget row (the hole the backfill closes).
    let capless = seed_bare_org(&pool, None).await;
    // B: org an admin configured with a custom cap — must be untouched.
    let configured = seed_bare_org(&pool, Some((Some(5_000_000), 9000))).await;
    // C: org an admin deliberately set to unlimited (NULL cap) — must be untouched.
    let chosen_unlimited = seed_bare_org(&pool, Some((None, 7000))).await;

    sqlx::query(BACKFILL_UP).execute(&pool).await.expect("up");

    assert_eq!(
        read_budget_row(&pool, capless).await,
        Some((
            Some(DEFAULT_ORG_MONTHLY_CAP_MICROS),
            i32::from(DEFAULT_WARN_BPS)
        )),
        "a capless org is stamped with the beta default"
    );
    assert_eq!(
        read_budget_row(&pool, configured).await,
        Some((Some(5_000_000), 9000)),
        "a configured cap is respected, not overwritten"
    );
    assert_eq!(
        read_budget_row(&pool, chosen_unlimited).await,
        Some((None, 7000)),
        "an admin-chosen unlimited (NULL cap) is respected"
    );

    // Idempotent: re-running inserts nothing new.
    sqlx::query(BACKFILL_UP)
        .execute(&pool)
        .await
        .expect("up again");
    assert_eq!(
        read_budget_row(&pool, capless).await,
        Some((
            Some(DEFAULT_ORG_MONTHLY_CAP_MICROS),
            i32::from(DEFAULT_WARN_BPS)
        )),
        "backfill is idempotent"
    );

    // Down drops default-shaped rows, preserving re-configured / unlimited ones.
    sqlx::query(BACKFILL_DOWN)
        .execute(&pool)
        .await
        .expect("down");
    assert!(
        read_budget_row(&pool, capless).await.is_none(),
        "the down migration removes the default-shaped backfilled row"
    );
    assert_eq!(
        read_budget_row(&pool, configured).await,
        Some((Some(5_000_000), 9000)),
        "the down migration preserves a re-configured cap"
    );
    assert_eq!(
        read_budget_row(&pool, chosen_unlimited).await,
        Some((None, 7000)),
        "the down migration preserves an admin-chosen unlimited"
    );
}

#[sqlx::test]
async fn cloud_personal_org_starts_with_default_cap(pool: PgPool) {
    let store = PgUserStore::new(pool.clone());
    let now = chrono::Utc::now();
    let user_id = seed_user(&pool).await;

    let new_org = store
        .create_personal_org(user_id, "acme", "Acme", Language::DEFAULT, now)
        .await
        .expect("create personal org");

    let row = read_budget_row(&pool, new_org.id)
        .await
        .expect("a cloud org must be stamped with a default budget row");
    assert_eq!(
        row.0,
        Some(DEFAULT_ORG_MONTHLY_CAP_MICROS),
        "new cloud org is capped at the beta default, not unlimited"
    );
    assert_eq!(
        row.1,
        i32::from(DEFAULT_WARN_BPS),
        "the default warn threshold is stamped alongside the cap"
    );
}

#[sqlx::test]
async fn self_host_bootstrap_org_is_unlimited(pool: PgPool) {
    let store = PgUserStore::new(pool.clone());
    let now = chrono::Utc::now();
    let user_id = seed_user(&pool).await;

    let new_org = store
        .bootstrap_initial_org_as_owner(user_id, "corp", "Corp", Language::DEFAULT, now)
        .await
        .expect("bootstrap")
        .expect("empty table ⇒ Some(org)");

    assert!(
        read_budget_row(&pool, new_org.id).await.is_none(),
        "the self-host bootstrap org gets no cap — the operator pays their own bill"
    );
}
