//! Postgres-backed [`TurnMetricsStore`].

use std::fmt;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::auth::run_privileged;
use crate::clock::SharedClock;
use crate::threads::AgentThreadId;

use super::error::TurnRecorderError;
use super::store::TurnMetricsStore;
use super::types::{InputTokens, TurnMetricsRow};

/// Pg-backed [`TurnMetricsStore`].
pub struct PgTurnMetricsStore {
    pool: PgPool,
    clock: SharedClock,
}

impl PgTurnMetricsStore {
    #[must_use]
    pub fn new(pool: PgPool, clock: SharedClock) -> Self {
        Self { pool, clock }
    }

    fn now(&self) -> DateTime<Utc> {
        self.clock.now_utc()
    }
}

impl fmt::Debug for PgTurnMetricsStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgTurnMetricsStore").finish_non_exhaustive()
    }
}

#[async_trait]
impl TurnMetricsStore for PgTurnMetricsStore {
    #[tracing::instrument(
        skip_all,
        name = "turn_metrics.record",
        fields(
            patom.state.id = %row.state_id,
            patom.agent.id = %row.agent_id,
            patom.request.id = %row.request_id,
            patom.request.kind = row.kind.as_str(),
            patom.provider = row.provider.as_str(),
            patom.model = row.model.as_str(),
            // §1: Model is the catalog handle; `as_str()` returns the &'static name.
            patom.tokens.input = row.input_tokens.get(),
            patom.tokens.output = row.output_tokens.get(),
            patom.duration_ms = row.duration_ms.get(),
        ),
    )]
    async fn record(&self, row: TurnMetricsRow) -> Result<(), TurnRecorderError> {
        let created_at = self.now();

        run_privileged::<(), TurnRecorderError>(&self.pool, async |tx| {
            sqlx::query(
                "INSERT INTO turn_metrics \
                     (id, request_id, org_id, state_id, agent_id, prompt_version_id, \
                      kind, model, provider, \
                      input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens, \
                      duration_ms, stop_reason, started_at, created_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17)",
            )
            .bind(row.id)
            .bind(row.request_id)
            .bind(row.org_id)
            .bind(row.state_id)
            .bind(row.agent_id)
            .bind(row.prompt_version_id)
            .bind(row.kind.as_str())
            .bind(row.model)
            .bind(row.provider.as_str())
            .bind(row.input_tokens.get())
            .bind(row.output_tokens.get())
            .bind(row.cache_creation_tokens.map(InputTokens::get))
            .bind(row.cache_read_tokens.map(InputTokens::get))
            .bind(row.duration_ms.get())
            .bind(row.stop_reason.as_str())
            .bind(row.started_at)
            .bind(created_at)
            .execute(&mut **tx)
            .await?;
            Ok(())
        })
        .await
    }

    async fn latest_full_prompt_input_tokens(
        &self,
        state_id: AgentThreadId,
    ) -> Result<Option<u32>, TurnRecorderError> {
        // Read keyed by `(state_id, started_at DESC)` — the index installed in
        // migration 63 — and excludes `compaction` fold sub-calls so only the
        // last assembled full-prompt turn is returned. Privileged like the
        // insert: this is an internal trigger read, not a tenant query path.
        let tokens: Option<i32> =
            run_privileged::<Option<i32>, TurnRecorderError>(&self.pool, async |tx| {
                let row: Option<i32> = sqlx::query_scalar(
                    "SELECT input_tokens FROM turn_metrics \
                         WHERE state_id = $1 AND kind <> 'compaction' \
                         ORDER BY started_at DESC \
                         LIMIT 1",
                )
                .bind(state_id)
                .fetch_optional(&mut **tx)
                .await?;
                Ok(row)
            })
            .await?;
        // The column CHECKs `>= 0`, so it fits u32; saturate defensively rather
        // than narrowing with `as` (CLAUDE.md §7).
        Ok(tokens.map(|t| u32::try_from(t).unwrap_or(0)))
    }
}
