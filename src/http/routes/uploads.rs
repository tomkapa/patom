//! Image upload endpoints:
//!
//!   * `POST /api/uploads/avatar` — the signed-in user's own avatar.
//!   * `POST /api/uploads/workspace-avatar` — owner/admin-only,
//!     active-org workspace avatar.
//!   * `POST /api/uploads/mcp-catalog/:catalog_id` — owner/admin-only,
//!     org-scoped catalog entries (built-ins refuse — they're seeded
//!     via migration).

use axum::Json;
use axum::Router;
use axum::extract::{DefaultBodyLimit, Multipart, Path, State};
use axum::routing::post;
use serde::Serialize;

use crate::assets::{AssetKind, AssetUrl, ObjectKey, SharedAssetStore, extract_single_image_field};
use crate::auth::{AuthError, Principal, Role};
use crate::mcp::{McpCatalogId, McpError};
use crate::types::AvatarUrl;

use super::super::error::HttpError;
use super::super::state::AppState;

/// JSON envelope returned by both upload endpoints.
#[derive(Debug, Serialize)]
struct UploadResponse {
    url: String,
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
    let url = assets.put(key, img.bytes, img.content_type).await?;
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
