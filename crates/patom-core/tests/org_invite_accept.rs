//! Trait-contract tests for token-based invite acceptance
//! ([`OrgStore::accept_invite`]) — the canonical accept path behind the
//! `/i/{slug}/{token}` link. Unlike `join_pending_invites` (keyed on the
//! IdP-verified email), this is keyed on possession of the URL token, so
//! it admits an already-authenticated user and switches them into the
//! inviting org.
//!
//! Each test runs against a fresh, fully-migrated database minted by
//! `#[sqlx::test]`.

#![allow(clippy::expect_used)]

use patom::auth::{
    Email, IssuerUrl, Language, OidcProfile, OidcSubject, PgUserStore, Role, UserId, UserStore,
};
use patom::orgs::{OrgError, OrgStore, PgOrgStore};
use sqlx::PgPool;

/// A syntactically-valid `InviteToken` (43-char URL-safe base64, the
/// minimum length) that is never persisted — used to probe the
/// unknown-token path.
const ABSENT_TOKEN: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

fn profile(subject: &str, email: &str) -> OidcProfile {
    OidcProfile {
        issuer: IssuerUrl::try_from("https://idp.test").expect("issuer"),
        subject: OidcSubject::try_from(subject).expect("subject"),
        email: Email::try_from(email).expect("email"),
        email_verified: true,
        display_name: Some("Test User".to_owned()),
        avatar_url: None,
        locale: None,
    }
}

/// Seed an admin + bootstrapped org and return `(orgs, users, org_id,
/// admin_id)` ready for invite issuance.
async fn seed_org(
    pool: &PgPool,
    now: chrono::DateTime<chrono::Utc>,
) -> (PgOrgStore, PgUserStore, patom::auth::OrgId, UserId) {
    let users = PgUserStore::new(pool.clone());
    let orgs = PgOrgStore::new(pool.clone());
    let admin = users
        .upsert_from_oidc(&profile("admin", "admin@corp.test"), now)
        .await
        .expect("admin")
        .user;
    let org = users
        .bootstrap_initial_org_as_owner(admin.id, "corp", "Corp", Language::DEFAULT, now)
        .await
        .expect("bootstrap")
        .expect("first ⇒ Some");
    (orgs, users, org.id, admin.id)
}

/// Issue one invite for `email` and return its cleartext token.
async fn issue_invite(
    orgs: &PgOrgStore,
    org_id: patom::auth::OrgId,
    admin_id: UserId,
    email: &str,
    role: Role,
    now: chrono::DateTime<chrono::Utc>,
    ttl: chrono::Duration,
) -> patom::auth::InviteToken {
    let email = Email::try_from(email).expect("email");
    let mut issued = orgs
        .create_invites(
            org_id,
            std::slice::from_ref(&email),
            role,
            admin_id,
            now,
            ttl,
        )
        .await
        .expect("create invite");
    assert_eq!(issued.len(), 1, "one email ⇒ one invite");
    issued.remove(0).token
}

/// Upsert a fresh invitee user (distinct identity from the admin).
async fn seed_invitee(users: &PgUserStore, email: &str) -> UserId {
    let now = chrono::Utc::now();
    users
        .upsert_from_oidc(&profile("invitee", email), now)
        .await
        .expect("invitee")
        .user
        .id
}

#[sqlx::test]
async fn accept_invite_joins_org_and_consumes(pool: PgPool) {
    let now = chrono::Utc::now();
    let (orgs, users, org_id, admin) = seed_org(&pool, now).await;
    let token = issue_invite(
        &orgs,
        org_id,
        admin,
        "teammate@corp.test",
        Role::Member,
        now,
        chrono::Duration::hours(48),
    )
    .await;
    // The invitee's account email need not match the invited address —
    // the token is the bearer capability.
    let invitee = seed_invitee(&users, "different@elsewhere.test").await;

    let accepted = orgs
        .accept_invite(invitee, &token, now)
        .await
        .expect("accept succeeds");
    assert_eq!(accepted.org_id, org_id, "joins the inviting org");
    assert_eq!(accepted.role, Role::Member, "joins with the invited role");
    assert_eq!(
        users
            .membership(invitee, org_id)
            .await
            .expect("membership read"),
        Some(Role::Member),
        "an org_members row now exists for the invitee",
    );
}

#[sqlx::test]
async fn accept_invite_rejects_already_consumed(pool: PgPool) {
    let now = chrono::Utc::now();
    let (orgs, users, org_id, admin) = seed_org(&pool, now).await;
    let token = issue_invite(
        &orgs,
        org_id,
        admin,
        "teammate@corp.test",
        Role::Member,
        now,
        chrono::Duration::hours(48),
    )
    .await;
    let invitee = seed_invitee(&users, "invitee@corp.test").await;

    orgs.accept_invite(invitee, &token, now)
        .await
        .expect("first accept");
    let again = orgs.accept_invite(invitee, &token, now).await;
    assert!(
        matches!(again, Err(OrgError::InviteAlreadyConsumed)),
        "a consumed token is rejected, got {again:?}",
    );
}

#[sqlx::test]
async fn accept_invite_rejects_expired(pool: PgPool) {
    let now = chrono::Utc::now();
    let (orgs, users, org_id, admin) = seed_org(&pool, now).await;
    let token = issue_invite(
        &orgs,
        org_id,
        admin,
        "teammate@corp.test",
        Role::Member,
        now,
        chrono::Duration::hours(1),
    )
    .await;
    let invitee = seed_invitee(&users, "invitee@corp.test").await;

    // Accept two hours later — the one-hour invite has lapsed.
    let later = now + chrono::Duration::hours(2);
    let outcome = orgs.accept_invite(invitee, &token, later).await;
    assert!(
        matches!(outcome, Err(OrgError::InviteExpired)),
        "an expired token is rejected, got {outcome:?}",
    );
    assert_eq!(
        users.membership(invitee, org_id).await.expect("membership"),
        None,
        "an expired accept does not create a membership",
    );
}

#[sqlx::test]
async fn accept_invite_rejects_unknown_token(pool: PgPool) {
    let now = chrono::Utc::now();
    let (orgs, users, _org_id, _admin) = seed_org(&pool, now).await;
    let invitee = seed_invitee(&users, "invitee@corp.test").await;
    let absent = patom::auth::InviteToken::try_from(ABSENT_TOKEN).expect("valid token shape");

    let outcome = orgs.accept_invite(invitee, &absent, now).await;
    assert!(
        matches!(outcome, Err(OrgError::NotFound)),
        "an unknown token is a 404, got {outcome:?}",
    );
}

#[sqlx::test]
async fn accept_invite_returns_effective_role_for_existing_member(pool: PgPool) {
    // The admin already owns the org. They accept a *member* invite minted
    // for their own address — the existing owner membership must win (no
    // demotion), and the returned role must be the persisted Owner, not
    // the invite's Member.
    let now = chrono::Utc::now();
    let (orgs, users, org_id, admin) = seed_org(&pool, now).await;
    let token = issue_invite(
        &orgs,
        org_id,
        admin,
        "admin@corp.test",
        Role::Member,
        now,
        chrono::Duration::hours(48),
    )
    .await;

    let accepted = orgs
        .accept_invite(admin, &token, now)
        .await
        .expect("accept succeeds");
    assert_eq!(accepted.org_id, org_id, "stays in the org");
    assert_eq!(
        accepted.role,
        Role::Owner,
        "returns the effective persisted role, not the invite's Member",
    );
    assert_eq!(
        users.membership(admin, org_id).await.expect("membership"),
        Some(Role::Owner),
        "the existing owner membership is not demoted",
    );
}
