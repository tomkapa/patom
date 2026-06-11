//! CRUD + membership endpoints for user-created channels.
//!
//! `POST   /channels`                       — create (anyone); creator auto-enrolled
//! `GET    /channels`                        — channels the caller is a member of
//! `PATCH  /channels/{id}`                   — rename / archive (creator-only)
//! `GET    /channels/{id}/members`           — member roster (members only)
//! `POST   /channels/{id}/members`           — add a human member (creator-only)
//! `DELETE /channels/{id}/members/{user_id}` — remove a member (creator-only)
//!
//! A channel groups human-initiated thread roots (`prompt_requests.channel_id`).
//! Members are humans only — agents reach every channel by default. Anyone may
//! create; rename / archive / membership changes are restricted to the channel's
//! creator (`created_by_user_id`). The per-org `#general` channel is
//! system-owned (`created_by_user_id IS NULL`) and therefore immutable.
//!
//! Queries run inline on `state.pool` inside a tenant-scoped tx
//! (`crate::auth::begin_as`) so the org-isolation RLS policy applies, mirroring
//! the `agents` / `threads` routes; member-scoping and creator-ownership are
//! enforced explicitly in the handlers. The active org is always pinned in the
//! `WHERE` because RLS gates on membership in *any* org, not the active one.

use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, patch, post};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::auth::{AuthError, OrgId, Principal, UserId, begin_as, run_privileged};
use crate::channels::{CHANNEL_LIST_FETCH_MAX, ChannelId, ChannelName, MAX_CHANNELS_PER_ORG};
use crate::colleagues::ColleagueId;

use super::super::error::HttpError;
use super::super::state::AppState;

pub(super) fn router() -> Router<AppState> {
    Router::new()
        .route("/channels", post(create_channel).get(list_channels))
        .route("/channels/{id}", patch(update_channel))
        .route(
            "/channels/{id}/members",
            get(list_members).merge(post(add_member)),
        )
        .route("/channels/{id}/members/{user_id}", delete(remove_member))
}

/// Wire shape for one channel. `system` marks the immutable per-org
/// `#general`; `can_manage` tells the FE whether to show rename / archive /
/// member affordances (true iff the caller created it).
#[derive(Debug, Serialize)]
struct ChannelResponse {
    id: ChannelId,
    name: ChannelName,
    system: bool,
    can_manage: bool,
    created_at: DateTime<Utc>,
    archived_at: Option<DateTime<Utc>>,
}

#[derive(sqlx::FromRow)]
struct ChannelRow {
    id: ChannelId,
    name: String,
    created_by_user_id: Option<UserId>,
    created_at: DateTime<Utc>,
    archived_at: Option<DateTime<Utc>>,
}

impl ChannelRow {
    /// Project into the wire shape for `caller`. The stored name re-parses
    /// through [`ChannelName`] (the column CHECK already guarantees it); a
    /// violation means schema and code disagree, so it surfaces as 500.
    fn into_response(self, caller: UserId) -> Result<ChannelResponse, HttpError> {
        let name = ChannelName::try_from(self.name.as_str()).map_err(|_| HttpError::Internal)?;
        Ok(ChannelResponse {
            id: self.id,
            name,
            system: self.created_by_user_id.is_none(),
            can_manage: self.created_by_user_id == Some(caller),
            created_at: self.created_at,
            archived_at: self.archived_at,
        })
    }
}

const CHANNEL_COLS: &str = "id, name, created_by_user_id, created_at, archived_at";

/// Map an INSERT/UPDATE failure: a unique-violation (SQLSTATE 23505) on the
/// partial `(org_id, name)` index is a taken name; anything else is an
/// auth/store fault.
fn name_conflict_or_auth(e: sqlx::Error) -> HttpError {
    match &e {
        sqlx::Error::Database(db) if db.code().as_deref() == Some("23505") => {
            HttpError::Conflict("channel.name_taken".into())
        }
        _ => HttpError::Auth(AuthError::from(e)),
    }
}

#[derive(Debug, Deserialize)]
struct CreateChannelRequest {
    name: String,
}

async fn create_channel(
    State(state): State<AppState>,
    principal: Principal,
    Json(payload): Json<CreateChannelRequest>,
) -> Result<(StatusCode, Json<ChannelResponse>), HttpError> {
    let name = ChannelName::try_from(payload.name)?;
    let org = principal.active_org_id;
    let now = state.clock.now_utc();
    let id = ChannelId::new();

    let mut tx = begin_as(&state.pool, &principal).await?;
    // §5: cap active channels per org. Benign count-then-insert TOCTOU —
    // acceptable while the cap is generous; the unique index is the real guard
    // against the racing duplicate-name case.
    let active: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM channels WHERE org_id = $1 AND archived_at IS NULL",
    )
    .bind(org)
    .fetch_one(&mut *tx)
    .await
    .map_err(AuthError::from)?;
    if active >= MAX_CHANNELS_PER_ORG {
        return Err(HttpError::Conflict("channel.limit_reached".into()));
    }

    sqlx::query(
        "INSERT INTO channels (id, org_id, name, created_by_user_id, created_at) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(id)
    .bind(org)
    .bind(name.as_str())
    .bind(principal.user_id)
    .bind(now)
    .execute(&mut *tx)
    .await
    .map_err(name_conflict_or_auth)?;

    // Creator is the first member, so the channel shows up in their own list.
    sqlx::query(
        "INSERT INTO channel_members (channel_id, user_id, org_id, added_at) \
         VALUES ($1, $2, $3, $4)",
    )
    .bind(id)
    .bind(principal.user_id)
    .bind(org)
    .bind(now)
    .execute(&mut *tx)
    .await
    .map_err(AuthError::from)?;
    tx.commit().await.map_err(AuthError::from)?;

    let response = ChannelResponse {
        id,
        name,
        system: false,
        can_manage: true,
        created_at: now,
        archived_at: None,
    };
    Ok((StatusCode::CREATED, Json(response)))
}

async fn list_channels(
    State(state): State<AppState>,
    principal: Principal,
) -> Result<Json<Vec<ChannelResponse>>, HttpError> {
    let sql = format!(
        "SELECT {CHANNEL_COLS} FROM channels c \
         JOIN channel_members cm ON cm.channel_id = c.id AND cm.user_id = $1 \
         WHERE c.org_id = $2 AND c.archived_at IS NULL \
         ORDER BY (c.created_by_user_id IS NULL) DESC, c.created_at ASC \
         LIMIT $3"
    );
    let mut tx = begin_as(&state.pool, &principal).await?;
    let rows = sqlx::query_as::<_, ChannelRow>(&sql)
        .bind(principal.user_id)
        .bind(principal.active_org_id)
        .bind(CHANNEL_LIST_FETCH_MAX)
        .fetch_all(&mut *tx)
        .await
        .map_err(AuthError::from)?;
    tx.commit().await.map_err(AuthError::from)?;
    let out = rows
        .into_iter()
        .map(|r| r.into_response(principal.user_id))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Json(out))
}

#[derive(Debug, Deserialize)]
struct UpdateChannelRequest {
    #[serde(default)]
    name: Option<String>,
    /// `Some(true)` archives (sets `archived_at`), `Some(false)` restores,
    /// `None` leaves it untouched.
    #[serde(default)]
    archived: Option<bool>,
}

async fn update_channel(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<ChannelId>,
    Json(payload): Json<UpdateChannelRequest>,
) -> Result<Json<ChannelResponse>, HttpError> {
    let new_name = payload.name.map(ChannelName::try_from).transpose()?;
    let now = state.clock.now_utc();

    let mut tx = begin_as(&state.pool, &principal).await?;
    // Load + creator-gate under the same tx so a concurrent rename/archive
    // serialises on the row. 404 hides cross-org / unknown ids.
    let existing = load_channel(&mut tx, id, principal.active_org_id).await?;
    ensure_creator(&existing, principal.user_id)?;

    let row = sqlx::query_as::<_, ChannelRow>(&format!(
        "UPDATE channels \
            SET name = COALESCE($3, name), \
                archived_at = CASE \
                    WHEN $4::bool IS NULL THEN archived_at \
                    WHEN $4 THEN $5 \
                    ELSE NULL END \
          WHERE id = $1 AND org_id = $2 \
        RETURNING {CHANNEL_COLS}"
    ))
    .bind(id)
    .bind(principal.active_org_id)
    .bind(new_name.as_ref().map(ChannelName::as_str))
    .bind(payload.archived)
    .bind(now)
    .fetch_one(&mut *tx)
    .await
    .map_err(name_conflict_or_auth)?;
    tx.commit().await.map_err(AuthError::from)?;

    Ok(Json(row.into_response(principal.user_id)?))
}

#[derive(Debug, sqlx::FromRow)]
struct MemberRow {
    user_id: UserId,
    added_at: DateTime<Utc>,
    colleague_id: Option<ColleagueId>,
}

/// One channel-roster row, enriched with the member's profile so the FE can
/// render humans in the mention list / DM sidebar without a second endpoint.
/// Profile fields come from the privileged user read — the tenant tx can't
/// touch `users` (migration 14).
#[derive(Debug, Serialize)]
struct MemberResponse {
    user_id: UserId,
    added_at: DateTime<Utc>,
    display_name: Option<String>,
    avatar_url: Option<String>,
    /// The member's `colleagues` row (the addressing satellite), distinct from
    /// `user_id`. Lets the FE map an agent's `{kind:"colleague", id}`
    /// send_message receiver to this human's name when rendering who a message
    /// addresses (web/src/lib/foldHistory.ts).
    colleague_id: Option<ColleagueId>,
}

async fn list_members(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<ChannelId>,
) -> Result<Json<Vec<MemberResponse>>, HttpError> {
    let org = principal.active_org_id;
    let mut tx = begin_as(&state.pool, &principal).await?;
    // Only members see the roster; non-members (and unknown ids) 404. Pinned to
    // the active org so a multi-org caller can't read another workspace's
    // roster by id.
    if !caller_is_member(&mut tx, id, principal.user_id, org).await? {
        return Err(HttpError::NotFound);
    }
    // LEFT JOIN colleagues so each member carries its addressing satellite id.
    // LEFT (not INNER) keeps a member listed even if its colleague row is
    // somehow absent — `colleague_id` just comes back null.
    let members = sqlx::query_as::<_, MemberRow>(
        "SELECT cm.user_id, cm.added_at, c.id AS colleague_id \
         FROM channel_members cm \
         LEFT JOIN colleagues c ON c.user_id = cm.user_id AND c.org_id = cm.org_id \
         WHERE cm.channel_id = $1 AND cm.org_id = $2 ORDER BY cm.added_at ASC LIMIT $3",
    )
    .bind(id)
    .bind(org)
    .bind(CHANNEL_LIST_FETCH_MAX)
    .fetch_all(&mut *tx)
    .await
    .map_err(AuthError::from)?;
    tx.commit().await.map_err(AuthError::from)?;

    // Profile enrichment, bounded by CHANNEL_LIST_FETCH_MAX (CLAUDE.md §5).
    let ids: Vec<UserId> = members.iter().map(|m| m.user_id).collect();
    let profiles = state.users.read_profiles(&ids).await?;
    let members = members
        .into_iter()
        .map(|m| {
            let profile = profiles.get(&m.user_id);
            MemberResponse {
                user_id: m.user_id,
                added_at: m.added_at,
                display_name: profile.map(|p| p.name.clone()),
                avatar_url: profile.and_then(|p| p.avatar_url.clone()),
                colleague_id: m.colleague_id,
            }
        })
        .collect();
    Ok(Json(members))
}

#[derive(Debug, Deserialize)]
struct AddMemberRequest {
    user_id: UserId,
}

async fn add_member(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<ChannelId>,
    Json(payload): Json<AddMemberRequest>,
) -> Result<StatusCode, HttpError> {
    let target = payload.user_id;
    let org = principal.active_org_id;
    let now = state.clock.now_utc();
    let mut tx = begin_as(&state.pool, &principal).await?;
    // Authorize first: load + creator-gate before probing org membership, so a
    // non-creator can't use the `not_in_org` vs `not_owner` response split to
    // enumerate who belongs to the org.
    let channel = load_channel(&mut tx, id, org).await?;
    ensure_creator(&channel, principal.user_id)?;
    // Target must already belong to the active org. `org_members` is REVOKEd
    // from `patom_app`, so the check runs through a privileged tx.
    if !is_org_member(&state, org, target).await? {
        return Err(HttpError::BadRequest("user.not_in_org".into()));
    }
    sqlx::query(
        "INSERT INTO channel_members (channel_id, user_id, org_id, added_at) \
         VALUES ($1, $2, $3, $4) ON CONFLICT (channel_id, user_id) DO NOTHING",
    )
    .bind(id)
    .bind(target)
    .bind(org)
    .bind(now)
    .execute(&mut *tx)
    .await
    .map_err(AuthError::from)?;
    tx.commit().await.map_err(AuthError::from)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn remove_member(
    State(state): State<AppState>,
    principal: Principal,
    Path((id, target)): Path<(ChannelId, UserId)>,
) -> Result<StatusCode, HttpError> {
    let mut tx = begin_as(&state.pool, &principal).await?;
    let channel = load_channel(&mut tx, id, principal.active_org_id).await?;
    ensure_creator(&channel, principal.user_id)?;
    sqlx::query("DELETE FROM channel_members WHERE channel_id = $1 AND user_id = $2")
        .bind(id)
        .bind(target)
        .execute(&mut *tx)
        .await
        .map_err(AuthError::from)?;
    tx.commit().await.map_err(AuthError::from)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Load a channel by id, pinned to `org`. Returns 404 when it is missing or
/// lives in another workspace (RLS already filters to the caller's orgs; the
/// `org_id` predicate pins the active one).
async fn load_channel(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    id: ChannelId,
    org: OrgId,
) -> Result<ChannelRow, HttpError> {
    sqlx::query_as::<_, ChannelRow>(&format!(
        "SELECT {CHANNEL_COLS} FROM channels WHERE id = $1 AND org_id = $2"
    ))
    .bind(id)
    .bind(org)
    .fetch_optional(&mut **tx)
    .await
    .map_err(AuthError::from)?
    .ok_or(HttpError::NotFound)
}

/// Reject mutation by anyone but the channel's creator. The system `#general`
/// (`created_by_user_id IS NULL`) is immutable, so it falls through to 403.
fn ensure_creator(channel: &ChannelRow, caller: UserId) -> Result<(), HttpError> {
    if channel.created_by_user_id == Some(caller) {
        return Ok(());
    }
    Err(HttpError::Forbidden("channel.not_owner"))
}

async fn caller_is_member(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    id: ChannelId,
    user: UserId,
    org: OrgId,
) -> Result<bool, HttpError> {
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM channel_members \
         WHERE channel_id = $1 AND user_id = $2 AND org_id = $3)",
    )
    .bind(id)
    .bind(user)
    .bind(org)
    .fetch_one(&mut **tx)
    .await
    .map_err(AuthError::from)?;
    Ok(exists)
}

/// Privileged membership probe — `org_members` is REVOKEd from `patom_app`,
/// so this reads it through an owner-role tx (RLS off).
async fn is_org_member(state: &AppState, org: OrgId, user: UserId) -> Result<bool, HttpError> {
    run_privileged(&state.pool, async |tx| {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM org_members WHERE org_id = $1 AND user_id = $2)",
        )
        .bind(org)
        .bind(user)
        .fetch_one(&mut **tx)
        .await?;
        Ok::<bool, AuthError>(exists)
    })
    .await
    .map_err(HttpError::Auth)
}
