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
            patom.session.id = %row.session_id,
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
            // §3: compile-time-checked query. The bind expressions keep the same
            // newtype Encode/Type impls the runtime form relied on; the macro adds
            // schema verification against the committed `.sqlx` cache (SQLX_OFFLINE).
            sqlx::query!(
                "INSERT INTO turn_metrics \
                     (request_id, org_id, session_id, agent_id, prompt_version_id, \
                      kind, model, provider, \
                      input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens, \
                      duration_ms, stop_reason, started_at, created_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)",
                // query! infers the bare Postgres type per column, so newtypes are
                // passed via their primitive accessor (as_uuid/as_str). The encode
                // is byte-identical to the prior .bind(newtype) form.
                row.request_id.as_uuid(),
                row.org_id.as_uuid(),
                row.session_id.as_uuid(),
                row.agent_id.as_uuid(),
                row.prompt_version_id.as_uuid(),
                row.kind.as_str(),
                row.model.as_str(),
                row.provider.as_str(),
                row.input_tokens.get(),
                row.output_tokens.get(),
                row.cache_creation_tokens.map(InputTokens::get),
                row.cache_read_tokens.map(InputTokens::get),
                row.duration_ms.get(),
                row.stop_reason.as_str(),
                row.started_at,
                created_at,
            )
            .execute(&mut **tx)
            .await?;
            Ok(())
        })
        .await
    }
}
