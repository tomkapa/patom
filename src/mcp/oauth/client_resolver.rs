//! The single seam that produces OAuth client credentials for an MCP
//! server's catalog entry. Branches on `mcp_catalog.client_source`:
//!
//!   * `Platform` — look up `(client_id, client_secret)` in the env-var
//!     map populated at boot ([`AuthSettings::platform_oauth_clients`]).
//!     `platform_client_alias` lets two catalog entries (Gmail + Calendar
//!     → `'google'`) share one upstream OAuth app.
//!   * `Dcr` — run RFC 7591 Dynamic Client Registration against the
//!     discovered AS and return the freshly issued client. The caller
//!     persists the result on the pending row (start path) and folds it
//!     into the encrypted `OAuth2Payload` on callback — there is no
//!     separate clients table any more.
//!   * `None` — precondition violation; only OAuth-bearing catalog
//!     entries reach the resolver.
//!
//! This module is the only place that reads `ClientSource`, so vendor
//! differentiation lives in `mcp_catalog` data + env, not in code.

use std::collections::HashMap;

use crate::config::PlatformOAuthClient;
use crate::mcp::catalog::{ClientSource, McpCatalogEntry, platform_env_keys, platform_env_middle};

use super::discovery::AsMetadata;
use super::errors::OAuthError;
use super::flow::{OAuthFlowClient, register_dynamic_client};
use super::store::{OAuthClientCreds, OAuthClientId, TokenAuthMethod};

/// Inputs to [`resolve`] — bundled so the public signature stays under
/// clippy's argument-count cap and so call sites read top-down at the
/// seams.
#[derive(Debug, Clone, Copy)]
pub struct ResolveCtx<'a> {
    pub catalog: &'a McpCatalogEntry,
    /// Env-keyed map of Patom-supported OAuth clients (the value of
    /// [`crate::config::AuthSettings::platform_oauth_clients`]). Keyed by
    /// the lowercased env-var middle of `PATOM_<X>_CLIENT_ID/SECRET`.
    pub platform_clients: &'a HashMap<String, PlatformOAuthClient>,
    pub as_metadata: &'a AsMetadata,
    pub redirect_uri: &'a str,
    pub requested_scope: Option<&'a str>,
    pub flow: &'a OAuthFlowClient,
}

/// Resolve OAuth client credentials for `ctx.catalog`. Returns
/// [`OAuthClientCreds`] — the same shape for both branches so downstream
/// code (authorize URL, code exchange, refresh) doesn't care how the
/// credentials were sourced.
#[tracing::instrument(
    name = "mcp.oauth.client_resolver.resolve",
    skip_all,
    fields(
        patom.mcp.catalog_id = %ctx.catalog.id,
        patom.mcp.oauth.client_source = %ctx.catalog.client_source.as_str(),
        patom.mcp.oauth.issuer = %ctx.as_metadata.issuer,
    ),
)]
pub async fn resolve(ctx: ResolveCtx<'_>) -> Result<OAuthClientCreds, OAuthError> {
    match ctx.catalog.client_source {
        ClientSource::None => Err(OAuthError::Misconfigured(format!(
            "catalog `{id}` has client_source=none; OAuth flow not applicable",
            id = ctx.catalog.id
        ))),
        ClientSource::Platform => resolve_platform(
            ctx.catalog,
            ctx.platform_clients,
            ctx.as_metadata,
            ctx.requested_scope,
        ),
        ClientSource::Dcr => {
            let new = register_dynamic_client(
                ctx.flow,
                ctx.as_metadata,
                ctx.redirect_uri,
                ctx.requested_scope,
            )
            .await?;
            Ok(new)
        }
    }
}

/// Build the env-backed client record. The lookup key is the catalog
/// entry's `platform_client_alias` if set, else the entry's own id —
/// `gmail.platform_client_alias = 'google'` redirects to
/// `PATOM_GOOGLE_CLIENT_ID/SECRET` so one Google OAuth app covers both
/// products.
fn resolve_platform(
    catalog: &McpCatalogEntry,
    platform_clients: &HashMap<String, PlatformOAuthClient>,
    as_metadata: &AsMetadata,
    requested_scope: Option<&str>,
) -> Result<OAuthClientCreds, OAuthError> {
    let key_catalog_id = catalog
        .platform_client_alias
        .as_ref()
        .unwrap_or(&catalog.id);
    let key = platform_env_middle(key_catalog_id);
    let creds = platform_clients.get(&key).ok_or_else(|| {
        let (env_id, _env_secret) = platform_env_keys(key_catalog_id);
        OAuthError::Misconfigured(format!(
            "platform OAuth client for catalog `{id}` not configured (env: {env_id})",
            id = catalog.id
        ))
    })?;
    let client_id = OAuthClientId::try_from(creds.client_id.expose().to_owned())
        .map_err(|e| OAuthError::Misconfigured(format!("platform client_id: {e}")))?;
    let supported = as_metadata
        .token_endpoint_auth_methods_supported
        .as_deref()
        .unwrap_or(&[]);
    let auth_method = pick_auth_method(supported, /*confidential=*/ true);
    Ok(OAuthClientCreds {
        issuer: as_metadata.issuer.clone(),
        client_id,
        client_secret: Some(creds.client_secret.clone()),
        authorization_endpoint: as_metadata.authorization_endpoint.clone(),
        token_endpoint: as_metadata.token_endpoint.clone(),
        token_endpoint_auth_method: auth_method,
        scope: requested_scope.map(str::to_owned),
    })
}

/// Choose a `token_endpoint_auth_method` for a fresh client. For
/// confidential clients (Platform / DCR with secret) prefer
/// `client_secret_post`, falling back to `client_secret_basic`. Drops
/// to `None` only when neither secret method is supported and the AS
/// claims `none` works — i.e. PKCE-only public client.
fn pick_auth_method(supported: &[String], confidential: bool) -> TokenAuthMethod {
    let supports = |m: TokenAuthMethod| supported.iter().any(|s| s == m.as_str());
    if confidential {
        if supports(TokenAuthMethod::ClientSecretPost) {
            return TokenAuthMethod::ClientSecretPost;
        }
        if supports(TokenAuthMethod::ClientSecretBasic) {
            return TokenAuthMethod::ClientSecretBasic;
        }
        TokenAuthMethod::ClientSecretPost
    } else if supports(TokenAuthMethod::None) {
        TokenAuthMethod::None
    } else {
        TokenAuthMethod::ClientSecretBasic
    }
}
