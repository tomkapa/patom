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
    OAuthError, PatomPendingCtx, ResumeCtx, SlackPingCtx, StartCtx, handle_callback,
    start_authorization,
};
use crate::mcp::{
    CatalogUpsert, ConnectionStatus, CredentialPayload, DiscoveredTool,
    MCP_CREDENTIAL_READ_TIMEOUT, McpAuthKind, McpCatalogDescription, McpCatalogDisplayName,
    McpCatalogId, McpClient, McpCredentialWrite, McpDescription, McpError, McpServerCreate,
    McpServerId, McpServerRecord, McpServerUpdate, McpTransport, OAUTH2_KIND_LABEL,
    OAuthAuthorizeExtras,
};
use crate::tools::{DEFAULT_TOOL_CALLS_PAGE, MAX_TOOL_CALLS_PAGE, ToolCallRowId};

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
}

/// Public router (no auth middleware) for the OAuth callback. The browser
/// is returning from the vendor's consent screen with `?state=&code=`;
/// CSRF protection comes from the PKCE `state` column being a one-shot
/// row, not the session cookie. Merged into the public subtree alongside
/// `auth::router()`.
pub(super) fn oauth_callback_router() -> Router<AppState> {
    Router::new().route("/mcp-oauth/callback", get(handle_oauth_callback))
}

/// Public Slack-connect router: `GET /slack/mcp/connect?token=...`.
/// Lives next to the OAuth callback because the handler reuses the
/// `install_from_catalog` + `start_oauth` plumbing in this module.
/// Auth is the signed token, not a cookie.
pub(super) fn slack_connect_router() -> Router<AppState> {
    Router::new().route("/slack/mcp/connect", get(handle_slack_connect))
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
    /// Display name written to the auto-created tenant-custom
    /// `mcp_catalog` row when the full form is used. Ignored on the
    /// short-form path (the existing catalog row carries its own).
    /// Defaults to the `catalog_id` when omitted.
    #[serde(default)]
    display_name: Option<String>,
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

    let (config, catalog_auth_kind) = resolve_catalog_for_create(
        &state,
        principal.active_org_id,
        &catalog_id,
        payload.config,
        payload.display_name.as_deref(),
        credentials_payload.as_ref(),
    )
    .await?;
    reject_oauth_with_inline_credentials(catalog_auth_kind, credentials_payload.as_ref())?;
    let connection_status = initial_connection_status(catalog_auth_kind);

    let record = state
        .mcp_store
        .create(McpServerCreate {
            org_id: principal.active_org_id,
            created_by_user_id: principal.user_id,
            catalog_id,
            config,
            description,
            enabled: payload.enabled.unwrap_or(true),
            connection_status,
        })
        .await?;
    let credentials_kind = persist_inline_credentials(
        &state,
        record.id,
        principal.active_org_id,
        credentials_payload,
    )
    .await?;

    state.mcp_refresh.request();
    Ok((
        StatusCode::CREATED,
        Json(McpServerResponse::from_record(record, credentials_kind)),
    ))
}

/// Resolve the `(config, catalog_auth_kind)` pair for [`create_mcp_server`].
///
/// Two paths split on whether the operator supplied `config`:
///   * Short-form (`config = None`) — wires an existing catalog
///     entry (built-in or pre-seeded). We just read the entry's
///     `default_transport` and proceed.
///   * Full-form (`config = Some`) — registers a brand-new
///     tenant-custom integration. We ensure the matching
///     `mcp_catalog` row exists first so the trigger on `mcp_servers`
///     sees a valid parent. Insert-if-absent (not upsert): the
///     first creator owns the metadata, and re-posts cannot mutate
///     it — otherwise the parallel `mcp_servers` insert's 409
///     would silently leak display_name / description / transport
///     / auth_kind changes on a request whose top-line response is
///     a refusal.
///
/// `catalog_auth_kind` is what the row *actually* stores; the
/// `connection_status` derivation downstream branches on it.
async fn resolve_catalog_for_create(
    state: &AppState,
    org_id: crate::auth::OrgId,
    catalog_id: &McpCatalogId,
    config: Option<McpTransport>,
    display_name: Option<&str>,
    credentials_payload: Option<&CredentialPayload>,
) -> Result<(McpTransport, McpAuthKind), HttpError> {
    if let Some(c) = config {
        let auth_kind = register_custom_catalog_entry(
            state,
            org_id,
            catalog_id,
            display_name,
            &c,
            credentials_payload,
        )
        .await?;
        return Ok((c, auth_kind));
    }
    let entry = state
        .mcp_catalog
        .get_for_org(org_id, catalog_id)
        .await?
        .ok_or_else(|| {
            HttpError::Mcp(crate::mcp::McpError::CatalogIdUnknown(catalog_id.clone()))
        })?;
    Ok((entry.default_transport, entry.auth_kind))
}

/// Reject the OAuth-catalog + inline-credentials combo at the boundary.
/// The only credential shape the wire format allows here is
/// `static_headers` (OAuth flows through the dedicated `oauth/start`
/// route), and a server whose catalog auth_kind is OAuth2 cannot serve
/// traffic from static headers — the refresher would publish it as `Ok`
/// indefinitely while every tool call 401s. Surface as `InvalidConfig`
/// (HTTP 400) so the FE can explain the mismatch.
fn reject_oauth_with_inline_credentials(
    catalog_auth_kind: McpAuthKind,
    credentials_payload: Option<&CredentialPayload>,
) -> Result<(), HttpError> {
    if matches!(catalog_auth_kind, McpAuthKind::OAuth2) && credentials_payload.is_some() {
        return Err(HttpError::Mcp(crate::mcp::McpError::InvalidConfig(
            "this catalog entry uses OAuth — connect via /oauth/start \
             instead of supplying inline credentials"
                .into(),
        )));
    }
    Ok(())
}

/// Park OAuth rows in `AuthPending` so the refresher skips them until
/// the callback's `mark_connected` flips them to `Ok`. Every other shape
/// (static headers, no-auth custom server, …) is ready to serve from
/// this request and starts in `Ok` so the refresher tries it
/// immediately.
fn initial_connection_status(catalog_auth_kind: McpAuthKind) -> ConnectionStatus {
    if matches!(catalog_auth_kind, McpAuthKind::OAuth2) {
        ConnectionStatus::AuthPending
    } else {
        ConnectionStatus::Ok
    }
}

/// Seal-and-write the inline credentials supplied alongside a create
/// (always `static_headers` per the wire format). Returns the kind
/// label so the create response can echo it.
async fn persist_inline_credentials(
    state: &AppState,
    server_id: McpServerId,
    org_id: crate::auth::OrgId,
    payload: Option<CredentialPayload>,
) -> Result<Option<String>, HttpError> {
    let Some(payload) = payload else {
        return Ok(None);
    };
    let kind = payload.kind_label().to_owned();
    state
        .mcp_credentials
        .upsert(McpCredentialWrite {
            server_id,
            org_id,
            payload,
        })
        .await?;
    Ok(Some(kind))
}

/// Ensure the tenant-custom `mcp_catalog` row backing the full-form
/// custom-URL create exists, returning the `auth_kind` actually stored
/// in the DB so the caller can drive `connection_status`. Driven only
/// from [`create_mcp_server`]; factored out to keep that handler under
/// the 70-line ceiling.
///
/// `auth_kind` for a freshly-inserted row is derived from the request:
/// any inline credential is necessarily `static_headers` (the
/// `CredentialInput` wire shape excludes OAuth — those flow through
/// the dedicated `oauth/start` route), and a row with no credentials
/// starts as `none`. OAuth-style custom servers have no wiring path
/// here today. For a pre-existing row, the stored value wins; the
/// store's insert-if-absent semantics guarantee the row's metadata
/// can't be mutated by a request that's about to 409 on the server
/// uniqueness check.
async fn register_custom_catalog_entry(
    state: &AppState,
    org_id: crate::auth::OrgId,
    catalog_id: &McpCatalogId,
    display_name: Option<&str>,
    config: &McpTransport,
    credentials_payload: Option<&CredentialPayload>,
) -> Result<McpAuthKind, HttpError> {
    let display_name = McpCatalogDisplayName::try_from(display_name.unwrap_or(catalog_id.as_str()))
        .map_err(HttpError::Parse)?;
    let description =
        McpCatalogDescription::try_from("Custom MCP server.").map_err(HttpError::Parse)?;
    let auth_kind = if credentials_payload.is_some() {
        McpAuthKind::StaticHeaders
    } else {
        McpAuthKind::None
    };
    let stored_auth_kind = state
        .mcp_catalog
        .ensure_org_scoped(CatalogUpsert {
            org_id,
            id: catalog_id,
            display_name: &display_name,
            description: &description,
            default_transport: config,
            auth_kind,
            now: state.clock.now_utc(),
        })
        .await?;
    Ok(stored_auth_kind)
}

/// Idempotent "wire this catalog entry for this org" helper.
///
/// Used by:
///   * the `POST /mcp-servers` short-form path implicitly via
///     [`create_mcp_server`] when the operator has a session cookie;
///   * the public `GET /slack/mcp/connect` handler, which has only a
///     signed token and must mint or reuse the existing server row.
///
/// Returns the existing `mcp_servers` row when one already exists for
/// `(org_id, catalog_id)` instead of erroring — the Slack flow is
/// expected to be retried (vendor consent page reload, Slack double-
/// click), and we never want two server rows for the same wiring.
pub(super) async fn install_from_catalog(
    state: &AppState,
    org_id: crate::auth::OrgId,
    user_id: crate::auth::UserId,
    catalog_id: &McpCatalogId,
) -> Result<McpServerRecord, HttpError> {
    // Reuse: scan the existing org rows for the same catalog id.
    let existing = state.mcp_store.list_for_org(org_id).await?;
    if let Some(row) = existing.into_iter().find(|r| r.catalog_id == *catalog_id) {
        return Ok(row);
    }
    let entry = state
        .mcp_catalog
        .get_for_org(org_id, catalog_id)
        .await?
        .ok_or_else(|| {
            HttpError::Mcp(crate::mcp::McpError::CatalogIdUnknown(catalog_id.clone()))
        })?;
    let record = state
        .mcp_store
        .create(McpServerCreate {
            org_id,
            created_by_user_id: user_id,
            catalog_id: catalog_id.clone(),
            config: entry.default_transport,
            description: None,
            enabled: true,
            connection_status: ConnectionStatus::AuthPending,
        })
        .await?;
    Ok(record)
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
    // Identity tables (`users`) are intentionally REVOKED from `patom_app`
    // (migration 14), so the creator-email enrichment runs as a second
    // round-trip through the privileged `users` store after the tx commits.
    // Pin the active org: RLS gates on membership in any org, not the
    // active one, so a multi-org user would otherwise see every
    // membership's servers at once.
    let mut tx = crate::auth::begin_as(&state.pool, &principal).await?;
    let rows = sqlx::query_as::<_, McpServerRowForList>(
        "SELECT s.id, s.org_id, s.catalog_id, s.enabled, s.config, s.description, \
                s.last_seen_at, s.last_error, s.discovered_tools, \
                s.created_by_user_id, s.connection_status, s.created_at, s.updated_at, \
                c.kind AS credentials_kind \
         FROM mcp_servers s \
         LEFT JOIN mcp_server_credentials c ON c.server_id = s.id \
         WHERE s.org_id = $1 \
         ORDER BY s.catalog_id ASC",
    )
    .bind(principal.active_org_id)
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
    // Pin the active org (see `list_mcp_servers`): a multi-org user must
    // not read a server that lives in a non-active workspace by id.
    let mut tx = crate::auth::begin_as(&state.pool, &principal).await?;
    let row = sqlx::query_as::<_, McpServerRowForList>(
        "SELECT s.id, s.org_id, s.catalog_id, s.enabled, s.config, s.description, \
                s.last_seen_at, s.last_error, s.discovered_tools, \
                s.created_by_user_id, s.connection_status, s.created_at, s.updated_at, \
                c.kind AS credentials_kind \
         FROM mcp_servers s \
         LEFT JOIN mcp_server_credentials c ON c.server_id = s.id \
         WHERE s.id = $1 AND s.org_id = $2",
    )
    .bind(id)
    .bind(principal.active_org_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(AuthError::from)?;
    tx.commit().await.map_err(AuthError::from)?;
    let mut row = row.ok_or(HttpError::NotFound)?;
    // Identity tables (`users`) are REVOKED from `patom_app` (migration
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
            row.token_expires_at = oauth.approx_expires_at();
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
        patom.user.id = %principal.user_id,
        patom.org.id = %principal.active_org_id,
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
        patom.mcp.url = %url.as_str(),
        patom.mcp.discovered = discovered.len(),
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
    /// revoked from `patom_app`).
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
const OAUTH_PENDING_TTL: std::time::Duration = std::time::Duration::from_mins(10);

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
    /// Resume context — populated by callers driving the start flow on
    /// behalf of a live conversation. When both `thread_id` and
    /// `agent_id` are present, the OAuth callback appends a synthetic
    /// continuation prompt into the thread so the agent loop resumes
    /// without the user typing anything. Universal across channels
    /// (web UI, Slack adapter, future Lark / Teams).
    ///
    /// Both-or-neither: the handler returns 400 if exactly one is set.
    #[serde(default)]
    thread_id: Option<crate::threads::ThreadId>,
    #[serde(default)]
    agent_id: Option<AgentId>,
}

#[derive(Debug, Serialize)]
struct OAuthStartResponse {
    authorize_url: String,
}

/// `POST /mcp-servers/{id}/oauth/start` — kick off the browser flow.
///
/// Single thin wrapper over `oauth::session::start_authorization`. The
/// branching on `client_source` (Platform vs DCR vs None) lives there;
/// discovery, PKCE, DCR (when needed), and authorize-URL construction
/// are all `rmcp::transport::auth`'s job now. Patom owns:
///
///   * The pending-row context (server_id, user_id, org_id, redirect_to,
///     resume_ctx, slack_ctx) captured into `PatomStateStore::writer`.
///   * Tenant validation (the catalog read enforces the principal's
///     active_org_id).
///   * Vendor-quirk extras (e.g. `access_type=offline` for Google) read
///     from `mcp_catalog.authorize_extra_params`.
async fn start_oauth(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<Uuid>,
    Json(body): Json<OAuthStartRequest>,
) -> Result<Json<OAuthStartResponse>, HttpError> {
    let server_id = McpServerId::from(id);

    let catalog_entry = catalog_entry_for_server(&state, principal.active_org_id, server_id)
        .await?
        .ok_or_else(|| {
            HttpError::BadRequest(
                "server's catalog entry no longer exists — re-wire the server".into(),
            )
        })?;
    let server = state
        .mcp_store
        .read(server_id, principal.active_org_id)
        .await
        .map_err(|_| HttpError::NotFound)?;
    let effective_scope = effective_oauth_scope(
        body.scope.as_deref(),
        catalog_entry.default_scope.as_deref(),
    );
    let extras_owned = authorize_extras_owned(catalog_entry.authorize_extra_params.as_ref());
    let resume_ctx = parse_resume_ctx(body.thread_id, body.agent_id)?;
    let redirect_uri = format!("{}{}", state.oauth_redirect_base, OAUTH_CALLBACK_PATH);
    let expires_at = state.clock.now_utc()
        + chrono::Duration::from_std(OAUTH_PENDING_TTL)
            .expect("invariant: OAUTH_PENDING_TTL fits in chrono::Duration");

    let writer = state.mcp_oauth_pending.clone().writer(PatomPendingCtx {
        server_id,
        user_id: principal.user_id,
        org_id: principal.active_org_id,
        redirect_to: body.redirect_to.clone(),
        resume_ctx,
        slack_ctx: None,
        expires_at,
    });

    let server_url = match &server.config {
        McpTransport::Http { url } => url.as_str().to_owned(),
    };
    let scopes: Vec<String> = effective_scope
        .map(|s| s.split_whitespace().map(str::to_owned).collect())
        .unwrap_or_default();
    let http = reqwest::Client::builder().build().map_err(|e| {
        tracing::error!(event = "mcp.oauth.start.http_client_build_failed", error = %e);
        HttpError::Internal
    })?;

    let authorize_url = start_authorization(StartCtx {
        catalog: &catalog_entry,
        server_url,
        http,
        scopes,
        authorize_extras: extras_owned,
        redirect_uri,
        platform_clients: &state.platform_oauth_clients,
        credentials: state.mcp_credentials.clone(),
        state_store: writer,
        server_id,
        org_id: principal.active_org_id,
    })
    .await
    .map_err(map_oauth_err)?;

    Ok(Json(OAuthStartResponse { authorize_url }))
}

/// Both-or-neither parsing of the (thread_id, agent_id) pair on the
/// OAuth-start request body. A half-populated payload would bypass the
/// universal auto-continue silently — surface the misuse as a 400.
fn parse_resume_ctx(
    thread_id: Option<crate::threads::ThreadId>,
    agent_id: Option<crate::agents::AgentId>,
) -> Result<Option<ResumeCtx>, HttpError> {
    match (thread_id, agent_id) {
        (Some(thread_id), Some(agent_id)) => Ok(Some(ResumeCtx {
            thread_id,
            agent_id,
        })),
        (None, None) => Ok(None),
        _ => Err(HttpError::BadRequest(
            "thread_id and agent_id must both be present or both absent".into(),
        )),
    }
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

#[allow(clippy::too_many_lines)] // composition path; each step is a typed branch with its own redirect
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

    // Recover patom-side context (server_id, org_id, redirect_to,
    // resume_ctx, slack_ctx) *without* deleting the row — rmcp's
    // `OAuthState::handle_callback` will fire `StateStore::delete`
    // internally after the code exchange succeeds.
    let pending = match state.mcp_oauth_pending.read_pending_ctx(state_val).await {
        Ok(Some(p)) => p,
        Ok(None) => {
            return Err(CallbackFail {
                redirect_to: None,
                reason: "unknown_or_expired_state",
            });
        }
        Err(e) => {
            tracing::error!(event = "mcp.oauth.callback.read_ctx_failed", error = ?e);
            return Err(CallbackFail {
                redirect_to: None,
                reason: "internal_error",
            });
        }
    };

    // Look up the catalog row + the server's URL so we know which
    // discovery + client config to feed rmcp.
    let catalog = match catalog_entry_for_server(state, pending.org_id, pending.server_id).await {
        Ok(Some(c)) => c,
        Ok(None) => {
            return Err(CallbackFail {
                redirect_to: pending.redirect_to.clone(),
                reason: "catalog_entry_missing",
            });
        }
        Err(e) => {
            tracing::error!(event = "mcp.oauth.callback.catalog_lookup_failed", error = ?e);
            return Err(CallbackFail {
                redirect_to: pending.redirect_to.clone(),
                reason: "internal_error",
            });
        }
    };
    let server = match state
        .mcp_store
        .read(pending.server_id, pending.org_id)
        .await
    {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(event = "mcp.oauth.callback.server_lookup_failed", error = ?e);
            return Err(CallbackFail {
                redirect_to: pending.redirect_to.clone(),
                reason: "internal_error",
            });
        }
    };
    let server_url = match &server.config {
        McpTransport::Http { url } => url.as_str().to_owned(),
    };
    let redirect_uri = format!("{}{}", state.oauth_redirect_base, OAUTH_CALLBACK_PATH);
    let http = match reqwest::Client::builder().build() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(event = "mcp.oauth.callback.http_build_failed", error = %e);
            return Err(CallbackFail {
                redirect_to: pending.redirect_to.clone(),
                reason: "internal_error",
            });
        }
    };
    let reader = state.mcp_oauth_pending.clone().reader();
    let now = state.clock.now_utc();

    if let Err(e) = handle_callback(
        &catalog,
        &server_url,
        http,
        code,
        state_val,
        &redirect_uri,
        &state.platform_oauth_clients,
        pending.server_id,
        pending.org_id,
        state.mcp_credentials.clone(),
        reader,
    )
    .await
    {
        tracing::warn!(event = "mcp.oauth.callback.exchange_failed", error = ?e);
        return Err(CallbackFail {
            redirect_to: pending.redirect_to.clone(),
            reason: "token_exchange_failed",
        });
    }

    if let Err(e) = mark_connected(state, pending.server_id, pending.org_id, now).await {
        tracing::error!(event = "mcp.oauth.callback.status_update_failed", error = ?e);
        return Err(CallbackFail {
            redirect_to: pending.redirect_to.clone(),
            reason: "internal_error",
        });
    }
    state.mcp_refresh.request();

    // Best-effort post-success behaviours; each is independent and never
    // fails the callback because the credential write has already
    // landed.
    let display_name = resolve_display_name(state, &pending).await;
    tokio::join!(
        do_auto_continue(state, &pending, state_val, &display_name),
        do_slack_ping(state, &pending, &display_name),
    );
    Ok(pending.redirect_to)
}

/// Single source of truth for the universal auto-continue prompt. The
/// frontend's previous `thread.wireRequest.resumePrompt` i18n string is
/// retired in favour of this constant so the BE callback owns the
/// canonical text across every channel.
const MCP_RESUME_PROMPT_TEMPLATE: &str = "I've connected {name}. Please continue.";

fn render_resume_prompt(display_name: &str) -> String {
    MCP_RESUME_PROMPT_TEMPLATE.replace("{name}", display_name)
}

/// Resolve the provider's display name for the post-success surfaces
/// (Slack ping + synthetic resume prompt). Returns `"the connector"` on
/// any lookup failure — better to fall back than block the callback,
/// since the credential write has already succeeded.
async fn resolve_display_name(state: &AppState, pending: &PatomPendingCtx) -> String {
    let Ok(server) = state
        .mcp_store
        .read(pending.server_id, pending.org_id)
        .await
    else {
        return "the connector".to_owned();
    };
    match state
        .mcp_catalog
        .get_for_org(pending.org_id, &server.catalog_id)
        .await
    {
        Ok(Some(entry)) => entry.display_name.as_str().to_owned(),
        _ => "the connector".to_owned(),
    }
}

/// Universal server-side auto-continue. Replaces the previous
/// FE-driven `POST /prompts` injection that lived on the web side; now
/// any channel that supplied `resume_ctx` (web UI, Slack adapter,
/// future Lark / Teams) gets the resume for free.
///
/// Idempotency: the key is derived from the OAuth `state` token (a
/// one-shot PKCE row) so vendor-side callback replays produce at most
/// one synthetic prompt.
async fn do_auto_continue(
    state: &AppState,
    pending: &PatomPendingCtx,
    state_token: &str,
    display_name: &str,
) {
    let Some(resume) = pending.resume_ctx else {
        return;
    };
    let content = match crate::types::Prompt::try_from(render_resume_prompt(display_name)) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, event = "mcp.oauth.callback.resume_prompt_invalid");
            return;
        }
    };
    let idem_raw = format!("mcp-resume:{state_token}");
    let idempotency_key = match crate::runtime::IdempotencyKey::try_from(idem_raw) {
        Ok(k) => k,
        Err(e) => {
            tracing::warn!(error = %e, event = "mcp.oauth.callback.resume_idem_invalid");
            return;
        }
    };
    match super::prompts::submit_internal(
        state,
        super::prompts::SubmitPromptParams {
            user_id: pending.user_id,
            org_id: pending.org_id,
            thread_id: Some(resume.thread_id),
            agent_id: Some(resume.agent_id),
            content,
            idempotency_key,
            // OAuth-resume continues an existing thread, so it's never a new
            // channel root.
            channel_id: None,
        },
    )
    .await
    {
        Ok(_) => tracing::info!(
            patom.thread.id = %resume.thread_id.as_uuid(),
            patom.agent.id = %resume.agent_id.as_uuid(),
            event = "mcp.oauth.callback.auto_continue_submitted",
        ),
        Err(e) => tracing::warn!(
            error = ?e,
            event = "mcp.oauth.callback.auto_continue_failed",
        ),
    }
}

/// Post the `✓ Connected — <Provider>` follow-up into the originating
/// Slack thread. Best effort — Slack returning an error never blocks
/// the credential write or the universal auto-continue.
async fn do_slack_ping(state: &AppState, pending: &PatomPendingCtx, display_name: &str) {
    let Some(ctx) = pending.slack_ctx.as_ref() else {
        return;
    };
    let Some(slack) = state.slack.as_ref() else {
        tracing::warn!(event = "mcp.oauth.callback.slack_ping_without_state");
        return;
    };
    let team_id = match crate::slack::SlackTeamId::try_from(ctx.team_id.as_str()) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(error = %e, event = "mcp.oauth.callback.bad_team_id");
            return;
        }
    };
    let channel_id = match crate::slack::SlackChannelId::try_from(ctx.channel_id.as_str()) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, event = "mcp.oauth.callback.bad_channel_id");
            return;
        }
    };
    let thread_ts = match crate::slack::SlackThreadTs::try_from(ctx.thread_ts.as_str()) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(error = %e, event = "mcp.oauth.callback.bad_thread_ts");
            return;
        }
    };
    let workspace = match slack.workspaces.read_by_team(&team_id).await {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!(error = ?e, event = "mcp.oauth.callback.slack_workspace_missing");
            return;
        }
    };
    if let Err(e) = slack
        .poster
        .post(crate::slack::poster::PostRequest {
            token: workspace.bot_token.clone(),
            channel: channel_id,
            thread_ts: Some(thread_ts),
            body: crate::slack::poster::PostBody::Text(format!("✓ Connected — {display_name}")),
            username: "Patom".to_owned(),
            icon_url: None,
        })
        .await
    {
        tracing::warn!(error = ?e, event = "mcp.oauth.callback.slack_ping_post_failed");
    }
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
/// lands on the right page. Reads the patom-side ctx then deletes the
/// row so a vendor retry on the same `state` token can't double-fire.
/// Swallows errors and absence — the failure redirect falls back to `/`
/// either way.
async fn consume_pending_for_redirect(state: &AppState, raw_state: Option<&str>) -> Option<String> {
    use rmcp::transport::auth::StateStore as _;
    let raw = raw_state?;
    let ctx = match state.mcp_oauth_pending.read_pending_ctx(raw).await {
        Ok(Some(c)) => c,
        Ok(None) => return None,
        Err(e) => {
            tracing::warn!(
                event = "mcp.oauth.callback.read_ctx_for_redirect_failed",
                error = ?e,
            );
            return None;
        }
    };
    let _ = state.mcp_oauth_pending.clone().reader().delete(raw).await;
    ctx.redirect_to
}

/// Build the successful-callback redirect URL. The base path is the
/// caller-supplied `redirect_to` if it survives `sanitize_return_to`
/// (relative path, ≤2048 bytes); otherwise `/`. A `status=ok` marker is
/// appended so the FE polling loop terminates immediately on first nav.
///
/// When `web_base` is `Some(origin)` (set via `PATOM_WEB_BASE_URL` for
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

/// Pick the OAuth `scope` value to forward into the authorize URL.
///
/// Precedence: request override → catalog `default_scope` → `None`.
/// An empty / whitespace-only request override is treated as if it
/// wasn't sent — `&scope=` in the authorize URL would otherwise cause
/// Google to reject the redirect with `Missing required parameter:
/// scope`, masking the catalog-side default we just looked up.
fn effective_oauth_scope<'a>(
    request: Option<&'a str>,
    catalog_default: Option<&'a str>,
) -> Option<&'a str> {
    request
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or(catalog_default)
}

/// Catalog-derived OAuth config for an `mcp_servers` row: the pinned
/// `default_scope` (RFC 6749 §3.3 wire format) and any non-standard
/// authorize-URL params the vendor needs. Returned together so the
/// OAuth start paths make exactly one catalog lookup per request.
///
/// One-shot catalog lookup that returns the full
/// [`crate::mcp::McpCatalogEntry`] for `server_id`. The resolver branches
/// on the entry's `client_source`, and the start handler uses
/// `default_scope` + `authorize_extra_params` to assemble the authorize
/// URL — having the whole entry in hand keeps the start path readable.
///
/// Returns `Ok(None)` when the catalog row no longer exists (the server
/// was wired against an entry that was since deleted); the caller maps
/// that to a misconfiguration error.
async fn catalog_entry_for_server(
    state: &AppState,
    org_id: crate::auth::OrgId,
    server_id: McpServerId,
) -> Result<Option<crate::mcp::McpCatalogEntry>, HttpError> {
    let server = state
        .mcp_store
        .read(server_id, org_id)
        .await
        .map_err(HttpError::Mcp)?;
    state
        .mcp_catalog
        .get_for_org(org_id, &server.catalog_id)
        .await
        .map_err(HttpError::Mcp)
}

/// Owned copy of [`OAuthAuthorizeExtras`] for handoff into
/// `oauth::session::start_authorization` (which can't borrow from the
/// short-lived `McpCatalogEntry`).
fn authorize_extras_owned(extras: Option<&OAuthAuthorizeExtras>) -> Vec<(String, String)> {
    extras
        .map(|e| {
            e.as_slice()
                .iter()
                .map(|p| (p.key.as_str().to_owned(), p.value.as_str().to_owned()))
                .collect()
        })
        .unwrap_or_default()
}

// ────────────────────────────────────────────────────────────────────────
// Slack-connect: GET /slack/mcp/connect?token=...
// ────────────────────────────────────────────────────────────────────────
//
// Public no-cookie route. The Block Kit connection-request card's
// `Connect <Provider>` button points here with a `connect_link`-signed
// token. We verify the token, resolve the Slack user → patom user via
// the workspace installer fallback (same identity model as
// `app_mention`), idempotently install the catalog entry as an
// `mcp_servers` row, mint PKCE + state, persist the pending row
// (with both `resume_ctx` and `slack_ctx` populated), and 302 the
// browser to the vendor's consent screen.

#[derive(Debug, Deserialize)]
struct SlackConnectQuery {
    #[serde(default)]
    token: Option<String>,
}

/// Render a minimal HTML error page. The Connect button lives in a
/// Slack thread; the browser tab is the only feedback channel back to
/// the user — never JSON.
fn slack_connect_error_html(reason: &str) -> axum::response::Response {
    use axum::response::IntoResponse as _;
    let body = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>Couldn't start connection</title></head>\
         <body style=\"font-family: -apple-system, system-ui, sans-serif; \
                       max-width: 32rem; margin: 4rem auto; line-height: 1.5; padding: 0 1rem;\">\
         <h1>Couldn't start connection</h1>\
         <p>{reason}</p>\
         <p>Return to Slack and try the Connect button again from a fresh agent reply.</p>\
         </body></html>",
        reason = ammonia_escape(reason),
    );
    let mut resp = axum::response::Html(body).into_response();
    *resp.status_mut() = StatusCode::BAD_REQUEST;
    resp
}

/// Minimal HTML escape — the reason strings are all internal short
/// labels, so escaping the five mandatory characters is enough.
fn ammonia_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

#[tracing::instrument(name = "slack.connect.start", skip_all)]
async fn handle_slack_connect(
    State(state): State<AppState>,
    Query(q): Query<SlackConnectQuery>,
) -> axum::response::Response {
    match handle_slack_connect_inner(&state, q.token.as_deref()).await {
        Ok(authorize_url) => {
            use axum::response::IntoResponse as _;
            Redirect::to(&authorize_url).into_response()
        }
        Err(reason) => slack_connect_error_html(reason),
    }
}

/// Composition of the Slack-connect handler split out of the axum
/// wrapper so its body stays under the 100-line ceiling. Errors are
/// short human-readable strings rendered into the error HTML.
async fn handle_slack_connect_inner(
    state: &AppState,
    token: Option<&str>,
) -> Result<String, &'static str> {
    let slack = state
        .slack
        .as_ref()
        .ok_or("Slack integration is not enabled on this Patom deployment.")?;
    let token = token.ok_or("Missing token.")?;

    let now = slack.clock.now_unix_secs();
    let claims = crate::slack::connect_link::verify_connect(
        slack.signing_secret.expose().as_bytes(),
        token,
        now,
    )
    .ok_or_else(|| {
        tracing::warn!(event = "slack.connect.bad_token");
        "This Connect link is expired or invalid. Ask the agent to send the request again."
    })?;

    let (user_id, org_id) = resolve_slack_connect_identity(slack, &claims).await?;
    let server = install_from_catalog(state, org_id, user_id, &claims.catalog_id)
        .await
        .map_err(|e| {
            tracing::warn!(error = ?e, event = "slack.connect.install_failed");
            "Couldn't install this connector. The catalog entry may be missing."
        })?;

    let authorize_url =
        slack_connect_build_oauth_start(state, &claims, server.id, user_id, org_id).await?;

    tracing::info!(
        patom.mcp.catalog_id = %claims.catalog_id,
        patom.org.id = %org_id.as_uuid(),
        patom.user.id = %user_id.as_uuid(),
        event = "slack.connect.redirect",
    );
    Ok(authorize_url)
}

async fn resolve_slack_connect_identity(
    slack: &crate::slack::SlackAppState,
    claims: &crate::slack::connect_link::SlackConnectClaims,
) -> Result<(crate::auth::UserId, crate::auth::OrgId), &'static str> {
    let workspace = slack
        .workspaces
        .read_by_team(&claims.team_id)
        .await
        .map_err(|e| {
            tracing::warn!(error = ?e, event = "slack.connect.workspace_missing");
            "Couldn't find the Slack workspace for this connection request."
        })?;
    match slack
        .identities
        .lookup(&claims.team_id, &claims.slack_user_id)
        .await
    {
        Ok(Some(linked)) => Ok((linked.user_id, linked.org_id)),
        Ok(None) => Ok((workspace.installed_by_user_id, workspace.org_id)),
        Err(e) => {
            tracing::error!(error = ?e, event = "slack.connect.identity_lookup_failed");
            Err("Internal error resolving Slack identity.")
        }
    }
}

/// Slack-flow start: same shape as the web-flow's `start_oauth`, but
/// the patom-side context carries Slack thread metadata so the
/// post-success ping fires after the callback.
async fn slack_connect_build_oauth_start(
    state: &AppState,
    claims: &crate::slack::connect_link::SlackConnectClaims,
    server_id: McpServerId,
    user_id: crate::auth::UserId,
    org_id: crate::auth::OrgId,
) -> Result<String, &'static str> {
    let catalog_entry = catalog_entry_for_server(state, org_id, server_id)
        .await
        .map_err(|e| {
            tracing::warn!(error = ?e, event = "slack.connect.catalog_lookup_failed");
            "Internal error resolving the connector's catalog entry."
        })?
        .ok_or_else(|| {
            tracing::warn!(event = "slack.connect.catalog_entry_missing");
            "Connector's catalog entry no longer exists."
        })?;
    let server = state.mcp_store.read(server_id, org_id).await.map_err(|e| {
        tracing::warn!(error = ?e, event = "slack.connect.server_lookup_failed");
        "Couldn't load the connector's server row."
    })?;
    let server_url = match &server.config {
        McpTransport::Http { url } => url.as_str().to_owned(),
    };
    let redirect_uri = format!("{}{}", state.oauth_redirect_base, OAUTH_CALLBACK_PATH);
    let scopes: Vec<String> = catalog_entry
        .default_scope
        .as_deref()
        .map(|s| s.split_whitespace().map(str::to_owned).collect())
        .unwrap_or_default();
    let extras_owned = authorize_extras_owned(catalog_entry.authorize_extra_params.as_ref());
    let expires_at = state.clock.now_utc()
        + chrono::Duration::from_std(OAUTH_PENDING_TTL)
            .expect("invariant: OAUTH_PENDING_TTL fits in chrono::Duration");

    let writer = state.mcp_oauth_pending.clone().writer(PatomPendingCtx {
        server_id,
        user_id,
        org_id,
        redirect_to: None,
        resume_ctx: Some(ResumeCtx {
            thread_id: claims.thread_id,
            agent_id: claims.agent_id,
        }),
        slack_ctx: Some(SlackPingCtx {
            team_id: claims.team_id.as_str().to_owned(),
            channel_id: claims.channel_id.as_str().to_owned(),
            thread_ts: claims.thread_ts.as_str().to_owned(),
        }),
        expires_at,
    });

    let http = reqwest::Client::builder().build().map_err(|e| {
        tracing::error!(event = "slack.connect.http_build_failed", error = %e);
        "Internal error creating the HTTP client."
    })?;

    start_authorization(StartCtx {
        catalog: &catalog_entry,
        server_url,
        http,
        scopes,
        authorize_extras: extras_owned,
        redirect_uri,
        platform_clients: &state.platform_oauth_clients,
        credentials: state.mcp_credentials.clone(),
        state_store: writer,
        server_id,
        org_id,
    })
    .await
    .map_err(|e| {
        tracing::warn!(event = "slack.connect.start_authorization_failed", error = ?e);
        "Couldn't start the OAuth flow."
    })
}

fn map_oauth_err(err: OAuthError) -> HttpError {
    match err {
        OAuthError::Rmcp(ref inner) => match inner {
            rmcp::transport::auth::AuthError::RegistrationFailed(_)
            | rmcp::transport::auth::AuthError::TokenExchangeFailed(_)
            | rmcp::transport::auth::AuthError::TokenRefreshFailed(_)
            | rmcp::transport::auth::AuthError::MetadataError(_)
            | rmcp::transport::auth::AuthError::NoAuthorizationSupport => {
                tracing::warn!(error = %err, "mcp.oauth.upstream_error");
                HttpError::BadRequest(err.to_string())
            }
            _ => {
                tracing::error!(error = %err, "mcp.oauth.internal_error");
                HttpError::Internal
            }
        },
        OAuthError::Crypto(_) | OAuthError::Db(_) => {
            tracing::error!(error = %err, "mcp.oauth.internal_error");
            HttpError::Internal
        }
        OAuthError::Misconfigured(_) => {
            tracing::warn!(error = %err, "mcp.oauth.misconfigured");
            HttpError::BadRequest(err.to_string())
        }
        OAuthError::Timeout(_) => {
            tracing::warn!(error = %err, "mcp.oauth.upstream_timeout");
            HttpError::BadRequest(err.to_string())
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

    // ────────────────────────────────────────────────────────────────
    // effective_oauth_scope precedence — the failure mode this guards
    // is `&scope=` in the Google authorize URL, which the AS rejects
    // with `Missing required parameter: scope`. Pre-fix, a request
    // body that sent `"scope": ""` won over the catalog default.
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn effective_oauth_scope_request_override_wins() {
        assert_eq!(
            effective_oauth_scope(Some("openid profile"), Some("default")),
            Some("openid profile"),
        );
    }

    #[test]
    fn effective_oauth_scope_empty_request_falls_back_to_default() {
        assert_eq!(
            effective_oauth_scope(Some(""), Some("default scope")),
            Some("default scope"),
        );
    }

    #[test]
    fn effective_oauth_scope_whitespace_request_falls_back_to_default() {
        assert_eq!(
            effective_oauth_scope(Some("   "), Some("default scope")),
            Some("default scope"),
        );
    }

    #[test]
    fn effective_oauth_scope_request_is_trimmed_when_non_empty() {
        assert_eq!(
            effective_oauth_scope(Some("  openid profile  "), Some("default")),
            Some("openid profile"),
        );
    }

    #[test]
    fn effective_oauth_scope_no_inputs_yields_none() {
        // DCR vendors (Notion / Linear) rely on this — the AS supplies
        // its own default at registration, so omitting `scope` from the
        // authorize URL is correct.
        assert_eq!(effective_oauth_scope(None, None), None);
    }
}
