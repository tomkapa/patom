#![allow(clippy::implicit_hasher)] // platform_clients is the shape used everywhere
#![allow(clippy::too_long_first_doc_paragraph)] // module doc inherently sets full context

//! Glue between patom's tenant model and rmcp's `OAuthState` lifecycle.
//!
//! The OAuth pipeline for an MCP server has three hand-offs that this
//! module shepherds:
//!
//!   1. **Connect path** — `registry::connect_oauth` calls
//!      [`build_manager_for_request`] to spin up an `AuthorizationManager`
//!      bound to one `(server_id, org_id)` tuple via a
//!      [`PatomCredentialStore`]. The manager is wrapped in
//!      `rmcp::transport::auth::AuthClient<reqwest::Client>` which
//!      injects bearer tokens and refreshes on 401 — refresh-on-acquire
//!      and refresh-on-401 are rmcp's responsibility now, not patom's.
//!   2. **Start path** — `POST /mcp-servers/{id}/oauth/start` calls
//!      [`start_authorization`]. The function mirrors codex's
//!      `perform_oauth_login::start_authorization`
//!      (codex-rs/rmcp-client/src/perform_oauth_login.rs:604-633),
//!      branching on `mcp_catalog.client_source`:
//!         * `Platform` — preconfigured client_id from env, no DCR
//!         * `Dcr` — rmcp's `OAuthState::new` does discovery + DCR + PKCE
//!         * `None` — error (precondition violation)
//!   3. **Callback path** — `GET /mcp-oauth/callback` calls
//!      [`handle_callback`] after the patom-side context is recovered
//!      from `mcp_oauth_pending` (without deleting; rmcp's
//!      `StateStore::delete` is what removes the row).
//!
//! Multi-replica safety: `AuthorizationManager`s are constructed fresh
//! per request — all durable state (`StoredCredentials`,
//! `StoredAuthorizationState`) is in Postgres via the adapters in
//! [`super::credential_adapter`] and [`super::state_adapter`]. Refresh
//! dedup within one connection is owned by rmcp's `AuthClient`
//! (internal `Arc<Mutex<AuthorizationManager>>`); patom holds no
//! in-process cache of its own.

use std::collections::HashMap;

use rmcp::transport::auth::{
    AuthorizationManager, AuthorizationSession, CredentialStore as _, OAuthClientConfig, OAuthState,
};
use url::Url;

use crate::config::PlatformOAuthClient;
use crate::mcp::catalog::{ClientSource, McpCatalogEntry, UserOAuthClient, platform_env_middle};
use crate::mcp::credentials::SharedMcpCredentialStore;
use crate::mcp::types::McpServerId;

use super::credential_adapter::PatomCredentialStore;
use super::errors::OAuthError;
use super::state_adapter::PatomStateStore;

/// Per-CLAUDE.md §5 every network await is wrapped. This helper bounds an
/// rmcp-returning future by [`super::super::limits::MCP_CONNECT_TIMEOUT`]
/// (10 s — the same outer bound the registry's connect path already uses,
/// see [`crate::mcp::registry`]) and maps both the timeout and the inner
/// rmcp error into `OAuthError`. `op` is a low-cardinality name that names
/// the wrapped step so logs and traces can pinpoint which await tripped.
async fn bounded<T, F>(op: &'static str, fut: F) -> Result<T, OAuthError>
where
    F: std::future::Future<Output = Result<T, rmcp::transport::auth::AuthError>>,
{
    // `Box::pin` the future on the heap so the enclosing future stays
    // under clippy's `large_futures` threshold — rmcp's `OAuthState`
    // machine alone is ~16 KiB and trips the lint otherwise.
    tokio::time::timeout(crate::mcp::limits::MCP_CONNECT_TIMEOUT, Box::pin(fut))
        .await
        .map_err(|_| OAuthError::Timeout(op))?
        .map_err(OAuthError::from)
}

/// Build an `AuthorizationManager` for `(server_id, org_id)` bound to
/// `base_url`. Used by the registry's connect path; rmcp's `AuthClient`
/// wraps the returned manager and handles bearer injection + refresh.
///
/// The manager loads stored credentials from `credentials` on the first
/// `get_access_token` call — no eager fetch here.
pub async fn build_manager_for_request(
    server_id: McpServerId,
    org_id: crate::auth::OrgId,
    base_url: &str,
    http: reqwest::Client,
    credentials: SharedMcpCredentialStore,
) -> Result<AuthorizationManager, OAuthError> {
    let mut manager = bounded(
        "AuthorizationManager::new[connect]",
        AuthorizationManager::new(base_url),
    )
    .await?;
    manager.with_client(http).map_err(OAuthError::from)?;
    manager.set_credential_store(PatomCredentialStore::new(server_id, org_id, credentials));
    Ok(manager)
}

/// Inputs to [`start_authorization`].
#[allow(missing_debug_implementations)] // contains reqwest::Client (no Debug), trait-object stores
pub struct StartCtx<'a> {
    pub catalog: &'a McpCatalogEntry,
    pub server_url: String,
    pub http: reqwest::Client,
    pub scopes: Vec<String>,
    pub authorize_extras: Vec<(String, String)>,
    pub redirect_uri: String,
    pub platform_clients: &'a HashMap<String, PlatformOAuthClient>,
    /// The operator's own OAuth client, decrypted from the catalog row.
    /// Required when `catalog.client_source == UserSupplied`, ignored
    /// otherwise; the HTTP handler fetches it via
    /// [`crate::mcp::McpCatalogStore::oauth_client`].
    pub user_oauth_client: Option<UserOAuthClient>,
    pub credentials: SharedMcpCredentialStore,
    pub state_store: PatomStateStore,
    pub server_id: McpServerId,
    pub org_id: crate::auth::OrgId,
}

/// Begin an OAuth flow. Returns the URL the browser should be redirected
/// to. PKCE / CSRF are minted inside rmcp and persisted via the supplied
/// `state_store`.
///
/// Mirrors codex's branching on whether a client_id is preconfigured —
/// patom's `client_source` enum picks the branch up front so the
/// behavior is data-driven, not vendor-coded.
#[tracing::instrument(
    name = "mcp.oauth.session.start",
    skip_all,
    fields(
        patom.mcp.server.id = %ctx.server_id,
        patom.mcp.catalog_id = %ctx.catalog.id,
        patom.mcp.oauth.client_source = %ctx.catalog.client_source.as_str(),
    ),
)]
pub async fn start_authorization(ctx: StartCtx<'_>) -> Result<String, OAuthError> {
    let scope_refs: Vec<&str> = ctx.scopes.iter().map(String::as_str).collect();
    let url = match ctx.catalog.client_source {
        ClientSource::None => {
            return Err(OAuthError::Misconfigured(format!(
                "catalog `{id}` has client_source=none; OAuth flow not applicable",
                id = ctx.catalog.id
            )));
        }
        ClientSource::UserSupplied => {
            let client = ctx.user_oauth_client.as_ref().ok_or_else(|| {
                OAuthError::Misconfigured(format!(
                    "catalog `{id}` is user_supplied but no OAuth client was provided",
                    id = ctx.catalog.id
                ))
            })?;
            start_user_supplied(
                client,
                &ctx.server_url,
                ctx.http,
                &scope_refs,
                &ctx.redirect_uri,
                ctx.server_id,
                ctx.org_id,
                ctx.credentials,
                ctx.state_store,
            )
            .await?
        }
        ClientSource::Platform => {
            start_platform(
                ctx.catalog,
                ctx.platform_clients,
                &ctx.server_url,
                ctx.http,
                &scope_refs,
                &ctx.redirect_uri,
                ctx.server_id,
                ctx.org_id,
                ctx.credentials,
                ctx.state_store,
            )
            .await?
        }
        ClientSource::Dcr => {
            start_dcr(
                &ctx.server_url,
                ctx.http,
                &scope_refs,
                &ctx.redirect_uri,
                ctx.server_id,
                ctx.org_id,
                ctx.credentials,
                ctx.state_store,
            )
            .await?
        }
    };
    Ok(append_extra_params(url, &ctx.authorize_extras))
}

/// Platform branch: preconfigured client_id+secret from env, no DCR.
/// Mirrors codex `perform_oauth_login.rs:619-632`.
#[allow(clippy::too_many_arguments)]
async fn start_platform(
    catalog: &McpCatalogEntry,
    platform_clients: &HashMap<String, PlatformOAuthClient>,
    server_url: &str,
    http: reqwest::Client,
    scopes: &[&str],
    redirect_uri: &str,
    server_id: McpServerId,
    org_id: crate::auth::OrgId,
    credentials: SharedMcpCredentialStore,
    state_store: PatomStateStore,
) -> Result<String, OAuthError> {
    let key_catalog_id = catalog
        .platform_client_alias
        .as_ref()
        .unwrap_or(&catalog.id);
    let key = platform_env_middle(key_catalog_id);
    let env_creds = platform_clients.get(&key).ok_or_else(|| {
        OAuthError::Misconfigured(format!(
            "platform OAuth client for catalog `{id}` not configured (alias `{key_catalog_id}`)",
            id = catalog.id,
        ))
    })?;
    let mut manager = bounded(
        "AuthorizationManager::new[start_platform]",
        AuthorizationManager::new(server_url),
    )
    .await?;
    manager.with_client(http).map_err(OAuthError::from)?;
    manager.set_credential_store(PatomCredentialStore::new(server_id, org_id, credentials));
    manager.set_state_store(state_store);
    let metadata = bounded(
        "discover_metadata[start_platform]",
        manager.discover_metadata(),
    )
    .await?;
    manager.set_metadata(metadata);
    let config = OAuthClientConfig::new(
        env_creds.client_id.expose().to_owned(),
        redirect_uri.to_owned(),
    )
    .with_client_secret(env_creds.client_secret.expose().to_owned())
    .with_scopes(scopes.iter().map(|s| (*s).to_owned()).collect());
    manager.configure_client(config).map_err(OAuthError::from)?;
    bounded(
        "get_authorization_url[platform]",
        manager.get_authorization_url(scopes),
    )
    .await
}

/// User-supplied branch: the operator's own `client_id` (+ optional
/// `client_secret`) for a custom server URL, read from the encrypted
/// `mcp_catalog` row. A near-clone of [`start_platform`] that reads the
/// client from storage instead of env, and omits `with_client_secret`
/// for a public/PKCE client.
#[allow(clippy::too_many_arguments)]
async fn start_user_supplied(
    client: &UserOAuthClient,
    server_url: &str,
    http: reqwest::Client,
    scopes: &[&str],
    redirect_uri: &str,
    server_id: McpServerId,
    org_id: crate::auth::OrgId,
    credentials: SharedMcpCredentialStore,
    state_store: PatomStateStore,
) -> Result<String, OAuthError> {
    let mut manager = bounded(
        "AuthorizationManager::new[start_user_supplied]",
        AuthorizationManager::new(server_url),
    )
    .await?;
    manager.with_client(http).map_err(OAuthError::from)?;
    manager.set_credential_store(PatomCredentialStore::new(server_id, org_id, credentials));
    manager.set_state_store(state_store);
    let metadata = bounded(
        "discover_metadata[start_user_supplied]",
        manager.discover_metadata(),
    )
    .await?;
    manager.set_metadata(metadata);
    let config = user_supplied_client_config(client, redirect_uri, Some(scopes));
    manager.configure_client(config).map_err(OAuthError::from)?;
    bounded(
        "get_authorization_url[user_supplied]",
        manager.get_authorization_url(scopes),
    )
    .await
}

/// Build the rmcp client config for a user-supplied OAuth client. The
/// secret is attached only for a confidential client; a public/PKCE client
/// omits it (rmcp drives PKCE without a secret, same as the DCR path).
///
/// `scopes` is `Some` at authorize time and `None` for the callback's
/// code exchange — the granted scopes are bound by the authorize step, so
/// re-sending them on exchange is unnecessary.
fn user_supplied_client_config(
    client: &UserOAuthClient,
    redirect_uri: &str,
    scopes: Option<&[&str]>,
) -> OAuthClientConfig {
    let mut config = OAuthClientConfig::new(
        client.client_id.as_str().to_owned(),
        redirect_uri.to_owned(),
    );
    if let Some(scopes) = scopes {
        config = config.with_scopes(scopes.iter().map(|s| (*s).to_owned()).collect());
    }
    if let Some(secret) = client.client_secret.as_ref() {
        config = config.with_client_secret(secret.expose().to_owned());
    }
    config
}

/// DCR branch: rmcp's `OAuthState::new` registers a client dynamically
/// inside `start_authorization`. Mirrors codex `perform_oauth_login.rs:611-617`.
///
/// Patom-specific: rmcp's `register_client` configures the freshly
/// DCR-issued client_id in-memory on the `AuthorizationManager` but
/// never *persists* it. Codex relies on the same process holding the
/// `OAuthState` between start and callback, so the in-memory client
/// survives. Patom's flow crosses replicas (callback may land on a
/// different process), so we must persist the client_id at the end of
/// start. The callback then reads it back via [`PatomCredentialStore`]
/// and calls `configure_client_id` to rebuild the manager.
#[allow(clippy::too_many_arguments)]
async fn start_dcr(
    server_url: &str,
    http: reqwest::Client,
    scopes: &[&str],
    redirect_uri: &str,
    server_id: McpServerId,
    org_id: crate::auth::OrgId,
    credentials: SharedMcpCredentialStore,
    state_store: PatomStateStore,
) -> Result<String, OAuthError> {
    // Construct the manager manually so we can attach our stores
    // before discovery + DCR fire. OAuthState::new doesn't expose a
    // store-injection seam; emulating its body is the documented
    // workaround.
    let mut manager = bounded(
        "AuthorizationManager::new[start_dcr]",
        AuthorizationManager::new(server_url),
    )
    .await?;
    manager.with_client(http).map_err(OAuthError::from)?;
    manager.set_credential_store(PatomCredentialStore::new(
        server_id,
        org_id,
        credentials.clone(),
    ));
    manager.set_state_store(state_store);
    let mut state = OAuthState::Unauthorized(manager);
    bounded(
        "state.start_authorization[dcr]",
        state.start_authorization(scopes, redirect_uri, Some("Patom")),
    )
    .await?;
    let OAuthState::Session(session) = state else {
        return Err(OAuthError::Misconfigured(
            "DCR start_authorization returned unexpected OAuthState variant".into(),
        ));
    };
    // Persist the DCR-issued client_id with `token_response = None` so the
    // callback (potentially on a different replica) can call
    // `configure_client_id` before the code exchange runs. `get_credentials`
    // returns `(client_id, token_response)`; token_response is None at this
    // point — that's expected, we'll overwrite the row inside
    // `exchange_code_for_token` once the code is exchanged.
    let (client_id, _token) = session.get_credentials().await.map_err(OAuthError::from)?;
    let cred_store = PatomCredentialStore::new(server_id, org_id, credentials);
    let bootstrap =
        rmcp::transport::auth::StoredCredentials::new(client_id, None, Vec::new(), None);
    cred_store.save(bootstrap).await.map_err(OAuthError::from)?;
    Ok(session.get_authorization_url().to_owned())
}

/// Append vendor-specific authorize-URL extras (Google's
/// `access_type=offline`, `prompt=consent` — sourced from
/// `mcp_catalog.authorize_extra_params`). Rmcp's
/// `AuthorizationManager::get_authorization_url` doesn't expose a
/// `add_extra_param` hook, so we patch the query string here. The PKCE +
/// CSRF + scope params are untouched.
fn append_extra_params(url: String, extras: &[(String, String)]) -> String {
    if extras.is_empty() {
        return url;
    }
    let Ok(mut parsed) = Url::parse(&url) else {
        return url;
    };
    {
        let mut q = parsed.query_pairs_mut();
        for (k, v) in extras {
            q.append_pair(k, v);
        }
    }
    parsed.to_string()
}

/// Drive the callback. Recovers the `OAuthState` for the recorded
/// `(server_id, org_id)`, exchanges the code, persists the resulting
/// tokens via [`PatomCredentialStore`].
#[allow(clippy::too_many_arguments)]
#[tracing::instrument(
    name = "mcp.oauth.session.callback",
    skip_all,
    fields(
        patom.mcp.server.id = %server_id,
        patom.mcp.catalog_id = %catalog.id,
        patom.mcp.oauth.client_source = %catalog.client_source.as_str(),
    ),
)]
pub async fn handle_callback(
    catalog: &McpCatalogEntry,
    server_url: &str,
    http: reqwest::Client,
    code: &str,
    state: &str,
    redirect_uri: &str,
    platform_clients: &HashMap<String, PlatformOAuthClient>,
    user_oauth_client: Option<UserOAuthClient>,
    server_id: McpServerId,
    org_id: crate::auth::OrgId,
    credentials: SharedMcpCredentialStore,
    state_store: PatomStateStore,
) -> Result<(), OAuthError> {
    let mut manager = bounded(
        "AuthorizationManager::new[callback]",
        AuthorizationManager::new(server_url),
    )
    .await?;
    manager.with_client(http).map_err(OAuthError::from)?;
    manager.set_credential_store(PatomCredentialStore::new(
        server_id,
        org_id,
        credentials.clone(),
    ));
    manager.set_state_store(state_store);
    let metadata = bounded("discover_metadata[callback]", manager.discover_metadata()).await?;
    manager.set_metadata(metadata);

    // Re-configure the client identically to the start path so rmcp's
    // internal state matches the AS's record of this flow.
    configure_client_for_callback(
        &mut manager,
        catalog,
        redirect_uri,
        platform_clients,
        user_oauth_client.as_ref(),
        server_id,
        org_id,
        credentials,
    )
    .await?;

    let session = AuthorizationSession::for_scope_upgrade(manager, String::new(), redirect_uri);
    bounded(
        "session.handle_callback",
        session.handle_callback(code, state),
    )
    .await?;
    Ok(())
}

/// Branch on `client_source` and call `configure_client` on the manager with
/// the right config. Extracted from [`handle_callback`] to keep that function
/// under the §4 length ceiling.
#[allow(clippy::too_many_arguments)]
async fn configure_client_for_callback(
    manager: &mut AuthorizationManager,
    catalog: &McpCatalogEntry,
    redirect_uri: &str,
    platform_clients: &HashMap<String, PlatformOAuthClient>,
    user_oauth_client: Option<&UserOAuthClient>,
    server_id: McpServerId,
    org_id: crate::auth::OrgId,
    credentials: SharedMcpCredentialStore,
) -> Result<(), OAuthError> {
    match catalog.client_source {
        ClientSource::None => Err(OAuthError::Misconfigured(format!(
            "catalog `{id}` has client_source=none; callback not applicable",
            id = catalog.id
        ))),
        ClientSource::UserSupplied => {
            // Rebuild the same client the start path configured: the
            // operator's id (+ secret for confidential clients). Scopes
            // aren't needed for the code exchange (see Platform arm).
            let client = user_oauth_client.ok_or_else(|| {
                OAuthError::Misconfigured(format!(
                    "callback for user_supplied catalog `{id}` has no OAuth client",
                    id = catalog.id
                ))
            })?;
            let config = user_supplied_client_config(client, redirect_uri, None);
            manager.configure_client(config).map_err(OAuthError::from)
        }
        ClientSource::Platform => {
            let key_catalog_id = catalog
                .platform_client_alias
                .as_ref()
                .unwrap_or(&catalog.id);
            let key = platform_env_middle(key_catalog_id);
            let env_creds = platform_clients.get(&key).ok_or_else(|| {
                OAuthError::Misconfigured(format!(
                    "platform OAuth client for `{id}` not configured (alias `{key_catalog_id}`)",
                    id = catalog.id,
                ))
            })?;
            let config = OAuthClientConfig::new(
                env_creds.client_id.expose().to_owned(),
                redirect_uri.to_owned(),
            )
            .with_client_secret(env_creds.client_secret.expose().to_owned());
            // Scopes aren't needed for code-exchange (the code is bound to
            // the scopes the AS granted at the authorize step); leave the
            // config's `scopes` empty.
            manager.configure_client(config).map_err(OAuthError::from)
        }
        ClientSource::Dcr => {
            // DCR: `start_dcr` persisted a bootstrap row with the
            // freshly-registered `client_id` and `token_response = None`.
            // Load it and call `configure_client` (NOT `configure_client_id`
            // — that helper hardcodes `redirect_uri = self.base_url`, which
            // is the MCP server URL, and the vendor will reject the code
            // exchange with `invalid_grant: Invalid redirect URI` because
            // OAuth 2.0 §4.1.3 requires byte-equality with the URI used
            // at the authorize step).
            let cred_store = PatomCredentialStore::new(server_id, org_id, credentials);
            let stored = cred_store.load().await.map_err(OAuthError::from)?;
            let stored = stored.ok_or_else(|| {
                OAuthError::Misconfigured(format!(
                    "callback for DCR server {server_id}/{org_id} has no persisted client_id; \
                     start_authorization must run first",
                ))
            })?;
            let config = OAuthClientConfig::new(stored.client_id, redirect_uri.to_owned());
            manager.configure_client(config).map_err(OAuthError::from)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::append_extra_params;

    #[test]
    fn append_extra_params_noop_when_empty() {
        let s = append_extra_params("https://example.test/?a=1".into(), &[]);
        assert_eq!(s, "https://example.test/?a=1");
    }

    #[test]
    fn append_extra_params_adds_after_existing_query() {
        let s = append_extra_params(
            "https://example.test/?a=1".into(),
            &[
                ("access_type".into(), "offline".into()),
                ("prompt".into(), "consent".into()),
            ],
        );
        assert!(s.contains("a=1"), "{s}");
        assert!(s.contains("access_type=offline"), "{s}");
        assert!(s.contains("prompt=consent"), "{s}");
    }
}
