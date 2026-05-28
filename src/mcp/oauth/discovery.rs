//! Bounded, two-hop OAuth authorization-server discovery.
//!
//! Given an MCP server URL, produce the authorization-server metadata
//! the OAuth flow + DCR consume. Two hops, one fetch each:
//!
//!   1. RFC 9728 §3.1 protected-resource metadata at
//!      `<origin>/.well-known/oauth-protected-resource[<path>]`
//!      → `authorization_servers[0]` is the issuer.
//!   2. RFC 8414 §3.1 authorization-server metadata at the issuer.
//!
//! Codex-shaped: no `WWW-Authenticate` 401-probe fallback, no
//! delegated-issuer chase. RFC 8414 §2.4 self-consistency is enforced by
//! a single equality check — a mismatched issuer is a hard error, not a
//! retry signal. Each fetch is timeout-bounded and size-bounded so a
//! poisoned response can't bloat the handler.

use reqwest::Client;
use serde::Deserialize;
use tokio::time::timeout;
use url::Url;

use super::errors::OAuthError;

/// Per-request timeout for one discovery fetch.
const DISCOVERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Max bytes any well-known response may serve. Real RFC 8414 docs are
/// well under 2KB; cap at 32KB for `scopes_supported` headroom.
const DISCOVERY_MAX_BYTES: usize = 32 * 1024;

/// Output of [`discover_authorization_server`] — exactly what the OAuth
/// flow + DCR need to proceed.
#[derive(Debug, Clone)]
pub struct AsMetadata {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    /// Some authorization servers do not support DCR (RFC 7591). When
    /// absent the resolver routes through the platform-env path or
    /// surfaces a typed misconfiguration up the stack.
    pub registration_endpoint: Option<String>,
    pub scopes_supported: Option<Vec<String>>,
    pub token_endpoint_auth_methods_supported: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct ProtectedResourceMetadata {
    authorization_servers: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct AsMetadataJson {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    #[serde(default)]
    registration_endpoint: Option<String>,
    #[serde(default)]
    scopes_supported: Option<Vec<String>>,
    #[serde(default)]
    token_endpoint_auth_methods_supported: Option<Vec<String>>,
}

/// Discover authorization-server metadata for an MCP server.
///
/// One PRM fetch followed by one AS metadata fetch. No fallbacks, no
/// chasing — if the AS metadata's `issuer` doesn't match the one the PRM
/// advertised, the call fails fast (RFC 8414 §2.4 self-consistency).
#[tracing::instrument(
    name = "mcp.oauth.discover",
    skip_all,
    fields(patom.mcp.url = %server_url),
)]
pub async fn discover_authorization_server(
    http: &Client,
    server_url: &str,
) -> Result<AsMetadata, OAuthError> {
    let server =
        Url::parse(server_url).map_err(|e| OAuthError::Discovery(format!("server url: {e}")))?;

    let prm_url = join_well_known(&server, ".well-known/oauth-protected-resource");
    let prm: ProtectedResourceMetadata = fetch_json(http, &prm_url).await?;
    let issuer = prm
        .authorization_servers
        .unwrap_or_default()
        .into_iter()
        .next()
        .ok_or_else(|| OAuthError::Discovery("no authorization_servers advertised".into()))?;

    let issuer_url =
        Url::parse(&issuer).map_err(|e| OAuthError::Discovery(format!("issuer url: {e}")))?;
    let as_url = join_well_known(&issuer_url, ".well-known/oauth-authorization-server");
    let raw: AsMetadataJson = fetch_json(http, &as_url).await?;

    // RFC 8414 §2.4: the AS metadata MUST echo back the issuer. A
    // mismatch is the issuer-confusion attack surface; reject it cleanly
    // instead of letting the resolver feed an attacker-controlled
    // authorization_endpoint to the user's browser.
    //
    // Real-world vendors are inconsistent about trailing slashes — Google's
    // PRM advertises `https://accounts.google.com/` while the AS metadata
    // at that URL self-declares as `https://accounts.google.com`. Strip
    // one trailing slash on both sides of the compare so the byte-equality
    // check survives the canonicalisation drift; downstream consumers see
    // the trailing-slash-free form returned from `AsMetadata.issuer`.
    if canonical_issuer(&raw.issuer) != canonical_issuer(&issuer) {
        return Err(OAuthError::Discovery(format!(
            "issuer mismatch: PRM says {issuer}, AS says {}",
            raw.issuer
        )));
    }

    Ok(AsMetadata {
        issuer: canonical_issuer(&raw.issuer).to_owned(),
        authorization_endpoint: raw.authorization_endpoint,
        token_endpoint: raw.token_endpoint,
        registration_endpoint: raw.registration_endpoint,
        scopes_supported: raw.scopes_supported,
        token_endpoint_auth_methods_supported: raw.token_endpoint_auth_methods_supported,
    })
}

async fn fetch_json<T: serde::de::DeserializeOwned>(
    http: &Client,
    url: &Url,
) -> Result<T, OAuthError> {
    // Wrap send + status check + body read in a single timeout. The
    // body stream is part of the same I/O event we want to bound — a
    // slow vendor that flushes headers but trickles bytes would
    // otherwise hang the handler indefinitely (CLAUDE.md §5).
    let url_for_err = url.clone();
    let bytes = timeout(DISCOVERY_TIMEOUT, async {
        let resp = http
            .get(url.clone())
            .send()
            .await
            .map_err(|e| OAuthError::Discovery(format!("http: {e}")))?;
        if !resp.status().is_success() {
            return Err(OAuthError::Discovery(format!(
                "{} {} {}",
                url_for_err,
                resp.status().as_u16(),
                resp.status().canonical_reason().unwrap_or("")
            )));
        }
        resp.bytes()
            .await
            .map_err(|e| OAuthError::Discovery(format!("body: {e}")))
    })
    .await
    .map_err(|_| OAuthError::Discovery("timed out".into()))??;
    if bytes.len() > DISCOVERY_MAX_BYTES {
        return Err(OAuthError::Discovery(format!(
            "response exceeds {DISCOVERY_MAX_BYTES} bytes"
        )));
    }
    serde_json::from_slice::<T>(&bytes).map_err(|e| OAuthError::Discovery(format!("parse: {e}")))
}

/// Strip one trailing slash for cross-vendor issuer comparison.
///
/// RFC 8414 §2 says the issuer MUST round-trip exactly, but real ASes
/// disagree on the trailing-slash form — notably Google. We normalise
/// at the discovery boundary so the rest of the OAuth subsystem only
/// ever sees one canonical shape.
#[inline]
#[must_use]
fn canonical_issuer(raw: &str) -> &str {
    raw.strip_suffix('/').unwrap_or(raw)
}

/// Build a well-known URL for `base` per RFC 9728 §3.1 / RFC 8414 §3.1:
/// the well-known segment is **inserted between the origin and the
/// path** of the resource/issuer identifier.
///
/// For `base = https://host[:port]/p1/p2` and `well_known =
/// .well-known/oauth-protected-resource`, the result is
/// `https://host[:port]/.well-known/oauth-protected-resource/p1/p2`.
/// When `base.path()` is `/` or empty, no path is appended. Query and
/// fragment on `base` are dropped — well-known fetches are
/// path-addressed only.
fn join_well_known(base: &Url, well_known: &str) -> Url {
    let suffix = base.path().trim_end_matches('/');
    let new_path = if suffix.is_empty() {
        format!("/{well_known}")
    } else {
        format!("/{well_known}{suffix}")
    };
    let mut out = base.clone();
    out.set_path(&new_path);
    out.set_query(None);
    out.set_fragment(None);
    out
}

#[cfg(test)]
mod tests {
    use super::join_well_known;
    use url::Url;

    fn parse(url: &str) -> Url {
        Url::parse(url).expect("test setup: literal url is valid")
    }

    #[test]
    fn join_well_known_inserts_for_pathful_resource() {
        let got = join_well_known(
            &parse("https://gmailmcp.googleapis.com/mcp/v1"),
            ".well-known/oauth-protected-resource",
        );
        assert_eq!(
            got.as_str(),
            "https://gmailmcp.googleapis.com/.well-known/oauth-protected-resource/mcp/v1"
        );
    }

    #[test]
    fn join_well_known_handles_root_path() {
        let got = join_well_known(
            &parse("https://accounts.google.com/"),
            ".well-known/oauth-authorization-server",
        );
        assert_eq!(
            got.as_str(),
            "https://accounts.google.com/.well-known/oauth-authorization-server"
        );
    }

    #[test]
    fn join_well_known_strips_trailing_slash_from_path() {
        let got = join_well_known(
            &parse("https://mcp.notion.com/mcp/"),
            ".well-known/oauth-protected-resource",
        );
        assert_eq!(
            got.as_str(),
            "https://mcp.notion.com/.well-known/oauth-protected-resource/mcp"
        );
    }

    #[test]
    fn join_well_known_drops_query_and_fragment() {
        let got = join_well_known(
            &parse("https://example.test/mcp?x=1#frag"),
            ".well-known/oauth-protected-resource",
        );
        assert_eq!(
            got.as_str(),
            "https://example.test/.well-known/oauth-protected-resource/mcp"
        );
    }

    #[test]
    fn join_well_known_preserves_non_default_port() {
        let got = join_well_known(
            &parse("https://example.test:8443/mcp/v1"),
            ".well-known/oauth-protected-resource",
        );
        assert_eq!(
            got.as_str(),
            "https://example.test:8443/.well-known/oauth-protected-resource/mcp/v1"
        );
    }
}
