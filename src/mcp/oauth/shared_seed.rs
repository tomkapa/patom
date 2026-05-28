//! Boot-time seeder for shared (platform-owned) OAuth clients.
//!
//! Google and (soon) Microsoft 365 expose remote MCP servers whose
//! authorization servers do not support RFC 7591 Dynamic Client
//! Registration. The platform pattern Anthropic uses for Claude
//! Desktop's Gmail connector — one OAuth client owned by the platform
//! operator, reused across every tenant — collapses the per-tenant
//! "register a Google Cloud project" UX wart into zero clicks for end
//! users. Patom borrows the pattern.
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
//! 1. Add `client_id` / `client_secret` fields to
//!    [`AuthSettings`][crate::config::AuthSettings] + the matching raw
//!    env vars in `RawSettings`. Required vs optional is a per-vendor
//!    call: if the catalog row is dead without those credentials (no
//!    DCR, no per-org fallback the deploy actually uses), make them
//!    required `SecretString` like `google_*` / `github_*`; otherwise
//!    `Option<SecretString>` and a paired-set match. Keep names
//!    symmetric with the existing pairs.
//! 2. Add the matching `out.push(...)` (required) or `if let Some(...)`
//!    (optional) branch in [`specs`]. The endpoint constants are stable
//!    RFC 8414 discovery values; inline them as `const` rather than
//!    fetching at boot.
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
    let mut out = Vec::with_capacity(3);

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

    // GitHub — official remote MCP server at api.githubcopilot.com.
    // GitHub's authorization server does NOT advertise RFC 7591 DCR, so
    // the platform must register an OAuth App up-front and share that
    // client across tenants. Required at startup (same shape as Google
    // above) — without it the `github` catalog row cannot complete a
    // flow.
    //
    // Issuer is `https://github.com/login/oauth`, not the bare
    // `https://github.com`. This is the value api.githubcopilot.com's
    // protected-resource metadata returns in `authorization_servers[0]`,
    // and the AS metadata document at
    // `https://github.com/login/oauth/.well-known/oauth-authorization-server`
    // self-identifies with that same string per RFC 8414 §2.4. The
    // shared-client lookup in `resolve_oauth_client` does strict
    // equality on what discovery returns, so any other spelling here
    // (e.g. the OAuth App homepage `https://github.com`) leaves the
    // catalog row falling through to DCR — which GitHub doesn't
    // support — and the user sees the "DcrUnsupported" 4xx.
    //
    // `ClientSecretPost` matches GitHub's documented token-endpoint
    // convention; their `Basic` auth path also works but `_post` is
    // what the OAuth App emits in its example payloads.
    out.push(SharedClientSpec {
        issuer: "https://github.com/login/oauth",
        authorization_endpoint: "https://github.com/login/oauth/authorize",
        token_endpoint: "https://github.com/login/oauth/access_token",
        token_endpoint_auth_method: TokenAuthMethod::ClientSecretPost,
        client_id: auth.github_client_id.clone(),
        client_secret: auth.github_client_secret.clone(),
    });

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
                    patom.oauth.shared.issuer = %issuer,
                    event = "mcp.oauth.shared.seeded",
                );
            }
            Err(e) => {
                // WARN (not ERROR) is intentional: the seeder is
                // best-effort by design (see module-level §6 note) and
                // `PUT /oauth/client` remains the per-org fallback. The
                // structured-Debug error field still matches the
                // CLAUDE.md §2 telemetry contract.
                tracing::warn!(
                    patom.oauth.shared.issuer = %issuer,
                    error = ?e,
                    event = "mcp.oauth.shared.seed_failed",
                );
            }
        }
    }
    tracing::debug!(
        patom.oauth.shared.specs = count,
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
