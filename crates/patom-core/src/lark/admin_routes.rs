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
use crate::auth::{Caller, Principal};
use crate::http::{AppState, HttpError};

use super::app_store::{LarkApp, LarkConnectTarget, NewLarkApp};
use super::error::LarkError;
use super::types::{LarkAppId, LarkAppSecret};

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

async fn register_app(
    State(state): State<AppState>,
    principal: Principal,
    Json(body): Json<RegisterRequest>,
) -> Result<StatusCode, HttpError> {
    let lark = state.lark.as_ref().ok_or(HttpError::NotFound)?;
    let app_id = LarkAppId::try_from(body.app_id).map_err(HttpError::Parse)?;
    let app_secret = LarkAppSecret::try_from(body.app_secret).map_err(HttpError::Parse)?;
    let caller = Caller::new(principal.user_id, principal.active_org_id);
    lark.apps
        .register(
            &caller,
            NewLarkApp {
                app_id: app_id.clone(),
                agent_id: body.agent_id,
                app_secret,
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
    let app_id = LarkAppId::try_from(app_id).map_err(HttpError::Parse)?;
    let caller = Caller::new(principal.user_id, principal.active_org_id);
    lark.apps.delete(&caller, &app_id).await.map_err(map_err)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Map a [`LarkError`] to its HTTP shape: unknown app → 404, a boundary parse →
/// 400, everything else → 500.
fn map_err(e: LarkError) -> HttpError {
    match e {
        LarkError::UnknownApp(_) => HttpError::NotFound,
        LarkError::Parse(pe) => HttpError::Parse(pe),
        _ => HttpError::Internal,
    }
}
