//! Postgres-backed [`UserStore`]. Identity tables are not RLS-protected
//! in this PR (every authenticated request can see at least their own
//! user/org rows via the `/me` route). Mutations still go through
//! [`super::begin_privileged`] so we can extend RLS to these tables
//! later without rewriting this module.

use std::fmt;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use sqlx::Row;
use tracing::info;

use super::error::AuthError;
use super::language::Language;
use super::limits::MAX_SLUG_RETRIES;
use super::locale_hint::LocaleHint;
use super::org_rule::OrganizationRule;
use super::store::{
    ConsumedOAuthState, NewOrg, OAuthStateRow, UpsertedUser, UserProfileLite, UserStore,
};
use super::types::{
    Email, OAuthState, OidcNonce, OidcProfile, OrgId, OrgMembership, OrgSlug, PkceVerifier, Role,
    User, UserId,
};
use crate::types::AvatarUrl;

pub struct PgUserStore {
    pool: PgPool,
}

impl PgUserStore {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

impl fmt::Debug for PgUserStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgUserStore").finish_non_exhaustive()
    }
}

#[async_trait]
impl UserStore for PgUserStore {
    async fn upsert_from_oidc(
        &self,
        profile: &OidcProfile,
        now: DateTime<Utc>,
    ) -> Result<UpsertedUser, AuthError> {
        let mut tx = super::begin_privileged(&self.pool).await?;

        // Serialize concurrent first-logins for the same (issuer, subject)
        // so the "look up identity, insert if missing" sequence below
        // resolves to one users row. Transaction-scoped — released
        // automatically on commit/rollback. `hashtextextended` is a
        // built-in stable hash returning bigint, the shape
        // `pg_advisory_xact_lock` wants.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!(
                "oidc:{}:{}",
                profile.issuer.as_str(),
                profile.subject.as_str()
            ))
            .execute(&mut *tx)
            .await?;

        // Canonical identity is (issuer, subject), not email. Email can
        // change at the IdP and a stale email-keyed upsert would sign the
        // callback into the wrong users row when the new email already
        // belongs to a different account.
        let existing: Option<UserId> = sqlx::query_scalar(
            "SELECT user_id FROM user_identities
             WHERE oidc_issuer = $1 AND oidc_subject = $2",
        )
        .bind(profile.issuer.as_str())
        .bind(profile.subject.as_str())
        .fetch_optional(&mut *tx)
        .await?;

        let (user_id, is_new_user) = if let Some(uid) = existing {
            sqlx::query(
                "UPDATE users
                 SET email        = $2,
                     display_name = $3,
                     avatar_url   = $4,
                     updated_at   = $5
                 WHERE id = $1",
            )
            .bind(uid)
            .bind(profile.email.as_str())
            .bind(profile.display_name.as_deref())
            .bind(profile.avatar_url.as_ref().map(AvatarUrl::as_str))
            .bind(now)
            .execute(&mut *tx)
            .await?;
            (uid, false)
        } else {
            let candidate_id = UserId::new();
            sqlx::query(
                "INSERT INTO users (id, email, display_name, avatar_url, created_at, updated_at)
                 VALUES ($1, $2, $3, $4, $5, $5)",
            )
            .bind(candidate_id)
            .bind(profile.email.as_str())
            .bind(profile.display_name.as_deref())
            .bind(profile.avatar_url.as_ref().map(AvatarUrl::as_str))
            .bind(now)
            .execute(&mut *tx)
            .await?;
            // `provider` + legacy `subject` are retained NOT NULL columns
            // (migration 53): write 'oidc' and mirror the subject so the
            // backfilled-Google rows and new rows share one shape. The PK
            // is now (oidc_issuer, oidc_subject).
            sqlx::query(
                "INSERT INTO user_identities
                     (user_id, provider, subject, oidc_issuer, oidc_subject, email_at_link, created_at)
                 VALUES ($1, 'oidc', $2, $3, $2, $4, $5)",
            )
            .bind(candidate_id)
            .bind(profile.subject.as_str())
            .bind(profile.issuer.as_str())
            .bind(profile.email.as_str())
            .bind(now)
            .execute(&mut *tx)
            .await?;
            (candidate_id, true)
        };

        // Read back the canonical user row.
        let row =
            sqlx::query("SELECT id, email, display_name, avatar_url FROM users WHERE id = $1")
                .bind(user_id)
                .fetch_one(&mut *tx)
                .await?;
        tx.commit().await?;

        let user = User {
            id: user_id,
            email: Email::try_from(row.get::<String, _>("email"))?,
            display_name: row.get("display_name"),
            avatar_url: row
                .get::<Option<String>, _>("avatar_url")
                .map(AvatarUrl::try_from)
                .transpose()?,
        };
        Ok(UpsertedUser { user, is_new_user })
    }

    async fn create_personal_org(
        &self,
        user_id: UserId,
        suggested_slug: &str,
        display_name: &str,
        language: Language,
        now: DateTime<Utc>,
    ) -> Result<NewOrg, AuthError> {
        let base = sanitize_slug(suggested_slug);
        let mut attempt = 0;
        loop {
            let candidate = if attempt == 0 {
                base.clone()
            } else {
                format!("{base}-{}", random_suffix())
            };
            // Re-parse through OrgSlug to make sure we don't insert a row
            // the CHECK constraint would reject.
            let slug = match OrgSlug::try_from(candidate.as_str()) {
                Ok(s) => s,
                Err(_) if attempt < MAX_SLUG_RETRIES => {
                    attempt += 1;
                    continue;
                }
                Err(e) => return Err(AuthError::Parse(e)),
            };
            match self
                .try_insert_org(user_id, slug.as_str(), display_name, language, now)
                .await
            {
                Ok(new_org) => return Ok(new_org),
                Err(AuthError::Db(sqlx::Error::Database(db)))
                    if db.code().as_deref() == Some("23505") =>
                {
                    if attempt >= MAX_SLUG_RETRIES {
                        return Err(AuthError::Internal("could not mint unique org slug"));
                    }
                    attempt += 1;
                }
                Err(e) => return Err(e),
            }
        }
    }

    async fn bootstrap_initial_org_as_owner(
        &self,
        user_id: UserId,
        suggested_slug: &str,
        display_name: &str,
        language: Language,
        now: DateTime<Utc>,
    ) -> Result<Option<NewOrg>, AuthError> {
        let mut tx = super::begin_privileged(&self.pool).await?;
        // One fixed lock for the whole bootstrap so two simultaneous
        // first logins serialize: the loser observes count > 0 below.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind("patom:bootstrap-admin")
            .execute(&mut *tx)
            .await?;
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM organizations")
            .fetch_one(&mut *tx)
            .await?;
        // §6: assert what we expect and what we don't before branching.
        assert!(count >= 0, "count(*) is never negative");
        if count > 0 {
            // Not the first login any more; release the lock and let the
            // caller fall through to the normal self-service path.
            tx.commit().await?;
            return Ok(None);
        }
        assert_eq!(count, 0, "bootstrap requires an empty organizations table");

        // Empty table ⇒ no slug can collide; re-parse through OrgSlug so
        // we never insert a row the CHECK would reject.
        let slug = OrgSlug::try_from(sanitize_slug(suggested_slug)).map_err(AuthError::Parse)?;
        let id = OrgId::new();
        insert_org_and_owner(
            &mut tx,
            id,
            user_id,
            slug.as_str(),
            display_name,
            language,
            now,
        )
        .await?;
        tx.commit().await?;
        info!(
            event = "auth.bootstrap.admin",
            patom.user.id = %user_id,
            patom.org.id = %id,
        );
        Ok(Some(NewOrg {
            id,
            slug: slug.as_str().to_owned(),
            name: display_name.to_owned(),
            default_language: language,
        }))
    }

    async fn list_user_orgs(&self, user_id: UserId) -> Result<Vec<OrgMembership>, AuthError> {
        let mut tx = super::begin_privileged(&self.pool).await?;
        let rows = sqlx::query(
            "SELECT o.id, o.name, o.slug::text AS slug, o.default_language, o.default_rule,
                    o.avatar_url, o.onboarded_at, m.role
             FROM org_members m
             JOIN organizations o ON o.id = m.org_id
             WHERE m.user_id = $1
             ORDER BY o.created_at ASC",
        )
        .bind(user_id)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        rows.into_iter()
            .map(|r| {
                let default_rule = r
                    .get::<Option<String>, _>("default_rule")
                    .map(OrganizationRule::try_from)
                    .transpose()?;
                Ok(OrgMembership {
                    org_id: OrgId::from(r.get::<uuid::Uuid, _>("id")),
                    org_name: r.get("name"),
                    org_slug: OrgSlug::try_from(r.get::<String, _>("slug"))?,
                    role: Role::parse(r.get::<&str, _>("role"))
                        .ok_or(AuthError::Internal("unknown role in db"))?,
                    default_language: r.get::<Language, _>("default_language"),
                    default_rule,
                    avatar_url: r
                        .get::<Option<String>, _>("avatar_url")
                        .map(AvatarUrl::try_from)
                        .transpose()?,
                    onboarded: r
                        .get::<Option<chrono::DateTime<chrono::Utc>>, _>("onboarded_at")
                        .is_some(),
                })
            })
            .collect()
    }

    async fn count_owned_orgs(&self, user_id: UserId) -> Result<i64, AuthError> {
        let mut tx = super::begin_privileged(&self.pool).await?;
        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM org_members WHERE user_id = $1 AND role = $2")
                .bind(user_id)
                .bind(Role::Owner.as_str())
                .fetch_one(&mut *tx)
                .await?;
        tx.commit().await?;
        // §6: a count is never negative; surfacing a corrupt aggregate
        // early beats letting a bogus cap check through.
        assert!(count >= 0, "count(*) of owned orgs is never negative");
        Ok(count)
    }

    async fn membership(&self, user_id: UserId, org_id: OrgId) -> Result<Option<Role>, AuthError> {
        let mut tx = super::begin_privileged(&self.pool).await?;
        let row: Option<String> =
            sqlx::query_scalar("SELECT role FROM org_members WHERE user_id = $1 AND org_id = $2")
                .bind(user_id)
                .bind(org_id)
                .fetch_optional(&mut *tx)
                .await?;
        tx.commit().await?;
        Ok(row.and_then(|r| Role::parse(&r)))
    }

    async fn read_user(&self, user_id: UserId) -> Result<Option<User>, AuthError> {
        let mut tx = super::begin_privileged(&self.pool).await?;
        let row =
            sqlx::query("SELECT id, email, display_name, avatar_url FROM users WHERE id = $1")
                .bind(user_id)
                .fetch_optional(&mut *tx)
                .await?;
        tx.commit().await?;
        let Some(r) = row else { return Ok(None) };
        Ok(Some(User {
            id: UserId::from(r.get::<uuid::Uuid, _>("id")),
            email: Email::try_from(r.get::<String, _>("email"))?,
            display_name: r.get("display_name"),
            avatar_url: r
                .get::<Option<String>, _>("avatar_url")
                .map(AvatarUrl::try_from)
                .transpose()?,
        }))
    }

    async fn read_emails(
        &self,
        ids: &[UserId],
    ) -> Result<std::collections::HashMap<UserId, Email>, AuthError> {
        if ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let mut tx = super::begin_privileged(&self.pool).await?;
        let rows = sqlx::query("SELECT id, email FROM users WHERE id = ANY($1)")
            .bind(ids)
            .fetch_all(&mut *tx)
            .await?;
        tx.commit().await?;
        let mut out = std::collections::HashMap::with_capacity(rows.len());
        for r in rows {
            let id = UserId::from(r.get::<uuid::Uuid, _>("id"));
            let email = Email::try_from(r.get::<String, _>("email"))?;
            out.insert(id, email);
        }
        Ok(out)
    }

    async fn read_profiles(
        &self,
        ids: &[UserId],
    ) -> Result<std::collections::HashMap<UserId, UserProfileLite>, AuthError> {
        if ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        // Name resolved in SQL with the same roster formula as
        // `colleagues/pg_store.rs` so a thread author's name matches the
        // `<colleagues>` roster. Avatar comes back as the raw (DB-checked)
        // string — a display read must not fail on a malformed value.
        let mut tx = super::begin_privileged(&self.pool).await?;
        let rows = sqlx::query(
            "SELECT id, COALESCE(display_name, split_part(email, '@', 1)) AS name, avatar_url \
             FROM users WHERE id = ANY($1)",
        )
        .bind(ids)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        let mut out = std::collections::HashMap::with_capacity(rows.len());
        for r in rows {
            let id = UserId::from(r.get::<uuid::Uuid, _>("id"));
            let profile = UserProfileLite {
                name: r.get::<String, _>("name"),
                avatar_url: r.get::<Option<String>, _>("avatar_url"),
            };
            out.insert(id, profile);
        }
        Ok(out)
    }

    async fn read_org_language(&self, org_id: OrgId) -> Result<Language, AuthError> {
        let mut tx = super::begin_privileged(&self.pool).await?;
        let value: Option<Language> =
            sqlx::query_scalar("SELECT default_language FROM organizations WHERE id = $1")
                .bind(org_id)
                .fetch_optional(&mut *tx)
                .await?;
        tx.commit().await?;
        // §6: the language column is NOT NULL and `id` is the primary
        // key; a missing row reachable from `Principal.active_org_id`
        // means the membership row out-lived the org, which is itself a
        // wiring bug we want surfaced.
        value.ok_or(AuthError::Internal(
            "org not found for default_language read",
        ))
    }

    async fn set_org_language(
        &self,
        org_id: OrgId,
        language: Language,
        now: DateTime<Utc>,
    ) -> Result<Language, AuthError> {
        let mut tx = super::begin_privileged(&self.pool).await?;
        let updated: Option<Language> = sqlx::query_scalar(
            "UPDATE organizations
             SET default_language = $2, updated_at = $3
             WHERE id = $1
             RETURNING default_language",
        )
        .bind(org_id)
        .bind(language)
        .bind(now)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        updated.ok_or(AuthError::Internal(
            "org not found for default_language write",
        ))
    }

    async fn read_org_rule(&self, org_id: OrgId) -> Result<Option<OrganizationRule>, AuthError> {
        let mut tx = super::begin_privileged(&self.pool).await?;
        // Outer `Option` = row existence, inner `Option<String>` = the
        // nullable column value. A missing row is a wiring bug (see
        // `read_org_language`); a NULL column is the "no rule
        // configured" sentinel and rides through to the caller.
        let row: Option<Option<String>> =
            sqlx::query_scalar("SELECT default_rule FROM organizations WHERE id = $1")
                .bind(org_id)
                .fetch_optional(&mut *tx)
                .await?;
        tx.commit().await?;
        let column = row.ok_or(AuthError::Internal("org not found for default_rule read"))?;
        column
            .map(OrganizationRule::try_from)
            .transpose()
            .map_err(AuthError::from)
    }

    async fn set_org_rule(
        &self,
        org_id: OrgId,
        rule: Option<OrganizationRule>,
        now: DateTime<Utc>,
    ) -> Result<Option<OrganizationRule>, AuthError> {
        let mut tx = super::begin_privileged(&self.pool).await?;
        // Bind `Option<&str>` so `None` writes a SQL NULL — the column is
        // nullable on purpose (see migration 40). Echo the persisted
        // value back via RETURNING so the handler doesn't need a
        // round-trip read.
        let updated: Option<Option<String>> = sqlx::query_scalar(
            "UPDATE organizations
             SET default_rule = $2, updated_at = $3
             WHERE id = $1
             RETURNING default_rule",
        )
        .bind(org_id)
        .bind(rule.as_ref().map(OrganizationRule::as_str))
        .bind(now)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        let column = updated.ok_or(AuthError::Internal("org not found for default_rule write"))?;
        column
            .map(OrganizationRule::try_from)
            .transpose()
            .map_err(AuthError::from)
    }

    async fn set_avatar_url(
        &self,
        user_id: UserId,
        avatar_url: Option<&AvatarUrl>,
        now: DateTime<Utc>,
    ) -> Result<(), AuthError> {
        let mut tx = super::begin_privileged(&self.pool).await?;
        let rows = sqlx::query(
            "UPDATE users
             SET avatar_url = $2, updated_at = $3
             WHERE id = $1",
        )
        .bind(user_id)
        .bind(avatar_url.map(AvatarUrl::as_str))
        .bind(now)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        tx.commit().await?;
        if rows == 0 {
            return Err(AuthError::Unauthenticated);
        }
        Ok(())
    }

    async fn insert_oauth_state(&self, row: &OAuthStateRow) -> Result<(), AuthError> {
        let mut tx = super::begin_privileged(&self.pool).await?;
        sqlx::query(
            "INSERT INTO oauth_login_states (state, pkce_verifier, nonce, redirect_to, detected_locale, created_at, expires_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(row.state.as_str())
        .bind(row.pkce_verifier.as_str())
        .bind(row.nonce.as_str())
        .bind(row.redirect_to.as_deref())
        .bind(row.detected_locale.as_ref().map(LocaleHint::as_str))
        .bind(row.created_at)
        .bind(row.expires_at)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn consume_oauth_state(
        &self,
        state: &OAuthState,
        now: DateTime<Utc>,
    ) -> Result<ConsumedOAuthState, AuthError> {
        let mut tx = super::begin_privileged(&self.pool).await?;
        let row = sqlx::query(
            "DELETE FROM oauth_login_states
             WHERE state = $1 AND expires_at > $2
             RETURNING pkce_verifier, nonce, redirect_to, detected_locale",
        )
        .bind(state.as_str())
        .bind(now)
        .fetch_optional(&mut *tx)
        .await?;
        // Best-effort cleanup of expired rows on every consume. Bounded
        // by the row count; this table is small (10 min TTL).
        sqlx::query("DELETE FROM oauth_login_states WHERE expires_at <= $1")
            .bind(now)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        let row = row.ok_or(AuthError::OAuthStateInvalid)?;
        let detected_locale = row
            .get::<Option<String>, _>("detected_locale")
            .map(LocaleHint::try_from)
            .transpose()?;
        Ok(ConsumedOAuthState {
            pkce_verifier: PkceVerifier::try_from(row.get::<String, _>("pkce_verifier"))?,
            nonce: OidcNonce::try_from(row.get::<String, _>("nonce"))?,
            redirect_to: row.get("redirect_to"),
            detected_locale,
        })
    }
}

impl PgUserStore {
    async fn try_insert_org(
        &self,
        user_id: UserId,
        slug: &str,
        display_name: &str,
        language: Language,
        now: DateTime<Utc>,
    ) -> Result<NewOrg, AuthError> {
        let mut tx = super::begin_privileged(&self.pool).await?;
        let id = OrgId::new();
        insert_org_and_owner(&mut tx, id, user_id, slug, display_name, language, now).await?;
        tx.commit().await?;
        Ok(NewOrg {
            id,
            slug: slug.to_owned(),
            name: display_name.to_owned(),
            default_language: language,
        })
    }
}

/// Insert a new organization and its owner `org_members` row inside an
/// existing transaction. The org-creation SQL lives here once and is
/// shared by the self-service (`try_insert_org`) and first-admin
/// (`bootstrap_initial_org_as_owner`) paths, which differ only in their
/// surrounding guards (slug-collision retry vs. empty-table assertion).
async fn insert_org_and_owner(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    id: OrgId,
    user_id: UserId,
    slug: &str,
    display_name: &str,
    language: Language,
    now: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO organizations (id, name, slug, default_language, created_at, updated_at)
         VALUES ($1, $2, $3, $4, $5, $5)",
    )
    .bind(id)
    .bind(display_name)
    .bind(slug)
    .bind(language)
    .bind(now)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "INSERT INTO org_members (org_id, user_id, role, created_at)
         VALUES ($1, $2, 'owner', $3)",
    )
    .bind(id)
    .bind(user_id)
    .bind(now)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

fn sanitize_slug(raw: &str) -> String {
    // Drop everything that isn't `[a-z0-9-]`; collapse runs of `-`;
    // strip leading non-alphanumerics; cap at 50 chars to leave room
    // for the random suffix on collision.
    let mut out = String::with_capacity(raw.len());
    let lower = raw.to_lowercase();
    let mut last_dash = false;
    for ch in lower.chars() {
        let ok = ch.is_ascii_lowercase() || ch.is_ascii_digit();
        if ok {
            out.push(ch);
            last_dash = false;
        } else if !last_dash && !out.is_empty() {
            out.push('-');
            last_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        out.push_str("org");
    }
    if out.len() > 50 {
        out.truncate(50);
        while out.ends_with('-') {
            out.pop();
        }
    }
    out
}

fn random_suffix() -> String {
    // 4-char random suffix from uuid; cheap, no extra dep needed.
    uuid::Uuid::new_v4().simple().to_string()[..4].to_owned()
}
