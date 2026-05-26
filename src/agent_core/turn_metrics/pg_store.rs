//! Postgres-backed [`TurnMetricsStore`].

use std::fmt;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::auth::run_privileged;
use crate::clock::SharedClock;

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
            relay.session.id = %row.session_id,
            relay.agent.id = %row.agent_id,
            relay.request.id = %row.request_id,
            relay.request.kind = row.kind.as_str(),
            relay.provider = row.provider.as_str(),
            relay.model = row.model.as_str(),
            // §1: Model is the catalog handle; `as_str()` returns the &'static name.
            relay.tokens.input = row.input_tokens.get(),
            relay.tokens.output = row.output_tokens.get(),
            relay.duration_ms = row.duration_ms.get(),
        ),
    )]
    async fn record(&self, row: TurnMetricsRow) -> Result<(), TurnRecorderError> {
        let created_at = self.now();

        run_privileged::<(), TurnRecorderError>(&self.pool, async |tx| {
            sqlx::query(
                "INSERT INTO turn_metrics \
                     (request_id, org_id, session_id, agent_id, prompt_version_id, \
                      kind, model, provider, \
                      input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens, \
                      duration_ms, stop_reason, history_count, started_at, created_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17)",
            )
            .bind(row.request_id)
            .bind(row.org_id)
            .bind(row.session_id)
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
            .bind(row.history_count.get())
            .bind(row.started_at)
            .bind(created_at)
            .execute(&mut **tx)
            .await?;
            Ok(())
        })
        .await
    }
}
