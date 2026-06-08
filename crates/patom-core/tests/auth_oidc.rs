//! Trait-contract tests for the OIDC identity surface (ADR-0011):
//! `upsert_from_oidc` idempotency on `(issuer, subject)`, cross-issuer
//! subject non-collision, and the first-admin bootstrap — including the
//! headline concurrency guarantee that two simultaneous first logins
//! produce exactly one initial org with exactly one owner.
//!
//! Each test runs against a fresh, fully-migrated database minted by
//! `#[sqlx::test]` (migration 53 included), so `organizations` starts
//! empty — the precondition the bootstrap path keys off.

#![allow(clippy::expect_used)]

use std::sync::Arc;

use patom::auth::{
    Email, IssuerUrl, Language, OidcProfile, OidcSubject, PgUserStore, Role, UserStore,
};
use patom::orgs::{OrgStore, PgOrgStore};
use sqlx::PgPool;

fn profile(issuer: &str, subject: &str, email: &str) -> OidcProfile {
    OidcProfile {
        issuer: IssuerUrl::try_from(issuer).expect("issuer"),
        subject: OidcSubject::try_from(subject).expect("subject"),
        email: Email::try_from(email).expect("email"),
        email_verified: true,
        display_name: Some("Test User".to_owned()),
        avatar_url: None,
        locale: None,
    }
}

#[sqlx::test]
async fn upsert_from_oidc_is_idempotent_on_issuer_subject(pool: PgPool) {
    let store = PgUserStore::new(pool);
    let now = chrono::Utc::now();
    let p = profile("https://idp.example.test", "subject-1", "a@example.test");

    let first = store.upsert_from_oidc(&p, now).await.expect("first upsert");
    assert!(first.is_new_user, "first upsert mints a new user");

    let second = store
        .upsert_from_oidc(&p, now)
        .await
        .expect("second upsert");
    assert!(!second.is_new_user, "second upsert reuses the same user");
    assert_eq!(
        first.user.id, second.user.id,
        "same (issuer, subject) resolves to one users row"
    );
}

#[sqlx::test]
async fn two_issuers_sharing_a_subject_do_not_collide(pool: PgPool) {
    let store = PgUserStore::new(pool);
    let now = chrono::Utc::now();
    // Same `sub` string, two different issuers, two different emails —
    // the ADR-0011 guarantee: identity is keyed on the pair, so these are
    // two distinct accounts.
    let a = store
        .upsert_from_oidc(
            &profile("https://idp-a.test", "shared-sub", "a@a.test"),
            now,
        )
        .await
        .expect("issuer A upsert");
    let b = store
        .upsert_from_oidc(
            &profile("https://idp-b.test", "shared-sub", "b@b.test"),
            now,
        )
        .await
        .expect("issuer B upsert");
    assert!(a.is_new_user && b.is_new_user, "both are fresh accounts");
    assert_ne!(
        a.user.id, b.user.id,
        "a shared subject across issuers must not collide"
    );
}

#[sqlx::test]
async fn bootstrap_creates_initial_org_when_table_empty(pool: PgPool) {
    let store = PgUserStore::new(pool);
    let now = chrono::Utc::now();
    let user = store
        .upsert_from_oidc(&profile("https://idp.test", "sub-1", "owner@idp.test"), now)
        .await
        .expect("seed user")
        .user;

    let org = store
        .bootstrap_initial_org_as_owner(user.id, "owner", "Owner", Language::DEFAULT, now)
        .await
        .expect("bootstrap")
        .expect("empty table ⇒ Some(org)");

    let role = store
        .membership(user.id, org.id)
        .await
        .expect("membership read");
    assert_eq!(
        role,
        Some(patom::auth::Role::Owner),
        "bootstrapped user owns the org"
    );
}

#[sqlx::test]
async fn bootstrap_is_a_noop_when_orgs_already_exist(pool: PgPool) {
    let store = PgUserStore::new(pool);
    let now = chrono::Utc::now();
    let first = store
        .upsert_from_oidc(
            &profile("https://idp.test", "sub-first", "first@idp.test"),
            now,
        )
        .await
        .expect("seed first user")
        .user;
    // First login bootstraps the only org.
    store
        .bootstrap_initial_org_as_owner(first.id, "first", "First", Language::DEFAULT, now)
        .await
        .expect("bootstrap")
        .expect("first ⇒ Some");

    // A later user must NOT bootstrap a second initial org.
    let second = store
        .upsert_from_oidc(
            &profile("https://idp.test", "sub-second", "second@idp.test"),
            now,
        )
        .await
        .expect("seed second user")
        .user;
    let outcome = store
        .bootstrap_initial_org_as_owner(second.id, "second", "Second", Language::DEFAULT, now)
        .await
        .expect("bootstrap call");
    assert!(
        outcome.is_none(),
        "non-empty org table ⇒ None (fall through)"
    );
}

#[sqlx::test]
async fn invited_user_joins_existing_org_on_login(pool: PgPool) {
    let users = PgUserStore::new(pool.clone());
    let orgs = PgOrgStore::new(pool.clone());
    let now = chrono::Utc::now();

    // Admin bootstraps the one org.
    let admin = users
        .upsert_from_oidc(
            &profile("https://idp.test", "admin", "admin@corp.test"),
            now,
        )
        .await
        .expect("admin")
        .user;
    let org = users
        .bootstrap_initial_org_as_owner(admin.id, "corp", "Corp", Language::DEFAULT, now)
        .await
        .expect("bootstrap")
        .expect("first ⇒ Some");

    // Admin invites a teammate.
    let invitee_email = Email::try_from("teammate@corp.test").expect("email");
    orgs.create_invites(
        org.id,
        std::slice::from_ref(&invitee_email),
        Role::Member,
        admin.id,
        now,
        chrono::Duration::hours(48),
    )
    .await
    .expect("create invite");

    // Teammate logs in for the first time → joins the existing org, no
    // personal org of their own.
    let teammate = users
        .upsert_from_oidc(
            &profile("https://idp.test", "teammate", "teammate@corp.test"),
            now,
        )
        .await
        .expect("teammate")
        .user;
    let joined = orgs
        .join_pending_invites(teammate.id, &invitee_email, now)
        .await
        .expect("join")
        .expect("pending invite ⇒ Some(org)");
    assert_eq!(joined, org.id, "teammate joins the inviting org");
    assert_eq!(
        users
            .membership(teammate.id, org.id)
            .await
            .expect("membership"),
        Some(Role::Member),
        "teammate joins with the invited role"
    );

    // Re-running the join is a no-op (invite already consumed).
    let again = orgs
        .join_pending_invites(teammate.id, &invitee_email, now)
        .await
        .expect("second join");
    assert!(again.is_none(), "a consumed invite does not re-join");
}

#[sqlx::test]
async fn login_without_an_invite_yields_no_org(pool: PgPool) {
    let users = PgUserStore::new(pool.clone());
    let orgs = PgOrgStore::new(pool.clone());
    let now = chrono::Utc::now();
    let stranger = users
        .upsert_from_oidc(
            &profile("https://idp.test", "stranger", "nobody@corp.test"),
            now,
        )
        .await
        .expect("stranger")
        .user;
    let email = Email::try_from("nobody@corp.test").expect("email");
    let outcome = orgs
        .join_pending_invites(stranger.id, &email, now)
        .await
        .expect("join call");
    assert!(
        outcome.is_none(),
        "no invite ⇒ no org (caller denies login)"
    );
}

#[sqlx::test]
async fn concurrent_first_logins_yield_exactly_one_initial_org(pool: PgPool) {
    let store = Arc::new(PgUserStore::new(pool.clone()));
    let now = chrono::Utc::now();
    // Two distinct identities racing to be the first admin.
    let u1 = store
        .upsert_from_oidc(&profile("https://idp.test", "race-1", "r1@idp.test"), now)
        .await
        .expect("user 1")
        .user;
    let u2 = store
        .upsert_from_oidc(&profile("https://idp.test", "race-2", "r2@idp.test"), now)
        .await
        .expect("user 2")
        .user;

    let s1 = Arc::clone(&store);
    let s2 = Arc::clone(&store);
    let t1 = tokio::spawn(async move {
        s1.bootstrap_initial_org_as_owner(u1.id, "u1", "U1", Language::DEFAULT, now)
            .await
    });
    let t2 = tokio::spawn(async move {
        s2.bootstrap_initial_org_as_owner(u2.id, "u2", "U2", Language::DEFAULT, now)
            .await
    });
    let r1 = t1.await.expect("join 1").expect("bootstrap 1");
    let r2 = t2.await.expect("join 2").expect("bootstrap 2");

    // Exactly one racer wins.
    assert_ne!(
        r1.is_some(),
        r2.is_some(),
        "exactly one of the two concurrent first logins bootstraps"
    );

    // And the database holds exactly one org with exactly one owner.
    let org_count: i64 = sqlx::query_scalar("SELECT count(*) FROM organizations")
        .fetch_one(&pool)
        .await
        .expect("count orgs");
    assert_eq!(org_count, 1, "exactly one initial org");
    let owner_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM org_members WHERE role = 'owner'")
            .fetch_one(&pool)
            .await
            .expect("count owners");
    assert_eq!(owner_count, 1, "exactly one owner membership");
}

/// `read_profiles` resolves each user's display identity for chat surfaces
/// that can't JOIN `users` under RLS (migration 14). Name uses the roster
/// formula: explicit `display_name` wins, else the email local-part.
#[sqlx::test]
async fn read_profiles_resolves_name_with_email_fallback(pool: PgPool) {
    let store = PgUserStore::new(pool.clone());
    let now = chrono::Utc::now();

    // Named user with an avatar.
    let named = patom::auth::UserId::new();
    sqlx::query(
        "INSERT INTO users (id, email, display_name, avatar_url, created_at, updated_at) \
         VALUES ($1, $2, $3, $4, $5, $5)",
    )
    .bind(named)
    .bind("named@example.test")
    .bind("Ada Lovelace")
    .bind("https://h.test/ada.png")
    .bind(now)
    .execute(&pool)
    .await
    .expect("insert named user");

    // Nameless user — name must fall back to the email local-part.
    let nameless = patom::auth::UserId::new();
    sqlx::query(
        "INSERT INTO users (id, email, display_name, created_at, updated_at) \
         VALUES ($1, $2, NULL, $3, $3)",
    )
    .bind(nameless)
    .bind("grace@example.test")
    .bind(now)
    .execute(&pool)
    .await
    .expect("insert nameless user");

    let map = store
        .read_profiles(&[named, nameless])
        .await
        .expect("read profiles");

    let a = map.get(&named).expect("named present");
    assert_eq!(a.name, "Ada Lovelace");
    assert_eq!(a.avatar_url.as_deref(), Some("https://h.test/ada.png"));

    let g = map.get(&nameless).expect("nameless present");
    assert_eq!(g.name, "grace", "name falls back to email local-part");
    assert_eq!(g.avatar_url, None);
}

#[sqlx::test]
async fn read_profiles_empty_input_returns_empty_map(pool: PgPool) {
    let store = PgUserStore::new(pool);
    let map = store.read_profiles(&[]).await.expect("read profiles");
    assert!(map.is_empty());
}
