//! Tests for the per-org BYO provider overlay (#141).
//!
//! The overlay is the synchronous hot-path read the agent factory uses to route
//! a turn to a BYO client. These tests drive it through the real Postgres
//! credential store: a refresh builds a client for every keyed (org, provider),
//! surfaces per-org default models, and isolates orgs from one another.

#![allow(clippy::expect_used)]

use std::sync::Arc;

use patom::clock::SystemClock;
use patom::crypto::OrgEncryptor;
use patom::provider::{
    Model, OrgProviderOverlay, PgOrgProviderCredentialStore, ProviderApiKey,
    ProviderCredentialWrite, ProviderId, SharedOrgProviderCredentialStore,
};
use sqlx::PgPool;

mod common;
use common::pg::seed_tenant;

fn store(pool: &PgPool) -> SharedOrgProviderCredentialStore {
    let clock = SystemClock::shared();
    let enc = Arc::new(OrgEncryptor::for_test([23u8; 32]));
    Arc::new(PgOrgProviderCredentialStore::new(pool.clone(), clock, enc))
}

async fn add_key(
    store: &SharedOrgProviderCredentialStore,
    org: patom::auth::OrgId,
    p: ProviderId,
    key: &str,
) {
    store
        .upsert(ProviderCredentialWrite {
            org_id: org,
            provider: p,
            api_key: ProviderApiKey::try_from(key.to_owned()).expect("key"),
            base_url: None,
        })
        .await
        .expect("upsert");
}

#[sqlx::test]
async fn refresh_builds_clients_for_keyed_providers(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);
    add_key(&store, seed.org_id, ProviderId::Anthropic, "sk-ant-x").await;
    add_key(&store, seed.org_id, ProviderId::Openai, "sk-oai-x").await;

    let overlay = OrgProviderOverlay::new(store);
    assert!(overlay.is_empty(), "empty before refresh");
    overlay.refresh().await.expect("refresh");

    // Keyed providers resolve to a BYO client; unkeyed do not.
    assert!(overlay.get(seed.org_id, ProviderId::Anthropic).is_some());
    assert!(overlay.get(seed.org_id, ProviderId::Openai).is_some());
    assert!(overlay.get(seed.org_id, ProviderId::Deepseek).is_none());
    assert!(overlay.has_key(seed.org_id, ProviderId::Anthropic));
    assert!(!overlay.has_key(seed.org_id, ProviderId::Deepseek));
}

#[sqlx::test]
async fn refresh_surfaces_per_org_default_model(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);
    add_key(&store, seed.org_id, ProviderId::Openai, "sk-oai-x").await;
    let model = Model::try_from("gpt-5.4-mini").expect("model");
    store
        .set_default_model(seed.org_id, model)
        .await
        .expect("set default");

    let overlay = OrgProviderOverlay::new(store);
    overlay.refresh().await.expect("refresh");

    assert_eq!(overlay.default_model(seed.org_id), Some(model));
}

#[sqlx::test]
async fn newly_added_key_visible_after_refresh(pool: PgPool) {
    // Models the "immediate activation" path: a key added after the first
    // refresh becomes routable on the next refresh (which the CRUD handler
    // triggers), without rebuilding the overlay handle.
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);
    let overlay = OrgProviderOverlay::new(store.clone());
    overlay.refresh().await.expect("refresh 1");
    assert!(overlay.get(seed.org_id, ProviderId::Anthropic).is_none());

    add_key(&store, seed.org_id, ProviderId::Anthropic, "sk-ant-new").await;
    overlay.refresh().await.expect("refresh 2");
    assert!(
        overlay.get(seed.org_id, ProviderId::Anthropic).is_some(),
        "key added post-refresh routes after the next refresh"
    );
}

#[sqlx::test]
async fn orgs_are_isolated_in_overlay(pool: PgPool) {
    let alice = seed_tenant(&pool).await;
    let bob = seed_tenant(&pool).await;
    let store = store(&pool);
    add_key(&store, alice.org_id, ProviderId::Anthropic, "sk-alice").await;
    add_key(&store, bob.org_id, ProviderId::Openai, "sk-bob").await;

    let overlay = OrgProviderOverlay::new(store);
    overlay.refresh().await.expect("refresh");

    // Alice's Anthropic key is hers alone; Bob has only OpenAI.
    assert!(overlay.has_key(alice.org_id, ProviderId::Anthropic));
    assert!(!overlay.has_key(alice.org_id, ProviderId::Openai));
    assert!(overlay.has_key(bob.org_id, ProviderId::Openai));
    assert!(!overlay.has_key(bob.org_id, ProviderId::Anthropic));
    assert_eq!(overlay.len(), 2);
}

#[sqlx::test]
async fn delete_then_refresh_drops_the_client(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);
    add_key(&store, seed.org_id, ProviderId::Deepseek, "sk-ds").await;
    let overlay = OrgProviderOverlay::new(store.clone());
    overlay.refresh().await.expect("refresh 1");
    assert!(overlay.get(seed.org_id, ProviderId::Deepseek).is_some());

    store
        .delete(seed.org_id, ProviderId::Deepseek)
        .await
        .expect("delete");
    overlay.refresh().await.expect("refresh 2");
    assert!(overlay.get(seed.org_id, ProviderId::Deepseek).is_none());
}
