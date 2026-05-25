//! Postgres-backed [`AgentStore`].
//!
//! Wall-clock values come from the injected [`SharedClock`] — never `NOW()` in app
//! SQL — so a `TestClock`-driven test sees stable timestamps (CLAUDE.md §11). Ids
//! cross the SQL boundary via the macro-generated `sqlx::Type` impl on
//! [`AgentId`].
//!
//! `description` is embedded synchronously before opening a transaction on every
//! create / description-update path (doc/agent_discovery_plan.md §5.3) — same
//! pattern the memory store uses (CLAUDE.md §5). The embedding backs
//! `search_agents`; a row whose description embed call fails never lands, so
//! discovery cannot silently skip an agent.

use std::fmt;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use sqlx::types::Json;

use crate::auth::{OrgId, UserId, run_as_user, run_privileged};
use crate::clock::SharedClock;
use crate::pg_vector;
use crate::provider::{SharedEmbeddingProvider, embed_one};

use super::error::AgentStoreError;
use super::store::{AgentStore, AgentUpdate, NewAgent};
use super::types::{
    AgentCard, AgentDescription, AgentId, AgentName, AgentRecord, AgentSystemPrompt,
    AllowedMcpTools, DefaultAgentSeed,
};

/// Hard cap on `list_names` SQL — fetched batches are bounded per
/// CLAUDE.md §5. Sized at 4× the inline-render cap; the renderer already
/// degrades above [`MAX_AGENT_NAMES_INLINE`], so this LIMIT just keeps
/// the wire transfer bounded if the registry grows past that.
const LIST_NAMES_MAX_ROWS: i64 = 512;

/// Transaction-scoped advisory-lock key used by [`PgAgentStore::seed_default`] to
/// serialise its "check default exists, insert if not" critical section across
/// concurrent app starts. Released automatically on commit/rollback. Literal is
/// `0x6167656E745F6473` (= ASCII "agent_ds") — chosen for readability and to
/// avoid colliding with the MCP create lock.
const AGENT_DEFAULT_SEED_LOCK_KEY: i64 = 0x6167_656E_745F_6473;

/// Single source of truth for the `agents` column list. Every SELECT that
/// hydrates an [`AgentRow`] must use this — adding a column then becomes a
/// one-line edit here plus the matching `AgentRow` field.
const AGENT_COLS: &str = "id, org_id, name, system_prompt, description, is_default, \
    allowed_mcp_tools, model, created_at, updated_at";

/// Postgres-backed [`AgentStore`]. Holds a cheap clone of a [`PgPool`], a
/// [`SharedClock`], and a [`SharedEmbeddingProvider`] for `description`
/// embedding; safe to share across the runtime.
pub struct PgAgentStore {
    pool: PgPool,
    clock: SharedClock,
    embeddings: SharedEmbeddingProvider,
}

impl PgAgentStore {
    #[must_use]
    pub fn new(pool: PgPool, clock: SharedClock, embeddings: SharedEmbeddingProvider) -> Self {
        Self {
            pool,
            clock,
            embeddings,
        }
    }

    fn now(&self) -> DateTime<Utc> {
        self.clock.now_utc()
    }

    /// Embed `description` synchronously. Errors propagate so the
    /// mutation aborts before the row lands; no `<agents>` entry is
    /// discoverable via `search_agents` without a vector to match.
    async fn embed(&self, description: &str) -> Result<Vec<f32>, AgentStoreError> {
        embed_one(self.embeddings.as_ref(), description)
            .await
            .map_err(|e| AgentStoreError::Backend(format!("description embed: {e}")))
    }

    /// See [`AgentStore::seed_default`]. Kept as an inherent method so
    /// tests that hold an `Arc<PgAgentStore>` can call it directly
    /// without coercing through the trait object.
    pub async fn seed_default(
        &self,
        org_id: OrgId,
        seed: DefaultAgentSeed,
    ) -> Result<AgentId, AgentStoreError> {
        // Embed before opening the transaction so a slow embedding call
        // does not hold the advisory lock. On embed failure the seeder
        // aborts without inserting; the next process start will retry.
        let embedding = self.embed(seed.description.as_str()).await?;
        let embedding_literal = pg_vector::encode(&embedding);

        let now = self.now();
        // Privileged tx: the seeder runs from the composition root (or
        // the OAuth callback) without a `Principal` in hand. RLS is
        // bypassed; tenant scoping is provided by the explicit `org_id`
        // bound on every statement.
        run_privileged(&self.pool, async |tx| {
            sqlx::query("SELECT pg_advisory_xact_lock($1)")
                .bind(AGENT_DEFAULT_SEED_LOCK_KEY)
                .execute(&mut **tx)
                .await?;

            let existing: Option<(AgentId,)> =
                sqlx::query_as("SELECT id FROM agents WHERE is_default = TRUE AND org_id = $1")
                    .bind(org_id)
                    .fetch_optional(&mut **tx)
                    .await?;
            if let Some((id,)) = existing {
                return Ok(id);
            }

            let id = AgentId::new();
            // `allowed_mcp_tools` is intentionally left to the column's
            // SQL default (`'{}'::jsonb`): a freshly seeded agent has no
            // MCP access. An operator opts it in via PUT after startup.
            sqlx::query(
                "INSERT INTO agents \
                     (id, org_id, name, system_prompt, description, description_embedding, \
                      is_default, created_at, updated_at) \
                 VALUES ($1, $2, $3, $4, $5, $6::vector, TRUE, $7, $7)",
            )
            .bind(id)
            .bind(org_id)
            .bind(seed.name.as_str())
            .bind(seed.system_prompt.as_str())
            .bind(seed.description.as_str())
            .bind(embedding_literal)
            .bind(now)
            .execute(&mut **tx)
            .await?;

            Ok(id)
        })
        .await
    }
}

impl fmt::Debug for PgAgentStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgAgentStore").finish_non_exhaustive()
    }
}

#[async_trait]
impl AgentStore for PgAgentStore {
    async fn seed_default(
        &self,
        org_id: OrgId,
        seed: DefaultAgentSeed,
    ) -> Result<AgentId, AgentStoreError> {
        Self::seed_default(self, org_id, seed).await
    }

    async fn create(&self, payload: NewAgent) -> Result<AgentRecord, AgentStoreError> {
        let embedding = self.embed(payload.description.as_str()).await?;
        run_privileged(&self.pool, async |tx| {
            create_in_tx(self, tx.tx_mut(), &embedding, payload).await
        })
        .await
    }

    async fn create_for_user(
        &self,
        acting_user_id: UserId,
        payload: NewAgent,
    ) -> Result<AgentRecord, AgentStoreError> {
        let embedding = self.embed(payload.description.as_str()).await?;
        run_as_user(&self.pool, acting_user_id, async |tx| {
            create_in_tx(self, tx.tx_mut(), &embedding, payload).await
        })
        .await
    }

    async fn list(&self) -> Result<Vec<AgentRecord>, AgentStoreError> {
        let sql = format!("SELECT {AGENT_COLS} FROM agents ORDER BY created_at ASC");
        let rows = run_privileged::<Vec<AgentRow>, AgentStoreError>(&self.pool, async |tx| {
            Ok(sqlx::query_as::<_, AgentRow>(&sql)
                .fetch_all(&mut **tx)
                .await?)
        })
        .await?;
        rows.into_iter().map(AgentRecord::try_from).collect()
    }

    async fn read(&self, id: AgentId) -> Result<AgentRecord, AgentStoreError> {
        let sql = format!("SELECT {AGENT_COLS} FROM agents WHERE id = $1");
        let row = run_privileged::<Option<AgentRow>, AgentStoreError>(&self.pool, async |tx| {
            Ok(sqlx::query_as::<_, AgentRow>(&sql)
                .bind(id)
                .fetch_optional(&mut **tx)
                .await?)
        })
        .await?;
        let row = row.ok_or(AgentStoreError::NotFound(id))?;
        row.try_into()
    }

    async fn update(
        &self,
        id: AgentId,
        payload: AgentUpdate,
    ) -> Result<AgentRecord, AgentStoreError> {
        // Embed the new description before opening the transaction so we
        // don't hold row locks across a network call. Empty payload.description
        // keeps the existing embedding.
        let new_embedding = match payload.description.as_ref() {
            Some(d) => Some(self.embed(d.as_str()).await?),
            None => None,
        };

        let now = self.now();
        run_privileged(&self.pool, async |tx| {
            let sql = format!("SELECT {AGENT_COLS} FROM agents WHERE id = $1 FOR UPDATE");
            let existing: Option<AgentRow> = sqlx::query_as::<_, AgentRow>(&sql)
                .bind(id)
                .fetch_optional(&mut **tx)
                .await?;
            let existing = existing.ok_or(AgentStoreError::NotFound(id))?;
            let mut current = AgentRecord::try_from(existing)?;

            // Demoting the only default in this org would leave the org
            // without one, which breaks every session-create that omits
            // `agent_id`. Reject it; the caller must promote another row
            // first (which atomically demotes this one).
            if matches!(payload.is_default, Some(false)) && current.is_default {
                return Err(AgentStoreError::DefaultDeletionForbidden);
            }

            if let Some(name) = payload.name {
                current.name = name;
            }
            if let Some(system_prompt) = payload.system_prompt {
                current.system_prompt = system_prompt;
            }
            if let Some(description) = payload.description {
                current.description = description;
            }
            if let Some(allowed) = payload.allowed_mcp_tools {
                current.allowed_mcp_tools = allowed;
            }
            if let Some(model) = payload.model {
                current.model = model;
            }

            // Promote: clear the old default in the *same org* in the same
            // transaction, then set the flag on this row. No-op if this row
            // is already the default.
            if matches!(payload.is_default, Some(true)) && !current.is_default {
                sqlx::query(
                    "UPDATE agents SET is_default = FALSE, updated_at = $1 \
                     WHERE is_default = TRUE AND org_id = $2",
                )
                .bind(now)
                .bind(current.org_id)
                .execute(&mut **tx)
                .await?;
                current.is_default = true;
            }

            current.updated_at = now;

            // `description_embedding` only moves when `description` does;
            // a `COALESCE($N::vector, description_embedding)` keeps the
            // existing vector untouched on the other update paths.
            let embedding_arg = new_embedding.as_deref().map(pg_vector::encode);
            sqlx::query(
                "UPDATE agents \
                 SET name = $2, system_prompt = $3, description = $4, \
                     description_embedding = COALESCE($5::vector, description_embedding), \
                     is_default = $6, allowed_mcp_tools = $7, model = $8, updated_at = $9 \
                 WHERE id = $1",
            )
            .bind(id)
            .bind(current.name.as_str())
            .bind(current.system_prompt.as_str())
            .bind(current.description.as_str())
            .bind(embedding_arg)
            .bind(current.is_default)
            .bind(Json(&current.allowed_mcp_tools))
            .bind(current.model)
            .bind(now)
            .execute(&mut **tx)
            .await?;

            Ok(current)
        })
        .await
    }

    async fn delete(&self, id: AgentId) -> Result<(), AgentStoreError> {
        run_privileged(&self.pool, async |tx| {
            let row: Option<(bool,)> =
                sqlx::query_as("SELECT is_default FROM agents WHERE id = $1 FOR UPDATE")
                    .bind(id)
                    .fetch_optional(&mut **tx)
                    .await?;
            let (is_default,) = row.ok_or(AgentStoreError::NotFound(id))?;
            if is_default {
                return Err(AgentStoreError::DefaultDeletionForbidden);
            }
            match sqlx::query("DELETE FROM agents WHERE id = $1")
                .bind(id)
                .execute(&mut **tx)
                .await
            {
                Ok(_) => Ok(()),
                Err(sqlx::Error::Database(db)) if db.code().as_deref() == Some("23503") => {
                    Err(AgentStoreError::InUse(id))
                }
                Err(e) => Err(e.into()),
            }
        })
        .await
    }

    async fn default_id_for(&self, org_id: OrgId) -> Result<AgentId, AgentStoreError> {
        let row = run_privileged::<Option<(AgentId,)>, AgentStoreError>(&self.pool, async |tx| {
            Ok(
                sqlx::query_as("SELECT id FROM agents WHERE is_default = TRUE AND org_id = $1")
                    .bind(org_id)
                    .fetch_optional(&mut **tx)
                    .await?,
            )
        })
        .await?;
        let (id,) = row.ok_or(AgentStoreError::NoDefault)?;
        Ok(id)
    }

    async fn read_by_name_for_viewer(
        &self,
        viewer: AgentId,
        name: &AgentName,
    ) -> Result<AgentRecord, AgentStoreError> {
        // Scope the lookup to the viewer's org via a correlated subquery
        // on `agents.org_id`. A name that's taken in a different org is
        // invisible — the model addressing only resolves peers it can
        // legitimately talk to.
        let sql = format!(
            "SELECT {AGENT_COLS} FROM agents \
             WHERE lower(name) = lower($1) \
               AND org_id = (SELECT org_id FROM agents WHERE id = $2)"
        );
        let name_str = name.as_str().to_owned();
        let row = run_privileged::<Option<AgentRow>, AgentStoreError>(&self.pool, async |tx| {
            Ok(sqlx::query_as::<_, AgentRow>(&sql)
                .bind(&name_str)
                .bind(viewer)
                .fetch_optional(&mut **tx)
                .await?)
        })
        .await?;
        let row = row.ok_or_else(|| AgentStoreError::NameNotFound(name.clone()))?;
        row.try_into()
    }

    async fn read_by_name_for_org(
        &self,
        org_id: OrgId,
        name: &AgentName,
    ) -> Result<AgentRecord, AgentStoreError> {
        // Same case-insensitive lookup as `_for_viewer`, but the org
        // gate is the literal `org_id` instead of one derived from a
        // viewer agent. Used by Slack/bridge resolution where there is
        // no in-DAG viewer.
        let sql = format!(
            "SELECT {AGENT_COLS} FROM agents \
             WHERE lower(name) = lower($1) AND org_id = $2"
        );
        let name_str = name.as_str().to_owned();
        let row = run_privileged::<Option<AgentRow>, AgentStoreError>(&self.pool, async |tx| {
            Ok(sqlx::query_as::<_, AgentRow>(&sql)
                .bind(&name_str)
                .bind(org_id)
                .fetch_optional(&mut **tx)
                .await?)
        })
        .await?;
        let row = row.ok_or_else(|| AgentStoreError::NameNotFound(name.clone()))?;
        row.try_into()
    }

    async fn list_names_for_viewer(
        &self,
        viewer: AgentId,
    ) -> Result<Vec<(AgentId, AgentName)>, AgentStoreError> {
        let rows =
            run_privileged::<Vec<(AgentId, String)>, AgentStoreError>(&self.pool, async |tx| {
                Ok(sqlx::query_as(
                    "SELECT id, name FROM agents \
                     WHERE org_id = (SELECT org_id FROM agents WHERE id = $1) \
                     ORDER BY lower(name) ASC LIMIT $2",
                )
                .bind(viewer)
                .bind(LIST_NAMES_MAX_ROWS)
                .fetch_all(&mut **tx)
                .await?)
            })
            .await?;
        let mut out = Vec::with_capacity(rows.len());
        for (id, name) in rows {
            out.push((id, AgentName::try_from(name)?));
        }
        Ok(out)
    }

    async fn list_for_org(
        &self,
        org_id: OrgId,
    ) -> Result<Vec<(AgentId, AgentName)>, AgentStoreError> {
        let rows =
            run_privileged::<Vec<(AgentId, String)>, AgentStoreError>(&self.pool, async |tx| {
                Ok(sqlx::query_as(
                    "SELECT id, name FROM agents \
                     WHERE org_id = $1 \
                     ORDER BY lower(name) ASC LIMIT $2",
                )
                .bind(org_id)
                .bind(LIST_NAMES_MAX_ROWS)
                .fetch_all(&mut **tx)
                .await?)
            })
            .await?;
        let mut out = Vec::with_capacity(rows.len());
        for (id, name) in rows {
            out.push((id, AgentName::try_from(name)?));
        }
        Ok(out)
    }

    async fn search_by_description(
        &self,
        embedding: &[f32],
        viewer: AgentId,
        k: usize,
    ) -> Result<Vec<AgentCard>, AgentStoreError> {
        if embedding.is_empty() || k == 0 {
            return Ok(Vec::new());
        }
        let limit = i64::try_from(k).unwrap_or(i64::MAX);
        let embedding_lit = pg_vector::encode(embedding);

        // Caller-excluded at the SQL boundary so the self row never lands
        // in the projection — keeps the three caller-excluded surfaces
        // (`<agents>`, `search_agents`, `send_message`) consistent. The
        // org-scope subquery restricts results to the viewer's tenant.
        let rows = run_privileged::<Vec<(AgentId, String, String)>, AgentStoreError>(
            &self.pool,
            async |tx| {
                Ok(sqlx::query_as(
                    "SELECT id, name, description \
                     FROM agents \
                     WHERE description_embedding IS NOT NULL \
                       AND id <> $3 \
                       AND org_id = (SELECT org_id FROM agents WHERE id = $3) \
                     ORDER BY description_embedding <=> $1::vector ASC \
                     LIMIT $2",
                )
                .bind(embedding_lit)
                .bind(limit)
                .bind(viewer)
                .fetch_all(&mut **tx)
                .await?)
            },
        )
        .await?;

        let mut out = Vec::with_capacity(rows.len());
        for (id, name, description) in rows {
            out.push(AgentCard {
                id,
                name: AgentName::try_from(name)?,
                description: AgentDescription::try_from(description)?,
            });
        }
        Ok(out)
    }
}

/// Body of `create` / `create_for_user`. Embed runs ahead of the tx
/// so the embedding call doesn't hold row locks; the runner owns
/// commit/rollback. The caller picks the runner (privileged for HTTP
/// route, tenant for the `create_agent` tool so the INSERT runs
/// RLS-checked).
async fn create_in_tx(
    store: &PgAgentStore,
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    embedding: &[f32],
    payload: NewAgent,
) -> Result<AgentRecord, AgentStoreError> {
    let embedding_literal = pg_vector::encode(embedding);
    let now = store.now();

    // Promoting a new row to default first demotes the existing
    // default in the same org so the partial unique index
    // `agents_default_unique` on `(org_id) WHERE is_default` stays
    // satisfied.
    if payload.is_default {
        sqlx::query(
            "UPDATE agents SET is_default = FALSE, updated_at = $1 \
                 WHERE is_default = TRUE AND org_id = $2",
        )
        .bind(now)
        .bind(payload.org_id)
        .execute(&mut **tx)
        .await?;
    }

    let id = AgentId::new();
    let insert = sqlx::query(
        "INSERT INTO agents \
                 (id, org_id, name, system_prompt, description, description_embedding, \
                  is_default, allowed_mcp_tools, model, created_at, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6::vector, $7, $8, $9, $10, $10)",
    )
    .bind(id)
    .bind(payload.org_id)
    .bind(payload.name.as_str())
    .bind(payload.system_prompt.as_str())
    .bind(payload.description.as_str())
    .bind(embedding_literal)
    .bind(payload.is_default)
    .bind(Json(&payload.allowed_mcp_tools))
    .bind(payload.model)
    .bind(now)
    .execute(&mut **tx)
    .await;
    match insert {
        Ok(_) => {}
        Err(sqlx::Error::Database(db)) if db.code().as_deref() == Some("23505") => {
            return Err(AgentStoreError::NameTaken(payload.name));
        }
        Err(e) => return Err(e.into()),
    }

    Ok(AgentRecord {
        id,
        org_id: payload.org_id,
        name: payload.name,
        system_prompt: payload.system_prompt,
        description: payload.description,
        is_default: payload.is_default,
        allowed_mcp_tools: payload.allowed_mcp_tools,
        model: payload.model,
        created_at: now,
        updated_at: now,
    })
}

#[derive(sqlx::FromRow)]
struct AgentRow {
    id: AgentId,
    org_id: OrgId,
    name: String,
    system_prompt: String,
    description: String,
    is_default: bool,
    allowed_mcp_tools: Json<AllowedMcpTools>,
    // Stored as TEXT NULL; the sqlx `Decode` impl on `Model` funnels the
    // string through the catalog smart constructor, so a row with an
    // out-of-catalog value here surfaces as `AgentStoreError::Backend` at
    // hydrate — loud (CLAUDE.md §6), not silent.
    model: Option<crate::provider::Model>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl TryFrom<AgentRow> for AgentRecord {
    type Error = AgentStoreError;

    fn try_from(row: AgentRow) -> Result<Self, Self::Error> {
        Ok(Self {
            id: row.id,
            org_id: row.org_id,
            name: AgentName::try_from(row.name)?,
            system_prompt: AgentSystemPrompt::try_from(row.system_prompt)?,
            description: AgentDescription::try_from(row.description)?,
            is_default: row.is_default,
            allowed_mcp_tools: row.allowed_mcp_tools.0,
            model: row.model,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }
}
