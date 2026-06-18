//! Admin bot-registration routes — `/api/lark/apps`.
//!
//! The only HTTP surface the Lark adapter exposes (inbound events ride the WS,
//! not a webhook). An org member registers a self-built app (`app_id` +
//! `app_secret` + the agent it speaks as); the secret is DB-encrypted by the
//! store. Merged into the private (auth-gated) router, so `Principal` is always
//! present and RLS scopes every write to the caller's org.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::agents::AgentId;
use crate::auth::{AuthError, Caller, Principal, Role};
use crate::http::{AppState, HttpError};

use super::app_store::{LarkApp, LarkConnectTarget, NewLarkApp};
use super::error::LarkError;
use super::types::{LarkAppId, LarkAppSecret, LarkEncryptKey, LarkVerificationToken};

/// The private (auth-gated) Lark admin router.
///
/// Paths are registered WITHOUT the `/api` prefix — the composition root nests
/// this whole router under `/api` (`http::routes::router` → `.nest("/api", …)`),
/// so these resolve to `/api/lark/apps` externally (mirrors `slack::oauth`,
/// which registers `/slack/install`).
pub fn private_router() -> Router<AppState> {
    Router::new()
        .route("/lark/apps", get(list_apps).post(register_app))
        .route("/lark/apps/{app_id}", delete(delete_app))
}

#[derive(Deserialize)]
struct RegisterRequest {
    app_id: String,
    app_secret: String,
    agent_id: AgentId,
    /// Card-callback Encrypt Key (#214). Provide together with
    /// `card_verification_token` to enable the `/lark/card-actions` route for
    /// this app; omit both for a long-connection-only bot.
    #[serde(default)]
    card_encrypt_key: Option<String>,
    #[serde(default)]
    card_verification_token: Option<String>,
}

#[derive(Serialize)]
struct AppView {
    app_id: String,
    agent_id: AgentId,
    tenant_key: Option<String>,
}

impl From<LarkApp> for AppView {
    fn from(a: LarkApp) -> Self {
        Self {
            app_id: a.app_id.as_str().to_owned(),
            agent_id: a.agent_id,
            tenant_key: a.tenant_key.map(|t| t.as_str().to_owned()),
        }
    }
}

/// Re-read the caller's live role on the active org (a stale JWT can't outlive a
/// demotion) and require owner/admin: registering/removing bot credentials is a
/// privileged operation, not something any member may do. Mirrors
/// `routes/provider_credentials::require_admin`.
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

async fn register_app(
    State(state): State<AppState>,
    principal: Principal,
    Json(body): Json<RegisterRequest>,
) -> Result<StatusCode, HttpError> {
    let lark = state.lark.as_ref().ok_or(HttpError::NotFound)?;
    require_admin(&state, &principal).await?;
    let app_id = LarkAppId::try_from(body.app_id).map_err(HttpError::Parse)?;
    let app_secret = LarkAppSecret::try_from(body.app_secret).map_err(HttpError::Parse)?;
    // Card credentials are all-or-nothing: a single one is a misconfiguration
    // that would leave the card-action route unverifiable.
    if body.card_encrypt_key.is_some() != body.card_verification_token.is_some() {
        return Err(HttpError::BadRequest(
            "card_encrypt_key and card_verification_token must be set together".to_owned(),
        ));
    }
    let card_encrypt_key = body
        .card_encrypt_key
        .map(LarkEncryptKey::try_from)
        .transpose()
        .map_err(HttpError::Parse)?;
    let card_verification_token = body
        .card_verification_token
        .map(LarkVerificationToken::try_from)
        .transpose()
        .map_err(HttpError::Parse)?;
    let caller = Caller::new(principal.user_id, principal.active_org_id);
    lark.apps
        .register(
            &caller,
            NewLarkApp {
                app_id: app_id.clone(),
                agent_id: body.agent_id,
                app_secret,
                card_encrypt_key,
                card_verification_token,
            },
        )
        .await
        .map_err(map_err)?;
    // Hot-connect the just-registered bot so it comes online without a restart.
    lark.ws_manager
        .connect(LarkConnectTarget {
            org_id: principal.active_org_id,
            app_id,
        })
        .await;
    Ok(StatusCode::CREATED)
}

async fn list_apps(
    State(state): State<AppState>,
    principal: Principal,
) -> Result<Json<Vec<AppView>>, HttpError> {
    let lark = state.lark.as_ref().ok_or(HttpError::NotFound)?;
    require_admin(&state, &principal).await?;
    let caller = Caller::new(principal.user_id, principal.active_org_id);
    let apps = lark.apps.list(&caller).await.map_err(map_err)?;
    Ok(Json(apps.into_iter().map(AppView::from).collect()))
}

async fn delete_app(
    State(state): State<AppState>,
    principal: Principal,
    Path(app_id): Path<String>,
) -> Result<StatusCode, HttpError> {
    let lark = state.lark.as_ref().ok_or(HttpError::NotFound)?;
    require_admin(&state, &principal).await?;
    let app_id = LarkAppId::try_from(app_id).map_err(HttpError::Parse)?;
    let caller = Caller::new(principal.user_id, principal.active_org_id);
    lark.apps.delete(&caller, &app_id).await.map_err(map_err)?;
    // Tear down the bot's live long-connection so its reconnect task doesn't
    // linger until LARK_RECONNECT_MAX after the credentials are gone.
    lark.ws_manager.disconnect(&app_id).await;
    Ok(StatusCode::NO_CONTENT)
}

/// Map a [`LarkError`] to its HTTP shape: unknown app → 404, a boundary parse →
/// 400, everything else → 500.
fn map_err(e: LarkError) -> HttpError {
    match e {
        LarkError::UnknownApp(_) => HttpError::NotFound,
        LarkError::Parse(pe) => HttpError::Parse(pe),
        other => {
            tracing::error!(error = ?other, event = "lark.apps.store_error");
            HttpError::Internal
        }
    }
}
