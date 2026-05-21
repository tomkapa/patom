//! CRUD endpoints for the MCP server registry.
//!
//! `POST /mcp-servers`               — create
//! `GET  /mcp-servers`               — list
//! `GET  /mcp-servers/{id}`          — read one
//! `PUT  /mcp-servers/{id}`          — update
//! `DELETE /mcp-servers/{id}`        — delete
//! `POST /mcp-servers/test-connect`  — validate a candidate config without persisting
//!
//! Every mutating handler signals the long-running MCP refresh coordinator (via the
//! cheap clone-able [`McpRefreshTrigger`]) so the registered tools become callable on
//! the next prompt without a restart. The CRUD response itself does not wait on the
//! refresh — operator visibility comes from the `last_seen_at`/`last_error`/
//! `discovered_tools` columns surfaced on the next read.

use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Redirect;
use axum::routing::{delete, get, post, put};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::agents::AgentId;
use crate::auth::{AuthError, Principal, VisibilityTable, visible_to};
use crate::mcp::oauth::{
    ClientProvenance, NewOAuthClient, OAuthClientId, OAuthError, PendingAuthorization,
    TokenAuthMethod, build_authorize_url, discover_authorization_server, exchange_code,
    register_dynamic_client,
};
use crate::mcp::{
    ConnectionStatus, CredentialPayload, DiscoveredTool, MCP_CREDENTIAL_READ_TIMEOUT, McpCatalogId,
    McpClient, McpCredentialWrite, McpDescription, McpError, McpServerCreate, McpServerId,
    McpServerRecord, McpServerUpdate, McpTransport, OAUTH2_KIND_LABEL, OAuth2Payload,
};
use crate::tools::{DEFAULT_TOOL_CALLS_PAGE, MAX_TOOL_CALLS_PAGE, ToolCallRowId};
use crate::types::SecretString;

use super::super::error::HttpError;
use super::super::state::AppState;

pub(super) fn router() -> Router<AppState> {
    Router::new()
        // Read-only catalog listing for the frontend connections page.
        // RLS already cuts to global + the caller's org rows.
        .route("/mcp-catalog", get(list_mcp_catalog))
        // The static path goes first so `/mcp-servers/test-connect` is not
        // captured by the `{id}` route.
        .route("/mcp-servers/test-connect", post(test_connect_mcp_server))
        .route(
            "/mcp-servers",
            post(create_mcp_server).get(list_mcp_servers),
        )
        .route(
            "/mcp-servers/{id}",
            get(read_mcp_server)
                .merge(put(update_mcp_server))
                .merge(delete(delete_mcp_server)),
        )
        .route(
            "/mcp-servers/{id}/credentials",
            put(put_mcp_credentials).merge(delete(delete_mcp_credentials)),
        )
        .route(
            "/mcp-servers/{id}/tool-calls",
            get(list_mcp_server_tool_calls),
        )
        .route("/mcp-servers/{id}/oauth/start", post(start_oauth))
        .route("/mcp-servers/{id}/oauth/disconnect", post(disconnect_oauth))
        .route("/mcp-servers/{id}/oauth/client", put(put_oauth_client))
}

/// Public router (no auth middleware) for the OAuth callback. The browser
/// is returning from the vendor's consent screen with `?state=&code=`;
/// CSRF protection comes from the PKCE `state` column being a one-shot
/// row, not the session cookie. Merged into the public subtree alongside
/// `auth::router()`.
pub(super) fn oauth_callback_router() -> Router<AppState> {
    Router::new().route("/mcp-oauth/callback", get(handle_oauth_callback))
}

/// Wire shape for the catalog listing. Mirrors [`crate::mcp::McpCatalogEntry`]
/// minus `default_transport` (operators don't need to see the URL on the
/// listing — they just click "Connect") and minus the per-row timestamps.
#[derive(Debug, Serialize)]
struct McpCatalogResponse {
    catalog_id: String,
    display_name: String,
    description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    homepage_url: Option<String>,
    /// Public URL of the tile icon, or `None` when the FE should fall
    /// back to its `Monogram` rendering.
    #[serde(skip_serializing_if = "Option::is_none")]
    icon_url: Option<String>,
    auth_kind: crate::mcp::McpAuthKind,
    /// `true` when the entry is tenant-custom (org_id set). UI uses this
    /// to surface a "managed by your org" badge.
    is_custom: bool,
    /// `true` when the tenant has a wired `mcp_servers` row for this
    /// catalog id; the UI uses this to hide the Connect button.
    wired: bool,
}

async fn list_mcp_catalog(
    State(state): State<AppState>,
    principal: Principal,
) -> Result<Json<Vec<McpCatalogResponse>>, HttpError> {
    let org_id = principal.active_org_id;
    // Catalog + wired-servers reads are independent — fan out.
    let (entries, wired) = tokio::try_join!(
        state.mcp_catalog.list_for_org(org_id),
        state.mcp_store.list_for_org(org_id),
    )?;
    let wired_ids: std::collections::HashSet<&str> =
        wired.iter().map(|r| r.catalog_id.as_str()).collect();
    let out: Vec<McpCatalogResponse> = entries
        .into_iter()
        .map(|e| McpCatalogResponse {
            wired: wired_ids.contains(e.id.as_str()),
            is_custom: e.org_id.is_some(),
            catalog_id: e.id.as_str().to_owned(),
            display_name: e.display_name.as_str().to_owned(),
            description: e.description.as_str().to_owned(),
            homepage_url: e.homepage_url,
            icon_url: e.icon_url,
            auth_kind: e.auth_kind,
        })
        .collect();
    Ok(Json(out))
}

/// What we hand back on every CRUD response. Mirrors `mcp_servers` plus a flag
/// telling the operator whether the row is currently exposed by the live registry.
///
/// **Never carries credential plaintext.** Secrets live in the encrypted
/// `mcp_server_credentials` table; the wire shape surfaces only "are
/// credentials set?" + "what kind?", so the UI can render a state badge
/// without the backend ever echoing the secret value back to the caller.
#[derive(Debug, Serialize)]
struct McpServerResponse {
    id: McpServerId,
    catalog_id: String,
    enabled: bool,
    config: McpTransport,
    description: Option<String>,
    last_seen_at: Option<chrono::DateTime<chrono::Utc>>,
    last_error: Option<String>,
    discovered_tools: Option<Vec<DiscoveredTool>>,
    created_by_user_id: crate::auth::UserId,
    /// `false` when no row exists in `mcp_server_credentials` for this
    /// server; `true` otherwise. The frontend uses this to render a state
    /// badge without us ever decrypting the payload.
    has_credentials: bool,
    /// `None` when `has_credentials = false`; otherwise the stable
    /// `kind` label (`"static_headers"` or `"oauth2"`).
    credentials_kind: Option<String>,
    /// Surfaced connection state — defaults to `"ok"`; the OAuth refresher
    /// (phase D) flips it when a refresh token is revoked.
    connection_status: crate::mcp::ConnectionStatus,
    /// Email of the user who created this connection (joined from `users`).
    /// `None` only if the row was created before tenancy enforcement and the
    /// FK is null — never the case for newly-created rows.
    creator_email: Option<String>,
    /// `None` on the list / create / update paths; only the single-server read
    /// path decrypts the credential blob to surface this.
    token_expires_at: Option<chrono::DateTime<chrono::Utc>>,
    created_at: chrono::DateTime<chrono::Utc>,
    updated_at: chrono::DateTime<chrono::Utc>,
}

impl McpServerResponse {
    fn from_record(r: McpServerRecord, credentials_kind: Option<String>) -> Self {
        Self {
            id: r.id,
            catalog_id: r.catalog_id.as_str().to_owned(),
            enabled: r.enabled,
            config: r.config,
            description: r.description.map(|d| d.as_str().to_owned()),
            last_seen_at: r.last_seen_at,
            last_error: r.last_error,
            discovered_tools: r.discovered_tools,
            created_by_user_id: r.created_by_user_id,
            has_credentials: credentials_kind.is_some(),
            credentials_kind,
            connection_status: r.connection_status,
            creator_email: None,
            token_expires_at: None,
            created_at: r.created_at,
            updated_at: r.updated_at,
        }
    }
}

/// Two shapes for `POST /mcp-servers`:
///   * **Short form** — `{"catalog_id": "notion"}`. Backend looks up the
///     catalog entry, fills `config` from `default_transport`, defaults
///     `enabled = true`, omits inline credentials (OAuth-style catalogs
///     drive credentials via the follow-up `oauth/start` flow). The UI's
///     click-to-wire button uses this.
///   * **Full form** — every field present. Operators with tenant-custom
///     transport needs (e.g. self-hosted endpoint with static headers)
///     can supply the whole payload.
///
/// The two are mutually exclusive at the request schema level: presence
/// of `config` forces the full-form path; absence forces short-form. The
/// `catalog_id` is required in both, since it's the FK target.
#[derive(Debug, Deserialize)]
struct CreateMcpServerRequest {
    catalog_id: String,
    #[serde(default)]
    config: Option<McpTransport>,
    #[serde(default)]
    description: Option<String>,
    /// Default `None` so the short form can omit it and we infer
    /// `enabled = true` from the catalog defaults; the full form should
    /// always supply.
    #[serde(default)]
    enabled: Option<bool>,
    /// Optional credentials, set in the same request the row is created in.
    /// When present, sealed under the org KEK and written to
    /// `mcp_server_credentials` before the create returns.
    #[serde(default)]
    credentials: Option<CredentialInput>,
}

#[derive(Debug, Deserialize)]
struct UpdateMcpServerRequest {
    #[serde(default)]
    config: Option<McpTransport>,
    /// HTTP PATCH semantics: outer `Option` distinguishes "field omitted (no change)"
    /// from "field present (set or clear)"; inner `Option` carries the new value
    /// (`null` clears the column). Clippy's suggested "custom enum" alternative is
    /// strictly more boilerplate for the same shape.
    #[serde(default, deserialize_with = "double_option::deserialize")]
    #[allow(clippy::option_option)] // explicit two-state representation; see doc comment
    description: Option<Option<String>>,
    #[serde(default)]
    enabled: Option<bool>,
}

mod double_option {
    use serde::{Deserialize, Deserializer};

    #[allow(clippy::option_option)] // see UpdateMcpServerRequest::description
    pub(super) fn deserialize<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
    where
        D: Deserializer<'de>,
        T: Deserialize<'de>,
    {
        Deserialize::deserialize(d).map(Some)
    }
}

async fn create_mcp_server(
    State(state): State<AppState>,
    principal: Principal,
    Json(payload): Json<CreateMcpServerRequest>,
) -> Result<(StatusCode, Json<McpServerResponse>), HttpError> {
    let catalog_id = McpCatalogId::try_from(payload.catalog_id).map_err(HttpError::Parse)?;
    let description = payload
        .description
        .map(McpDescription::try_from)
        .transpose()
        .map_err(HttpError::Parse)?;
    let credentials_payload = match payload.credentials {
        Some(c) => {
            c.check_caps()?;
            Some(c.into_payload())
        }
        None => None,
    };

    // Short-form path: only `catalog_id` (+ optional credentials) supplied.
    // Look the entry up and use `default_transport` / catalog defaults so
    // the click-to-wire button on the UI can fire `POST /mcp-servers
    // {catalog_id}` without knowing the URL.
    let config = match payload.config {
        Some(c) => c,
        None => {
            state
                .mcp_catalog
                .get_for_org(principal.active_org_id, &catalog_id)
                .await?
                .ok_or_else(|| {
                    HttpError::Mcp(crate::mcp::McpError::CatalogIdUnknown(catalog_id.clone()))
                })?
                .default_transport
        }
    };
    let enabled = payload.enabled.unwrap_or(true);

    // A row without inline credentials is parked in `AuthPending` so
    // the registry refresher skips it until the OAuth callback's
    // `mark_connected` flips it to `Ok`. Rows that carry static-header
    // credentials in the same request are ready to serve and start in
    // `Ok`.
    let connection_status = if credentials_payload.is_some() {
        ConnectionStatus::Ok
    } else {
        ConnectionStatus::AuthPending
    };

    let record = state
        .mcp_store
        .create(McpServerCreate {
            org_id: principal.active_org_id,
            created_by_user_id: principal.user_id,
            catalog_id,
            config,
            description,
            enabled,
            connection_status,
        })
        .await?;

    let credentials_kind = if let Some(payload) = credentials_payload {
        let kind = payload.kind_label().to_owned();
        state
            .mcp_credentials
            .upsert(McpCredentialWrite {
                server_id: record.id,
                org_id: principal.active_org_id,
                payload,
            })
            .await?;
        Some(kind)
    } else {
        None
    };

    state.mcp_refresh.request();
    Ok((
        StatusCode::CREATED,
        Json(McpServerResponse::from_record(record, credentials_kind)),
    ))
}

async fn list_mcp_servers(
    State(state): State<AppState>,
    principal: Principal,
) -> Result<Json<Vec<McpServerResponse>>, HttpError> {
    // Tenant-scoped read: open a tx, set `app.user_id` via the GUC, and
    // let the `mcp_servers_org_isolation` RLS policy do the filtering.
    // Bypasses the store's privileged read path so the user can see only
    // their own org's rows. The LEFT JOIN onto `mcp_server_credentials`
    // surfaces only the `kind` label — never the ciphertext — so the
    // response can render `has_credentials` without an extra round-trip.
    //
    // Identity tables (`users`) are intentionally REVOKED from `relay_app`
    // (migration 14), so the creator-email enrichment runs as a second
    // round-trip through the privileged `users` store after the tx commits.
    let mut tx = crate::auth::begin_as(&state.pool, &principal).await?;
    let rows = sqlx::query_as::<_, McpServerRowForList>(
        "SELECT s.id, s.org_id, s.catalog_id, s.enabled, s.config, s.description, \
                s.last_seen_at, s.last_error, s.discovered_tools, \
                s.created_by_user_id, s.connection_status, s.created_at, s.updated_at, \
                c.kind AS credentials_kind \
         FROM mcp_servers s \
         LEFT JOIN mcp_server_credentials c ON c.server_id = s.id \
         ORDER BY s.catalog_id ASC",
    )
    .fetch_all(&mut *tx)
    .await
    .map_err(AuthError::from)?;
    tx.commit().await.map_err(AuthError::from)?;

    let creator_ids: Vec<crate::auth::UserId> = rows.iter().map(|r| r.created_by_user_id).collect();
    let emails = state.users.read_emails(&creator_ids).await?;

    let mut out = Vec::with_capacity(rows.len());
    for mut r in rows {
        r.creator_email = emails
            .get(&r.created_by_user_id)
            .map(|e| e.as_str().to_owned());
        out.push(r.try_into_response()?);
    }
    Ok(Json(out))
}

async fn read_mcp_server(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<Uuid>,
) -> Result<Json<McpServerResponse>, HttpError> {
    let id = McpServerId::from(id);
    let mut tx = crate::auth::begin_as(&state.pool, &principal).await?;
    let row = sqlx::query_as::<_, McpServerRowForList>(
        "SELECT s.id, s.org_id, s.catalog_id, s.enabled, s.config, s.description, \
                s.last_seen_at, s.last_error, s.discovered_tools, \
                s.created_by_user_id, s.connection_status, s.created_at, s.updated_at, \
                c.kind AS credentials_kind \
         FROM mcp_servers s \
         LEFT JOIN mcp_server_credentials c ON c.server_id = s.id \
         WHERE s.id = $1",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(AuthError::from)?;
    tx.commit().await.map_err(AuthError::from)?;
    let mut row = row.ok_or(HttpError::NotFound)?;
    // Identity tables (`users`) are REVOKED from `relay_app` (migration
    // 14), so the email lookup runs through the privileged user store
    // outside the tenant-scoped tx above.
    let emails = state
        .users
        .read_emails(std::slice::from_ref(&row.created_by_user_id))
        .await?;
    row.creator_email = emails
        .get(&row.created_by_user_id)
        .map(|e| e.as_str().to_owned());
    if row.credentials_kind.as_deref() == Some(OAUTH2_KIND_LABEL) {
        let cred = tokio::time::timeout(
            MCP_CREDENTIAL_READ_TIMEOUT,
            state.mcp_credentials.read(id, principal.active_org_id),
        )
        .await
        .map_err(|e| {
            tracing::error!(event = "mcp.read.credential_read_timeout", error = ?e);
            HttpError::Internal
        })??;
        if let Some(record) = cred
            && let CredentialPayload::Oauth2(oauth) = &record.payload
        {
            row.token_expires_at = Some(oauth.expires_at);
        }
    }
    Ok(Json(row.try_into_response()?))
}

/// Cursor-paginated listing of tool calls recorded against an MCP server.
///
/// The `tool_calls_per_connection_idx` partial index covers the access
/// pattern `(mcp_server_id, started_at DESC)` so the query is index-only.
/// We pre-gate visibility on `mcp_servers` so a foreign / unknown id 404s
/// with the same shape as `read_mcp_server`, without leaking existence
/// through an empty-list response.
async fn list_mcp_server_tool_calls(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<Uuid>,
    Query(params): Query<ToolCallsQuery>,
) -> Result<Json<ToolCallListResponse>, HttpError> {
    let server_id = McpServerId::from(id);
    if !visible_to(&state.pool, &principal, VisibilityTable::McpServers, id).await? {
        return Err(HttpError::NotFound);
    }

    let limit = params
        .limit
        .unwrap_or(DEFAULT_TOOL_CALLS_PAGE)
        .clamp(1, MAX_TOOL_CALLS_PAGE);
    // Fetch one extra row to detect whether a next page exists. Stored as
    // i64 because sqlx binds `LIMIT` through bigint params on Postgres.
    let fetch_limit = i64::from(limit) + 1;

    let mut tx = crate::auth::begin_as(&state.pool, &principal).await?;
    let mut items = sqlx::query_as::<_, ToolCallResponse>(
        "SELECT tc.id, tc.tool_name, tc.agent_id, a.name AS agent_name, \
                tc.started_at, tc.duration_ms, tc.is_error, tc.error_message \
         FROM tool_calls tc \
         LEFT JOIN agents a ON a.id = tc.agent_id \
         WHERE tc.mcp_server_id = $1 \
           AND ($2::timestamptz IS NULL OR tc.started_at < $2) \
         ORDER BY tc.started_at DESC \
         LIMIT $3",
    )
    .bind(server_id)
    .bind(params.before)
    .bind(fetch_limit)
    .fetch_all(&mut *tx)
    .await
    .map_err(AuthError::from)?;
    tx.commit().await.map_err(AuthError::from)?;

    // `fetch_limit = limit + 1`: the extra row is a peek to detect "more
    // exists" without committing it to this page. Cursor is the *last
    // returned* row's started_at so the next call's strict `< cursor`
    // begins exactly where this page ended (no missed row).
    let has_more = items.len() > usize::from(limit);
    if has_more {
        items.pop();
    }
    let next_cursor = has_more
        .then(|| items.last().map(|r| r.started_at))
        .flatten();

    Ok(Json(ToolCallListResponse { items, next_cursor }))
}

async fn update_mcp_server(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<Uuid>,
    Json(payload): Json<UpdateMcpServerRequest>,
) -> Result<Json<McpServerResponse>, HttpError> {
    let id = McpServerId::from(id);
    let description = payload
        .description
        .map(|inner| inner.map(McpDescription::try_from).transpose())
        .transpose()
        .map_err(HttpError::Parse)?;
    // Tenant gate: 404 cross-org / unknown ids without leaking existence
    // before dispatching the privileged update.
    if !crate::auth::visible_to(
        &state.pool,
        &principal,
        crate::auth::VisibilityTable::McpServers,
        id.as_uuid(),
    )
    .await?
    {
        return Err(HttpError::NotFound);
    }
    let row = state
        .mcp_store
        .update(
            id,
            principal.active_org_id,
            McpServerUpdate {
                config: payload.config,
                description,
                enabled: payload.enabled,
            },
        )
        .await?;
    // Look up the credential kind label after the update so the response
    // surface remains uniform across CRUD endpoints.
    let credentials_kind = state
        .mcp_credentials
        .read(row.id, principal.active_org_id)
        .await?
        .map(|c| c.payload.kind_label().to_owned());
    state.mcp_refresh.request();
    Ok(Json(McpServerResponse::from_record(row, credentials_kind)))
}

async fn delete_mcp_server(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, HttpError> {
    let id = McpServerId::from(id);
    if !crate::auth::visible_to(
        &state.pool,
        &principal,
        crate::auth::VisibilityTable::McpServers,
        id.as_uuid(),
    )
    .await?
    {
        return Err(HttpError::NotFound);
    }
    state.mcp_store.delete(id, principal.active_org_id).await?;
    state.mcp_refresh.request();
    Ok(StatusCode::NO_CONTENT)
}

/// PUT `/mcp-servers/{id}/credentials` — replace (or insert) the credential
/// row for `id`. Always writes fresh ciphertext without reading the old
/// one back, so a replacement cannot expose the prior credential.
async fn put_mcp_credentials(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<Uuid>,
    Json(payload): Json<CredentialInput>,
) -> Result<StatusCode, HttpError> {
    payload.check_caps()?;
    let server_id = McpServerId::from(id);
    // Tenant gate: 404 cross-org / unknown ids without leaking existence.
    if !crate::auth::visible_to(
        &state.pool,
        &principal,
        crate::auth::VisibilityTable::McpServers,
        server_id.as_uuid(),
    )
    .await?
    {
        return Err(HttpError::NotFound);
    }
    state
        .mcp_credentials
        .upsert(McpCredentialWrite {
            server_id,
            org_id: principal.active_org_id,
            payload: payload.into_payload(),
        })
        .await?;
    state.mcp_refresh.request();
    Ok(StatusCode::NO_CONTENT)
}

/// DELETE `/mcp-servers/{id}/credentials` — drop the credential row.
/// Idempotent: no body, returns 204 whether or not a row existed.
async fn delete_mcp_credentials(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, HttpError> {
    let server_id = McpServerId::from(id);
    if !crate::auth::visible_to(
        &state.pool,
        &principal,
        crate::auth::VisibilityTable::McpServers,
        server_id.as_uuid(),
    )
    .await?
    {
        return Err(HttpError::NotFound);
    }
    state
        .mcp_credentials
        .delete(server_id, principal.active_org_id)
        .await?;
    state.mcp_refresh.request();
    Ok(StatusCode::NO_CONTENT)
}

/// Boundary shape for credential input on create / replace / test paths.
///
/// Matches the on-disk [`CredentialPayload`] enum: the wire form carries a
/// `kind` discriminant and the variant payload. The OAuth flow does not
/// populate credentials through this path (the callback writes them
/// directly); only `static_headers` is accepted here today.
///
/// Validation: every header name and value passes through its own newtype
/// smart constructor; the map size is bounded by the
/// [`crate::mcp::MCP_MAX_HEADERS`] cap, checked in the route handler.
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum CredentialInput {
    StaticHeaders {
        headers: std::collections::BTreeMap<crate::mcp::McpHeaderName, crate::mcp::McpHeaderValue>,
    },
}

impl CredentialInput {
    fn into_payload(self) -> CredentialPayload {
        match self {
            Self::StaticHeaders { headers } => CredentialPayload::StaticHeaders { headers },
        }
    }

    /// Validate boundary-only caps (the per-newtype parsers already cover
    /// length/charset; this catches the map-size limit that no parser sees).
    fn check_caps(&self) -> Result<(), HttpError> {
        match self {
            Self::StaticHeaders { headers } => {
                if headers.len() > crate::mcp::MCP_MAX_HEADERS {
                    return Err(HttpError::BadRequest(format!(
                        "credentials: too many headers (max {})",
                        crate::mcp::MCP_MAX_HEADERS
                    )));
                }
                Ok(())
            }
        }
    }
}

/// Request body for `POST /mcp-servers/test-connect`. Carries a transport
/// config plus optional credentials; nothing is persisted — the handler
/// builds an [`McpClient`] in-process, performs the MCP `initialize`
/// handshake plus one `list_tools` round-trip, then drops the client. The
/// secret-redact contract still applies: the request body may carry bearer
/// tokens in `credentials.headers`, so on every response we surface either
/// the tool list (on success) or a free-text error string — never echo the
/// input back.
#[derive(Debug, Deserialize)]
struct TestConnectRequest {
    config: McpTransport,
    #[serde(default)]
    credentials: Option<CredentialInput>,
}

/// Response shape: a single discriminant indicates success vs. failure so the
/// frontend can render a clear pass/fail state without parsing error strings.
#[derive(Debug, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
enum TestConnectResponse {
    Ok {
        discovered_tools: Vec<DiscoveredTool>,
    },
    Failed {
        error: String,
    },
}

async fn test_connect_mcp_server(
    State(state): State<AppState>,
    principal: Principal,
    Json(payload): Json<TestConnectRequest>,
) -> Result<Json<TestConnectResponse>, HttpError> {
    // Per-user rate limit, enforced before we open any outbound connection.
    // This is the SSRF guardrail: a logged-in user can probe at most
    // `MCP_TEST_CONNECT_PER_MIN` distinct URLs per rolling minute.
    if !state.mcp_test_rate.try_admit(principal.user_id) {
        return Err(HttpError::TooManyRequests);
    }

    let span = tracing::info_span!(
        "mcp.test_connect",
        relay.user.id = %principal.user_id,
        relay.org.id = %principal.active_org_id,
    );
    let _guard = span.enter();

    // Connect + list_tools inside the operator-trusted MCP-client path. Both
    // calls are bounded by their own internal timeouts (MCP_CONNECT_TIMEOUT,
    // MCP_LIST_TOOLS_TIMEOUT). Failures collapse to a structured 200 response
    // body — the call itself succeeded from a transport perspective even
    // when the upstream MCP server refused the handshake.
    let credentials = payload.credentials.map(CredentialInput::into_payload);
    let connect = McpClient::connect(&payload.config, credentials.as_ref()).await;
    let client = match connect {
        Ok(c) => c,
        Err(e) => {
            tracing::info!(error = %e, "mcp.test_connect.connect_failed");
            return Ok(Json(TestConnectResponse::Failed {
                error: redact_error(&e),
            }));
        }
    };

    let McpTransport::Http { url, .. } = &payload.config;
    // Fixed prefix on test-connect: we have no alias yet, and using one of
    // the user's real aliases would leak it through the rendered name.
    let alias_prefix = "test";

    let listed = match client.list_tools().await {
        Ok(t) => t,
        Err(e) => {
            tracing::info!(error = %e, "mcp.test_connect.list_failed");
            return Ok(Json(TestConnectResponse::Failed {
                error: redact_error(&e),
            }));
        }
    };

    let discovered: Vec<DiscoveredTool> = listed
        .into_iter()
        .map(|t| {
            let remote_name = t.name.to_string();
            DiscoveredTool {
                prefixed_name: format!("mcp_{alias_prefix}_{remote_name}"),
                description: t.description.as_deref().map(str::to_owned),
                remote_name,
            }
        })
        .collect();

    tracing::info!(
        relay.mcp.url = %url.as_str(),
        relay.mcp.discovered = discovered.len(),
        "mcp.test_connect.ok"
    );
    Ok(Json(TestConnectResponse::Ok {
        discovered_tools: discovered,
    }))
}

/// Strip any potentially-sensitive sub-strings (currently a no-op: McpError's
/// Display impls already omit bearer-token-carrying header bytes) and clamp
/// the message length so a stack-traced underlying error can't bloat the
/// response. Kept as a single seam so a future format-string regression can
/// be patched in one place.
fn redact_error(err: &McpError) -> String {
    const MAX: usize = 512;
    let s = err.to_string();
    if s.len() > MAX {
        // Floor by char boundary, not byte: an MCP error string is ASCII in
        // practice, but a stray UTF-8 sequence inside Url::parse output is
        // possible. `s.is_char_boundary` walks at most 3 bytes back.
        let mut cut = MAX;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}…", &s[..cut])
    } else {
        s
    }
}

// Local row type for the tenant-scoped SELECTs. Mirrors the columns
// returned by the store's read path but lives here so the route can
// run raw SQL inside the principal-scoped tx without going through
// the store's privileged transaction.
#[derive(sqlx::FromRow)]
struct McpServerRowForList {
    id: McpServerId,
    org_id: crate::auth::OrgId,
    catalog_id: String,
    enabled: bool,
    config: serde_json::Value,
    description: Option<String>,
    last_seen_at: Option<chrono::DateTime<chrono::Utc>>,
    last_error: Option<String>,
    discovered_tools: Option<serde_json::Value>,
    created_by_user_id: crate::auth::UserId,
    connection_status: crate::mcp::ConnectionStatus,
    created_at: chrono::DateTime<chrono::Utc>,
    updated_at: chrono::DateTime<chrono::Utc>,
    credentials_kind: Option<String>,
    /// Stitched in by the handler after the tenant-scoped tx commits —
    /// the SELECT does not return this column (identity tables are
    /// revoked from `relay_app`).
    #[sqlx(default)]
    creator_email: Option<String>,
    /// Surfaced only on the per-server read path, by decrypting the
    /// OAuth credential payload outside the SELECT.
    #[sqlx(default)]
    token_expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl McpServerRowForList {
    fn try_into_response(self) -> Result<McpServerResponse, HttpError> {
        // Rebuild the typed record exactly as the store does so the
        // response shape matches whether the call path is via the store
        // or via a tenant-scoped raw query here.
        let catalog_id = McpCatalogId::try_from(self.catalog_id).map_err(HttpError::Parse)?;
        let config: McpTransport = serde_json::from_value(self.config).map_err(|e| {
            tracing::error!(error = ?e, "mcp.row.deserialize_transport");
            HttpError::Internal
        })?;
        let description = self
            .description
            .map(McpDescription::try_from)
            .transpose()
            .map_err(HttpError::Parse)?;
        let discovered_tools = self
            .discovered_tools
            .map(serde_json::from_value::<Vec<DiscoveredTool>>)
            .transpose()
            .map_err(|e| {
                tracing::error!(error = ?e, "mcp.row.deserialize_discovered");
                HttpError::Internal
            })?;
        let _ = self.org_id; // not on the wire shape — RLS already filtered.
        Ok(McpServerResponse {
            id: self.id,
            catalog_id: catalog_id.as_str().to_owned(),
            enabled: self.enabled,
            config,
            description: description.map(|d| d.as_str().to_owned()),
            last_seen_at: self.last_seen_at,
            last_error: self.last_error,
            discovered_tools,
            created_by_user_id: self.created_by_user_id,
            has_credentials: self.credentials_kind.is_some(),
            credentials_kind: self.credentials_kind,
            connection_status: self.connection_status,
            creator_email: self.creator_email,
            token_expires_at: self.token_expires_at,
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}

// ────────────────────────────────────────────────────────────────────────
// Tool-call audit list
// ────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ToolCallsQuery {
    /// Defaults to [`DEFAULT_TOOL_CALLS_PAGE`], clamped to
    /// `1..=MAX_TOOL_CALLS_PAGE` by the handler.
    limit: Option<u16>,
    /// Exclusive `started_at` cursor — returned rows have `started_at < before`.
    /// Pass the previous page's `next_cursor` to walk backwards in time.
    before: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
struct ToolCallResponse {
    id: ToolCallRowId,
    tool_name: String,
    agent_id: AgentId,
    /// LEFT JOIN: an audit row outlives its agent if the agent row is ever
    /// removed. Today the schema doesn't ON DELETE, but the join stays
    /// nullable so a future cascade doesn't break the list query.
    agent_name: Option<String>,
    started_at: chrono::DateTime<chrono::Utc>,
    duration_ms: i32,
    is_error: bool,
    /// Non-null only when `is_error = true` (migration-27 CHECK).
    error_message: Option<String>,
}

#[derive(Debug, Serialize)]
struct ToolCallListResponse {
    items: Vec<ToolCallResponse>,
    /// `Some(ts)` when more rows exist beyond this page — pass it back as
    /// `?before=` to fetch the next slice. `None` when the page is the tail.
    next_cursor: Option<chrono::DateTime<chrono::Utc>>,
}

// ────────────────────────────────────────────────────────────────────────
// Upstream OAuth
// ────────────────────────────────────────────────────────────────────────

const OAUTH_CALLBACK_PATH: &str = "/mcp-oauth/callback";
/// How long a pending OAuth row stays valid. Long enough for the user to
/// complete the consent flow even with a slow network; short enough that
/// an abandoned row is reaped on schedule. Matches the spec's typical
/// "10 minutes" cap.
const OAUTH_PENDING_TTL: std::time::Duration = std::time::Duration::from_secs(600);

#[derive(Debug, Deserialize)]
struct OAuthStartRequest {
    /// Optional path the frontend wants us to redirect back to after the
    /// callback completes. Length-capped by the DB CHECK constraint;
    /// path-only by convention.
    #[serde(default)]
    redirect_to: Option<String>,
    /// Optional scope override. When absent we use whatever the AS
    /// advertises as default; vendor-specific scopes can be requested by
    /// the frontend (e.g. Notion needs `read_content read_user`).
    #[serde(default)]
    scope: Option<String>,
}

#[derive(Debug, Serialize)]
struct OAuthStartResponse {
    authorize_url: String,
}

/// `POST /mcp-servers/{id}/oauth/start` — kick off the browser flow.
///
/// Steps:
///   1. Resolve server, ensure caller is authorized for it.
///   2. Discover authorization-server metadata (RFC 9728 + RFC 8414).
///   3. Look up the registered DCR client for `(org, issuer)`, or
///      register one via RFC 7591 if first time.
///   4. Mint PKCE + state, persist the pending row.
///   5. Build the authorize URL and hand it back.
#[allow(clippy::too_many_lines)] // composition path — branching is what each step is
async fn start_oauth(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<Uuid>,
    Json(body): Json<OAuthStartRequest>,
) -> Result<Json<OAuthStartResponse>, HttpError> {
    let server_id = McpServerId::from(id);

    // Step 1: tenant gate — look up the server and ensure the caller can
    // see it. The store read goes through `run_privileged` but the
    // explicit org_id filter pins the row to the principal's org.
    let as_metadata =
        resolve_as_metadata_for_server(&state, principal.active_org_id, server_id).await?;

    // Step 3: load-or-register DCR client.
    let redirect_uri = format!("{}{}", state.oauth_redirect_base, OAUTH_CALLBACK_PATH);
    let dcr = if let Some(existing) = state
        .mcp_oauth_clients
        .read(principal.active_org_id, &as_metadata.issuer)
        .await
        .map_err(map_oauth_err)?
    {
        existing
    } else {
        let new: NewOAuthClient = register_dynamic_client(
            &state.mcp_oauth_flow,
            &as_metadata,
            principal.active_org_id,
            &redirect_uri,
            body.scope.as_deref(),
        )
        .await
        .map_err(map_oauth_err)?;
        state
            .mcp_oauth_clients
            .upsert(new)
            .await
            .map_err(map_oauth_err)?
    };

    // Step 4: PKCE + state, persist pending row.
    let start =
        build_authorize_url(&dcr, &redirect_uri, body.scope.as_deref()).map_err(map_oauth_err)?;
    let now = state.clock.now_utc();
    let expires_at = now
        + chrono::Duration::from_std(OAUTH_PENDING_TTL)
            .expect("invariant: OAUTH_PENDING_TTL fits in chrono::Duration");
    state
        .mcp_oauth_pending
        .insert(crate::mcp::oauth::PendingAuthorizationWrite {
            state: start.state.clone(),
            server_id,
            user_id: principal.user_id,
            org_id: principal.active_org_id,
            issuer: dcr.issuer.clone(),
            pkce_verifier: start.pkce_verifier.clone(),
            redirect_to: body.redirect_to.clone(),
            expires_at,
        })
        .await
        .map_err(map_oauth_err)?;

    Ok(Json(OAuthStartResponse {
        authorize_url: start.authorize_url.to_string(),
    }))
}

#[derive(Debug, Deserialize)]
struct OAuthCallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

/// `GET /mcp-oauth/callback?code=&state=`. Runs without a session
/// cookie — CSRF protection comes from `state` being a one-shot row.
/// Always returns a 303 redirect to the FE callback page with
/// `?status=ok` or `?status=failed&reason=…` so the user lands on a
/// rendered page either way.
#[tracing::instrument(name = "mcp.oauth.callback", skip_all)]
async fn handle_oauth_callback(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<OAuthCallbackQuery>,
) -> axum::response::Response {
    let web_base = state.web_base_url.as_deref();
    match callback_flow(&state, q).await {
        Ok(redirect_to) => {
            tracing::info!(event = "mcp.oauth.callback.ok");
            redirect_ok(web_base, redirect_to.as_deref())
        }
        Err(CallbackFail {
            redirect_to,
            reason,
        }) => {
            tracing::info!(event = "mcp.oauth.callback.failed", reason = %reason);
            redirect_failed(web_base, redirect_to.as_deref(), reason)
        }
    }
}

struct CallbackFail {
    redirect_to: Option<String>,
    reason: &'static str,
}

async fn callback_flow(
    state: &AppState,
    q: OAuthCallbackQuery,
) -> Result<Option<String>, CallbackFail> {
    if let Some(err) = q.error.as_deref() {
        tracing::warn!(
            event = "mcp.oauth.callback.vendor_error",
            error = %err,
            description = q.error_description.as_deref().unwrap_or(""),
        );
        let redirect_to = consume_pending_for_redirect(state, q.state.as_deref()).await;
        // The vendor's `error` token already passes through `sanitize_reason`
        // in `failed_redirect`, so a raw value here is safe.
        return Err(CallbackFail {
            redirect_to,
            reason: vendor_error_to_reason(err),
        });
    }
    let state_val = q.state.as_deref().ok_or(CallbackFail {
        redirect_to: None,
        reason: "state_missing",
    })?;
    let Some(code) = q.code.as_deref() else {
        let redirect_to = consume_pending_for_redirect(state, Some(state_val)).await;
        return Err(CallbackFail {
            redirect_to,
            reason: "code_missing",
        });
    };
    let now = state.clock.now_utc();
    let pending = consume_pending(state, state_val, now).await?;
    let dcr = load_dcr(state, &pending).await?;
    let token = exchange_token(state, &dcr, &pending, code, now).await?;
    persist_oauth_success(state, &pending, token, now)
        .await
        .map_err(|reason| CallbackFail {
            redirect_to: pending.redirect_to.clone(),
            reason,
        })?;
    Ok(pending.redirect_to)
}

fn redirect_ok(web_base: Option<&str>, redirect_to: Option<&str>) -> axum::response::Response {
    use axum::response::IntoResponse as _;
    Redirect::to(&ok_redirect(web_base, redirect_to)).into_response()
}

fn redirect_failed(
    web_base: Option<&str>,
    redirect_to: Option<&str>,
    reason: &str,
) -> axum::response::Response {
    use axum::response::IntoResponse as _;
    Redirect::to(&failed_redirect(web_base, redirect_to, reason)).into_response()
}

/// Convert a vendor `error` query token into our short reason key. Anything
/// other than the standard set falls through to a generic `vendor_error`
/// so we never leak unbounded vendor text into the FE URL.
fn vendor_error_to_reason(raw: &str) -> &'static str {
    match raw {
        "access_denied" => "access_denied",
        "invalid_scope" => "invalid_scope",
        "server_error" => "vendor_server_error",
        "temporarily_unavailable" => "vendor_unavailable",
        _ => "vendor_error",
    }
}

async fn consume_pending(
    state: &AppState,
    state_val: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<PendingAuthorization, CallbackFail> {
    match state.mcp_oauth_pending.consume(state_val, now).await {
        Ok(Some(p)) => Ok(p),
        Ok(None) => Err(CallbackFail {
            redirect_to: None,
            reason: "unknown_or_expired_state",
        }),
        Err(e) => {
            tracing::error!(
                event = "mcp.oauth.callback.consume_failed",
                error = ?e,
            );
            Err(CallbackFail {
                redirect_to: None,
                reason: "internal_error",
            })
        }
    }
}

async fn load_dcr(
    state: &AppState,
    pending: &PendingAuthorization,
) -> Result<crate::mcp::oauth::DcrClientRecord, CallbackFail> {
    match state
        .mcp_oauth_clients
        .read(pending.org_id, &pending.issuer)
        .await
    {
        Ok(Some(dcr)) => Ok(dcr),
        Ok(None) => Err(CallbackFail {
            redirect_to: pending.redirect_to.clone(),
            reason: "oauth_client_missing",
        }),
        Err(e) => {
            tracing::error!(
                event = "mcp.oauth.callback.client_lookup_failed",
                error = ?e,
            );
            Err(CallbackFail {
                redirect_to: pending.redirect_to.clone(),
                reason: "internal_error",
            })
        }
    }
}

async fn exchange_token(
    state: &AppState,
    dcr: &crate::mcp::oauth::DcrClientRecord,
    pending: &PendingAuthorization,
    code: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<crate::mcp::oauth::TokenExchangeResult, CallbackFail> {
    let redirect_uri = format!("{}{}", state.oauth_redirect_base, OAUTH_CALLBACK_PATH);
    exchange_code(
        &state.mcp_oauth_flow,
        dcr,
        &redirect_uri,
        code,
        &pending.pkce_verifier,
        now,
    )
    .await
    .map_err(|e| {
        // Upstream/vendor failure — not our error, so `warn`. CLAUDE.md
        // §2: "ERROR = user-visible failure"; vendor refusing the
        // exchange is an operating error from our side.
        tracing::warn!(
            event = "mcp.oauth.callback.exchange_failed",
            error = ?e,
        );
        CallbackFail {
            redirect_to: pending.redirect_to.clone(),
            reason: "token_exchange_failed",
        }
    })
}

async fn persist_oauth_success(
    state: &AppState,
    pending: &PendingAuthorization,
    token: crate::mcp::oauth::TokenExchangeResult,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<(), &'static str> {
    let payload = CredentialPayload::Oauth2(OAuth2Payload {
        access_token: token.access_token,
        refresh_token: token.refresh_token,
        expires_at: token.expires_at,
        scope: token.scope,
        issuer: token.issuer,
        token_endpoint: token.token_endpoint,
    });
    if let Err(e) = state
        .mcp_credentials
        .upsert(McpCredentialWrite {
            server_id: pending.server_id,
            org_id: pending.org_id,
            payload,
        })
        .await
    {
        tracing::error!(
            event = "mcp.oauth.callback.credentials_write_failed",
            error = ?e,
        );
        return Err("credentials_write_failed");
    }
    mark_connected(state, pending.server_id, pending.org_id, now)
        .await
        .map_err(|e| {
            tracing::error!(
                event = "mcp.oauth.callback.status_update_failed",
                error = ?e,
            );
            "internal_error"
        })?;
    state.mcp_refresh.request();
    Ok(())
}

// Privileged write: the callback runs without a session cookie, so we
// can't go through `begin_as`. `org_id = $2` pins the row to the tenant
// the pending row was issued for.
async fn mark_connected(
    state: &AppState,
    server_id: McpServerId,
    org_id: crate::auth::OrgId,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<(), crate::auth::AuthError> {
    crate::auth::run_privileged::<(), crate::auth::AuthError>(&state.pool, async |tx| {
        sqlx::query(
            "UPDATE mcp_servers SET connection_status = 'ok', last_error = NULL, \
                                    updated_at = $3 \
             WHERE id = $1 AND org_id = $2",
        )
        .bind(server_id)
        .bind(org_id)
        .bind(now)
        .execute(&mut **tx)
        .await?;
        Ok(())
    })
    .await
}

/// Best-effort consumption of the pending row when we already know the
/// flow has failed and just want to extract the `redirect_to` so the FE
/// lands on the right page. Swallows errors and absence — the failure
/// redirect falls back to `/` either way.
async fn consume_pending_for_redirect(state: &AppState, raw_state: Option<&str>) -> Option<String> {
    let raw = raw_state?;
    let now = state.clock.now_utc();
    match state.mcp_oauth_pending.consume(raw, now).await {
        Ok(Some(p)) => p.redirect_to,
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(
                event = "mcp.oauth.callback.consume_for_redirect_failed",
                error = ?e,
            );
            None
        }
    }
}

/// Build the successful-callback redirect URL. The base path is the
/// caller-supplied `redirect_to` if it survives `sanitize_return_to`
/// (relative path, ≤2048 bytes); otherwise `/`. A `status=ok` marker is
/// appended so the FE polling loop terminates immediately on first nav.
///
/// When `web_base` is `Some(origin)` (set via `RELAY_WEB_BASE_URL` for
/// cross-origin dev where FE and BE live on different ports), the origin
/// is prepended so the browser lands on the FE host. The path itself
/// stays relative via `sanitize_return_to`, preserving open-redirect
/// protection — only operator config can change the origin.
fn ok_redirect(web_base: Option<&str>, redirect_to: Option<&str>) -> String {
    let base = sanitized_base(web_base, redirect_to);
    append_query(&base, "status=ok")
}

/// Build the failed-callback redirect URL. Same allow-list as
/// [`ok_redirect`]; appends `status=failed` plus a sanitized short
/// `reason` token so the FE Failed frame can branch on it.
fn failed_redirect(web_base: Option<&str>, redirect_to: Option<&str>, reason: &str) -> String {
    let base = sanitized_base(web_base, redirect_to);
    let safe_reason = sanitize_reason(reason);
    append_query(
        &base,
        &format!("status=failed&reason={}", encode_query_value(&safe_reason)),
    )
}

fn sanitized_base(web_base: Option<&str>, redirect_to: Option<&str>) -> String {
    let path = redirect_to
        .and_then(super::auth::sanitize_return_to)
        .unwrap_or_else(|| "/".to_owned());
    match web_base {
        Some(origin) => format!("{origin}{path}"),
        None => path,
    }
}

fn append_query(base: &str, kv: &str) -> String {
    if base.contains('?') {
        format!("{base}&{kv}")
    } else {
        format!("{base}?{kv}")
    }
}

/// Clamp the failure reason to an ASCII-safe short token so a vendor
/// can't smuggle markup or unbounded text into our FE via the query
/// string. We keep `[a-z0-9_-]`, lowercase the rest into `_`, and cap
/// the length.
fn sanitize_reason(raw: &str) -> String {
    const MAX: usize = 64;
    let mut out = String::with_capacity(raw.len().min(MAX));
    for ch in raw.chars().take(MAX) {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "unknown".to_owned()
    } else {
        out
    }
}

/// Minimal percent-encoder for a query value. Our reasons are already
/// `[a-z0-9_-]` after `sanitize_reason`, so this is a belt-and-braces
/// pass that escapes any stragglers without pulling in a new dep.
fn encode_query_value(raw: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(raw.len());
    for &b in raw.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

#[derive(Debug, Serialize)]
struct OAuthDisconnectResponse {
    ok: bool,
}

async fn disconnect_oauth(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<Uuid>,
) -> Result<Json<OAuthDisconnectResponse>, HttpError> {
    let server_id = McpServerId::from(id);
    if !crate::auth::visible_to(
        &state.pool,
        &principal,
        crate::auth::VisibilityTable::McpServers,
        server_id.as_uuid(),
    )
    .await?
    {
        return Err(HttpError::NotFound);
    }
    state
        .mcp_credentials
        .delete(server_id, principal.active_org_id)
        .await?;
    state.mcp_refresh.request();
    Ok(Json(OAuthDisconnectResponse { ok: true }))
}

/// Boundary input for `PUT /mcp-servers/{id}/oauth/client`. Length caps
/// and the secret↔method cross-field invariant are enforced by the
/// smart-constructor for [`OAuthClientCredentials`]; the handler does
/// not branch on the relation after that point.
#[derive(Debug, Deserialize)]
struct OAuthClientRequest {
    client_id: String,
    #[serde(default)]
    client_secret: Option<String>,
    #[serde(default)]
    token_endpoint_auth_method: Option<TokenAuthMethod>,
    #[serde(default)]
    scope: Option<String>,
}

#[derive(Debug, Serialize)]
struct OAuthClientResponse {
    issuer: String,
    client_id: String,
    token_endpoint_auth_method: TokenAuthMethod,
    scope: Option<String>,
}

/// Parsed credentials. Constructing this is the only path that produces
/// a coherent (secret, method) pair.
#[derive(Debug)]
enum OAuthClientCredentials {
    Public,
    Confidential {
        secret: SecretString,
        method: TokenAuthMethod,
    },
}

impl OAuthClientCredentials {
    const SECRET_MAX_BYTES: usize = 4 * 1024;

    const fn method(&self) -> TokenAuthMethod {
        match self {
            Self::Public => TokenAuthMethod::None,
            Self::Confidential { method, .. } => *method,
        }
    }

    fn into_secret(self) -> Option<SecretString> {
        match self {
            Self::Public => None,
            Self::Confidential { secret, .. } => Some(secret),
        }
    }

    fn parse(
        raw_secret: Option<String>,
        raw_method: Option<TokenAuthMethod>,
    ) -> Result<Self, &'static str> {
        match (raw_secret, raw_method) {
            (None, None | Some(TokenAuthMethod::None)) => Ok(Self::Public),
            (None, Some(_)) => {
                Err("client_secret required for the selected token_endpoint_auth_method")
            }
            (Some(_), Some(TokenAuthMethod::None)) => {
                Err("token_endpoint_auth_method=none is incompatible with client_secret")
            }
            (Some(s), method) => {
                if s.is_empty() || s.len() > Self::SECRET_MAX_BYTES {
                    return Err("client_secret length out of range");
                }
                let secret = SecretString::try_from(s).map_err(|_| "client_secret rejected")?;
                Ok(Self::Confidential {
                    secret,
                    method: method.unwrap_or(TokenAuthMethod::ClientSecretBasic),
                })
            }
        }
    }
}

const OAUTH_SCOPE_MAX_BYTES: usize = 2 * 1024;

/// `PUT /mcp-servers/{id}/oauth/client` — register or replace an
/// operator-supplied OAuth client for vendors that do not implement RFC
/// 7591 Dynamic Client Registration. Runs discovery so the AS endpoints
/// we store stay in lockstep with what `start_oauth` resolves at flow
/// time.
async fn put_oauth_client(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<Uuid>,
    Json(body): Json<OAuthClientRequest>,
) -> Result<Json<OAuthClientResponse>, HttpError> {
    let server_id = McpServerId::from(id);
    let client_id = OAuthClientId::try_from(body.client_id).map_err(HttpError::Parse)?;
    let credentials =
        OAuthClientCredentials::parse(body.client_secret, body.token_endpoint_auth_method)
            .map_err(|m| HttpError::BadRequest(m.to_owned()))?;
    if let Some(s) = body.scope.as_deref()
        && s.len() > OAUTH_SCOPE_MAX_BYTES
    {
        return Err(HttpError::BadRequest("scope too large".into()));
    }

    let as_metadata =
        resolve_as_metadata_for_server(&state, principal.active_org_id, server_id).await?;
    let method = credentials.method();
    let new = NewOAuthClient {
        org_id: principal.active_org_id,
        issuer: as_metadata.issuer.clone(),
        client_id,
        client_secret: credentials.into_secret(),
        authorization_endpoint: as_metadata.authorization_endpoint,
        token_endpoint: as_metadata.token_endpoint,
        token_endpoint_auth_method: method,
        scope: body.scope,
        provenance: ClientProvenance::Operator,
    };
    let stored = state
        .mcp_oauth_clients
        .upsert(new)
        .await
        .map_err(map_oauth_err)?;
    tracing::info!(
        relay.org.id = %principal.active_org_id,
        relay.oauth.issuer = %stored.issuer,
        relay.mcp.server.id = %server_id,
        event = "mcp.oauth.client.operator_set",
    );
    Ok(Json(OAuthClientResponse {
        issuer: stored.issuer,
        client_id: stored.client_id.into_inner(),
        token_endpoint_auth_method: stored.token_endpoint_auth_method,
        scope: stored.scope,
    }))
}

/// Tenant-gate a server, then run discovery against its HTTP URL.
/// Shared by every OAuth handler that needs the same `AsMetadata`
/// `start_oauth` would resolve.
async fn resolve_as_metadata_for_server(
    state: &AppState,
    org_id: crate::auth::OrgId,
    server_id: McpServerId,
) -> Result<crate::mcp::oauth::AsMetadata, HttpError> {
    let server = state
        .mcp_store
        .read(server_id, org_id)
        .await
        .map_err(HttpError::Mcp)?;
    let url = server.config.http_url();
    discover_authorization_server(&state.mcp_oauth_flow.http, url.as_str())
        .await
        .map_err(map_oauth_err)
}

fn map_oauth_err(err: OAuthError) -> HttpError {
    match err {
        OAuthError::InvalidState | OAuthError::Expired => HttpError::BadRequest(err.to_string()),
        OAuthError::Discovery(_) | OAuthError::Dcr(_) | OAuthError::TokenEndpoint(_) => {
            tracing::warn!(error = %err, "mcp.oauth.upstream_error");
            HttpError::BadRequest(err.to_string())
        }
        OAuthError::RefreshRevoked => HttpError::Conflict(err.to_string()),
        OAuthError::Crypto(_) | OAuthError::Db(_) | OAuthError::Misconfigured(_) => {
            tracing::error!(error = %err, "mcp.oauth.internal_error");
            HttpError::Internal
        }
        OAuthError::Mcp(e) => HttpError::Mcp(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ok_redirect_uses_root_when_caller_omitted_path() {
        assert_eq!(ok_redirect(None, None), "/?status=ok");
    }

    #[test]
    fn ok_redirect_appends_status_to_safe_path() {
        assert_eq!(
            ok_redirect(None, Some("/connections/oauth-callback?server_id=abc")),
            "/connections/oauth-callback?server_id=abc&status=ok"
        );
    }

    // open-redirect attempts get clamped to `/` via sanitize_return_to.
    #[test]
    fn ok_redirect_rejects_absolute_url() {
        assert_eq!(
            ok_redirect(None, Some("https://attacker.example/steal")),
            "/?status=ok"
        );
    }

    #[test]
    fn ok_redirect_rejects_protocol_relative_url() {
        assert_eq!(ok_redirect(None, Some("//attacker.example")), "/?status=ok");
    }

    #[test]
    fn ok_redirect_prepends_web_base_when_set() {
        assert_eq!(
            ok_redirect(
                Some("http://localhost:5173"),
                Some("/connections/oauth-callback?server_id=abc"),
            ),
            "http://localhost:5173/connections/oauth-callback?server_id=abc&status=ok"
        );
    }

    #[test]
    fn ok_redirect_with_web_base_still_clamps_user_supplied_origin() {
        // The `redirect_to` is still constrained to a relative path, so
        // only the operator-controlled `web_base` decides the origin.
        assert_eq!(
            ok_redirect(
                Some("http://localhost:5173"),
                Some("https://attacker.example/steal"),
            ),
            "http://localhost:5173/?status=ok"
        );
    }

    #[test]
    fn failed_redirect_lowercases_and_escapes_reason() {
        assert_eq!(
            failed_redirect(None, None, "Access Denied!"),
            "/?status=failed&reason=access_denied_"
        );
    }

    #[test]
    fn failed_redirect_falls_back_when_reason_is_empty() {
        assert_eq!(
            failed_redirect(None, None, ""),
            "/?status=failed&reason=unknown"
        );
    }

    #[test]
    fn failed_redirect_prepends_web_base_when_set() {
        assert_eq!(
            failed_redirect(
                Some("http://localhost:5173"),
                Some("/connections/oauth-callback?server_id=abc"),
                "access_denied",
            ),
            "http://localhost:5173/connections/oauth-callback?server_id=abc&status=failed&reason=access_denied"
        );
    }

    #[test]
    fn sanitize_reason_caps_length_and_normalizes() {
        let long = "x".repeat(200);
        let out = sanitize_reason(&long);
        assert_eq!(out.len(), 64);
        assert!(out.chars().all(|c| c == 'x'));
    }

    #[test]
    fn encode_query_value_passes_through_safe_chars() {
        assert_eq!(encode_query_value("abc_123-XYZ.~"), "abc_123-XYZ.~");
    }

    #[test]
    fn encode_query_value_percent_encodes_unsafe_chars() {
        assert_eq!(encode_query_value("a b/c"), "a%20b%2Fc");
    }
}
