//! Lark agent↔human DM binding — the `lark_dms` table (issue #178).
//!
//! A never-seen Lark DM has no chat_id; the bot sends by the recipient's
//! `open_id` (`receive_id_type=open_id`) and Lark routes it to the p2p chat.
//! Every outbound turn can re-send by the same open_id, so the binding the
//! router needs is the recipient open_id — known up front, no post-send
//! capture. That p2p chat isn't threaded, so it can't live in `lark_threads`.
//! The router looks up by `patom_thread_id` before binding (idempotency — a
//! re-fire reuses the same recipient).
//!
//! Privileged throughout: the router holds no `Caller`; the org comes from the
//! resolved app registration.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use sqlx::PgPool;

use crate::auth::{OrgId, run_privileged};
use crate::clock::SharedClock;
use crate::threads::ThreadId;

use super::error::LarkError;
use super::types::{LarkAppId, LarkOpenId};

/// Reverse projection used by the outbound router: which bot + recipient open_id
/// a Patom DM thread is bound to.
#[derive(Debug, Clone)]
pub struct LarkDmBinding {
    pub app_id: LarkAppId,
    pub open_id: LarkOpenId,
}

#[async_trait]
pub trait LarkDmStore: fmt::Debug + Send + Sync {
    /// The p2p chat a Patom thread is bound to, or `None` if it has no Lark DM
    /// binding yet.
    async fn lookup_by_patom_thread(
        &self,
        thread_id: ThreadId,
    ) -> Result<Option<LarkDmBinding>, LarkError>;

    /// Bind a Patom DM thread to a bot + recipient open_id. Idempotent on the
    /// `patom_thread_id` unique index.
    async fn bind(
        &self,
        org_id: OrgId,
        app_id: &LarkAppId,
        patom_thread_id: ThreadId,
        open_id: &LarkOpenId,
    ) -> Result<(), LarkError>;
}

pub type SharedLarkDmStore = Arc<dyn LarkDmStore>;

pub struct PgLarkDmStore {
    pool: PgPool,
    clock: SharedClock,
}

impl PgLarkDmStore {
    #[must_use]
    pub fn new(pool: PgPool, clock: SharedClock) -> Self {
        Self { pool, clock }
    }
}

impl fmt::Debug for PgLarkDmStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgLarkDmStore").finish_non_exhaustive()
    }
}

#[async_trait]
impl LarkDmStore for PgLarkDmStore {
    async fn lookup_by_patom_thread(
        &self,
        thread_id: ThreadId,
    ) -> Result<Option<LarkDmBinding>, LarkError> {
        type Row = (String, String);
        let row: Option<Row> = run_privileged::<Option<Row>, LarkError>(&self.pool, async |tx| {
            Ok(
                sqlx::query_as("SELECT app_id, open_id FROM lark_dms WHERE patom_thread_id = $1")
                    .bind(thread_id)
                    .fetch_optional(&mut **tx)
                    .await?,
            )
        })
        .await?;
        let Some((app_id, open_id)) = row else {
            return Ok(None);
        };
        Ok(Some(LarkDmBinding {
            app_id: LarkAppId::try_from(app_id)?,
            open_id: LarkOpenId::try_from(open_id)?,
        }))
    }

    async fn bind(
        &self,
        org_id: OrgId,
        app_id: &LarkAppId,
        patom_thread_id: ThreadId,
        open_id: &LarkOpenId,
    ) -> Result<(), LarkError> {
        let now = self.clock.now_utc();
        run_privileged::<(), LarkError>(&self.pool, async |tx| {
            sqlx::query(
                "INSERT INTO lark_dms \
                   (org_id, app_id, patom_thread_id, open_id, created_at) \
                 VALUES ($1, $2, $3, $4, $5) \
                 ON CONFLICT (patom_thread_id) DO NOTHING",
            )
            .bind(org_id)
            .bind(app_id.as_str())
            .bind(patom_thread_id)
            .bind(open_id.as_str())
            .bind(now)
            .execute(&mut **tx)
            .await?;
            Ok(())
        })
        .await
    }
}
