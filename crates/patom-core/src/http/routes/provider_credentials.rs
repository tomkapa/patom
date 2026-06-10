//! BYO provider-credential routes (#141): per-org LLM provider API keys.
//!
//! `GET/PUT/DELETE /me/org/provider-credentials[/{provider}]` plus a
//! `POST .../{provider}/validate` that tests a candidate key against the live
//! provider before (or after) it is saved. Mirrors the MCP credential routes
//! (`routes/mcp.rs`): keys are never returned in plaintext — only a masked
//! suffix + status — and every mutation fires the overlay refresh so a saved
//! key routes within one tick.
//!
//! Mutations are owner/admin-gated (managing provider billing keys is an admin
//! action); the masked read is open to any member so the settings UI can render
//! status. The validate endpoint reuses the per-user `mcp_test_rate` budget —
//! both "test an external credential" actions share one rolling-minute limit.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::auth::{AuthError, Principal, Role};
use crate::provider::{
    ChatMessage, ChatRequest, Model, ProviderApiKey, ProviderBaseUrl, ProviderCredentialWrite,
    ProviderError, ProviderId, UserContent, build_byo_client,
};
use crate::types::MaxOutputTokens;

use super::super::error::HttpError;
use super::super::state::AppState;

pub(super) fn router() -> Router<AppState> {
    Router::new()
        .route("/me/org/provider-credentials", get(list_credentials))
        .route(
            "/me/org/provider-credentials/{provider}",
            axum::routing::put(put_credential).delete(delete_credential),
        )
        .route(
            "/me/org/provider-credentials/{provider}/validate",
            post(validate_credential),
        )
}

// ── role gate ─────────────────────────────────────────────────────────

/// Re-read the caller's live role on the active org (a stale JWT cannot outlive
/// a demotion) and require owner/admin. Mirrors `routes/org.rs`.
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

/// Parse the `{provider}` path segment against the closed provider allowlist
/// (CLAUDE.md §10: dynamic identifiers match a domain enum, never interpolate).
fn parse_provider(raw: &str) -> Result<ProviderId, HttpError> {
    ProviderId::try_from(raw).map_err(HttpError::from)
}

// ── GET: masked list ──────────────────────────────────────────────────

/// One provider's BYO status. Never carries plaintext — only a masked suffix.
#[derive(Debug, Serialize)]
struct ProviderCredentialView {
    /// `snake_case` provider id (`"anthropic"`, …).
    provider: &'static str,
    /// `"active"` when a key is stored (routes immediately, #141), else
    /// `"not_set"`. Transient `"invalid"` is surfaced only by the validate
    /// endpoint's response, never persisted.
    status: &'static str,
    /// Masked key suffix (e.g. `••••••••abcd`); `None` when not set.
    masked_key: Option<String>,
    /// Non-secret endpoint override, if configured.
    base_url: Option<String>,
    /// Last successful live validation, if ever.
    last_validated_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[tracing::instrument(name = "provider_credentials.list", skip_all, fields(patom.org.id = %principal.active_org_id))]
async fn list_credentials(
    State(state): State<AppState>,
    principal: Principal,
) -> Result<Json<Vec<ProviderCredentialView>>, HttpError> {
    let rows = state
        .provider_credentials
        .list_for_org(principal.active_org_id)
        .await?;
    // Project every known provider so the UI renders a full grid; a stored row
    // flips its tile to "active".
    let out = ProviderId::ALL
        .iter()
        .map(|&p| {
            let row = rows.iter().find(|r| r.provider == p);
            ProviderCredentialView {
                provider: p.as_str(),
                status: if row.is_some() { "active" } else { "not_set" },
                masked_key: row.map(|r| r.api_key.masked()),
                base_url: row.and_then(|r| r.base_url.as_ref().map(|u| u.as_str().to_owned())),
                last_validated_at: row.and_then(|r| r.last_validated_at),
            }
        })
        .collect();
    Ok(Json(out))
}

// ── PUT: add / rotate ─────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct PutCredentialInput {
    /// The provider API key. Parsed into [`ProviderApiKey`] (length-capped,
    /// non-empty) at the boundary.
    api_key: String,
    /// Optional endpoint override (proxy / compatible gateway).
    #[serde(default)]
    base_url: Option<String>,
    /// Per-org default model, honored **only** when this is the org's first
    /// stored key (chosen when BYO is first enabled, #141).
    #[serde(default)]
    default_model: Option<String>,
}

#[tracing::instrument(name = "provider_credentials.put", skip_all, fields(patom.org.id = %principal.active_org_id, patom.provider = %provider_raw))]
async fn put_credential(
    State(state): State<AppState>,
    principal: Principal,
    Path(provider_raw): Path<String>,
    Json(input): Json<PutCredentialInput>,
) -> Result<StatusCode, HttpError> {
    require_admin(&state, &principal).await?;
    let provider = parse_provider(&provider_raw)?;
    let api_key = ProviderApiKey::try_from(input.api_key)?;
    let base_url = input.base_url.map(ProviderBaseUrl::try_from).transpose()?;
    let org = principal.active_org_id;

    // First-key default model: only set when the org has no stored keys yet.
    if let Some(raw_model) = input.default_model {
        let model = Model::try_from(raw_model.as_str())
            .map_err(|e| HttpError::BadRequest(e.to_string()))?;
        let is_first = state
            .provider_credentials
            .list_for_org(org)
            .await?
            .is_empty();
        if is_first {
            state
                .provider_credentials
                .set_default_model(org, model)
                .await?;
        }
    }

    state
        .provider_credentials
        .upsert(ProviderCredentialWrite {
            org_id: org,
            provider,
            api_key,
            base_url,
        })
        .await?;
    // Activate the new key on the next turn (#141).
    state.provider_refresh.request();
    Ok(StatusCode::NO_CONTENT)
}

// ── DELETE ────────────────────────────────────────────────────────────

#[tracing::instrument(name = "provider_credentials.delete", skip_all, fields(patom.org.id = %principal.active_org_id, patom.provider = %provider_raw))]
async fn delete_credential(
    State(state): State<AppState>,
    principal: Principal,
    Path(provider_raw): Path<String>,
) -> Result<StatusCode, HttpError> {
    require_admin(&state, &principal).await?;
    let provider = parse_provider(&provider_raw)?;
    state
        .provider_credentials
        .delete(principal.active_org_id, provider)
        .await?;
    state.provider_refresh.request();
    Ok(StatusCode::NO_CONTENT)
}

// ── POST validate ─────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ValidateInput {
    api_key: String,
    #[serde(default)]
    base_url: Option<String>,
}

/// Validation outcome. A single discriminant so the FE renders pass/fail
/// without parsing prose; `error` carries a fixed, key-free reason.
#[derive(Debug, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
enum ValidateResponse {
    /// The provider accepted the key.
    Ok,
    /// The provider rejected the key as unauthorized.
    Invalid { error: &'static str },
    /// Could not reach / get a clean answer from the provider — try again.
    Error { error: &'static str },
}

#[tracing::instrument(name = "provider_credentials.validate", skip_all, fields(patom.org.id = %principal.active_org_id, patom.provider = %provider_raw))]
async fn validate_credential(
    State(state): State<AppState>,
    principal: Principal,
    Path(provider_raw): Path<String>,
    Json(input): Json<ValidateInput>,
) -> Result<Json<ValidateResponse>, HttpError> {
    require_admin(&state, &principal).await?;
    // Reuse the MCP test-connect per-user budget before opening any outbound
    // connection (CLAUDE.md §5).
    if !state.mcp_test_rate.try_admit(principal.user_id) {
        return Err(HttpError::TooManyRequests);
    }
    let provider = parse_provider(&provider_raw)?;
    let api_key = ProviderApiKey::try_from(input.api_key)?;
    let base_url = input.base_url.map(ProviderBaseUrl::try_from).transpose()?;

    let client = match build_byo_client(provider, &api_key, base_url.as_ref()) {
        Ok(c) => c,
        // Construction only fails on a malformed key the parser already
        // accepted — treat as invalid rather than 500.
        Err(_) => {
            return Ok(Json(ValidateResponse::Invalid {
                error: "key rejected by provider client",
            }));
        }
    };

    // Smallest possible probe: one user turn, 16-token ceiling.
    let model = Model::all()
        .find(|m| m.provider() == provider)
        .expect("invariant: every ProviderId has at least one catalog model");
    let request = ChatRequest {
        model,
        system: Arc::from(""),
        messages: vec![ChatMessage::User(vec![UserContent::Text(
            "ping".to_owned(),
        )])],
        tools: Arc::from(Vec::new()),
        max_output_tokens: MaxOutputTokens::try_from(16).expect("16 within cap"),
    };

    let response = match client.send(request).await {
        Ok(_) => {
            // A live-validated key: stamp the row if one exists (validate may
            // run after save). No-op pre-save.
            let now = state.clock.now_utc();
            let _ = state
                .provider_credentials
                .mark_validated(principal.active_org_id, provider, now)
                .await;
            ValidateResponse::Ok
        }
        Err(e) => {
            // Validation-failure observability (#141, §2): low-cardinality
            // provider + reason, never the key or the raw provider message.
            let reason = match &e {
                ProviderError::Unauthorized => "unauthorized",
                ProviderError::InvalidRequest(_) => "invalid_request",
                ProviderError::RateLimited => "rate_limited",
                _ => "transport",
            };
            tracing::info!(
                event = "provider.credential.validate_failed",
                patom.provider = provider.as_str(),
                patom.validate.reason = reason,
            );
            match e {
                ProviderError::Unauthorized => ValidateResponse::Invalid {
                    error: "provider rejected the key",
                },
                ProviderError::InvalidRequest(_) => ValidateResponse::Invalid {
                    error: "provider rejected the request",
                },
                _ => ValidateResponse::Error {
                    error: "could not reach the provider",
                },
            }
        }
    };
    Ok(Json(response))
}
