//! Postgres-backed [`OrgStore`].
//!
//! Writes go through `auth::run_as_user` so RLS scopes every query to
//! the caller's org (the active org pinned on the JWT). The single
//! exception is invite issuance + acceptance, which touch `users` and
//! `org_members` — both REVOKEd from `patom_app` per migration 14 —
//! and therefore have to run privileged. Each privileged path is
//! gated by an explicit `app_user_is_member` check upstream in the
//! handler.

use async_trait::async_trait;
use base64::Engine as _;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use rand::RngCore;
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};

use super::error::OrgError;
use super::store::{
    AcceptedInvite, InviteRow, IssuedInvite, MemberFilter, MemberPage, MemberRow, MemberStatus,
    OrgDetails, OrgStore, OrgUpdate,
};
use crate::auth;
use crate::auth::{Email, InviteId, InviteToken, Language, OrgId, OrgName, OrgSlug, Role, UserId};
use crate::types::AvatarUrl;

#[derive(Debug)]
pub struct PgOrgStore {
    pool: PgPool,
}

impl PgOrgStore {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

/// SHA-256 of the cleartext invite token. The hash is what gets
/// persisted; the cleartext only ever lives in memory at issuance.
fn hash_token(token: &InviteToken) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(token.as_str().as_bytes());
    hasher.finalize().to_vec()
}

/// Mint a fresh URL-safe single-use token. Uses `InviteToken`'s
/// `RAW_ENTROPY_BYTES` (32) of OS-side randomness.
fn mint_token() -> InviteToken {
    let mut bytes = [0u8; InviteToken::RAW_ENTROPY_BYTES];
    rand::thread_rng().fill_bytes(&mut bytes);
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    // `try_from` is infallible here by construction — 32 random
    // bytes encode to 43 URL-safe characters which is exactly
    // `InviteToken::MIN_BYTES`. The `expect` is named after the
    // invariant per CLAUDE.md §6.
    InviteToken::try_from(encoded.as_str())
        .expect("invariant: 32-byte random encodes to URL_SAFE_NO_PAD within InviteToken bounds")
}

async fn fetch_org_details_priv(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    org_id: OrgId,
) -> Result<OrgDetails, OrgError> {
    let row = sqlx::query(
        "SELECT o.id, o.name, o.slug::text AS slug, o.default_language, o.created_at,
                o.avatar_url, o.onboarded_at,
                (SELECT COUNT(*)::bigint FROM org_members m WHERE m.org_id = o.id) AS member_count
         FROM organizations o
         WHERE o.id = $1",
    )
    .bind(org_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(OrgError::NotFound)?;

    Ok(OrgDetails {
        id: OrgId::from(row.get::<uuid::Uuid, _>("id")),
        name: row.get("name"),
        slug: OrgSlug::try_from(row.get::<String, _>("slug"))?,
        default_language: row.get::<Language, _>("default_language"),
        created_at: row.get("created_at"),
        member_count: row.get("member_count"),
        avatar_url: row
            .get::<Option<String>, _>("avatar_url")
            .map(AvatarUrl::try_from)
            .transpose()?,
        onboarded: row
            .get::<Option<DateTime<Utc>>, _>("onboarded_at")
            .is_some(),
    })
}

#[async_trait]
impl OrgStore for PgOrgStore {
    #[tracing::instrument(skip(self), fields(patom.org.id = %org_id))]
    async fn read_org(&self, org_id: OrgId) -> Result<OrgDetails, OrgError> {
        let mut tx = auth::begin_privileged(&self.pool).await?;
        let details = fetch_org_details_priv(&mut tx, org_id).await?;
        tx.commit().await?;
        Ok(details)
    }

    #[tracing::instrument(skip(self, patch, now), fields(patom.org.id = %org_id))]
    async fn update_org(
        &self,
        org_id: OrgId,
        patch: OrgUpdate,
        now: DateTime<Utc>,
    ) -> Result<OrgDetails, OrgError> {
        // Nothing to do — keep the round trip cheap.
        if patch.name.is_none() && patch.slug.is_none() && !patch.mark_onboarded {
            return self.read_org(org_id).await;
        }
        let mut tx = auth::begin_privileged(&self.pool).await?;
        // COALESCE-style update so unset patches don't clobber the
        // existing value. Bind the typed newtype string (already
        // boundary-validated) — never interpolate.
        //
        // `onboarded_at` is stamped iff `mark_onboarded=true` AND the
        // column is currently NULL — COALESCE(onboarded_at, NOW())
        // preserves the original timestamp on a second mark. Never
        // un-marked through this seam (clearing the flag would have to
        // be a separate, deliberate operation).
        let result = sqlx::query(
            "UPDATE organizations
             SET name         = COALESCE($2, name),
                 slug         = COALESCE($3, slug)::citext,
                 onboarded_at = CASE WHEN $5::bool
                                     THEN COALESCE(onboarded_at, $4)
                                     ELSE onboarded_at END,
                 updated_at   = $4
             WHERE id = $1",
        )
        .bind(org_id)
        .bind(patch.name.as_ref().map(OrgName::as_str))
        .bind(patch.slug.as_ref().map(OrgSlug::as_str))
        .bind(now)
        .bind(patch.mark_onboarded)
        .execute(&mut *tx)
        .await;
        match result {
            Ok(_) => {}
            Err(sqlx::Error::Database(db)) if db.code().as_deref() == Some("23505") => {
                return Err(OrgError::SlugTaken);
            }
            Err(e) => return Err(OrgError::Db(e)),
        }
        let details = fetch_org_details_priv(&mut tx, org_id).await?;
        tx.commit().await?;
        Ok(details)
    }

    // The union + filter + count query is naturally long; splitting
    // would either bloat the trait surface or force three round trips.
    #[allow(clippy::too_many_lines)]
    #[tracing::instrument(skip(self, filter, now), fields(patom.org.id = %org_id))]
    async fn list_members(
        &self,
        org_id: OrgId,
        filter: MemberFilter,
        now: DateTime<Utc>,
    ) -> Result<MemberPage, OrgError> {
        let per_page = filter.per_page.clamp(1, super::MAX_MEMBERS_PER_PAGE);
        let page = filter.page.max(1);
        let offset = i64::from(page - 1) * i64::from(per_page);

        // Bind the role filter as text|null so the same prepared
        // statement covers "all" + "one role". CITEXT for `email` /
        // `slug` so `LIKE` is case-insensitive.
        //
        // Escape backslash first so the subsequent `%` / `_` escapes
        // don't get re-escaped, then add `ESCAPE '\'` on the ILIKE
        // clause (see below) so PostgreSQL treats backslash as the
        // escape char. Without all three steps, an input of `a\%b`
        // would still match a literal `%` and skew search results.
        let query_pattern = filter.query.as_ref().map(|q| {
            let escaped = q
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_");
            format!("%{escaped}%")
        });

        let role_filter = filter.role.map(|r| r.as_str().to_owned());

        let mut tx = auth::begin_privileged(&self.pool).await?;

        // Single unified "members + invites" SELECT. UNION ALL keeps
        // the columns positional and avoids two round trips. The
        // outer SELECT orders + paginates over the union.
        let union_sql = "
WITH unified AS (
    SELECT
        'member'        AS kind,
        m.user_id::uuid AS user_id,
        NULL::uuid      AS invite_id,
        u.email::text   AS email,
        u.display_name  AS display_name,
        u.avatar_url    AS avatar_url,
        m.role          AS role,
        m.created_at    AS joined_at,
        NULL::timestamptz AS expires_at,
        'active'        AS status
    FROM org_members m
    JOIN users u ON u.id = m.user_id
    WHERE m.org_id = $1

    UNION ALL

    SELECT
        'invite'        AS kind,
        NULL::uuid      AS user_id,
        i.id::uuid      AS invite_id,
        i.email::text   AS email,
        NULL            AS display_name,
        NULL            AS avatar_url,
        i.role          AS role,
        i.created_at    AS joined_at,
        i.expires_at    AS expires_at,
        CASE
            WHEN i.expires_at < $5 THEN 'expired'
            ELSE 'invited'
        END             AS status
    FROM org_invites i
    WHERE i.org_id = $1 AND i.consumed_at IS NULL
)
SELECT *, COUNT(*) OVER () AS total
FROM unified
WHERE ($2::text IS NULL OR email ILIKE $2 ESCAPE '\')
  AND ($3::text IS NULL OR role = $3)
  AND ($4::text IS NULL OR status = $4)
ORDER BY joined_at DESC
LIMIT $6 OFFSET $7
        ";

        let rows = sqlx::query(union_sql)
            .bind(org_id)
            .bind(query_pattern.as_deref())
            .bind(role_filter.as_deref())
            .bind(filter.status.map(MemberStatus::as_str))
            .bind(now)
            .bind(i64::from(per_page))
            .bind(offset)
            .fetch_all(&mut *tx)
            .await?;

        // Cardinality counters for the filter tabs. Run as one
        // GROUP-BY-status pass over the same union so the FE can
        // show accurate badges regardless of the active filter.
        let counts = sqlx::query(
            "
WITH unified AS (
    SELECT 'active'::text AS status FROM org_members WHERE org_id = $1
    UNION ALL
    SELECT CASE WHEN i.expires_at < $2 THEN 'expired' ELSE 'invited' END
    FROM org_invites i
    WHERE i.org_id = $1 AND i.consumed_at IS NULL
)
SELECT status, COUNT(*)::bigint AS n FROM unified GROUP BY status
            ",
        )
        .bind(org_id)
        .bind(now)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;

        let mut active_count = 0_i64;
        let mut invited_count = 0_i64;
        let mut expired_count = 0_i64;
        for c in counts {
            let label: String = c.get("status");
            let n: i64 = c.get("n");
            match label.as_str() {
                "active" => active_count = n,
                "invited" => invited_count = n,
                "expired" => expired_count = n,
                _ => {}
            }
        }

        let total = rows.first().map_or(0_i64, |r| r.get::<i64, _>("total"));
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let kind: String = r.get("kind");
            let email = Email::try_from(r.get::<String, _>("email"))?;
            let role = Role::parse(r.get::<&str, _>("role"))
                .ok_or(OrgError::Auth(auth::AuthError::Internal("unknown role")))?;
            let joined_at: DateTime<Utc> = r.get("joined_at");
            if kind.as_str() == "member" {
                out.push(MemberRow::Member {
                    user_id: UserId::from(r.get::<uuid::Uuid, _>("user_id")),
                    email,
                    display_name: r.get("display_name"),
                    avatar_url: r
                        .get::<Option<String>, _>("avatar_url")
                        .map(AvatarUrl::try_from)
                        .transpose()?,
                    role,
                    joined_at,
                });
            } else {
                let status_label: String = r.get("status");
                let status = if status_label == "expired" {
                    MemberStatus::Expired
                } else {
                    MemberStatus::Invited
                };
                out.push(MemberRow::Invite(InviteRow {
                    invite_id: InviteId::from(r.get::<uuid::Uuid, _>("invite_id")),
                    email,
                    role,
                    status,
                    invited_at: joined_at,
                    expires_at: r.get("expires_at"),
                }));
            }
        }

        Ok(MemberPage {
            rows: out,
            total,
            active_count,
            invited_count,
            expired_count,
        })
    }

    #[tracing::instrument(
        skip(self),
        fields(
            patom.org.id = %org_id,
            patom.target.user_id = %user_id,
            patom.role = %new_role.as_str(),
        )
    )]
    async fn change_role(
        &self,
        org_id: OrgId,
        user_id: UserId,
        new_role: Role,
    ) -> Result<(), OrgError> {
        let mut tx = auth::begin_privileged(&self.pool).await?;
        last_owner_guard(&mut tx, org_id, user_id, Some(new_role)).await?;
        let n = sqlx::query("UPDATE org_members SET role = $3 WHERE org_id = $1 AND user_id = $2")
            .bind(org_id)
            .bind(user_id)
            .bind(new_role.as_str())
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if n == 0 {
            return Err(OrgError::NotFound);
        }
        tx.commit().await?;
        Ok(())
    }

    #[tracing::instrument(skip(self), fields(patom.org.id = %org_id, patom.target.user_id = %user_id))]
    async fn remove_member(&self, org_id: OrgId, user_id: UserId) -> Result<(), OrgError> {
        let mut tx = auth::begin_privileged(&self.pool).await?;
        last_owner_guard(&mut tx, org_id, user_id, None).await?;
        let n = sqlx::query("DELETE FROM org_members WHERE org_id = $1 AND user_id = $2")
            .bind(org_id)
            .bind(user_id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if n == 0 {
            return Err(OrgError::NotFound);
        }
        tx.commit().await?;
        Ok(())
    }

    #[tracing::instrument(
        skip(self, emails, now, ttl),
        fields(
            patom.org.id = %org_id,
            patom.role = %role.as_str(),
            patom.invite.batch_size = emails.len(),
        )
    )]
    async fn create_invites(
        &self,
        org_id: OrgId,
        emails: &[Email],
        role: Role,
        invited_by: UserId,
        now: DateTime<Utc>,
        ttl: ChronoDuration,
    ) -> Result<Vec<IssuedInvite>, OrgError> {
        if emails.len() > super::MAX_INVITE_BATCH {
            return Err(OrgError::InviteBatchTooLarge {
                max: super::MAX_INVITE_BATCH,
                got: emails.len(),
            });
        }
        let expires_at = now + ttl;
        let mut tx = auth::begin_privileged(&self.pool).await?;
        let mut out = Vec::with_capacity(emails.len());

        for email in emails {
            let token = mint_token();
            let token_hash = hash_token(&token);
            // Upsert by `(org_id, email)`: if a pending invite already
            // exists, rotate its token + expiry. The partial unique
            // index `org_invites_org_email_pending_idx` enforces the
            // single-pending-row invariant.
            let row = sqlx::query(
                "INSERT INTO org_invites
                   (id, org_id, email, role, token_hash, invited_by, created_at, expires_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
                 ON CONFLICT (org_id, email) WHERE consumed_at IS NULL
                 DO UPDATE SET
                     role       = EXCLUDED.role,
                     token_hash = EXCLUDED.token_hash,
                     invited_by = EXCLUDED.invited_by,
                     created_at = EXCLUDED.created_at,
                     expires_at = EXCLUDED.expires_at
                 RETURNING id",
            )
            .bind(InviteId::new())
            .bind(org_id)
            .bind(email.as_str())
            .bind(role.as_str())
            .bind(&token_hash)
            .bind(invited_by)
            .bind(now)
            .bind(expires_at)
            .fetch_one(&mut *tx)
            .await?;
            out.push(IssuedInvite {
                invite_id: InviteId::from(row.get::<uuid::Uuid, _>("id")),
                email: email.clone(),
                role,
                token,
                expires_at,
            });
        }
        tx.commit().await?;
        Ok(out)
    }

    #[tracing::instrument(
        skip(self, now, ttl),
        fields(patom.org.id = %org_id, patom.invite.id = %invite_id)
    )]
    async fn resend_invite(
        &self,
        org_id: OrgId,
        invite_id: InviteId,
        now: DateTime<Utc>,
        ttl: ChronoDuration,
    ) -> Result<IssuedInvite, OrgError> {
        let token = mint_token();
        let token_hash = hash_token(&token);
        let expires_at = now + ttl;
        let mut tx = auth::begin_privileged(&self.pool).await?;
        let row = sqlx::query(
            "UPDATE org_invites
             SET token_hash = $3, created_at = $4, expires_at = $5
             WHERE id = $1 AND org_id = $2 AND consumed_at IS NULL
             RETURNING email::text AS email, role",
        )
        .bind(invite_id)
        .bind(org_id)
        .bind(&token_hash)
        .bind(now)
        .bind(expires_at)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(OrgError::NotFound)?;
        tx.commit().await?;
        Ok(IssuedInvite {
            invite_id,
            email: Email::try_from(row.get::<String, _>("email"))?,
            role: Role::parse(row.get::<&str, _>("role"))
                .ok_or(OrgError::Auth(auth::AuthError::Internal("unknown role")))?,
            token,
            expires_at,
        })
    }

    #[tracing::instrument(skip(self), fields(patom.org.id = %org_id, patom.invite.id = %invite_id))]
    async fn revoke_invite(&self, org_id: OrgId, invite_id: InviteId) -> Result<(), OrgError> {
        let mut tx = auth::begin_privileged(&self.pool).await?;
        let n = sqlx::query("DELETE FROM org_invites WHERE id = $1 AND org_id = $2")
            .bind(invite_id)
            .bind(org_id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if n == 0 {
            return Err(OrgError::NotFound);
        }
        tx.commit().await?;
        Ok(())
    }

    #[tracing::instrument(skip(self, avatar_url, now), fields(patom.org.id = %org_id))]
    async fn set_avatar_url(
        &self,
        org_id: OrgId,
        avatar_url: Option<&AvatarUrl>,
        now: DateTime<Utc>,
    ) -> Result<(), OrgError> {
        let mut tx = auth::begin_privileged(&self.pool).await?;
        let rows = sqlx::query(
            "UPDATE organizations
             SET avatar_url = $2, updated_at = $3
             WHERE id = $1",
        )
        .bind(org_id)
        .bind(avatar_url.map(AvatarUrl::as_str))
        .bind(now)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if rows == 0 {
            return Err(OrgError::NotFound);
        }
        tx.commit().await?;
        Ok(())
    }

    async fn delete_org(&self, org_id: OrgId) -> Result<(), OrgError> {
        let mut tx = auth::begin_privileged(&self.pool).await?;
        // One DELETE cascades to every `org_id` FK (all declared
        // ON DELETE CASCADE): members, agents, channels, sessions,
        // invites, budgets, MCP servers, Slack rows, … No manual teardown.
        let rows = sqlx::query("DELETE FROM organizations WHERE id = $1")
            .bind(org_id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        // §6: assert what we expect AND what we don't — the PK is unique,
        // so a successful delete touches exactly one row, never more.
        assert!(
            rows <= 1,
            "organizations.id is unique; delete touched >1 row"
        );
        if rows == 0 {
            return Err(OrgError::NotFound);
        }
        tx.commit().await?;
        Ok(())
    }

    async fn join_pending_invites(
        &self,
        user_id: UserId,
        email: &Email,
        now: DateTime<Utc>,
    ) -> Result<Option<OrgId>, OrgError> {
        let mut tx = auth::begin_privileged(&self.pool).await?;
        // Pending, non-expired invites addressed to the verified email,
        // oldest first so the active org is deterministic.
        let rows = sqlx::query(
            "SELECT id, org_id, role FROM org_invites
             WHERE email = $1 AND consumed_at IS NULL AND expires_at > $2
             ORDER BY created_at ASC",
        )
        .bind(email.as_str())
        .bind(now)
        .fetch_all(&mut *tx)
        .await?;
        if rows.is_empty() {
            tx.commit().await?;
            return Ok(None);
        }
        let mut active: Option<OrgId> = None;
        for row in &rows {
            let org_id = OrgId::from(row.get::<uuid::Uuid, _>("org_id"));
            let invite_id = row.get::<uuid::Uuid, _>("id");
            let role: String = row.get("role");
            // §6: the column CHECK guarantees this; the assertion detects
            // corruption rather than silently inserting a bad membership.
            assert!(
                Role::parse(&role).is_some(),
                "org_invites.role must be a known role"
            );
            // Claim the invite FIRST: the conditional UPDATE is the atomic
            // gate. Two concurrent logins proving the same email race here;
            // exactly one wins the row (RETURNING yields it), the loser sees
            // zero rows and skips. Only the winner mints the membership, so a
            // single invite can never admit two accounts.
            let claimed = sqlx::query(
                "UPDATE org_invites SET consumed_at = $2
                 WHERE id = $1 AND consumed_at IS NULL
                 RETURNING id",
            )
            .bind(invite_id)
            .bind(now)
            .fetch_optional(&mut *tx)
            .await?;
            if claimed.is_none() {
                continue;
            }
            // Idempotent on (org_id, user_id) so a benign self double-login
            // (same user already a member) leaves the existing row untouched.
            sqlx::query(
                "INSERT INTO org_members (org_id, user_id, role, created_at)
                 VALUES ($1, $2, $3, $4)
                 ON CONFLICT (org_id, user_id) DO NOTHING",
            )
            .bind(org_id)
            .bind(user_id)
            .bind(&role)
            .bind(now)
            .execute(&mut *tx)
            .await?;
            if active.is_none() {
                active = Some(org_id);
            }
        }
        tx.commit().await?;
        // Note: `active` may be None even though `rows` was non-empty — a
        // concurrent login can have claimed every invite between our SELECT
        // and our UPDATE. The caller treats None as "no org" and denies.
        Ok(active)
    }

    #[tracing::instrument(skip(self, token), fields(patom.target.user_id = %user_id))]
    async fn accept_invite(
        &self,
        user_id: UserId,
        token: &InviteToken,
        now: DateTime<Utc>,
    ) -> Result<AcceptedInvite, OrgError> {
        let token_hash = hash_token(token);
        let mut tx = auth::begin_privileged(&self.pool).await?;
        // Look up by token hash regardless of state so we can return a
        // precise error (expired vs. consumed vs. unknown) to the FE.
        let row = sqlx::query(
            "SELECT id, org_id, role, consumed_at, expires_at
             FROM org_invites WHERE token_hash = $1",
        )
        .bind(&token_hash)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(OrgError::NotFound)?;
        if row.get::<Option<DateTime<Utc>>, _>("consumed_at").is_some() {
            tx.commit().await?;
            return Err(OrgError::InviteAlreadyConsumed);
        }
        if row.get::<DateTime<Utc>, _>("expires_at") <= now {
            tx.commit().await?;
            return Err(OrgError::InviteExpired);
        }
        let org_id = OrgId::from(row.get::<uuid::Uuid, _>("org_id"));
        let invite_id = row.get::<uuid::Uuid, _>("id");
        let invite_role_raw: String = row.get("role");
        // §6: the column CHECK guarantees this; the assertion detects
        // corruption rather than silently inserting a bad membership.
        let invite_role = Role::parse(&invite_role_raw);
        assert!(
            invite_role.is_some(),
            "org_invites.role must be a known role"
        );
        let invite_role = invite_role.expect("invariant: role validated by assert above");
        // Claim FIRST: the conditional UPDATE is the atomic gate. Two
        // concurrent accepts of the same token race here; exactly one
        // wins the row, the loser sees zero rows and reports the invite
        // as already consumed — a single invite never admits twice.
        let claimed = sqlx::query(
            "UPDATE org_invites SET consumed_at = $2
             WHERE id = $1 AND consumed_at IS NULL
             RETURNING id",
        )
        .bind(invite_id)
        .bind(now)
        .fetch_optional(&mut *tx)
        .await?;
        if claimed.is_none() {
            tx.commit().await?;
            return Err(OrgError::InviteAlreadyConsumed);
        }
        // Join the org. A user who is already a member keeps their
        // existing role (the no-op `DO UPDATE SET role = org_members.role`
        // exists only so `RETURNING` yields the *effective* persisted role
        // on conflict). We return that, not the invite's role, so an
        // already-member accepting a lower-privileged invite isn't
        // mis-reported as demoted.
        let effective_role_raw: String = sqlx::query_scalar(
            "INSERT INTO org_members (org_id, user_id, role, created_at)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (org_id, user_id) DO UPDATE SET role = org_members.role
             RETURNING role",
        )
        .bind(org_id)
        .bind(user_id)
        .bind(invite_role.as_str())
        .bind(now)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        // §6: the column CHECK guarantees a known role here too.
        let role = Role::parse(&effective_role_raw);
        assert!(role.is_some(), "org_members.role must be a known role");
        let role = role.expect("invariant: role validated by assert above");
        Ok(AcceptedInvite { org_id, role })
    }
}

/// Guard a role-change or member-removal against demoting / removing
/// the last owner. `new_role = None` means "removing", `Some(r)` means
/// "changing to r". When the targeted user is not currently an owner
/// the guard is a no-op.
async fn last_owner_guard(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    org_id: OrgId,
    user_id: UserId,
    new_role: Option<Role>,
) -> Result<(), OrgError> {
    let current: Option<String> =
        sqlx::query_scalar("SELECT role FROM org_members WHERE org_id = $1 AND user_id = $2")
            .bind(org_id)
            .bind(user_id)
            .fetch_optional(&mut **tx)
            .await?;
    let Some(current) = current.and_then(|r| Role::parse(&r)) else {
        return Err(OrgError::NotFound);
    };
    if !matches!(current, Role::Owner) {
        return Ok(());
    }
    // Demoting to Owner is a no-op; nothing to guard.
    if matches!(new_role, Some(Role::Owner)) {
        return Ok(());
    }
    let owners: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM org_members WHERE org_id = $1 AND role = 'owner'",
    )
    .bind(org_id)
    .fetch_one(&mut **tx)
    .await?;
    if owners <= 1 {
        return Err(OrgError::LastOwnerProtected);
    }
    Ok(())
}
