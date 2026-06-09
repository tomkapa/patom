//! Postgres-backed [`BackgroundStore`].
//!
//! Turns live in `background_turns`; their private message log in
//! `background_turn_messages (turn_id, seq, …)`, PK `(turn_id, seq)`. A
//! background turn is single-worker (the `claim_leases` fence serialises it), so
//! the next `seq` is computed as `MAX(seq)+1` without a dedicated counter table.
//! Wall clock comes from the injected [`SharedClock`] (CLAUDE.md §11).

use std::fmt;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::agents::AgentId;
use crate::auth::{Caller, run_as_user};
use crate::clock::SharedClock;
use crate::provider::ChatMessage;

use super::error::BackgroundError;
use super::traits::{BackgroundStore, BackgroundTurnId, NewBackgroundMessage};

/// Postgres-backed [`BackgroundStore`].
pub struct PgBackgroundStore {
    pool: PgPool,
    clock: SharedClock,
}

impl PgBackgroundStore {
    #[must_use]
    pub fn new(pool: PgPool, clock: SharedClock) -> Self {
        Self { pool, clock }
    }

    fn now(&self) -> DateTime<Utc> {
        self.clock.now_utc()
    }
}

impl fmt::Debug for PgBackgroundStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgBackgroundStore").finish_non_exhaustive()
    }
}

#[async_trait]
impl BackgroundStore for PgBackgroundStore {
    #[tracing::instrument(skip_all, name = "background.create_turn", fields(patom.agent.id = %agent, patom.background.turn.id = tracing::field::Empty))]
    async fn create_turn(
        &self,
        caller: &Caller,
        agent: AgentId,
    ) -> Result<BackgroundTurnId, BackgroundError> {
        let now = self.now();
        let id = BackgroundTurnId::new();
        run_as_user(&self.pool, caller.user_id, async |tx| {
            sqlx::query(
                "INSERT INTO background_turns (id, agent_id, org_id, created_at) \
                 VALUES ($1, $2, $3, $4)",
            )
            .bind(id)
            .bind(agent)
            .bind(caller.org_id)
            .bind(now)
            .execute(&mut **tx)
            .await?;
            Ok::<(), BackgroundError>(())
        })
        .await?;
        tracing::Span::current().record("patom.background.turn.id", tracing::field::display(id));
        Ok(id)
    }

    #[tracing::instrument(skip_all, name = "background.append", fields(patom.background.turn.id = %turn))]
    async fn append(
        &self,
        caller: &Caller,
        turn: BackgroundTurnId,
        message: NewBackgroundMessage,
    ) -> Result<i64, BackgroundError> {
        let now = self.now();
        let body = serde_json::to_value(&message.body)
            .map_err(|e| BackgroundError::Backend(format!("serialize message: {e}")))?;
        // Next seq = MAX+1 for this turn, gated on the turn existing (`bt`). A
        // background turn is single-worker (lease-serialised), so no race on MAX.
        let row: Option<(i64,)> = run_as_user::<Option<(i64,)>, BackgroundError>(
            &self.pool,
            caller.user_id,
            async |tx| {
                Ok(sqlx::query_as(
                    "INSERT INTO background_turn_messages \
                         (turn_id, seq, sender_colleague_id, body, request_id, org_id, created_at) \
                     SELECT $1, \
                            COALESCE((SELECT MAX(m.seq) FROM background_turn_messages m \
                                      WHERE m.turn_id = $1), 0) + 1, \
                            $2, $3, $4, bt.org_id, $5 \
                     FROM background_turns bt WHERE bt.id = $1 \
                     RETURNING seq",
                )
                .bind(turn)
                .bind(message.sender)
                .bind(body)
                .bind(message.request_id)
                .bind(now)
                .fetch_optional(&mut **tx)
                .await?)
            },
        )
        .await?;
        row.map(|(seq,)| seq).ok_or(BackgroundError::NotFound(turn))
    }

    #[tracing::instrument(skip_all, name = "background.context", fields(patom.background.turn.id = %turn, patom.history.count = tracing::field::Empty))]
    async fn context(
        &self,
        caller: &Caller,
        turn: BackgroundTurnId,
    ) -> Result<Vec<ChatMessage>, BackgroundError> {
        let rows: Vec<(serde_json::Value,)> = run_as_user::<
            Vec<(serde_json::Value,)>,
            BackgroundError,
        >(&self.pool, caller.user_id, async |tx| {
            Ok(sqlx::query_as(
                "SELECT body FROM background_turn_messages \
                         WHERE turn_id = $1 ORDER BY seq ASC",
            )
            .bind(turn)
            .fetch_all(&mut **tx)
            .await?)
        })
        .await?;
        tracing::Span::current().record("patom.history.count", rows.len());
        let mut out = Vec::with_capacity(rows.len());
        for (body,) in rows {
            out.push(
                serde_json::from_value(body)
                    .map_err(|e| BackgroundError::Backend(format!("deserialize message: {e}")))?,
            );
        }
        Ok(out)
    }
}
