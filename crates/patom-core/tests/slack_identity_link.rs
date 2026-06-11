//! Integration tests for the Slack Phase-2 identity-link store
//! (`SlackIdentityStore::link_with_org` / `unlink`), GitHub issue #41.
//!
//! These exercise the real Postgres schema (FKs, the `slack_identities`
//! RLS policy, and the `org_members_mint_colleague` trigger) via
//! `#[sqlx::test]`, so they assert the behaviour the bridge and the
//! completion route depend on: a fresh Slack member is made an org
//! member, gets a Human colleague, and resolves to their own identity.

use patom::auth::{OrgId, Principal, Role, UserId};
use patom::clock::{SharedClock, SystemClock};
use patom::slack::identity::{LinkedVia, PgSlackIdentityStore, SharedSlackIdentityStore};
use patom::slack::types::{SlackTeamId, SlackUserId};
use sqlx::PgPool;
use uuid::Uuid;

mod common;
use common::pg::seed_tenant;

const TEAM: &str = "T0LINKED";

/// Insert a `slack_workspaces` row directly (table owner, RLS-bypassing in
/// the test image) so `slack_identities`' composite FK to
/// `(org_id, team_id)` is satisfiable. The bot-token crypto columns hold
/// arbitrary bytes — no test here reads the token back.
async fn seed_workspace(pool: &PgPool, org_id: OrgId, installer: UserId) {
    sqlx::query(
        "INSERT INTO slack_workspaces \
           (org_id, team_id, team_name, bot_user_id, bot_token_ciphertext, \
            bot_token_nonce, key_version, scopes, installed_by_user_id, installed_at) \
         VALUES ($1, $2, 'Link Test WS', 'UBOT001', $3, $4, 1, 'chat:write', $5, now())",
    )
    .bind(org_id)
    .bind(TEAM)
    .bind(vec![0u8; 16])
    .bind(vec![0u8; 12])
    .bind(installer)
    .execute(pool)
    .await
    .expect("seed slack workspace");
}

/// Insert a bare `users` row (no org membership, no OIDC identity) — the
/// shape of a freshly-onboarded Slack member before `link_with_org` runs.
async fn seed_user(pool: &PgPool) -> UserId {
    let id = UserId::new();
    let email = format!("member-{}@example.test", Uuid::new_v4().simple());
    sqlx::query(
        "INSERT INTO users (id, email, display_name, created_at, updated_at) \
         VALUES ($1, $2, 'Slack Member', now(), now())",
    )
    .bind(id)
    .bind(&email)
    .execute(pool)
    .await
    .expect("seed member user");
    id
}

fn store(pool: PgPool) -> SharedSlackIdentityStore {
    let clock: SharedClock = SystemClock::shared();
    std::sync::Arc::new(PgSlackIdentityStore::new(pool, clock))
}

fn team() -> SlackTeamId {
    SlackTeamId::try_from(TEAM).expect("valid team")
}

#[sqlx::test]
async fn link_with_org_creates_membership_colleague_and_link(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    seed_workspace(&pool, seed.org_id, seed.user_id).await;
    let member = seed_user(&pool).await;
    let slack_user = SlackUserId::try_from("U0MEMBER").expect("valid");
    let id = store(pool.clone());

    id.link_with_org(
        member,
        seed.org_id,
        &team(),
        &slack_user,
        LinkedVia::SlackOauth,
    )
    .await
    .expect("link");

    // 1. lookup resolves to the member's own identity in the workspace org.
    let linked = id
        .lookup(&team(), &slack_user)
        .await
        .expect("lookup")
        .expect("row present");
    assert_eq!(linked.user_id, member, "links to the member, not installer");
    assert_eq!(linked.org_id, seed.org_id);

    // 2. membership was established (role 'member').
    let role: (String,) =
        sqlx::query_as("SELECT role FROM org_members WHERE org_id = $1 AND user_id = $2")
            .bind(seed.org_id)
            .bind(member)
            .fetch_one(&pool)
            .await
            .expect("membership row");
    assert_eq!(role.0, "member");

    // 3. the colleague the agent addresses was minted by the trigger.
    patom::colleagues::resolve_user_colleague(&pool, seed.org_id, member)
        .await
        .expect("member has a Human colleague after linking");

    // 4. provenance recorded.
    let via: (Option<String>,) =
        sqlx::query_as("SELECT linked_via FROM slack_identities WHERE team_id = $1")
            .bind(TEAM)
            .fetch_one(&pool)
            .await
            .expect("identity row");
    assert_eq!(via.0.as_deref(), Some("slack_oauth"));
}

#[sqlx::test]
async fn link_with_org_is_idempotent_and_rebinds_user(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    seed_workspace(&pool, seed.org_id, seed.user_id).await;
    let first = seed_user(&pool).await;
    let second = seed_user(&pool).await;
    let slack_user = SlackUserId::try_from("U0REBIND").expect("valid");
    let id = store(pool.clone());

    id.link_with_org(
        first,
        seed.org_id,
        &team(),
        &slack_user,
        LinkedVia::SlackOauth,
    )
    .await
    .expect("first link");
    id.link_with_org(
        second,
        seed.org_id,
        &team(),
        &slack_user,
        LinkedVia::SlackOauth,
    )
    .await
    .expect("re-link");

    let linked = id
        .lookup(&team(), &slack_user)
        .await
        .expect("lookup")
        .expect("row present");
    assert_eq!(linked.user_id, second, "re-link rebinds to the newer user");

    let count: (i64,) = sqlx::query_as("SELECT count(*) FROM slack_identities WHERE team_id = $1")
        .bind(TEAM)
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(count.0, 1, "ON CONFLICT updates in place, no duplicate row");
}

#[sqlx::test]
async fn installer_link_preserves_owner_role(pool: PgPool) {
    let seed = seed_tenant(&pool).await; // seed.user_id is the org 'owner'
    seed_workspace(&pool, seed.org_id, seed.user_id).await;
    let slack_user = SlackUserId::try_from("U0OWNER").expect("valid");
    let id = store(pool.clone());

    id.link_with_org(
        seed.user_id,
        seed.org_id,
        &team(),
        &slack_user,
        LinkedVia::Installer,
    )
    .await
    .expect("installer link");

    let role: (String,) =
        sqlx::query_as("SELECT role FROM org_members WHERE org_id = $1 AND user_id = $2")
            .bind(seed.org_id)
            .bind(seed.user_id)
            .fetch_one(&pool)
            .await
            .expect("membership row");
    assert_eq!(
        role.0, "owner",
        "ON CONFLICT DO NOTHING must not downgrade owner"
    );
}

#[sqlx::test]
async fn unlink_removes_link_for_org_member(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    seed_workspace(&pool, seed.org_id, seed.user_id).await;
    let member = seed_user(&pool).await;
    let slack_user = SlackUserId::try_from("U0UNLINK").expect("valid");
    let id = store(pool.clone());

    id.link_with_org(
        member,
        seed.org_id,
        &team(),
        &slack_user,
        LinkedVia::SlackOauth,
    )
    .await
    .expect("link");

    // The owner (a member of the workspace's org) unlinks; their active
    // org is the workspace org here, but the delete is matched by
    // (team, slack_user) under the membership RLS policy.
    let principal = Principal {
        user_id: seed.user_id,
        active_org_id: seed.org_id,
        role: Role::Owner,
    };
    id.unlink(&principal, &team(), &slack_user)
        .await
        .expect("unlink");

    assert!(
        id.lookup(&team(), &slack_user)
            .await
            .expect("lookup")
            .is_none(),
        "row gone after unlink"
    );
}
