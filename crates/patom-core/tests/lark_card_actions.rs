//! Lark card-action credential tests (issue #214).
//!
//! The `POST /lark/card-actions` route verifies each callback with the app's
//! sealed Encrypt Key + Verification Token (migration 92). These tests cover the
//! store round-trip — register → seal → read-back → open — and the fail-closed
//! "credentials absent" path the route relies on (an app with no card creds 404s
//! rather than accepting an unverifiable callback).
//!
//! The pure verification math lives in `lark::card_verify` unit tests; the
//! decision seam (authorize → decide → resume) is integration-tested via the
//! Discord button path in `discord_bridge.rs` (the shared `ApprovalDecider`).

#![allow(clippy::expect_used)]

use std::sync::Arc;

use sqlx::PgPool;

use patom::auth::Caller;
use patom::clock::{SharedClock, SystemClock};
use patom::crypto::OrgEncryptor;
use patom::lark::app_store::{LarkAppStore, NewLarkApp, PgLarkAppStore};
use patom::lark::types::{LarkAppId, LarkAppSecret, LarkEncryptKey, LarkVerificationToken};

mod common;
use common::pg::seed_tenant;

fn store(pool: &PgPool) -> PgLarkAppStore {
    let clock: SharedClock = SystemClock::shared();
    let enc = Arc::new(OrgEncryptor::for_test([7u8; 32]));
    PgLarkAppStore::new(pool.clone(), clock, enc)
}

#[sqlx::test]
async fn card_credentials_round_trip_seals_and_opens(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);
    let caller = Caller::new(seed.user_id, seed.org_id);
    let app_id = LarkAppId::try_from("cli_card_app").expect("app id");

    store
        .register(
            &caller,
            NewLarkApp {
                app_id: app_id.clone(),
                agent_id: seed.agent_id,
                app_secret: LarkAppSecret::try_from("app-secret".to_owned()).expect("secret"),
                card_encrypt_key: Some(
                    LarkEncryptKey::try_from("encrypt-key-123".to_owned()).expect("ek"),
                ),
                card_verification_token: Some(
                    LarkVerificationToken::try_from("verify-token-456".to_owned()).expect("vt"),
                ),
            },
        )
        .await
        .expect("register app with card credentials");

    let creds = store
        .card_credentials(&app_id)
        .await
        .expect("card_credentials query")
        .expect("credentials present");
    assert_eq!(creds.org_id, seed.org_id);
    assert_eq!(creds.encrypt_key.expose(), "encrypt-key-123");
    assert_eq!(creds.verification_token.expose(), "verify-token-456");
}

#[sqlx::test]
async fn card_credentials_absent_when_unconfigured(pool: PgPool) {
    // A long-connection-only app sets no card credentials; the route must 404
    // (fail-closed) rather than accept an unverifiable callback.
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);
    let caller = Caller::new(seed.user_id, seed.org_id);
    let app_id = LarkAppId::try_from("cli_plain_app").expect("app id");

    store
        .register(
            &caller,
            NewLarkApp {
                app_id: app_id.clone(),
                agent_id: seed.agent_id,
                app_secret: LarkAppSecret::try_from("app-secret".to_owned()).expect("secret"),
                card_encrypt_key: None,
                card_verification_token: None,
            },
        )
        .await
        .expect("register app without card credentials");

    assert!(
        store
            .card_credentials(&app_id)
            .await
            .expect("card_credentials query")
            .is_none(),
        "an app without card credentials reads back as None",
    );
}

#[sqlx::test]
async fn card_credentials_unknown_app_is_none(pool: PgPool) {
    // An unknown app and a known-but-unconfigured app are indistinguishable to
    // the route (both 404), so both read back as `None` — fail-closed.
    let _seed = seed_tenant(&pool).await;
    let store = store(&pool);
    let app_id = LarkAppId::try_from("cli_missing").expect("app id");
    assert!(
        store
            .card_credentials(&app_id)
            .await
            .expect("card_credentials query")
            .is_none(),
    );
}
