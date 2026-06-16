//! Image upload endpoints:
//!
//!   * `POST /api/uploads/avatar` — the signed-in user's own avatar.
//!   * `POST /api/uploads/workspace-avatar` — owner/admin-only,
//!     active-org workspace avatar.
//!   * `POST /api/uploads/mcp-catalog/:catalog_id` — owner/admin-only,
//!     org-scoped catalog entries (built-ins refuse — they're seeded
//!     via migration).
//!   * `POST /api/uploads/agent-avatar/:agent_id` — per-agent avatar,
//!     scoped to an agent visible to the caller's org (issue #43). The
//!     object is stored and the validated URL returned; persistence to
//!     `agents.avatar_url` happens on the next `PUT /agents/{id}`.

use axum::Json;
use axum::Router;
use axum::extract::{DefaultBodyLimit, Multipart, Path, State};
use axum::routing::post;
use serde::Serialize;
use uuid::Uuid;

use crate::agents::AgentId;
use crate::assets::limits::MAX_ATTACHMENT_FILE_BYTES;
use crate::assets::{
    AssetKind, AssetUrl, ObjectKey, SharedAssetStore, extract_attachment_field,
    extract_single_image_field,
};
use crate::auth::{AuthError, Principal, Role, VisibilityTable, visible_to};
use crate::mcp::{McpCatalogId, McpError};
use crate::provider::FileName;
use crate::types::AvatarUrl;

use super::super::error::HttpError;
use super::super::state::AppState;

/// JSON envelope returned by the avatar/icon upload endpoints.
#[derive(Debug, Serialize)]
struct UploadResponse {
    url: String,
}

/// JSON envelope returned by `POST /uploads/attachment`. Mirrors the
/// `RawAttachment` shape `POST /prompts` consumes, so the FE can forward it
/// verbatim (issue #187).
#[derive(Debug, Serialize)]
struct AttachmentUploadResponse {
    url: String,
    mime: String,
    filename: String,
    size: u64,
}

pub(super) fn router() -> Router<AppState> {
    // Body limits attach to each `MethodRouter` so the cap is scoped to
    // its own route; `Router::layer` would stack and apply the second
    // cap to the first route too.
    Router::new()
        .route(
            "/uploads/avatar",
            post(upload_avatar).layer(DefaultBodyLimit::max(
                AssetKind::Avatar.max_bytes() + MULTIPART_OVERHEAD,
            )),
        )
        .route(
            "/uploads/workspace-avatar",
            post(upload_workspace_avatar).layer(DefaultBodyLimit::max(
                AssetKind::WorkspaceAvatar.max_bytes() + MULTIPART_OVERHEAD,
            )),
        )
        .route(
            "/uploads/mcp-catalog/{catalog_id}",
            post(upload_mcp_catalog_icon).layer(DefaultBodyLimit::max(
                AssetKind::McpCatalogIcon.max_bytes() + MULTIPART_OVERHEAD,
            )),
        )
        .route(
            "/uploads/agent-avatar/{agent_id}",
            post(upload_agent_avatar).layer(DefaultBodyLimit::max(
                AssetKind::AgentAvatar.max_bytes() + MULTIPART_OVERHEAD,
            )),
        )
        .route(
            "/uploads/attachment",
            post(upload_attachment).layer(DefaultBodyLimit::max(
                MAX_ATTACHMENT_FILE_BYTES + MULTIPART_OVERHEAD,
            )),
        )
}

/// Headroom on top of the per-kind byte cap for the multipart envelope
/// (boundary line, Content-Disposition, Content-Type headers).
const MULTIPART_OVERHEAD: usize = 4 * 1024;

fn assets_or_503(state: &AppState) -> Result<&SharedAssetStore, HttpError> {
    state.assets.as_ref().ok_or(HttpError::AssetStorageMissing)
}

/// Re-read membership and require owner/admin. `principal.role` is
/// JWT-minted and lags a recent demotion; this mirrors the live-role
/// pattern in `routes/org.rs` and `set_org_language` in `routes/me.rs`.
async fn require_admin(state: &AppState, principal: &Principal) -> Result<(), HttpError> {
    let role = state
        .users
        .membership(principal.user_id, principal.active_org_id)
        .await?
        .ok_or(AuthError::NotMember(principal.active_org_id))?;
    match role {
        Role::Owner | Role::Admin => Ok(()),
        Role::Member => Err(HttpError::Forbidden("owner or admin role required")),
    }
}

/// Extract one image field, derive its kinded object key, and put it in
/// the bucket. Returns the persisted URL the caller writes to its
/// domain row.
async fn extract_and_store(
    assets: &SharedAssetStore,
    kind: AssetKind,
    stable_id: &str,
    multipart: Multipart,
) -> Result<AssetUrl, HttpError> {
    let img = extract_single_image_field(multipart, kind).await?;
    let key = ObjectKey::derive(kind, stable_id, img.content_type)
        .map_err(crate::assets::AssetError::from)?;
    // The store takes the broader `AssetContentType`; an avatar's
    // `ImageContentType` widens losslessly.
    let url = assets.put(key, img.bytes, img.content_type.into()).await?;
    Ok(url)
}

async fn upload_avatar(
    State(state): State<AppState>,
    principal: Principal,
    multipart: Multipart,
) -> Result<Json<UploadResponse>, HttpError> {
    let assets = assets_or_503(&state)?;
    let stable_id = principal.user_id.to_string();
    let url = extract_and_store(assets, AssetKind::Avatar, &stable_id, multipart).await?;
    // The store URL is already an `AssetUrl`; re-parse it through
    // `AvatarUrl` so the persisted column and the `/me` read path share
    // one typed invariant (CLAUDE.md §1). Identical validation, so this
    // only fails on a genuinely malformed store URL.
    let avatar = AvatarUrl::try_from(url.as_str()).map_err(HttpError::Parse)?;
    let now = state.clock.now_utc();
    state
        .users
        .set_avatar_url(principal.user_id, Some(&avatar), now)
        .await?;
    Ok(Json(UploadResponse {
        url: avatar.as_str().to_owned(),
    }))
}

async fn upload_workspace_avatar(
    State(state): State<AppState>,
    principal: Principal,
    multipart: Multipart,
) -> Result<Json<UploadResponse>, HttpError> {
    let assets = assets_or_503(&state)?;
    require_admin(&state, &principal).await?;
    let stable_id = principal.active_org_id.to_string();
    let url = extract_and_store(assets, AssetKind::WorkspaceAvatar, &stable_id, multipart).await?;
    let avatar = AvatarUrl::try_from(url.as_str()).map_err(HttpError::Parse)?;
    let now = state.clock.now_utc();
    state
        .orgs
        .set_avatar_url(principal.active_org_id, Some(&avatar), now)
        .await?;
    Ok(Json(UploadResponse {
        url: avatar.as_str().to_owned(),
    }))
}

async fn upload_mcp_catalog_icon(
    State(state): State<AppState>,
    principal: Principal,
    Path(catalog_id_raw): Path<String>,
    multipart: Multipart,
) -> Result<Json<UploadResponse>, HttpError> {
    let assets = assets_or_503(&state)?;
    let catalog_id = McpCatalogId::try_from(catalog_id_raw.as_str()).map_err(HttpError::Parse)?;
    require_admin(&state, &principal).await?;

    // Built-ins (org_id IS NULL) are immutable. `set_icon_url` already
    // refuses global rows via `org_id = $2`; re-reading here surfaces a
    // clearer error than a silent zero-rows-affected.
    let entry = state
        .mcp_catalog
        .get_for_org(principal.active_org_id, &catalog_id)
        .await?
        .ok_or_else(|| HttpError::Mcp(McpError::CatalogIdUnknown(catalog_id.clone())))?;
    if entry.org_id.is_none() {
        return Err(HttpError::Forbidden(
            "built-in catalog icons are not editable",
        ));
    }

    let url = extract_and_store(
        assets,
        AssetKind::McpCatalogIcon,
        catalog_id.as_str(),
        multipart,
    )
    .await?;
    let now = state.clock.now_utc();
    state
        .mcp_catalog
        .set_icon_url(principal.active_org_id, &catalog_id, url.as_str(), now)
        .await?;
    Ok(Json(UploadResponse {
        url: url.as_str().to_owned(),
    }))
}

async fn upload_agent_avatar(
    State(state): State<AppState>,
    principal: Principal,
    Path(agent_id): Path<Uuid>,
    multipart: Multipart,
) -> Result<Json<UploadResponse>, HttpError> {
    let assets = assets_or_503(&state)?;
    let agent_id = AgentId::from(agent_id);
    // Tenant gate: an agent the caller's org can't see (cross-org or
    // unknown) 404s without leaking existence, and — crucially — without
    // letting a caller write an object under another org's agent key.
    if !visible_to(
        &state.pool,
        &principal,
        VisibilityTable::Agents,
        agent_id.as_uuid(),
    )
    .await?
    {
        return Err(HttpError::NotFound);
    }
    let stable_id = agent_id.as_uuid().to_string();
    let url = extract_and_store(assets, AssetKind::AgentAvatar, &stable_id, multipart).await?;
    // Validate through the shared `AvatarUrl` so the returned value
    // matches what `PUT /agents/{id}` will accept; the form persists it
    // on the next save (no DB write here — see the module header).
    let avatar = AvatarUrl::try_from(url.as_str()).map_err(HttpError::Parse)?;
    Ok(Json(UploadResponse {
        url: avatar.as_str().to_owned(),
    }))
}

/// Upload one message attachment (image / PDF / Office). Any signed-in user
/// may attach to their own messages — no admin gate — and the object lands
/// under a fresh, immutable `attachments/{uuid}.{ext}` key (unlike avatars,
/// which overwrite a deterministic per-subject key). Returns the reference
/// `POST /prompts` will consume (issue #187).
async fn upload_attachment(
    State(state): State<AppState>,
    _principal: Principal,
    multipart: Multipart,
) -> Result<Json<AttachmentUploadResponse>, HttpError> {
    let assets = assets_or_503(&state)?;
    let att = extract_attachment_field(multipart).await?;
    let size = u64::try_from(att.bytes.len()).unwrap_or(u64::MAX);
    // Validate the filename through the same typed invariant `/prompts` uses,
    // so a name it would reject fails fast here instead of at submit.
    let filename = FileName::try_from(att.filename.as_str()).map_err(HttpError::Parse)?;
    let key_raw = format!(
        "attachments/{}.{}",
        Uuid::new_v4(),
        att.content_type.extension()
    );
    let key = ObjectKey::try_from(key_raw.as_str()).map_err(crate::assets::AssetError::from)?;
    let mime = att.content_type.as_mime().to_owned();
    let url = assets.put(key, att.bytes, att.content_type).await?;
    Ok(Json(AttachmentUploadResponse {
        url: url.as_str().to_owned(),
        mime,
        filename: filename.as_str().to_owned(),
        size,
    }))
}
