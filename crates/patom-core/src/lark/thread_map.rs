//! Lark-thread ↔ Patom-thread bridge — `lark_threads` table.
//!
//! One row per Lark-rooted conversation: a Lark `(tenant_key, chat_id,
//! lark_thread_id)` triple ↔ one Patom `thread_id`. Both lookups are
//! privileged (no `Caller` — live events arrive tenant-keyed, not
//! caller-authenticated):
//! - `lookup_by_thread` for the inbound path: given a Lark `(tenant_key,
//!   chat_id, lark_thread_id)` triple, find the bound Patom thread or `None`
//!   (the caller then starts a fresh thread).
//! - `lookup_by_patom_thread` for the outbound stream pump: given a Patom
//!   `thread_id`, find which Lark thread (if any) it is bound to.
//!
//! Writes happen via `bind`: the inbound bridge writes after it creates (or
//! resolves) the Patom thread for the first message in a Lark thread. The
//! insert is idempotent on the primary key, so the second message in a Lark
//! thread still resolves to the existing Patom thread.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use sqlx::PgPool;

use crate::auth::{OrgId, run_privileged};
use crate::clock::SharedClock;
use crate::threads::ThreadId;

use super::error::LarkError;
use super::types::{LarkAppId, LarkChatId, LarkThreadId, TenantKey};

/// Existing Lark-thread → Patom-thread mapping (inbound resolve).
#[derive(Debug, Clone)]
pub struct LarkThreadMapping {
    pub thread_id: ThreadId,
}

/// Reverse projection used by the stream pump: where a Patom thread's chunks
/// should land in Lark.
#[derive(Debug, Clone)]
pub struct LarkThreadBinding {
    pub app_id: LarkAppId,
    pub tenant_key: TenantKey,
    pub chat_id: LarkChatId,
    pub lark_thread_id: LarkThreadId,
}

#[async_trait]
pub trait LarkThreadStore: fmt::Debug + Send + Sync {
    /// Inbound resolve: given a Lark `(tenant_key, chat_id, lark_thread_id)`
    /// triple, return the bound Patom thread or `None` (no binding yet).
    async fn lookup_by_thread(
        &self,
        tenant_key: &TenantKey,
        chat_id: &LarkChatId,
        lark_thread_id: &LarkThreadId,
    ) -> Result<Option<LarkThreadMapping>, LarkError>;

    /// Reverse projection used by the outbound stream pump: given a
    /// `thread_id`, return the Lark `(app, tenant_key, chat, thread)` where the
    /// thread's chunks should land — or `None` if this thread has no Lark
    /// binding (it originated on the web, not in Lark).
    async fn lookup_by_patom_thread(
        &self,
        thread_id: ThreadId,
    ) -> Result<Option<LarkThreadBinding>, LarkError>;

    /// Insert a `(tenant_key, chat_id, lark_thread_id) → patom_thread_id` row.
    /// Idempotent on the primary key; an existing row is left alone so a later
    /// message in the same Lark thread still resolves to the existing Patom
    /// thread.
    async fn bind(
        &self,
        org_id: OrgId,
        app_id: &LarkAppId,
        tenant_key: &TenantKey,
        chat_id: &LarkChatId,
        lark_thread_id: &LarkThreadId,
        patom_thread_id: ThreadId,
    ) -> Result<(), LarkError>;
}

pub type SharedLarkThreadStore = Arc<dyn LarkThreadStore>;

/// Postgres-backed [`LarkThreadStore`]. All methods run privileged: live Lark
/// events are tenant-keyed, not caller-authenticated.
pub struct PgLarkThreadStore {
    pool: PgPool,
    clock: SharedClock,
}

impl PgLarkThreadStore {
    #[must_use]
    pub fn new(pool: PgPool, clock: SharedClock) -> Self {
        Self { pool, clock }
    }
}

impl fmt::Debug for PgLarkThreadStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgLarkThreadStore").finish_non_exhaustive()
    }
}

#[async_trait]
impl LarkThreadStore for PgLarkThreadStore {
    async fn lookup_by_thread(
        &self,
        tenant_key: &TenantKey,
        chat_id: &LarkChatId,
        lark_thread_id: &LarkThreadId,
    ) -> Result<Option<LarkThreadMapping>, LarkError> {
        type Row = (ThreadId,);
        let row: Option<Row> = run_privileged::<Option<Row>, LarkError>(&self.pool, async |tx| {
            Ok(sqlx::query_as(
                "SELECT patom_thread_id \
                 FROM lark_threads \
                 WHERE tenant_key = $1 AND chat_id = $2 AND lark_thread_id = $3",
            )
            .bind(tenant_key.as_str())
            .bind(chat_id.as_str())
            .bind(lark_thread_id.as_str())
            .fetch_optional(&mut **tx)
            .await?)
        })
        .await?;
        Ok(row.map(|(thread_id,)| LarkThreadMapping { thread_id }))
    }

    async fn lookup_by_patom_thread(
        &self,
        thread_id: ThreadId,
    ) -> Result<Option<LarkThreadBinding>, LarkError> {
        type Row = (String, String, String, String);
        let row: Option<Row> = run_privileged::<Option<Row>, LarkError>(&self.pool, async |tx| {
            Ok(sqlx::query_as(
                "SELECT app_id, tenant_key, chat_id, lark_thread_id \
                 FROM lark_threads WHERE patom_thread_id = $1",
            )
            .bind(thread_id)
            .fetch_optional(&mut **tx)
            .await?)
        })
        .await?;
        let Some((app_id_str, tenant_key_str, chat_id_str, lark_thread_id_str)) = row else {
            return Ok(None);
        };
        Ok(Some(LarkThreadBinding {
            app_id: LarkAppId::try_from(app_id_str)?,
            tenant_key: TenantKey::try_from(tenant_key_str)?,
            chat_id: LarkChatId::try_from(chat_id_str)?,
            lark_thread_id: LarkThreadId::try_from(lark_thread_id_str)?,
        }))
    }

    async fn bind(
        &self,
        org_id: OrgId,
        app_id: &LarkAppId,
        tenant_key: &TenantKey,
        chat_id: &LarkChatId,
        lark_thread_id: &LarkThreadId,
        patom_thread_id: ThreadId,
    ) -> Result<(), LarkError> {
        let now = self.clock.now_utc();
        run_privileged::<(), LarkError>(&self.pool, async |tx| {
            sqlx::query(
                "INSERT INTO lark_threads \
                   (org_id, app_id, tenant_key, chat_id, lark_thread_id, patom_thread_id, created_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7) \
                 ON CONFLICT (tenant_key, chat_id, lark_thread_id) DO NOTHING",
            )
            .bind(org_id)
            .bind(app_id.as_str())
            .bind(tenant_key.as_str())
            .bind(chat_id.as_str())
            .bind(lark_thread_id.as_str())
            .bind(patom_thread_id)
            .bind(now)
            .execute(&mut **tx)
            .await?;
            Ok(())
        })
        .await
    }
}

/// In-memory [`LarkThreadStore`] for tests. Records writes and answers reads
/// without touching Postgres. Not `#[cfg(test)]` so integration tests in
/// `tests/` can reach it.
#[derive(Debug, Default)]
pub struct FakeLarkThreadStore {
    inner: std::sync::Mutex<Vec<(ThreadKey, OrgId, LarkAppId, ThreadId)>>,
}

/// The composite key of a `lark_threads` row.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ThreadKey {
    tenant_key: TenantKey,
    chat_id: LarkChatId,
    lark_thread_id: LarkThreadId,
}

impl FakeLarkThreadStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of bindings currently recorded.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .expect("invariant: fake-thread-store mutex poisoned")
            .len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[async_trait]
impl LarkThreadStore for FakeLarkThreadStore {
    async fn lookup_by_thread(
        &self,
        tenant_key: &TenantKey,
        chat_id: &LarkChatId,
        lark_thread_id: &LarkThreadId,
    ) -> Result<Option<LarkThreadMapping>, LarkError> {
        let key = ThreadKey {
            tenant_key: tenant_key.clone(),
            chat_id: chat_id.clone(),
            lark_thread_id: lark_thread_id.clone(),
        };
        let guard = self
            .inner
            .lock()
            .expect("invariant: fake-thread-store mutex poisoned");
        let hit = guard
            .iter()
            .find(|(k, _, _, _)| *k == key)
            .map(|(_, _, _, thread_id)| LarkThreadMapping {
                thread_id: *thread_id,
            });
        Ok(hit)
    }

    async fn lookup_by_patom_thread(
        &self,
        thread_id: ThreadId,
    ) -> Result<Option<LarkThreadBinding>, LarkError> {
        let guard = self
            .inner
            .lock()
            .expect("invariant: fake-thread-store mutex poisoned");
        let hit =
            guard
                .iter()
                .find(|(_, _, _, tid)| *tid == thread_id)
                .map(|(key, _, app_id, _)| LarkThreadBinding {
                    app_id: app_id.clone(),
                    tenant_key: key.tenant_key.clone(),
                    chat_id: key.chat_id.clone(),
                    lark_thread_id: key.lark_thread_id.clone(),
                });
        Ok(hit)
    }

    async fn bind(
        &self,
        org_id: OrgId,
        app_id: &LarkAppId,
        tenant_key: &TenantKey,
        chat_id: &LarkChatId,
        lark_thread_id: &LarkThreadId,
        patom_thread_id: ThreadId,
    ) -> Result<(), LarkError> {
        let key = ThreadKey {
            tenant_key: tenant_key.clone(),
            chat_id: chat_id.clone(),
            lark_thread_id: lark_thread_id.clone(),
        };
        let mut guard = self
            .inner
            .lock()
            .expect("invariant: fake-thread-store mutex poisoned");
        // PK conflict on (tenant_key, chat_id, lark_thread_id) — leave the
        // first writer (mirrors `ON CONFLICT DO NOTHING`).
        if guard.iter().any(|(k, _, _, _)| *k == key) {
            return Ok(());
        }
        // UNIQUE(patom_thread_id) — one Patom thread binds to at most one Lark
        // thread.
        if guard.iter().any(|(_, _, _, tid)| *tid == patom_thread_id) {
            return Err(LarkError::Internal(format!(
                "fake: duplicate binding for patom thread {patom_thread_id}"
            )));
        }
        guard.push((key, org_id, app_id.clone(), patom_thread_id));
        Ok(())
    }
}
