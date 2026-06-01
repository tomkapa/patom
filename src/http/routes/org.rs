//! Workspace-settings routes — General + Members + Invites.
//!
//! All routes mount under `/api` behind the auth + CSRF layers. The
//! `Principal` extractor confirms the caller is a member of *some* org;
//! the route handlers re-confirm membership (via
//! `state.users.membership`) before any mutation so a stale JWT cannot
//! be used to act past a recent demotion. The General tab's read is
//! the only happy-path that trusts the JWT's role claim — read-only
//! and idempotent.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use chrono::Duration as ChronoDuration;
use serde::{Deserialize, Serialize};

use crate::auth::{
    AuthError, Email, InviteId, Language, OrgName, OrgSlug, Principal, Role, UserId,
};
use crate::budget::{BudgetConfig, MonthlyCapMicros, WarnThresholdBps};
use crate::orgs::{
    INVITE_TTL, MAX_INVITE_BATCH, MAX_MEMBERS_PER_PAGE, MemberFilter, MemberRow, MemberStatus,
    OrgError, OrgUpdate,
};

use super::super::error::HttpError;
use super::super::state::AppState;

pub(super) fn router() -> Router<AppState> {
    Router::new()
        .route("/me/org", get(read_org).patch(update_org))
        .route("/me/org/budget", get(read_budget).put(set_budget))
        .route("/me/org/members", get(list_members))
        .route("/me/org/members/{user_id}", delete(remove_member))
        .route("/me/org/members/{user_id}/role", patch(change_role))
        .route("/me/org/invites", post(invite_members))
        .route("/me/org/invites/{invite_id}", delete(revoke_invite))
        .route("/me/org/invites/{invite_id}/resend", post(resend_invite))
        .route("/me/org/leave", post(leave_org))
}

// ─────────────────────────────────────────────────────────────────────
// Authorization helper.
// ─────────────────────────────────────────────────────────────────────

/// Re-read the caller's role on the active org. A stale JWT cannot
/// outlive a recent demotion. Returns the live role for callers that
/// need to branch on it (e.g. role-change handler).
async fn live_role(state: &AppState, principal: &Principal) -> Result<Role, HttpError> {
    let role = state
        .users
        .membership(principal.user_id, principal.active_org_id)
        .await?
        .ok_or(AuthError::NotMember(principal.active_org_id))?;
    Ok(role)
}

fn require_admin(role: Role) -> Result<(), HttpError> {
    match role {
        Role::Owner | Role::Admin => Ok(()),
        Role::Member => Err(HttpError::Forbidden("owner or admin role required")),
    }
}

// ─────────────────────────────────────────────────────────────────────
// GET /me/org — General-tab payload.
// ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct OrgDetailsView {
    id: crate::auth::OrgId,
    name: String,
    slug: String,
    default_language: Language,
    member_count: i64,
    created_at: chrono::DateTime<chrono::Utc>,
    role: Role,
    /// `null` → FE renders the default tile.
    avatar_url: Option<String>,
}

impl OrgDetailsView {
    fn new(details: crate::orgs::OrgDetails, role: Role) -> Self {
        Self {
            id: details.id,
            name: details.name,
            slug: details.slug.as_str().to_owned(),
            default_language: details.default_language,
            member_count: details.member_count,
            created_at: details.created_at,
            role,
            avatar_url: details.avatar_url,
        }
    }
}

async fn read_org(
    State(state): State<AppState>,
    principal: Principal,
) -> Result<Response, HttpError> {
    let role = live_role(&state, &principal).await?;
    let details = state.orgs.read_org(principal.active_org_id).await?;
    Ok(Json(OrgDetailsView::new(details, role)).into_response())
}

// ─────────────────────────────────────────────────────────────────────
// PATCH /me/org — update name + slug.
// ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct UpdateOrgRequest {
    /// New display name. Absent or null → no change.
    name: Option<String>,
    /// New URL slug. Absent or null → no change.
    slug: Option<String>,
}

async fn update_org(
    State(state): State<AppState>,
    principal: Principal,
    Json(req): Json<UpdateOrgRequest>,
) -> Result<Response, HttpError> {
    let role = live_role(&state, &principal).await?;
    require_admin(role)?;
    let name = req.name.map(OrgName::try_from).transpose()?;
    let slug = req.slug.map(OrgSlug::try_from).transpose()?;
    let now = state.clock.now_utc();
    let details = state
        .orgs
        .update_org(principal.active_org_id, OrgUpdate { name, slug }, now)
        .await?;
    Ok(Json(OrgDetailsView::new(details, role)).into_response())
}

// ─────────────────────────────────────────────────────────────────────
// GET /me/org/budget — current cap + warn threshold + this period's spend.
// PUT /me/org/budget — set/clear the cap + warn threshold (owner/admin).
// ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct BudgetView {
    /// `null` → unlimited (no cap configured).
    monthly_cap_micro_usd: Option<i64>,
    warn_threshold_bps: u16,
    used_micro_usd: i64,
    /// `cap - used`, floored at zero; `null` when unlimited.
    remaining_micro_usd: Option<i64>,
    /// Set once per period when usage first crossed the warn threshold.
    warned_at: Option<chrono::DateTime<chrono::Utc>>,
    /// First day of the current billing month (UTC).
    period_start: chrono::NaiveDate,
    /// The caller's live role, so the FE can render read-only for members.
    role: Role,
}

impl BudgetView {
    fn new(config: BudgetConfig, role: Role) -> Self {
        let remaining_micro_usd = config
            .cap_micro_usd
            .map(|cap| (cap - config.used_micro_usd).max(0));
        Self {
            monthly_cap_micro_usd: config.cap_micro_usd,
            warn_threshold_bps: config.warn_threshold_bps,
            used_micro_usd: config.used_micro_usd,
            remaining_micro_usd,
            warned_at: config.warned_at,
            period_start: config.period_start,
            role,
        }
    }
}

async fn read_budget(
    State(state): State<AppState>,
    principal: Principal,
) -> Result<Response, HttpError> {
    // No role gate — any member can see the workspace's spend; the FE
    // renders the edit controls read-only for members.
    let role = live_role(&state, &principal).await?;
    let config = state
        .budget
        .get_config(principal.user_id, principal.active_org_id)
        .await?;
    Ok(Json(BudgetView::new(config, role)).into_response())
}

#[derive(Debug, Deserialize)]
struct SetBudgetRequest {
    /// `null` → clear the cap (unlimited). A configured cap must be positive.
    monthly_cap_micro_usd: Option<i64>,
    warn_threshold_bps: u16,
}

async fn set_budget(
    State(state): State<AppState>,
    principal: Principal,
    Json(req): Json<SetBudgetRequest>,
) -> Result<Response, HttpError> {
    let role = live_role(&state, &principal).await?;
    require_admin(role)?;
    // Parse at the boundary (CLAUDE.md §1): invalid values become a
    // `ParseError` → 400 before any write.
    let cap = req
        .monthly_cap_micro_usd
        .map(MonthlyCapMicros::try_from)
        .transpose()?;
    let warn = WarnThresholdBps::try_from(req.warn_threshold_bps)?;
    // `set_config` returns the fresh view read in the write transaction — no
    // second round-trip needed.
    let config = state
        .budget
        .set_config(principal.user_id, principal.active_org_id, cap, warn)
        .await?;
    Ok(Json(BudgetView::new(config, role)).into_response())
}

// ─────────────────────────────────────────────────────────────────────
// GET /me/org/members — paginated list with filters + counts.
// ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ListMembersQuery {
    q: Option<String>,
    status: Option<String>,
    role: Option<String>,
    #[serde(default)]
    page: Option<u32>,
    #[serde(default)]
    per_page: Option<u32>,
}

#[derive(Debug, Serialize)]
struct MemberView {
    /// `"member"` (real `org_members` row) or `"invite"` (pending).
    kind: &'static str,
    user_id: Option<UserId>,
    invite_id: Option<InviteId>,
    email: String,
    display_name: Option<String>,
    avatar_url: Option<String>,
    role: Role,
    status: &'static str,
    joined_at: chrono::DateTime<chrono::Utc>,
    expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Serialize)]
struct ListMembersResponse {
    rows: Vec<MemberView>,
    total: i64,
    counts: CountsView,
    page: u32,
    per_page: u32,
}

#[derive(Debug, Serialize)]
struct CountsView {
    all: i64,
    active: i64,
    invited: i64,
    expired: i64,
}

fn parse_member_status(s: &str) -> Option<MemberStatus> {
    match s {
        "active" => Some(MemberStatus::Active),
        "invited" => Some(MemberStatus::Invited),
        "expired" => Some(MemberStatus::Expired),
        _ => None,
    }
}

async fn list_members(
    State(state): State<AppState>,
    principal: Principal,
    Query(req): Query<ListMembersQuery>,
) -> Result<Response, HttpError> {
    // No role gate — any member can see who else is in the org.
    let _ = live_role(&state, &principal).await?;

    let page = req.page.unwrap_or(1).max(1);
    let per_page = req.per_page.unwrap_or(20).clamp(1, MAX_MEMBERS_PER_PAGE);

    let status = req.status.as_deref().and_then(parse_member_status);
    let role_filter = req.role.as_deref().and_then(Role::parse);

    let filter = MemberFilter {
        query: req
            .q
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
        status,
        role: role_filter,
        page,
        per_page,
    };

    let now = state.clock.now_utc();
    let page_data = state
        .orgs
        .list_members(principal.active_org_id, filter, now)
        .await?;

    let rows = page_data
        .rows
        .into_iter()
        .map(|r| match r {
            MemberRow::Member {
                user_id,
                email,
                display_name,
                avatar_url,
                role,
                joined_at,
            } => MemberView {
                kind: "member",
                user_id: Some(user_id),
                invite_id: None,
                email: email.as_str().to_owned(),
                display_name,
                avatar_url,
                role,
                status: "active",
                joined_at,
                expires_at: None,
            },
            MemberRow::Invite(i) => MemberView {
                kind: "invite",
                user_id: None,
                invite_id: Some(i.invite_id),
                email: i.email.as_str().to_owned(),
                display_name: None,
                avatar_url: None,
                role: i.role,
                status: i.status.as_str(),
                joined_at: i.invited_at,
                expires_at: Some(i.expires_at),
            },
        })
        .collect();

    Ok(Json(ListMembersResponse {
        rows,
        total: page_data.total,
        counts: CountsView {
            all: page_data.active_count + page_data.invited_count + page_data.expired_count,
            active: page_data.active_count,
            invited: page_data.invited_count,
            expired: page_data.expired_count,
        },
        page,
        per_page,
    })
    .into_response())
}

// ─────────────────────────────────────────────────────────────────────
// PATCH /me/org/members/{user_id}/role
// ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ChangeRoleRequest {
    role: Role,
}

async fn change_role(
    State(state): State<AppState>,
    principal: Principal,
    Path(target_user_id): Path<UserId>,
    Json(req): Json<ChangeRoleRequest>,
) -> Result<Response, HttpError> {
    let role = live_role(&state, &principal).await?;
    require_admin(role)?;
    // Two owner-only seams:
    //   1. Granting Owner — admins can't promote past their own rank.
    //   2. Demoting an existing Owner to anything else — only another
    //      Owner can revoke that grant.
    // We collapse both into one pre-check + one DB read.
    let target_role = state
        .users
        .membership(target_user_id, principal.active_org_id)
        .await?
        .ok_or(HttpError::NotFound)?;
    let touches_owner = matches!(req.role, Role::Owner) || matches!(target_role, Role::Owner);
    if touches_owner && !matches!(role, Role::Owner) {
        return Err(HttpError::Forbidden(
            "owner role required to grant or revoke owner",
        ));
    }
    state
        .orgs
        .change_role(principal.active_org_id, target_user_id, req.role)
        .await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ─────────────────────────────────────────────────────────────────────
// DELETE /me/org/members/{user_id}
// ─────────────────────────────────────────────────────────────────────

async fn remove_member(
    State(state): State<AppState>,
    principal: Principal,
    Path(target_user_id): Path<UserId>,
) -> Result<Response, HttpError> {
    let role = live_role(&state, &principal).await?;
    // Allow members to remove *themselves* (self-leave); for any
    // other target the caller must be admin or owner. Removing an
    // owner additionally requires the caller to be an owner — an
    // admin must not be able to evict a peer or superior. The
    // last-owner guard fires inside the store.
    if target_user_id != principal.user_id {
        require_admin(role)?;
        let target_role = state
            .users
            .membership(target_user_id, principal.active_org_id)
            .await?
            .ok_or(HttpError::NotFound)?;
        if matches!(target_role, Role::Owner) && !matches!(role, Role::Owner) {
            return Err(HttpError::Forbidden("owner role required to remove owner"));
        }
    }
    state
        .orgs
        .remove_member(principal.active_org_id, target_user_id)
        .await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ─────────────────────────────────────────────────────────────────────
// POST /me/org/leave — self-leave shortcut. Identical effect to
// DELETE /me/org/members/{principal.user_id}, but matches the design
// surface where the General tab has a "Leave workspace" button that
// doesn't need to know the caller's user id.
// ─────────────────────────────────────────────────────────────────────

async fn leave_org(
    State(state): State<AppState>,
    principal: Principal,
) -> Result<Response, HttpError> {
    state
        .orgs
        .remove_member(principal.active_org_id, principal.user_id)
        .await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ─────────────────────────────────────────────────────────────────────
// POST /me/org/invites — batch invite.
// ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct InviteMembersRequest {
    emails: Vec<String>,
    role: Role,
}

#[derive(Debug, Serialize)]
struct IssuedInviteView {
    invite_id: InviteId,
    email: String,
    role: Role,
    /// Cleartext URL token. Returned **only at issuance** so the FE
    /// can render the copy-link affordance. The server hashes the
    /// token before storage; this is the only response that carries
    /// it.
    token: String,
    expires_at: chrono::DateTime<chrono::Utc>,
}

impl From<crate::orgs::IssuedInvite> for IssuedInviteView {
    fn from(i: crate::orgs::IssuedInvite) -> Self {
        Self {
            invite_id: i.invite_id,
            email: i.email.as_str().to_owned(),
            role: i.role,
            token: i.token.as_str().to_owned(),
            expires_at: i.expires_at,
        }
    }
}

async fn invite_members(
    State(state): State<AppState>,
    principal: Principal,
    Json(req): Json<InviteMembersRequest>,
) -> Result<Response, HttpError> {
    let role = live_role(&state, &principal).await?;
    require_admin(role)?;
    if matches!(req.role, Role::Owner) && !matches!(role, Role::Owner) {
        return Err(HttpError::Forbidden("owner role required to invite owners"));
    }
    if req.emails.len() > MAX_INVITE_BATCH {
        return Err(HttpError::Org(OrgError::InviteBatchTooLarge {
            max: MAX_INVITE_BATCH,
            got: req.emails.len(),
        }));
    }
    let emails: Vec<Email> = req
        .emails
        .iter()
        .map(|e| Email::try_from(e.as_str()))
        .collect::<Result<_, _>>()?;
    let now = state.clock.now_utc();
    let ttl = ChronoDuration::from_std(INVITE_TTL).map_err(|_| HttpError::Internal)?;
    let issued = state
        .orgs
        .create_invites(
            principal.active_org_id,
            &emails,
            req.role,
            principal.user_id,
            now,
            ttl,
        )
        .await?;

    // Best-effort outbound mail. Failures here are NOT errors — the
    // invite row is real and the link copy in the FE is the
    // user-facing share affordance until SMTP lands. The org name +
    // slug come from the same read used by the General tab so we
    // don't repeat the join here.
    let details = state.orgs.read_org(principal.active_org_id).await?;
    for inv in &issued {
        state
            .mailer
            .send_invite(crate::orgs::mailer::InviteMail {
                to: &inv.email,
                org_name: &details.name,
                org_slug: &details.slug,
                role: inv.role,
                token: &inv.token,
                web_base_url: state.web_base_url.as_deref(),
            })
            .await;
    }

    let view: Vec<IssuedInviteView> = issued.into_iter().map(Into::into).collect();
    Ok((StatusCode::CREATED, Json(view)).into_response())
}

// ─────────────────────────────────────────────────────────────────────
// POST /me/org/invites/{invite_id}/resend  – rotate + extend.
// ─────────────────────────────────────────────────────────────────────

async fn resend_invite(
    State(state): State<AppState>,
    principal: Principal,
    Path(invite_id): Path<InviteId>,
) -> Result<Response, HttpError> {
    let role = live_role(&state, &principal).await?;
    require_admin(role)?;
    let now = state.clock.now_utc();
    let ttl = ChronoDuration::from_std(INVITE_TTL).map_err(|_| HttpError::Internal)?;
    let issued = state
        .orgs
        .resend_invite(principal.active_org_id, invite_id, now, ttl)
        .await?;
    let details = state.orgs.read_org(principal.active_org_id).await?;
    state
        .mailer
        .send_invite(crate::orgs::mailer::InviteMail {
            to: &issued.email,
            org_name: &details.name,
            org_slug: &details.slug,
            role: issued.role,
            token: &issued.token,
            web_base_url: state.web_base_url.as_deref(),
        })
        .await;
    Ok(Json(IssuedInviteView::from(issued)).into_response())
}

// ─────────────────────────────────────────────────────────────────────
// DELETE /me/org/invites/{invite_id} — revoke pending.
// ─────────────────────────────────────────────────────────────────────

async fn revoke_invite(
    State(state): State<AppState>,
    principal: Principal,
    Path(invite_id): Path<InviteId>,
) -> Result<Response, HttpError> {
    let role = live_role(&state, &principal).await?;
    require_admin(role)?;
    state
        .orgs
        .revoke_invite(principal.active_org_id, invite_id)
        .await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}
