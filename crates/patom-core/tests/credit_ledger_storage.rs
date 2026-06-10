//! Storage-level contract for the credit ledger (#154 S2): the `org_credit_ledger`
//! (append-only) and `org_credits` (materialized balance) tables, their RLS
//! isolation, the idempotency-key uniqueness, and the append-only REVOKE that
//! stops the tenant role from rewriting history.
//!
//! These probe the schema directly (raw SQL) — the `BillingService` credit
//! methods land in later stages.

#![allow(clippy::expect_used)]

use patom::auth::{OrgId, begin_as_user, begin_privileged};
use sqlx::PgPool;
use uuid::Uuid;

mod common;
use common::pg::seed_tenant;

/// Append one ledger row for `org`, RLS-bypassing (mirrors the privileged
/// grant/settle paths). Returns the row id.
async fn insert_ledger(
    pool: &PgPool,
    org: OrgId,
    delta: i64,
    kind: &str,
    reason: &str,
    key: Option<&str>,
) -> Uuid {
    let id = Uuid::new_v4();
    let mut tx = begin_privileged(pool).await.expect("privileged tx");
    sqlx::query(
        "INSERT INTO org_credit_ledger
             (id, org_id, delta_micro_usd, kind, reason, idempotency_key, actor, created_at)
         VALUES ($1, $2, $3, $4, $5, $6, NULL, now())",
    )
    .bind(id)
    .bind(org)
    .bind(delta)
    .bind(kind)
    .bind(reason)
    .bind(key)
    .execute(&mut *tx)
    .await
    .expect("insert ledger row");
    tx.commit().await.expect("commit");
    id
}

async fn upsert_credits(pool: &PgPool, org: OrgId, balance: i64, granted: i64, used: i64) {
    let mut tx = begin_privileged(pool).await.expect("privileged tx");
    sqlx::query(
        "INSERT INTO org_credits
             (org_id, balance_micro_usd, granted_total_micro_usd, used_total_micro_usd, updated_at)
         VALUES ($1, $2, $3, $4, now())",
    )
    .bind(org)
    .bind(balance)
    .bind(granted)
    .bind(used)
    .execute(&mut *tx)
    .await
    .expect("insert credits row");
    tx.commit().await.expect("commit");
}

#[sqlx::test]
async fn tables_accept_grant_and_credits_rows(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    insert_ledger(
        &pool,
        seed.org_id,
        2_000_000,
        "grant",
        "signup_bonus",
        Some(&format!("signup:{}", seed.org_id.as_uuid())),
    )
    .await;
    upsert_credits(&pool, seed.org_id, 2_000_000, 2_000_000, 0).await;

    let (delta, kind, reason): (i64, String, String) = sqlx::query_as(
        "SELECT delta_micro_usd, kind, reason FROM org_credit_ledger WHERE org_id = $1",
    )
    .bind(seed.org_id)
    .fetch_one(&pool)
    .await
    .expect("read ledger");
    assert_eq!(delta, 2_000_000);
    assert_eq!(kind, "grant");
    assert_eq!(reason, "signup_bonus");

    let (balance, granted, used): (i64, i64, i64) = sqlx::query_as(
        "SELECT balance_micro_usd, granted_total_micro_usd, used_total_micro_usd \
         FROM org_credits WHERE org_id = $1",
    )
    .bind(seed.org_id)
    .fetch_one(&pool)
    .await
    .expect("read credits");
    assert_eq!(balance, 2_000_000);
    assert_eq!(granted, 2_000_000);
    assert_eq!(used, 0);
    // The materialized invariant the service must keep: balance == granted − used.
    assert_eq!(balance, granted - used);
}

#[sqlx::test]
async fn ledger_idempotency_key_is_unique(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let key = format!("signup:{}", seed.org_id.as_uuid());
    insert_ledger(
        &pool,
        seed.org_id,
        2_000_000,
        "grant",
        "signup_bonus",
        Some(&key),
    )
    .await;

    // A second row with the same key must violate the UNIQUE constraint.
    let mut tx = begin_privileged(&pool).await.expect("privileged tx");
    let dup = sqlx::query(
        "INSERT INTO org_credit_ledger
             (id, org_id, delta_micro_usd, kind, reason, idempotency_key, actor, created_at)
         VALUES ($1, $2, $3, 'grant', 'signup_bonus', $4, NULL, now())",
    )
    .bind(Uuid::new_v4())
    .bind(seed.org_id)
    .bind(2_000_000_i64)
    .bind(&key)
    .execute(&mut *tx)
    .await;
    assert!(dup.is_err(), "duplicate idempotency_key must be rejected");
}

#[sqlx::test]
async fn many_null_idempotency_keys_coexist(pool: PgPool) {
    // Usage debits may carry no key; the SQL standard lets multiple NULLs share
    // a UNIQUE column, so two key-less rows must both insert.
    let seed = seed_tenant(&pool).await;
    insert_ledger(&pool, seed.org_id, -1_000, "debit", "usage", None).await;
    insert_ledger(&pool, seed.org_id, -2_000, "debit", "usage", None).await;
    let (n,): (i64,) = sqlx::query_as("SELECT count(*) FROM org_credit_ledger WHERE org_id = $1")
        .bind(seed.org_id)
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(n, 2);
}

#[sqlx::test]
async fn ledger_is_append_only_for_tenant_role(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let row = insert_ledger(&pool, seed.org_id, 2_000_000, "grant", "manual", None).await;

    // The tenant role (patom_app) may SELECT and INSERT...
    {
        let mut tx = begin_as_user(&pool, seed.user_id).await.expect("as user");
        let (n,): (i64,) =
            sqlx::query_as("SELECT count(*) FROM org_credit_ledger WHERE org_id = $1")
                .bind(seed.org_id)
                .fetch_one(&mut *tx)
                .await
                .expect("tenant may read its ledger");
        assert_eq!(n, 1);
    }
    {
        let mut tx = begin_as_user(&pool, seed.user_id).await.expect("as user");
        let appended = sqlx::query(
            "INSERT INTO org_credit_ledger
                 (id, org_id, delta_micro_usd, kind, reason, idempotency_key, actor, created_at)
             VALUES ($1, $2, -500, 'debit', 'usage', NULL, NULL, now())",
        )
        .bind(Uuid::new_v4())
        .bind(seed.org_id)
        .execute(&mut *tx)
        .await;
        assert!(appended.is_ok(), "tenant may append a new entry");
        tx.commit().await.expect("commit append");
    }

    // ...but never UPDATE or DELETE history (REVOKE).
    {
        let mut tx = begin_as_user(&pool, seed.user_id).await.expect("as user");
        let updated = sqlx::query("UPDATE org_credit_ledger SET delta_micro_usd = 0 WHERE id = $1")
            .bind(row)
            .execute(&mut *tx)
            .await;
        assert!(updated.is_err(), "tenant must not UPDATE the ledger");
    }
    {
        let mut tx = begin_as_user(&pool, seed.user_id).await.expect("as user");
        let deleted = sqlx::query("DELETE FROM org_credit_ledger WHERE id = $1")
            .bind(row)
            .execute(&mut *tx)
            .await;
        assert!(deleted.is_err(), "tenant must not DELETE the ledger");
    }
}

#[sqlx::test]
async fn credit_tables_are_rls_isolated(pool: PgPool) {
    let a = seed_tenant(&pool).await;
    let b = seed_tenant(&pool).await;
    insert_ledger(&pool, a.org_id, 2_000_000, "grant", "signup_bonus", None).await;
    upsert_credits(&pool, a.org_id, 2_000_000, 2_000_000, 0).await;

    // A's own member sees A's ledger + credits.
    {
        let mut tx = begin_as_user(&pool, a.user_id).await.expect("as A");
        let (ledger,): (i64,) =
            sqlx::query_as("SELECT count(*) FROM org_credit_ledger WHERE org_id = $1")
                .bind(a.org_id)
                .fetch_one(&mut *tx)
                .await
                .expect("A reads its ledger");
        let (credits,): (i64,) = sqlx::query_as("SELECT count(*) FROM org_credits")
            .fetch_one(&mut *tx)
            .await
            .expect("A reads its credits");
        assert_eq!(ledger, 1);
        assert_eq!(credits, 1);
    }

    // B's member cannot see A's rows (RLS filters by membership).
    {
        let mut tx = begin_as_user(&pool, b.user_id).await.expect("as B");
        let (ledger,): (i64,) =
            sqlx::query_as("SELECT count(*) FROM org_credit_ledger WHERE org_id = $1")
                .bind(a.org_id)
                .fetch_one(&mut *tx)
                .await
                .expect("B query runs");
        let (credits,): (i64,) = sqlx::query_as("SELECT count(*) FROM org_credits")
            .fetch_one(&mut *tx)
            .await
            .expect("B query runs");
        assert_eq!(ledger, 0, "B must not see A's ledger");
        assert_eq!(credits, 0, "B must not see A's credits");
    }
}
