//! Postgres-backed [`ProfileStore`].
//!
//! Mirrors [`crate::colleagues::PgColleagueStore`] (privileged, org-scoped reads
//! that join `users`, which is REVOKEd from `patom_app`) and
//! [`crate::agents::PgAgentStore`] (embed *before* the write tx so a slow
//! embedding call never holds row locks; a row whose embed fails never lands).
//!
//! Timeouts follow the sibling stores: per-statement bounds come from the pool's
//! `acquire_timeout` (§9) and the embedding provider's own HTTP timeout, so the
//! queries here are not individually wrapped in `tokio::time::timeout`.

use std::collections::HashMap;

use async_trait::async_trait;
use sqlx::PgPool;

use crate::auth::{OrgId, run_privileged};
use crate::clock::SharedClock;
use crate::colleagues::{ColleagueId, ColleagueKind, ColleagueName};
use crate::pg_vector;
use crate::provider::{SharedEmbeddingProvider, embed_one};
use crate::types::ParseError;

use super::error::ProfileError;
use super::limits::{MAX_PROFILE_FETCH, PROFILE_SNIPPET_LEN};
use super::store::ProfileStore;
use super::types::{
    ColleagueMatch, ColleagueProfile, Expertise, Preferences, Role, compose_profile_text,
};

/// Phase-1 read: verify the subject is a colleague in the trusted org and learn
/// whether the stored `profile_text` already matches (so we can keep its vector
/// and skip the embed call). Both inner selects key on the PK, so this is cheap.
const PRE_UPSERT_SQL: &str = "SELECT \
        EXISTS(SELECT 1 FROM colleagues WHERE id = $1 AND org_id = $2) AS in_org, \
        (SELECT profile_text FROM colleague_profiles WHERE colleague_id = $1) AS existing_text, \
        (SELECT embedding IS NOT NULL FROM colleague_profiles WHERE colleague_id = $1) AS has_emb";

/// Phase-3 write. `$7` is the pgvector literal or NULL; on the update path a NULL
/// `$7` keeps the stored embedding via `COALESCE`. `created_at` is untouched on
/// conflict (only `updated_at` moves).
const UPSERT_SQL: &str = "INSERT INTO colleague_profiles \
        (colleague_id, org_id, role, expertise, preferences, profile_text, \
         embedding, updated_by_colleague, created_at, updated_at) \
     VALUES ($1, $2, $3, $4, $5, $6, $7::vector, $8, $9, $9) \
     ON CONFLICT (colleague_id) DO UPDATE SET \
         org_id               = EXCLUDED.org_id, \
         role                 = EXCLUDED.role, \
         expertise            = EXCLUDED.expertise, \
         preferences          = EXCLUDED.preferences, \
         profile_text         = EXCLUDED.profile_text, \
         embedding            = COALESCE(EXCLUDED.embedding, colleague_profiles.embedding), \
         updated_by_colleague = EXCLUDED.updated_by_colleague, \
         updated_at           = EXCLUDED.updated_at";

/// Batch profile fetch, keyed on the PK array.
const GET_MANY_SQL: &str = "SELECT colleague_id, role, expertise, preferences, \
        updated_by_colleague \
     FROM colleague_profiles \
     WHERE colleague_id = ANY($1)";

/// Unified colleague search: agents (rich card) UNION profiled humans (sparse
/// board), org-scoped to the viewer's tenant, viewer excluded, ranked by cosine
/// distance. `$1` = query vector, `$2` = snippet length, `$3` = viewer, `$4` = k.
const SEARCH_SQL: &str = "SELECT colleague_id, kind, name, snippet FROM ( \
        SELECT c.id AS colleague_id, 'agent'::text AS kind, a.name AS name, \
               LEFT(a.description, $2) AS snippet, \
               a.description_embedding <=> $1::vector AS distance \
          FROM agents a \
          JOIN colleagues c ON c.agent_id = a.id \
         WHERE a.description_embedding IS NOT NULL \
           AND c.id <> $3 \
           AND c.org_id = (SELECT org_id FROM colleagues WHERE id = $3) \
        UNION ALL \
        SELECT cp.colleague_id AS colleague_id, 'human'::text AS kind, \
               COALESCE(u.display_name, split_part(u.email, '@', 1)) AS name, \
               LEFT(cp.profile_text, $2) AS snippet, \
               cp.embedding <=> $1::vector AS distance \
          FROM colleague_profiles cp \
          JOIN colleagues c ON c.id = cp.colleague_id \
          LEFT JOIN users u ON u.id = c.user_id \
         WHERE cp.embedding IS NOT NULL \
           AND cp.colleague_id <> $3 \
           AND cp.org_id = (SELECT org_id FROM colleagues WHERE id = $3) \
    ) ranked \
    ORDER BY ranked.distance ASC \
    LIMIT $4";

/// Decoded `get_many` row shape: the PK plus the three nullable structured
/// fields and the provenance id.
type ProfileRow = (
    ColleagueId,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<ColleagueId>,
);

/// Postgres-backed profile board. Cheap to clone (pool + two `Arc` handles).
#[derive(Debug, Clone)]
pub struct PgProfileStore {
    pool: PgPool,
    clock: SharedClock,
    embeddings: SharedEmbeddingProvider,
}

impl PgProfileStore {
    #[must_use]
    pub fn new(pool: PgPool, clock: SharedClock, embeddings: SharedEmbeddingProvider) -> Self {
        Self {
            pool,
            clock,
            embeddings,
        }
    }
}

#[async_trait]
impl ProfileStore for PgProfileStore {
    async fn upsert(&self, org: OrgId, profile: &ColleagueProfile) -> Result<(), ProfileError> {
        let subject = profile.colleague_id();
        // Compose first so an all-empty profile fails before any I/O.
        let profile_text = compose_profile_text(profile)?;

        // Phase 1 — verify org membership + decide whether to reuse the vector.
        let (in_org, existing_text, has_emb) =
            run_privileged::<(bool, Option<String>, Option<bool>), ProfileError>(
                &self.pool,
                async |tx| {
                    Ok(sqlx::query_as(PRE_UPSERT_SQL)
                        .bind(subject)
                        .bind(org)
                        .fetch_one(&mut **tx)
                        .await?)
                },
            )
            .await?;
        if !in_org {
            return Err(ProfileError::SubjectNotInOrg { subject });
        }
        let reuse_embedding =
            has_emb == Some(true) && existing_text.as_deref() == Some(profile_text.as_str());

        // Phase 2 — embed outside the write tx, only when we can't reuse.
        let embedding_arg: Option<String> = if reuse_embedding {
            None
        } else {
            let vector = embed_one(self.embeddings.as_ref(), profile_text.as_str()).await?;
            Some(pg_vector::encode(&vector))
        };

        // Phase 3 — land the row.
        let now = self.clock.now_utc();
        run_privileged::<(), ProfileError>(&self.pool, async |tx| {
            sqlx::query(UPSERT_SQL)
                .bind(subject)
                .bind(org)
                .bind(profile.role().map(Role::as_str))
                .bind(profile.expertise().map(Expertise::as_str))
                .bind(profile.preferences().map(Preferences::as_str))
                .bind(profile_text.as_str())
                .bind(embedding_arg.as_deref())
                .bind(profile.updated_by())
                .bind(now)
                .execute(&mut **tx)
                .await?;
            Ok(())
        })
        .await
    }

    async fn get_many(
        &self,
        ids: &[ColleagueId],
    ) -> Result<HashMap<ColleagueId, ColleagueProfile>, ProfileError> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let capped = &ids[..ids.len().min(MAX_PROFILE_FETCH)];

        let rows = run_privileged::<Vec<ProfileRow>, ProfileError>(&self.pool, async |tx| {
            Ok(sqlx::query_as(GET_MANY_SQL)
                .bind(capped)
                .fetch_all(&mut **tx)
                .await?)
        })
        .await?;

        let mut out = HashMap::with_capacity(rows.len());
        for (cid, role, expertise, preferences, updated_by) in rows {
            let profile = ColleagueProfile::new(
                cid,
                role.map(Role::try_from).transpose()?,
                expertise.map(Expertise::try_from).transpose()?,
                preferences.map(Preferences::try_from).transpose()?,
                updated_by,
            );
            out.insert(cid, profile);
        }
        Ok(out)
    }

    async fn search_colleagues(
        &self,
        embedding: &[f32],
        viewer: ColleagueId,
        k: usize,
    ) -> Result<Vec<ColleagueMatch>, ProfileError> {
        if embedding.is_empty() || k == 0 {
            return Ok(Vec::new());
        }
        let limit = i64::try_from(k).unwrap_or(i64::MAX);
        let snippet_len = i32::try_from(PROFILE_SNIPPET_LEN).unwrap_or(i32::MAX);
        let query_lit = pg_vector::encode(embedding);

        let rows = run_privileged::<Vec<(ColleagueId, String, String, String)>, ProfileError>(
            &self.pool,
            async |tx| {
                Ok(sqlx::query_as(SEARCH_SQL)
                    .bind(query_lit)
                    .bind(snippet_len)
                    .bind(viewer)
                    .bind(limit)
                    .fetch_all(&mut **tx)
                    .await?)
            },
        )
        .await?;

        let mut out = Vec::with_capacity(rows.len());
        for (colleague_id, kind, name, snippet) in rows {
            // The UNION emits the literals 'agent'/'human', so a parse miss means
            // schema and code disagree (§6) — surface it, never silently drop.
            let kind = ColleagueKind::parse(&kind).ok_or(ParseError::Malformed {
                field: "colleague_kind",
                detail: "search union returned an unknown kind",
            })?;
            out.push(ColleagueMatch {
                colleague_id,
                kind,
                name: ColleagueName::try_from(name)?,
                snippet,
            });
        }
        Ok(out)
    }
}
