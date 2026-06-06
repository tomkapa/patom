//! Persistence layer for [`TodoList`].
//!
//! One row per session. Writes go through `run_as_user` so RLS WITH
//! CHECK fires against the human who initiated the DAG; reads from the
//! worker's pre-turn assembly use `run_privileged` (same justification
//! as [`crate::memory::PgMemoryStore::apply`] — the worker has already
//! claimed the session and is rendering its own context, no foreign
//! principal to defend against on that path).

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use sqlx::types::Json;
use thiserror::Error;

use crate::auth::{OrgId, UserId, run_as_user, run_privileged};
use crate::clock::SharedClock;
use crate::runtime::PromptRequestId;
use crate::session::SessionId;
use crate::types::ParseError;

use super::types::{TodoItem, TodoList};

#[derive(Debug, Error)]
pub enum TodoStoreError {
    #[error("invariant violation: {0}")]
    Invariant(#[from] ParseError),

    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
}

#[async_trait]
pub trait SessionTodoStore: Send + Sync + fmt::Debug {
    /// Atomically replace the session's todo list. Returns the freshly
    /// stored list (echoed back to the model so it sees its own state).
    async fn replace(
        &self,
        acting_user_id: UserId,
        session_id: SessionId,
        org_id: OrgId,
        updated_in_request_id: PromptRequestId,
        list: TodoList,
    ) -> Result<TodoList, TodoStoreError>;

    /// Read the current list for the worker's pre-turn context
    /// assembly. Bypasses RLS (worker-internal read path — the worker
    /// has already claimed `session_id`, which is the PK, so a foreign
    /// principal cannot reach this code path with a foreign id);
    /// returns the empty list when no row exists yet.
    async fn get(&self, session_id: SessionId) -> Result<TodoList, TodoStoreError>;
}

pub type SharedSessionTodoStore = Arc<dyn SessionTodoStore>;

pub struct PgSessionTodoStore {
    pool: PgPool,
    clock: SharedClock,
}

impl PgSessionTodoStore {
    #[must_use]
    pub fn new(pool: PgPool, clock: SharedClock) -> Self {
        Self { pool, clock }
    }

    fn now(&self) -> DateTime<Utc> {
        self.clock.now_utc()
    }
}

impl fmt::Debug for PgSessionTodoStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgSessionTodoStore").finish_non_exhaustive()
    }
}

#[async_trait]
impl SessionTodoStore for PgSessionTodoStore {
    async fn replace(
        &self,
        acting_user_id: UserId,
        session_id: SessionId,
        org_id: OrgId,
        updated_in_request_id: PromptRequestId,
        list: TodoList,
    ) -> Result<TodoList, TodoStoreError> {
        let now = self.now();
        let item_count = i16::try_from(list.len()).expect("invariant: list ≤ MAX_TODOS_PER_LIST");
        let items_for_bind: &[TodoItem] = list.as_slice();
        run_as_user(&self.pool, acting_user_id, async |tx| {
            sqlx::query(
                "INSERT INTO session_todos \
                   (session_id, org_id, items, item_count, updated_at, updated_in_request_id) \
                 VALUES ($1, $2, $3, $4, $5, $6) \
                 ON CONFLICT (session_id) DO UPDATE SET \
                   items = EXCLUDED.items, \
                   item_count = EXCLUDED.item_count, \
                   updated_at = EXCLUDED.updated_at, \
                   updated_in_request_id = EXCLUDED.updated_in_request_id",
            )
            .bind(session_id)
            .bind(org_id)
            .bind(Json(items_for_bind))
            .bind(item_count)
            .bind(now)
            .bind(updated_in_request_id)
            .execute(&mut **tx.tx_mut())
            .await?;
            Ok::<_, TodoStoreError>(())
        })
        .await?;
        Ok(list)
    }

    async fn get(&self, session_id: SessionId) -> Result<TodoList, TodoStoreError> {
        let row: Option<(Json<Vec<TodoItem>>,)> = run_privileged(&self.pool, async |tx| {
            let row = sqlx::query_as::<_, (Json<Vec<TodoItem>>,)>(
                "SELECT items FROM session_todos WHERE session_id = $1",
            )
            .bind(session_id)
            .fetch_optional(&mut **tx.tx_mut())
            .await?;
            Ok::<_, TodoStoreError>(row)
        })
        .await?;

        match row {
            None => Ok(TodoList::empty()),
            Some((Json(items),)) => Ok(TodoList::try_from(items)?),
        }
    }
}
