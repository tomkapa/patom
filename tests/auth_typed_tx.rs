//! Tests for the typed-tx runners ([`run_as_user`], [`run_privileged`]).
//!
//! Each runner wraps the open / `SET LOCAL` / commit-or-rollback lifecycle
//! around an `AsyncFnOnce` closure. The tests assert the four guarantees:
//! commit-on-`Ok`, rollback-on-`Err`, the `TenantTx::acting_user()` invariant,
//! and that the SQL session GUCs are set inside the closure.

#![allow(clippy::expect_used)]

mod common;

use patom_rs::auth::{AuthError, OrgId, UserId, run_as_user, run_privileged};
use sqlx::Row as _;
use uuid::Uuid;

use crate::common::pg::TestDb;

#[derive(Debug, thiserror::Error)]
enum TxTestError {
    #[error("expected failure")]
    Sentinel,
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// Reads the current `app.user_id` GUC inside the tx, returning the parsed
/// UUID. The runner sets this on entry to `run_as_user`; `run_privileged`
/// leaves it unset (NULL via `current_setting(..., true)`).
async fn current_app_user_id(conn: &mut sqlx::PgConnection) -> Option<Uuid> {
    let row = sqlx::query("SELECT current_setting('app.user_id', true) AS v")
        .fetch_one(conn)
        .await
        .expect("read app.user_id");
    let s: Option<String> = row.try_get("v").expect("v column");
    s.filter(|v| !v.is_empty())
        .and_then(|v| Uuid::parse_str(&v).ok())
}

#[tokio::test(flavor = "multi_thread")]
async fn run_as_user_commits_on_ok_and_pins_app_user_id() {
    let db = TestDb::fresh().await;
    let user = db.default_user_id;
    let new_org = OrgId::new();
    let slug = format!("rc-{}", Uuid::new_v4().simple());

    run_as_user::<(), AuthError>(&db.pool, user, async |tx| {
        // GUC is set inside the closure.
        let pinned = current_app_user_id(tx).await;
        assert_eq!(pinned, Some(user.as_uuid()));
        assert_eq!(tx.acting_user(), user);

        // Writes that don't touch RLS-bound tables succeed under
        // `patom_app`. `organizations` is granted to the role; the
        // INSERT lets us verify post-commit visibility below.
        sqlx::query(
            "INSERT INTO organizations (id, name, slug, default_language, created_at, updated_at) \
             VALUES ($1, $2, $3, 'en', now(), now())",
        )
        .bind(new_org)
        .bind("Run-As Commit")
        .bind(&slug)
        .execute(&mut **tx)
        .await?;
        Ok(())
    })
    .await
    .expect("runner returns Ok");

    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM organizations WHERE id = $1)")
            .bind(new_org)
            .fetch_one(&db.pool)
            .await
            .expect("post-commit read");
    assert!(exists, "row should be visible after tenant commit");
}

#[tokio::test(flavor = "multi_thread")]
async fn run_as_user_rolls_back_on_err() {
    let db = TestDb::fresh().await;
    let user = db.default_user_id;
    let new_org = OrgId::new();
    let slug = format!("ru-{}", Uuid::new_v4().simple());

    let res = run_as_user::<(), TxTestError>(&db.pool, user, async |tx| {
        sqlx::query(
            "INSERT INTO organizations (id, name, slug, default_language, created_at, updated_at) \
             VALUES ($1, $2, $3, 'en', now(), now())",
        )
        .bind(new_org)
        .bind("Rolled Back")
        .bind(&slug)
        .execute(&mut **tx)
        .await?;
        Err(TxTestError::Sentinel)
    })
    .await;
    assert!(matches!(res, Err(TxTestError::Sentinel)));

    // The INSERT must not be visible after rollback.
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM organizations WHERE id = $1)")
            .bind(new_org)
            .fetch_one(&db.pool)
            .await
            .expect("post-rollback read");
    assert!(!exists, "row should not be visible after rollback");
}

#[tokio::test(flavor = "multi_thread")]
async fn run_privileged_commits_on_ok_and_skips_app_user_id() {
    let db = TestDb::fresh().await;
    let new_org = OrgId::new();
    let slug = format!("pr-{}", Uuid::new_v4().simple());

    run_privileged::<(), AuthError>(&db.pool, async |tx| {
        // `app.user_id` is not set under privileged.
        let pinned = current_app_user_id(tx).await;
        assert!(pinned.is_none(), "privileged tx should not set app.user_id");

        sqlx::query(
            "INSERT INTO organizations (id, name, slug, default_language, created_at, updated_at) \
             VALUES ($1, $2, $3, 'en', now(), now())",
        )
        .bind(new_org)
        .bind("Privileged Commit")
        .bind(&slug)
        .execute(&mut **tx)
        .await?;
        Ok(())
    })
    .await
    .expect("runner returns Ok");

    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM organizations WHERE id = $1)")
            .bind(new_org)
            .fetch_one(&db.pool)
            .await
            .expect("post-commit read");
    assert!(exists, "row should be visible after privileged commit");
}

#[tokio::test(flavor = "multi_thread")]
async fn run_privileged_rolls_back_on_err() {
    let db = TestDb::fresh().await;
    let new_org = OrgId::new();
    let slug = format!("pe-{}", Uuid::new_v4().simple());

    let res = run_privileged::<(), TxTestError>(&db.pool, async |tx| {
        sqlx::query(
            "INSERT INTO organizations (id, name, slug, default_language, created_at, updated_at) \
             VALUES ($1, $2, $3, 'en', now(), now())",
        )
        .bind(new_org)
        .bind("Privileged Rollback")
        .bind(&slug)
        .execute(&mut **tx)
        .await?;
        Err(TxTestError::Sentinel)
    })
    .await;
    assert!(matches!(res, Err(TxTestError::Sentinel)));

    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM organizations WHERE id = $1)")
            .bind(new_org)
            .fetch_one(&db.pool)
            .await
            .expect("post-rollback read");
    assert!(
        !exists,
        "row should not be visible after privileged rollback"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn run_as_user_acting_user_matches_input() {
    let db = TestDb::fresh().await;
    let user = db.default_user_id;
    let observed: UserId = run_as_user::<_, AuthError>(&db.pool, user, async |tx| {
        Ok::<UserId, AuthError>(tx.acting_user())
    })
    .await
    .expect("runner returns Ok");
    assert_eq!(observed, user);
}
