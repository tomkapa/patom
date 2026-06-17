//! Stage A (issue #178): the outbound router's reverse-channel lookups.
//!
//! Each platform's channel store maps a Patom `channel_id` to its external
//! chat/guild ids; the inverse `lookup_by_channel` is what the `OutboundRouter`
//! uses to decide a proactive Patom channel thread belongs to this surface.
//! These exercise the real `discord_channels` / `lark_channels` tables.

#![allow(clippy::expect_used)]

use patom::channels::ChannelId;
use patom::clock::SystemClock;
use patom::discord::channel_map::{DiscordChannelStore, PgDiscordChannelStore};
use patom::discord::types::{ContainerId, GuildId};
use patom::lark::channel_map::{LarkChannelStore, PgLarkChannelStore};
use patom::lark::types::{LarkChatId, TenantKey};
use sqlx::PgPool;

mod common;
use common::pg::seed_tenant;

#[sqlx::test]
async fn discord_lookup_by_channel_roundtrips(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = PgDiscordChannelStore::new(pool.clone(), SystemClock::shared());

    let guild = GuildId::try_from("222222222222222222").expect("guild");
    let container = ContainerId::try_from("333333333333333333").expect("container");
    let channel_id = store
        .ensure_channel(seed.org_id, &guild, &container, seed.user_id)
        .await
        .expect("ensure_channel");

    let binding = store
        .lookup_by_channel(channel_id)
        .await
        .expect("lookup_by_channel")
        .expect("channel is Discord-backed");
    assert_eq!(binding.guild_id, guild);
    assert_eq!(binding.discord_channel_id, container);
}

#[sqlx::test]
async fn discord_lookup_by_channel_unknown_is_none(pool: PgPool) {
    seed_tenant(&pool).await;
    let store = PgDiscordChannelStore::new(pool.clone(), SystemClock::shared());
    let unknown = ChannelId::new();
    let got = store
        .lookup_by_channel(unknown)
        .await
        .expect("lookup_by_channel");
    assert!(
        got.is_none(),
        "a never-mapped channel has no Discord binding"
    );
}

#[sqlx::test]
async fn lark_lookup_by_channel_roundtrips(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let store = PgLarkChannelStore::new(pool.clone(), SystemClock::shared());

    let tenant = TenantKey::try_from("tk_test").expect("tenant key");
    let chat = LarkChatId::try_from("oc_abc123def456").expect("chat id");
    let channel_id = store
        .ensure_channel(seed.org_id, &tenant, &chat, seed.user_id)
        .await
        .expect("ensure_channel");

    let binding = store
        .lookup_by_channel(channel_id)
        .await
        .expect("lookup_by_channel")
        .expect("channel is Lark-backed");
    assert_eq!(binding.tenant_key, tenant);
    assert_eq!(binding.chat_id, chat);
}

#[sqlx::test]
async fn lark_lookup_by_channel_unknown_is_none(pool: PgPool) {
    seed_tenant(&pool).await;
    let store = PgLarkChannelStore::new(pool.clone(), SystemClock::shared());
    let unknown = ChannelId::new();
    let got = store
        .lookup_by_channel(unknown)
        .await
        .expect("lookup_by_channel");
    assert!(got.is_none(), "a never-mapped channel has no Lark binding");
}
