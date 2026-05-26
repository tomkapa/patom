//! Postgres-backed [`PromptVersionStore`].

use std::fmt;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::agents::{AgentId, AgentSystemPrompt};
use crate::auth::{OrgId, UserId, run_privileged};
use crate::clock::SharedClock;
use crate::provider::Model;

use super::error::PromptVersionError;
use super::store::PromptVersionStore;
use super::types::{NewPromptVersion, PromptVersionId, PromptVersionNumber, PromptVersionRow};

/// Pg-backed [`PromptVersionStore`]. Carries the pool + clock by value so the
/// handle is cheap to clone via `Arc`. Mirrors the shape of
/// [`crate::tools::PgToolCallStore`].
pub struct PgPromptVersionStore {
    pool: PgPool,
    clock: SharedClock,
}

impl PgPromptVersionStore {
    #[must_use]
    pub fn new(pool: PgPool, clock: SharedClock) -> Self {
        Self { pool, clock }
    }

    fn now(&self) -> DateTime<Utc> {
        self.clock.now_utc()
    }
}

impl fmt::Debug for PgPromptVersionStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgPromptVersionStore")
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl PromptVersionStore for PgPromptVersionStore {
    #[tracing::instrument(
        skip_all,
        name = "agent_prompt_versions.insert_bump",
        fields(
            relay.agent.id = %payload.agent_id,
            relay.org.id = %payload.org_id,
            relay.prompt_version = tracing::field::Empty,
        ),
    )]
    async fn insert_bump(
        &self,
        payload: NewPromptVersion,
    ) -> Result<PromptVersionRow, PromptVersionError> {
        let now = self.now();
        let id = PromptVersionId::new();
        let agent_id = payload.agent_id;

        run_privileged::<PromptVersionRow, PromptVersionError>(&self.pool, async |tx| {
            // Compute next version under the row lock — UNIQUE (agent_id,
            // version) is the load-bearing defence against concurrent
            // bumpers, but reading the max-as-of-now keeps the common case
            // monotonic and stable. CLAUDE.md §10: bound parameters only.
            let row: Option<(i32,)> = sqlx::query_as(
                "SELECT COALESCE(MAX(version), 0) \
                 FROM agent_prompt_versions \
                 WHERE agent_id = $1",
            )
            .bind(agent_id)
            .fetch_optional(&mut **tx)
            .await?;
            let prev = row.map_or(0, |(v,)| v);
            let next_raw = prev.saturating_add(1);
            let version = PromptVersionNumber::try_from(next_raw)?;
            tracing::Span::current().record("relay.prompt_version", version.get());

            sqlx::query(
                "INSERT INTO agent_prompt_versions \
                     (id, agent_id, org_id, version, system_prompt, model, edited_by, created_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
            )
            .bind(id)
            .bind(agent_id)
            .bind(payload.org_id)
            .bind(version.get())
            .bind(payload.system_prompt.as_str())
            .bind(payload.model)
            .bind(payload.edited_by)
            .bind(now)
            .execute(&mut **tx)
            .await?;

            Ok(PromptVersionRow {
                id,
                agent_id,
                org_id: payload.org_id,
                version,
                system_prompt: payload.system_prompt,
                model: payload.model,
                edited_by: payload.edited_by,
                created_at: now,
            })
        })
        .await
    }

    #[tracing::instrument(
        skip_all,
        name = "agent_prompt_versions.current_for_agent",
        fields(relay.agent.id = %agent_id),
    )]
    async fn current_for_agent(
        &self,
        agent_id: AgentId,
    ) -> Result<PromptVersionRow, PromptVersionError> {
        let row = run_privileged::<Option<PromptVersionPgRow>, PromptVersionError>(
            &self.pool,
            async |tx| {
                Ok(sqlx::query_as::<_, PromptVersionPgRow>(
                    "SELECT id, agent_id, org_id, version, system_prompt, model, edited_by, created_at \
                     FROM agent_prompt_versions \
                     WHERE agent_id = $1 \
                     ORDER BY version DESC \
                     LIMIT 1",
                )
                .bind(agent_id)
                .fetch_optional(&mut **tx)
                .await?)
            },
        )
        .await?;
        let row = row.ok_or(PromptVersionError::NoVersionsForAgent(agent_id))?;
        row.try_into()
    }
}

#[derive(sqlx::FromRow)]
struct PromptVersionPgRow {
    id: PromptVersionId,
    agent_id: AgentId,
    org_id: OrgId,
    version: i32,
    system_prompt: String,
    model: Option<Model>,
    edited_by: Option<UserId>,
    created_at: DateTime<Utc>,
}

impl TryFrom<PromptVersionPgRow> for PromptVersionRow {
    type Error = PromptVersionError;

    fn try_from(row: PromptVersionPgRow) -> Result<Self, Self::Error> {
        Ok(Self {
            id: row.id,
            agent_id: row.agent_id,
            org_id: row.org_id,
            version: PromptVersionNumber::try_from(row.version)?,
            system_prompt: AgentSystemPrompt::try_from(row.system_prompt)?,
            model: row.model,
            edited_by: row.edited_by,
            created_at: row.created_at,
        })
    }
}
