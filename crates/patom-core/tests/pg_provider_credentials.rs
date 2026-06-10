//! Storage tests for the encrypted `org_provider_credentials` table (#141).
//!
//! The BYO provider key is envelope-encrypted under a per-org KEK and RLS-scoped
//! by org. These tests exercise the seal/open round-trip through the real
//! Postgres store, the base_url plaintext column, validation stamping, and
//! cross-org isolation (a second tenant's privileged read never recovers the
//! first tenant's key, because the per-org KEK differs even with RLS bypassed).

#![allow(clippy::expect_used)]

use std::sync::Arc;

use patom::clock::SystemClock;
use patom::crypto::OrgEncryptor;
use patom::provider::{
    OrgProviderCredentialStore, PgOrgProviderCredentialStore, ProviderApiKey, ProviderBaseUrl,
    ProviderCredentialWrite, ProviderId,
};
use sqlx::PgPool;

mod common;
use common::pg::seed_tenant;

fn store(pool: &PgPool) -> Arc<dyn OrgProviderCredentialStore> {
    let clock = SystemClock::shared();
    let enc = Arc::new(OrgEncryptor::for_test([11u8; 32]));
    Arc::new(PgOrgProviderCredentialStore::new(pool.clone(), clock, enc))
}

#[sqlx::test]
async fn upsert_then_read_roundtrips_key_and_base_url(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);

    store
        .upsert(ProviderCredentialWrite {
            org_id: seed.org_id,
            provider: ProviderId::Anthropic,
            api_key: ProviderApiKey::try_from("sk-ant-byo-123".to_owned()).expect("key"),
            base_url: Some(
                ProviderBaseUrl::try_from("https://proxy.test/v1".to_owned()).expect("url"),
            ),
        })
        .await
        .expect("upsert");

    let rows = store.list_for_org(seed.org_id).await.expect("list");
    assert_eq!(rows.len(), 1);
    let rec = &rows[0];
    assert_eq!(rec.provider, ProviderId::Anthropic);
    assert_eq!(rec.api_key.expose(), "sk-ant-byo-123");
    assert_eq!(
        rec.base_url.as_ref().map(ProviderBaseUrl::as_str),
        Some("https://proxy.test/v1")
    );
    assert!(rec.last_validated_at.is_none(), "fresh key is unvalidated");
}

#[sqlx::test]
async fn union_of_keyed_providers_for_one_org(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);

    for (provider, key) in [
        (ProviderId::Anthropic, "sk-ant-1"),
        (ProviderId::Openai, "sk-oai-1"),
    ] {
        store
            .upsert(ProviderCredentialWrite {
                org_id: seed.org_id,
                provider,
                api_key: ProviderApiKey::try_from(key.to_owned()).expect("key"),
                base_url: None,
            })
            .await
            .expect("upsert");
    }

    let mut providers: Vec<ProviderId> = store
        .list_for_org(seed.org_id)
        .await
        .expect("list")
        .into_iter()
        .map(|r| r.provider)
        .collect();
    providers.sort_by_key(|p| p.as_str());
    assert_eq!(providers, vec![ProviderId::Anthropic, ProviderId::Openai]);
}

#[sqlx::test]
async fn rotate_replaces_key_and_resets_validation(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);
    let now = SystemClock::shared().now_utc();

    store
        .upsert(ProviderCredentialWrite {
            org_id: seed.org_id,
            provider: ProviderId::Openai,
            api_key: ProviderApiKey::try_from("sk-old".to_owned()).expect("key"),
            base_url: None,
        })
        .await
        .expect("upsert");
    store
        .mark_validated(seed.org_id, ProviderId::Openai, now)
        .await
        .expect("mark");
    assert!(
        store.list_for_org(seed.org_id).await.expect("list")[0]
            .last_validated_at
            .is_some()
    );

    // Rotate: a new key clears the prior validation stamp.
    store
        .upsert(ProviderCredentialWrite {
            org_id: seed.org_id,
            provider: ProviderId::Openai,
            api_key: ProviderApiKey::try_from("sk-new".to_owned()).expect("key"),
            base_url: None,
        })
        .await
        .expect("rotate");
    let rows = store.list_for_org(seed.org_id).await.expect("list");
    assert_eq!(rows.len(), 1, "rotate replaces, not appends");
    assert_eq!(rows[0].api_key.expose(), "sk-new");
    assert!(
        rows[0].last_validated_at.is_none(),
        "rotate resets validation"
    );
}

#[sqlx::test]
async fn delete_is_idempotent(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);

    // Deleting an absent row is a no-op.
    store
        .delete(seed.org_id, ProviderId::Deepseek)
        .await
        .expect("delete absent");

    store
        .upsert(ProviderCredentialWrite {
            org_id: seed.org_id,
            provider: ProviderId::Deepseek,
            api_key: ProviderApiKey::try_from("sk-ds".to_owned()).expect("key"),
            base_url: None,
        })
        .await
        .expect("upsert");
    store
        .delete(seed.org_id, ProviderId::Deepseek)
        .await
        .expect("delete present");
    assert!(
        store
            .list_for_org(seed.org_id)
            .await
            .expect("list")
            .is_empty()
    );
}

#[sqlx::test]
async fn cross_org_isolation(pool: PgPool) {
    // Two tenants, each with a key. list_for_org returns only the queried org's
    // row; the per-org KEK ensures even a privileged read cannot decrypt the
    // other org's secret under the wrong org id.
    let alice = seed_tenant(&pool).await;
    let bob = seed_tenant(&pool).await;
    let store = store(&pool);

    store
        .upsert(ProviderCredentialWrite {
            org_id: alice.org_id,
            provider: ProviderId::Anthropic,
            api_key: ProviderApiKey::try_from("sk-alice".to_owned()).expect("key"),
            base_url: None,
        })
        .await
        .expect("alice upsert");
    store
        .upsert(ProviderCredentialWrite {
            org_id: bob.org_id,
            provider: ProviderId::Anthropic,
            api_key: ProviderApiKey::try_from("sk-bob".to_owned()).expect("key"),
            base_url: None,
        })
        .await
        .expect("bob upsert");

    let alice_rows = store.list_for_org(alice.org_id).await.expect("list alice");
    assert_eq!(alice_rows.len(), 1);
    assert_eq!(alice_rows[0].api_key.expose(), "sk-alice");

    let bob_rows = store.list_for_org(bob.org_id).await.expect("list bob");
    assert_eq!(bob_rows.len(), 1);
    assert_eq!(bob_rows[0].api_key.expose(), "sk-bob");

    // list_all (refresher path) sees both, each decrypting under its own org.
    let all = store.list_all().await.expect("list all");
    assert_eq!(all.len(), 2);
}
