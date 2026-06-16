//! Admin bot-registration routes — `/api/discord/apps`.
//!
//! The only inbound HTTP surface the Discord adapter exposes (events ride the
//! Gateway, not a webhook; interactions ride the Gateway too). An org admin
//! registers a self-built app (`application_id` + `bot_token` + the agent it
//! speaks as); the token is DB-encrypted by the store. Merged into the private
//! (auth-gated) router, so `Principal` is always present and RLS scopes every
//! write to the caller's org.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::agents::AgentId;
use crate::auth::{AuthError, Caller, Principal, Role};
use crate::http::{AppState, HttpError};

use super::app_store::{DiscordApp, DiscordConnectTarget, NewDiscordApp};
use super::error::DiscordError;
use super::types::{ApplicationId, BotToken};

/// The private (auth-gated) Discord admin router. Paths are registered WITHOUT
/// the `/api` prefix — the composition root nests this under `/api`, so they
/// resolve to `/api/discord/apps` externally.
pub fn private_router() -> Router<AppState> {
    Router::new()
        .route("/discord/apps", get(list_apps).post(register_app))
        .route("/discord/apps/{application_id}", delete(delete_app))
}

#[derive(Deserialize)]
struct RegisterRequest {
    application_id: String,
    bot_token: String,
    agent_id: AgentId,
}

// The field names mirror the Discord/JSON wire contract (renaming would break
// the admin API), so the shared `_id` postfix is intentional.
#[derive(Serialize)]
#[allow(clippy::struct_field_names)]
struct AppView {
    application_id: String,
    agent_id: AgentId,
    bot_user_id: Option<String>,
}

impl From<DiscordApp> for AppView {
    fn from(a: DiscordApp) -> Self {
        Self {
            application_id: a.application_id.as_str().to_owned(),
            agent_id: a.agent_id,
            bot_user_id: a.bot_user_id.map(|b| b.as_str().to_owned()),
        }
    }
}

/// Re-read the caller's live role and require owner/admin (a stale JWT can't
/// outlive a demotion). Registering/removing bot credentials is privileged.
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
    let discord = state.discord.as_ref().ok_or(HttpError::NotFound)?;
    require_admin(&state, &principal).await?;
    let application_id = ApplicationId::try_from(body.application_id).map_err(HttpError::Parse)?;
    let bot_token = BotToken::try_from(body.bot_token).map_err(HttpError::Parse)?;
    let caller = Caller::new(principal.user_id, principal.active_org_id);
    discord
        .apps
        .register(
            &caller,
            NewDiscordApp {
                application_id: application_id.clone(),
                agent_id: body.agent_id,
                bot_token,
            },
        )
        .await
        .map_err(map_err)?;
    // Hot-connect the just-registered bot so it comes online without a restart.
    discord
        .ws_manager
        .connect(DiscordConnectTarget {
            org_id: principal.active_org_id,
            application_id,
        })
        .await;
    Ok(StatusCode::CREATED)
}

async fn list_apps(
    State(state): State<AppState>,
    principal: Principal,
) -> Result<Json<Vec<AppView>>, HttpError> {
    let discord = state.discord.as_ref().ok_or(HttpError::NotFound)?;
    require_admin(&state, &principal).await?;
    let caller = Caller::new(principal.user_id, principal.active_org_id);
    let apps = discord.apps.list(&caller).await.map_err(map_err)?;
    Ok(Json(apps.into_iter().map(AppView::from).collect()))
}

async fn delete_app(
    State(state): State<AppState>,
    principal: Principal,
    Path(application_id): Path<String>,
) -> Result<StatusCode, HttpError> {
    let discord = state.discord.as_ref().ok_or(HttpError::NotFound)?;
    require_admin(&state, &principal).await?;
    let application_id = ApplicationId::try_from(application_id).map_err(HttpError::Parse)?;
    let caller = Caller::new(principal.user_id, principal.active_org_id);
    discord
        .apps
        .delete(&caller, &application_id)
        .await
        .map_err(map_err)?;
    // Tear down the bot's live connection so its reconnect task doesn't linger.
    discord.ws_manager.disconnect(&application_id).await;
    Ok(StatusCode::NO_CONTENT)
}

/// Map a [`DiscordError`] to its HTTP shape: unknown app → 404, a boundary parse
/// → 400, everything else → 500.
fn map_err(e: DiscordError) -> HttpError {
    match e {
        DiscordError::UnknownApp(_) => HttpError::NotFound,
        DiscordError::Parse(pe) => HttpError::Parse(pe),
        other => {
            tracing::error!(error = ?other, event = "discord.apps.store_error");
            HttpError::Internal
        }
    }
}
