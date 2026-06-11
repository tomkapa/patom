//! Integration tests for the Slack Phase-2 identity-link store
//! (`SlackIdentityStore::link_with_org` / `unlink`), GitHub issue #41.
//!
//! These exercise the real Postgres schema (FKs, the `slack_identities`
//! RLS policy, and the `org_members_mint_colleague` trigger) via
//! `#[sqlx::test]`, so they assert the behaviour the bridge and the
//! completion route depend on: a fresh Slack member is made an org
//! member, gets a Human colleague, and resolves to their own identity.

use patom::auth::{OrgId, Principal, Role, UserId};
use patom::channels::ChannelId;
use patom::clock::{SharedClock, SystemClock};
use patom::slack::channel_map::{PgSlackChannelStore, SlackChannelStore};
use patom::slack::identity::{LinkedVia, PgSlackIdentityStore, SharedSlackIdentityStore};
use patom::slack::types::{SlackChannelId, SlackTeamId, SlackUserId};
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

// ── Channel-thread mapping (issue #41, point 3) ─────────────────────────

fn channel_store(pool: PgPool) -> PgSlackChannelStore {
    PgSlackChannelStore::new(pool, SystemClock::shared())
}

/// Count `channel_members` rows for a channel — proves multi-human membership.
async fn member_count(pool: &PgPool, channel: ChannelId) -> i64 {
    let (n,): (i64,) = sqlx::query_as("SELECT count(*) FROM channel_members WHERE channel_id = $1")
        .bind(channel)
        .fetch_one(pool)
        .await
        .expect("count members");
    n
}

#[sqlx::test]
async fn ensure_channel_maps_creates_and_adds_member(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    seed_workspace(&pool, seed.org_id, seed.user_id).await;
    let slack_chan = SlackChannelId::try_from("C0CHAN01").expect("valid");
    let store = channel_store(pool.clone());

    let cid = store
        .ensure_channel(seed.org_id, &team(), &slack_chan, seed.user_id)
        .await
        .expect("ensure");

    // A Patom channel was created with the derived slug, and the mapping
    // points at it.
    let (name,): (String,) = sqlx::query_as("SELECT name FROM channels WHERE id = $1")
        .bind(cid)
        .fetch_one(&pool)
        .await
        .expect("channel row");
    assert_eq!(name, "slack-c0chan01");
    let (mapped,): (ChannelId,) =
        sqlx::query_as("SELECT channel_id FROM slack_channels WHERE slack_channel_id = $1")
            .bind("C0CHAN01")
            .fetch_one(&pool)
            .await
            .expect("mapping row");
    assert_eq!(mapped, cid);
    assert_eq!(member_count(&pool, cid).await, 1, "creator is a member");
}

#[sqlx::test]
async fn ensure_channel_is_idempotent_and_admits_a_second_human(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    seed_workspace(&pool, seed.org_id, seed.user_id).await;
    let second = seed_user(&pool).await;
    // The second human must be an org member for the FK + colleague model;
    // link them so they're enrolled (link_with_org adds org_members).
    store(pool.clone())
        .link_with_org(
            second,
            seed.org_id,
            &team(),
            &SlackUserId::try_from("U0SECOND").expect("valid"),
            LinkedVia::SlackOauth,
        )
        .await
        .expect("link second human");

    let slack_chan = SlackChannelId::try_from("C0SHARED").expect("valid");
    let cm = channel_store(pool.clone());

    let first_call = cm
        .ensure_channel(seed.org_id, &team(), &slack_chan, seed.user_id)
        .await
        .expect("first");
    let second_call = cm
        .ensure_channel(seed.org_id, &team(), &slack_chan, second)
        .await
        .expect("second");

    assert_eq!(
        first_call, second_call,
        "same Slack channel → one Patom channel"
    );
    assert_eq!(
        member_count(&pool, first_call).await,
        2,
        "both humans are members of the one channel (multi-human collaboration)"
    );
}

#[sqlx::test]
async fn distinct_slack_channels_map_to_distinct_patom_channels(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    seed_workspace(&pool, seed.org_id, seed.user_id).await;
    let cm = channel_store(pool.clone());

    let a = cm
        .ensure_channel(
            seed.org_id,
            &team(),
            &SlackChannelId::try_from("C0AAA").expect("valid"),
            seed.user_id,
        )
        .await
        .expect("a");
    let b = cm
        .ensure_channel(
            seed.org_id,
            &team(),
            &SlackChannelId::try_from("C0BBB").expect("valid"),
            seed.user_id,
        )
        .await
        .expect("b");
    assert_ne!(a, b, "different Slack channels are isolated Patom channels");
}

// ── Per-platform name override resolution (issue #41) ───────────────────

#[sqlx::test]
async fn slack_backed_thread_overrides_human_name_with_slack_handle(pool: PgPool) {
    use patom::auth::Caller;
    use patom::colleagues::{ColleagueName, ThreadDisplayNames};
    use patom::slack::display_overrides::PgSlackThreadDisplayNames;
    use patom::threads::{PgThreadStore, ThreadId, ThreadStore};

    let seed = seed_tenant(&pool).await;
    seed_workspace(&pool, seed.org_id, seed.user_id).await;
    let id = store(pool.clone());
    let cm = channel_store(pool.clone());
    let slack_user = SlackUserId::try_from("U0OWNER").expect("valid");
    let slack_chan = SlackChannelId::try_from("C0OVR01").expect("valid");

    // Link the human + record their Slack handle (the per-platform label).
    id.link_with_org(
        seed.user_id,
        seed.org_id,
        &team(),
        &slack_user,
        LinkedVia::Installer,
    )
    .await
    .expect("link");
    id.set_display_name(&team(), &slack_user, "tomkapa")
        .await
        .expect("set slack name");

    // Mirror the Slack channel and open a channel thread in it.
    let channel_id = cm
        .ensure_channel(seed.org_id, &team(), &slack_chan, seed.user_id)
        .await
        .expect("channel");
    let owner_colleague =
        patom::colleagues::resolve_user_colleague(&pool, seed.org_id, seed.user_id)
            .await
            .expect("colleague");
    let threads = PgThreadStore::new(pool.clone(), SystemClock::shared());
    let thread = threads
        .create_thread(
            &Caller::new(seed.user_id, seed.org_id),
            Some(channel_id),
            None,
            owner_colleague,
            None,
        )
        .await
        .expect("thread");

    let provider = PgSlackThreadDisplayNames::new(pool.clone());
    let overrides = provider.overrides_for_thread(thread).await;
    assert_eq!(
        overrides.get(&owner_colleague).map(ColleagueName::as_str),
        Some("tomkapa"),
        "the agent sees the Slack handle for a Slack-backed thread, keyed by canonical colleague_id"
    );

    // A thread with no Slack-channel mapping resolves no overrides
    // (canonical names everywhere else).
    assert!(
        provider
            .overrides_for_thread(ThreadId::new())
            .await
            .is_empty(),
        "non-Slack threads get canonical names"
    );
}
