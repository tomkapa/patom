//! #218: the per-org sandbox egress allowlist (`org_egress_allowlist`, migration
//! 92). Default-deny is the floor — an org with no row resolves to an empty
//! allowlist and `EgressPolicy::DenyAll`. A round-trip preserves the hosts. The
//! deny floor is enforced on the way in, so a blocked host (loopback / metadata)
//! can never be stored. These are the safety-critical paths, so the refusals
//! matter as much as the happy round-trip.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use patom::sandbox::{EgressAllowlist, EgressPolicy, OrgEgressStore, PgOrgEgressStore};
use sqlx::PgPool;

mod common;
use common::pg::seed_tenant;

#[sqlx::test]
async fn missing_row_defaults_to_deny_all(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = PgOrgEgressStore::new(pool);

    let allowlist = store
        .allowlist_for_org(seed.org_id)
        .await
        .expect("default lookup");
    assert!(allowlist.is_empty(), "an unconfigured org must be empty");
    assert_eq!(
        allowlist.to_policy(),
        EgressPolicy::DenyAll,
        "empty allowlist must resolve to deny-all"
    );
}

#[sqlx::test]
async fn set_then_get_round_trips(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = PgOrgEgressStore::new(pool);

    let allow = EgressAllowlist::try_from(vec![
        "api.example.com".to_owned(),
        "files.example.org".to_owned(),
    ])
    .expect("valid allowlist");
    store
        .set_allowlist(seed.org_id, allow.clone())
        .await
        .expect("set");

    let back = store
        .allowlist_for_org(seed.org_id)
        .await
        .expect("get after set");
    assert_eq!(back, allow);
    assert!(matches!(back.to_policy(), EgressPolicy::Allow(hosts) if hosts.len() == 2));
}

#[sqlx::test]
async fn upsert_replaces_previous_allowlist(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = PgOrgEgressStore::new(pool);

    let first = EgressAllowlist::try_from(vec!["a.example.com".to_owned()]).expect("first");
    store
        .set_allowlist(seed.org_id, first)
        .await
        .expect("set 1");
    let second = EgressAllowlist::try_from(vec!["b.example.com".to_owned()]).expect("second");
    store
        .set_allowlist(seed.org_id, second.clone())
        .await
        .expect("set 2");

    let back = store.allowlist_for_org(seed.org_id).await.expect("get");
    assert_eq!(back, second, "the second write must replace the first");
}

#[sqlx::test]
async fn deny_floor_rejects_blocked_hosts_before_storage(_pool: PgPool) {
    // The newtype refuses a blocked host at construction, so a denied target can
    // never even reach `set_allowlist`.
    for bad in ["localhost", "169.254.169.254", "10.0.0.1", "127.0.0.1"] {
        assert!(
            EgressAllowlist::try_from(vec![bad.to_owned()]).is_err(),
            "`{bad}` must be rejected by the deny floor"
        );
    }
}
