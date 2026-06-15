//! Lark chat ↔ Patom channel mapping — `lark_channels` table.
//!
//! A Lark-rooted conversation is a normal multi-participant **channel
//! thread**, not a two-party DM. To get there, each Lark chat is mirrored
//! to a Patom `channels` row: the bridge get-or-creates the mapping on the
//! first inbound event and adds each acting linked human to the Patom
//! channel's `channel_members`, so a second human in the same Lark chat
//! can read and post (the DM visibility predicate would have excluded
//! them).
//!
//! Privileged throughout: the WS frame carries no Principal, and the org
//! comes from the resolved `lark_apps` registration, not a session.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use sqlx::PgPool;

use crate::auth::{OrgId, UserId, run_privileged};
use crate::channels::{ChannelId, ChannelName};
use crate::clock::SharedClock;

use super::error::LarkError;
use super::types::{LarkChatId, TenantKey};

#[async_trait]
pub trait LarkChannelStore: fmt::Debug + Send + Sync {
    /// Get-or-create the Patom channel mirroring a Lark chat, and ensure
    /// `user_id` is a member of it. Returns the Patom [`ChannelId`].
    /// Idempotent: repeated calls return the same channel and only top up
    /// membership.
    async fn ensure_channel(
        &self,
        org_id: OrgId,
        tenant_key: &TenantKey,
        chat_id: &LarkChatId,
        user_id: UserId,
    ) -> Result<ChannelId, LarkError>;
}

pub type SharedLarkChannelStore = Arc<dyn LarkChannelStore>;

pub struct PgLarkChannelStore {
    pool: PgPool,
    clock: SharedClock,
}

impl PgLarkChannelStore {
    #[must_use]
    pub fn new(pool: PgPool, clock: SharedClock) -> Self {
        Self { pool, clock }
    }
}

impl fmt::Debug for PgLarkChannelStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgLarkChannelStore").finish_non_exhaustive()
    }
}

/// Derive a valid Patom channel slug from a Lark chat id.
///
/// Lark chat ids are `oc_<hex>` (`[A-Za-z0-9_]`); the `ChannelName` rule is
/// `^[a-z0-9][a-z0-9-]{0,62}$`, so we lowercase and map `_`→`-`. A real
/// `oc_<32 hex>` is ~35 bytes, well under the 63-byte cap; a pathologically
/// long id would surface as a parse error (logged, the event dropped). A future
/// enhancement could fetch the human chat name via `im/v1/chats/:id`.
fn channel_name_for(chat_id: &LarkChatId) -> Result<ChannelName, LarkError> {
    let raw = format!("lark-{}", chat_id.as_str().to_lowercase().replace('_', "-"));
    Ok(ChannelName::try_from(raw.as_str())?)
}

#[async_trait]
impl LarkChannelStore for PgLarkChannelStore {
    async fn ensure_channel(
        &self,
        org_id: OrgId,
        tenant_key: &TenantKey,
        chat_id: &LarkChatId,
        user_id: UserId,
    ) -> Result<ChannelId, LarkError> {
        let now = self.clock.now_utc();
        let tenant = tenant_key.as_str().to_owned();
        let chat = chat_id.as_str().to_owned();
        let name = channel_name_for(chat_id)?;
        run_privileged::<ChannelId, LarkError>(&self.pool, async move |tx| {
            let existing: Option<(ChannelId,)> = sqlx::query_as(
                "SELECT channel_id FROM lark_channels \
                 WHERE org_id = $1 AND tenant_key = $2 AND chat_id = $3",
            )
            .bind(org_id)
            .bind(&tenant)
            .bind(&chat)
            .fetch_optional(&mut **tx)
            .await?;

            let channel_id = match existing {
                Some((c,)) => c,
                None => create_channel_and_mapping(tx, org_id, &tenant, &chat, &name, now).await?,
            };

            // Top up membership so the acting human can read + post in the
            // channel thread (channel-membership is the visibility gate).
            sqlx::query(
                "INSERT INTO channel_members (channel_id, user_id, org_id, added_at) \
                 VALUES ($1, $2, $3, $4) \
                 ON CONFLICT (channel_id, user_id) DO NOTHING",
            )
            .bind(channel_id)
            .bind(user_id)
            .bind(org_id)
            .bind(now)
            .execute(&mut **tx)
            .await?;
            Ok(channel_id)
        })
        .await
    }
}

/// Create the Patom channel and the `lark_channels` mapping.
///
/// Idempotent on the active-name unique index; returns the channel id. Only
/// reached when no mapping exists yet (the caller gated on that), so a
/// concurrent first-touch resolves the same channel for both callers: the
/// active-name unique index makes both compute the same `cid` from the
/// shared slug, and the mapping insert binds that same `cid` — no re-read
/// needed.
async fn create_channel_and_mapping(
    tx: &mut crate::auth::PrivilegedTx<'_>,
    org_id: OrgId,
    tenant: &str,
    chat: &str,
    name: &ChannelName,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<ChannelId, LarkError> {
    let new_id = ChannelId::new();
    sqlx::query(
        "INSERT INTO channels (id, org_id, name, created_by_user_id, created_at) \
         VALUES ($1, $2, $3, NULL, $4) \
         ON CONFLICT (org_id, name) WHERE archived_at IS NULL DO NOTHING",
    )
    .bind(new_id)
    .bind(org_id)
    .bind(name.as_str())
    .bind(now)
    .execute(&mut **tx)
    .await?;
    let (cid,): (ChannelId,) = sqlx::query_as(
        "SELECT id FROM channels WHERE org_id = $1 AND name = $2 AND archived_at IS NULL",
    )
    .bind(org_id)
    .bind(name.as_str())
    .fetch_one(&mut **tx)
    .await?;
    sqlx::query(
        "INSERT INTO lark_channels (org_id, tenant_key, chat_id, channel_id, created_at) \
         VALUES ($1, $2, $3, $4, $5) \
         ON CONFLICT (org_id, tenant_key, chat_id) DO NOTHING",
    )
    .bind(org_id)
    .bind(tenant)
    .bind(chat)
    .bind(cid)
    .bind(now)
    .execute(&mut **tx)
    .await?;
    Ok(cid)
}
