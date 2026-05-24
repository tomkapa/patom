//! Boot-time seeder for shared (platform-owned) OAuth clients.
//!
//! Google and (soon) Microsoft 365 expose remote MCP servers whose
//! authorization servers do not support RFC 7591 Dynamic Client
//! Registration. The platform pattern Anthropic uses for Claude
//! Desktop's Gmail connector — one OAuth client owned by the platform
//! operator, reused across every tenant — collapses the per-tenant
//! "register a Google Cloud project" UX wart into zero clicks for end
//! users. Relay borrows the pattern.
//!
//! Concretely: at boot, [`seed_shared_clients`] reads
//! [`AuthSettings`][crate::config::AuthSettings] for any provider whose
//! credentials are present and idempotently upserts one
//! `mcp_oauth_clients` row per provider with `org_id IS NULL` and
//! `provenance = Shared`. The MCP OAuth flow's lookup precedence
//! (`start_oauth` step 3 in [`crate::http::routes::mcp`]) then resolves
//! to that row before falling through to DCR.
//!
//! ## Adding a provider
//!
//! 1. Add `Option<SecretString>` fields for `client_id` / `client_secret`
//!    to [`AuthSettings`][crate::config::AuthSettings] + the matching
//!    raw env vars in `RawSettings`. Keep the names symmetric with the
//!    existing `google_*` pair.
//! 2. Uncomment / add the matching `if let Some(...)` branch in
//!    [`specs`]. The endpoint constants are stable RFC 8414 discovery
//!    values; inline them as `const` rather than fetching at boot.
//! 3. Add a catalog entry that points at the provider's remote MCP
//!    server URL (migrations/) so tenants can wire an `mcp_servers`
//!    row against the right `catalog_id`.
//!
//! ## CLAUDE.md guardrails
//!
//! - §5 bounded: [`specs`] returns at most one entry per compiled-in
//!   provider. No runtime growth, no config-driven list parsing.
//! - §6 assertions: per-spec failure is logged at WARN and the loop
//!   continues — boot must not block on a misconfigured shared client.
//!   The fallback path (`PUT /api/mcp-servers/{id}/oauth/client`) keeps
//!   per-org provisioning available either way.

use crate::config::AuthSettings;
use crate::types::SecretString;

use super::errors::OAuthError;
use super::store::{
    ClientProvenance, NewOAuthClient, OAuthClientId, SharedMcpOAuthClientStore, TokenAuthMethod,
};

/// A provider-agnostic shared-client descriptor. Endpoints are stable
/// RFC 8414 discovery values, inlined to sidestep a network round-trip
/// at boot.
#[derive(Debug, Clone)]
struct SharedClientSpec {
    issuer: &'static str,
    authorization_endpoint: &'static str,
    token_endpoint: &'static str,
    token_endpoint_auth_method: TokenAuthMethod,
    client_id: SecretString,
    client_secret: SecretString,
}

/// Build the list of shared-client specs for the current process. One
/// entry per provider whose credentials are present in `AuthSettings`.
///
/// Adding Microsoft 365 is a matter of plumbing
/// `auth.microsoft_client_id` / `_secret` (both `Option<SecretString>`)
/// and uncommenting the second branch. Endpoints below are the
/// multi-tenant `common` flavour — tenant-scoped
/// (`/<tenant_id>/`) remains a future per-org override via
/// `PUT /oauth/client`.
fn specs(auth: &AuthSettings) -> Vec<SharedClientSpec> {
    let mut out = Vec::with_capacity(2);

    // Google — always present in this build (the same credentials power
    // Login with Google, plumbed via `AuthSettings`).
    //
    // Issuer is stored without a trailing slash to match the AS-canonical
    // form per RFC 8414 §2 ("The `issuer` value returned MUST be identical
    // to the authorization server's issuer identifier"). The store layer
    // additionally canonicalizes both writes and reads (`canonical_issuer`
    // in `pg_store`), so the trailing-slash variant
    // `https://accounts.google.com/` returned by Google's
    // protected-resource document also resolves here — keeping a single
    // mis-keyed character from silently breaking every Gmail connection.
    out.push(SharedClientSpec {
        issuer: "https://accounts.google.com",
        authorization_endpoint: "https://accounts.google.com/o/oauth2/v2/auth",
        token_endpoint: "https://oauth2.googleapis.com/token",
        token_endpoint_auth_method: TokenAuthMethod::ClientSecretPost,
        client_id: auth.google_client_id.clone(),
        client_secret: auth.google_client_secret.clone(),
    });

    // Microsoft 365 — uncomment when the credentials land. Drop-in slot
    // intentionally left in the file (not in scratch notes) so the
    // follow-up PR stays mechanical.
    //
    // if let (Some(id), Some(secret)) =
    //     (&auth.microsoft_client_id, &auth.microsoft_client_secret)
    // {
    //     out.push(SharedClientSpec {
    //         issuer: "https://login.microsoftonline.com/common/v2.0",
    //         authorization_endpoint:
    //             "https://login.microsoftonline.com/common/oauth2/v2.0/authorize",
    //         token_endpoint:
    //             "https://login.microsoftonline.com/common/oauth2/v2.0/token",
    //         token_endpoint_auth_method: TokenAuthMethod::ClientSecretPost,
    //         client_id: id.clone(),
    //         client_secret: secret.clone(),
    //     });
    // }

    out
}

/// Idempotently upsert one shared `mcp_oauth_clients` row per provider
/// returned by [`specs`]. Safe to call on every boot.
///
/// Failures are logged at WARN and the loop continues so a single
/// vendor outage / misconfigured credential can't block process start.
#[tracing::instrument(name = "mcp.oauth.shared_seed", skip_all)]
pub async fn seed_shared_clients(store: &SharedMcpOAuthClientStore, auth: &AuthSettings) {
    let specs = specs(auth);
    let count = specs.len();
    for spec in specs {
        let issuer = spec.issuer;
        match upsert_one(store, spec).await {
            Ok(()) => {
                tracing::info!(
                    relay.oauth.shared.issuer = %issuer,
                    event = "mcp.oauth.shared.seeded",
                );
            }
            Err(e) => {
                tracing::warn!(
                    relay.oauth.shared.issuer = %issuer,
                    error = %e,
                    event = "mcp.oauth.shared.seed_failed",
                );
            }
        }
    }
    tracing::debug!(
        relay.oauth.shared.specs = count,
        "shared-client seeder finished"
    );
}

async fn upsert_one(
    store: &SharedMcpOAuthClientStore,
    spec: SharedClientSpec,
) -> Result<(), OAuthError> {
    let client_id = OAuthClientId::try_from(spec.client_id.expose().to_owned())
        .map_err(|e| OAuthError::Misconfigured(format!("shared client_id: {e}")))?;
    let new = NewOAuthClient {
        issuer: spec.issuer.to_owned(),
        client_id,
        client_secret: Some(spec.client_secret),
        authorization_endpoint: spec.authorization_endpoint.to_owned(),
        token_endpoint: spec.token_endpoint.to_owned(),
        token_endpoint_auth_method: spec.token_endpoint_auth_method,
        scope: None,
        provenance: ClientProvenance::Shared,
    };
    store.upsert(new).await.map(drop)
}
