//! `PgDiscordAppStore` registration + token-decryption tests.
//!
//! Covers the static-token lifecycle the gateway manager and poster depend on:
//! register (sealing the bot token at rest), read-back, the `BotTokenSource`
//! decrypt round-trip, the `READY`-time `bot_user_id` recording, and tenant-
//! scoped delete.

#![allow(clippy::expect_used)]

use std::sync::Arc;

use sqlx::PgPool;

use patom::auth::Caller;
use patom::clock::{SharedClock, SystemClock};
use patom::crypto::{OrgEncryptor, SharedOrgEncryptor};
use patom::discord::app_store::{
    BotTokenSource, DiscordAppStore, NewDiscordApp, PgDiscordAppStore,
};
use patom::discord::types::{ApplicationId, BotToken, DiscordUserId};

mod common;
use common::pg::seed_tenant;

const APP_ID: &str = "123456789012345678";
const BOT_TOKEN: &str = "MTk4NjIzODc0MTI0MzM3MzQ0.example.7b9c-decrypt-me";

fn store(pool: &PgPool) -> PgDiscordAppStore {
    let clock: SharedClock = SystemClock::shared();
    let enc: SharedOrgEncryptor = Arc::new(OrgEncryptor::for_test([7u8; 32]));
    PgDiscordAppStore::new(pool.clone(), clock, enc)
}

fn new_app(agent_id: patom::agents::AgentId) -> NewDiscordApp {
    NewDiscordApp {
        application_id: ApplicationId::try_from(APP_ID).expect("app id"),
        agent_id,
        bot_token: BotToken::try_from(BOT_TOKEN.to_owned()).expect("token"),
    }
}

#[sqlx::test]
async fn register_then_read_and_decrypt_token(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);
    let caller = Caller::new(seed.user_id, seed.org_id);
    let app_id = ApplicationId::try_from(APP_ID).expect("app id");

    store
        .register(&caller, new_app(seed.agent_id))
        .await
        .expect("register");

    // The projection reads back without the token; bot_user_id is None pre-READY.
    let app = store.read_by_app_id(&app_id).await.expect("read");
    assert_eq!(app.org_id, seed.org_id);
    assert_eq!(app.agent_id, seed.agent_id);
    assert_eq!(app.application_id.as_str(), APP_ID);
    assert!(app.bot_user_id.is_none());

    // The BotTokenSource decrypts the sealed token back to the original bytes.
    let token = store.token(&app_id).await.expect("decrypt token");
    assert_eq!(token.expose(), BOT_TOKEN);

    // It appears in the org's list and the agent reverse-lookup.
    let listed = store.list(&caller).await.expect("list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].application_id.as_str(), APP_ID);
    let by_agent = store
        .app_id_for_agent(seed.org_id, seed.agent_id)
        .await
        .expect("reverse lookup");
    assert_eq!(
        by_agent.map(|a| a.as_str().to_owned()),
        Some(APP_ID.to_owned())
    );

    // And as a connect target for the gateway manager.
    let targets = store.list_connect_targets().await.expect("connect targets");
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].org_id, seed.org_id);
}

#[sqlx::test]
async fn set_bot_user_id_is_recorded(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);
    let caller = Caller::new(seed.user_id, seed.org_id);
    let app_id = ApplicationId::try_from(APP_ID).expect("app id");
    store
        .register(&caller, new_app(seed.agent_id))
        .await
        .expect("register");

    let bot_user = DiscordUserId::try_from("80351110224678912").expect("bot user");
    store
        .set_bot_user_id(&app_id, &bot_user)
        .await
        .expect("set bot user");

    let app = store.read_by_app_id(&app_id).await.expect("read");
    assert_eq!(app.bot_user_id, Some(bot_user));
}

#[sqlx::test]
async fn register_upsert_rotates_token(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);
    let caller = Caller::new(seed.user_id, seed.org_id);
    let app_id = ApplicationId::try_from(APP_ID).expect("app id");
    store
        .register(&caller, new_app(seed.agent_id))
        .await
        .expect("register");

    // Re-register with a fresh token (the rotation path) → upsert in place.
    let rotated = "MTk4NjIzODc0MTI0MzM3MzQ0.rotated.new-token-value";
    store
        .register(
            &caller,
            NewDiscordApp {
                application_id: app_id.clone(),
                agent_id: seed.agent_id,
                bot_token: BotToken::try_from(rotated.to_owned()).expect("token"),
            },
        )
        .await
        .expect("re-register");

    let token = store.token(&app_id).await.expect("decrypt");
    assert_eq!(token.expose(), rotated, "the rotated token is now live");
    assert_eq!(
        store.list(&caller).await.expect("list").len(),
        1,
        "still one row"
    );
}

#[sqlx::test]
async fn delete_removes_the_app(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = store(&pool);
    let caller = Caller::new(seed.user_id, seed.org_id);
    let app_id = ApplicationId::try_from(APP_ID).expect("app id");
    store
        .register(&caller, new_app(seed.agent_id))
        .await
        .expect("register");

    store.delete(&caller, &app_id).await.expect("delete");

    // Gone: read_by_app_id surfaces UnknownApp, and a second delete is a 404.
    assert!(store.read_by_app_id(&app_id).await.is_err());
    assert!(store.delete(&caller, &app_id).await.is_err());
}
