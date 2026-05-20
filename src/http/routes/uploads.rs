//! Image upload endpoints. Two surfaces today:
//!
//!   * `POST /api/uploads/avatar` — the signed-in user's own avatar.
//!   * `POST /api/uploads/mcp-catalog/:catalog_id` — owner/admin-only,
//!     org-scoped catalog entries (built-ins refuse — they're seeded
//!     via migration).
//!
//! Both routes go through one trust boundary in `assets::multipart` and
//! one storage seam in `assets::AssetStore`. The handlers themselves
//! only do auth/role gating, key derivation, and DB write-through.

use axum::Json;
use axum::Router;
use axum::extract::{DefaultBodyLimit, Multipart, Path, State};
use axum::routing::post;
use serde::Serialize;

use crate::assets::{AssetKind, AssetUrl, ObjectKey, extract_single_image_field};
use crate::auth::{AuthError, Principal, Role};
use crate::mcp::{McpCatalogId, McpError};

use super::super::error::HttpError;
use super::super::state::AppState;

/// JSON envelope returned by both upload endpoints.
#[derive(Debug, Serialize)]
struct UploadResponse {
    url: String,
}

pub(super) fn router() -> Router<AppState> {
    Router::new()
        .route("/uploads/avatar", post(upload_avatar))
        // Per-route body limit overrides the global
        // RequestBodyLimitLayer so an oversized payload is rejected at
        // the framework boundary before we allocate.
        .layer(DefaultBodyLimit::max(
            AssetKind::Avatar.max_bytes() + MULTIPART_OVERHEAD,
        ))
        .route(
            "/uploads/mcp-catalog/{catalog_id}",
            post(upload_mcp_catalog_icon),
        )
        .layer(DefaultBodyLimit::max(
            AssetKind::McpCatalogIcon.max_bytes() + MULTIPART_OVERHEAD,
        ))
}

/// Headroom on top of the per-kind byte cap for the multipart envelope
/// (boundary line, Content-Disposition, Content-Type headers).
const MULTIPART_OVERHEAD: usize = 4 * 1024;

async fn upload_avatar(
    State(state): State<AppState>,
    principal: Principal,
    multipart: Multipart,
) -> Result<Json<UploadResponse>, HttpError> {
    let assets = state
        .assets
        .as_ref()
        .ok_or(HttpError::AssetStorageMissing)?;

    let img = extract_single_image_field(multipart, AssetKind::Avatar).await?;

    let stable_id = principal.user_id.to_string();
    let key = ObjectKey::derive(AssetKind::Avatar, &stable_id, img.content_type)
        .map_err(crate::assets::AssetError::from)?;
    let url: AssetUrl = assets.put(key, img.bytes, img.content_type).await?;

    let now = state.clock.now_utc();
    state
        .users
        .set_avatar_url(principal.user_id, Some(url.as_str()), now)
        .await?;

    Ok(Json(UploadResponse {
        url: url.as_str().to_owned(),
    }))
}

async fn upload_mcp_catalog_icon(
    State(state): State<AppState>,
    principal: Principal,
    Path(catalog_id_raw): Path<String>,
    multipart: Multipart,
) -> Result<Json<UploadResponse>, HttpError> {
    let assets = state
        .assets
        .as_ref()
        .ok_or(HttpError::AssetStorageMissing)?;

    let catalog_id = McpCatalogId::try_from(catalog_id_raw.as_str()).map_err(HttpError::Parse)?;

    // Re-read membership: principal.role is JWT-minted and may be stale
    // (mirrors set_org_language's defence in me.rs). Owner/admin only;
    // members can wire connections but not curate the catalog.
    let role = state
        .users
        .membership(principal.user_id, principal.active_org_id)
        .await?
        .ok_or(AuthError::NotMember(principal.active_org_id))?;
    match role {
        Role::Owner | Role::Admin => {}
        Role::Member => return Err(HttpError::Forbidden("owner or admin role required")),
    }

    // The catalog row must exist AND be org-scoped — built-ins
    // (org_id IS NULL) are immutable. `set_icon_url` already refuses
    // global rows because its UPDATE matches on `org_id = $2`; we
    // surface a clearer error here by re-reading first.
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

    let img = extract_single_image_field(multipart, AssetKind::McpCatalogIcon).await?;

    let key = ObjectKey::derive(
        AssetKind::McpCatalogIcon,
        catalog_id.as_str(),
        img.content_type,
    )
    .map_err(crate::assets::AssetError::from)?;
    let url: AssetUrl = assets.put(key, img.bytes, img.content_type).await?;

    let now = state.clock.now_utc();
    state
        .mcp_catalog
        .set_icon_url(principal.active_org_id, &catalog_id, url.as_str(), now)
        .await?;

    Ok(Json(UploadResponse {
        url: url.as_str().to_owned(),
    }))
}
