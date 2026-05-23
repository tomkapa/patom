//! RFC 9728 + RFC 8414 discovery.
//!
//! Given an MCP server URL, produce the authorization-server metadata we
//! need to drive PKCE + DCR. Two hops, both bounded:
//!   1. RFC 9728 §3.1 protected-resource metadata, fetched by **inserting**
//!      `/.well-known/oauth-protected-resource` between the origin and
//!      the resource's path (e.g. `https://gmailmcp.googleapis.com/mcp/v1`
//!      → `https://gmailmcp.googleapis.com/.well-known/oauth-protected-resource/mcp/v1`)
//!      → `authorization_servers[0]`. If the well-known is unavailable,
//!      fall back to probing the server with no auth and parsing the
//!      `WWW-Authenticate: Bearer resource_metadata=…` header.
//!   2. RFC 8414 §3.1 authorization-server metadata, fetched the same way
//!      against the issuer URL → `authorization_endpoint` /
//!      `token_endpoint` / `registration_endpoint` / supported scopes.
//!
//! Bounded so a vendor that hands back a 10MB JSON blob cannot bloat
//! memory or stall the handler.

use reqwest::Client;
use reqwest::header::WWW_AUTHENTICATE;
use serde::Deserialize;
use tokio::time::timeout;
use url::Url;

use super::errors::OAuthError;

/// Per-request timeout for one discovery fetch (server-side limit).
const DISCOVERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Max bytes we'll read from any well-known endpoint. RFC 8414 docs are
/// usually < 2KB; we cap at 32KB to leave room for `scopes_supported`
/// lists.
const DISCOVERY_MAX_BYTES: usize = 32 * 1024;

/// Max bytes we'll scan from a single `WWW-Authenticate` challenge header.
/// Real challenges are < 512B; cap at 4KB to fend off header bombs.
const DISCOVERY_HEADER_MAX_BYTES: usize = 4 * 1024;

/// Output of [`discover_authorization_server`] — exactly what the OAuth
/// flow + DCR need to proceed.
#[derive(Debug, Clone)]
pub struct AsMetadata {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    /// Some authorization servers do not support DCR (RFC 7591). When
    /// absent we surface a typed misconfiguration up the stack and ask
    /// the operator to provision a client out-of-band.
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

/// Find the authorization-server metadata for `server_url`.
///
/// The fetch chain is explicit (no recursion, hop counter inlined).
/// We try resource-metadata first; if that 404s, we follow the spec'd
/// 401-probe fallback. Either way the inner discovery URL is fetched
/// at most once.
#[tracing::instrument(
    name = "mcp.oauth.discover",
    skip_all,
    fields(
        relay.mcp.url = %server_url,
    ),
)]
pub async fn discover_authorization_server(
    http: &Client,
    server_url: &str,
) -> Result<AsMetadata, OAuthError> {
    // Hop 1: resource metadata.
    let server =
        Url::parse(server_url).map_err(|e| OAuthError::Discovery(format!("server url: {e}")))?;
    let resource_metadata_url = join_well_known(&server, ".well-known/oauth-protected-resource");
    let issuer = match fetch_protected_resource_issuer(http, &resource_metadata_url).await {
        Ok(iss) => iss,
        Err(first_err) => {
            tracing::debug!(
                error = %first_err,
                "mcp.oauth.discover.resource_metadata_failed; trying 401 probe"
            );
            // RFC 9728 §5.1 fallback: probe the resource URL unauthenticated,
            // parse `WWW-Authenticate: Bearer resource_metadata=…`, retry hop 1
            // against the URL the challenge points at. Exactly one extra hop.
            let probed = match probe_resource_metadata_url(http, &server).await {
                Ok(u) => u,
                Err(probe_err) => {
                    tracing::debug!(error = %probe_err, "mcp.oauth.discover.probe_failed");
                    // Surface the original well-known error: it's the more
                    // actionable signal for an operator reading logs.
                    return Err(first_err);
                }
            };
            fetch_protected_resource_issuer(http, &probed).await?
        }
    };

    // Hop 2: AS metadata.
    let issuer_url =
        Url::parse(&issuer).map_err(|e| OAuthError::Discovery(format!("issuer url: {e}")))?;
    let as_metadata_url = join_well_known(&issuer_url, ".well-known/oauth-authorization-server");
    let raw = fetch_json::<AsMetadataJson>(http, &as_metadata_url).await?;

    // RFC 8414 §2.4 says the AS metadata MUST echo back the URL it
    // sits at. Some real-world ASes deliberately delegate, so we accept
    // exactly one hop: if the first document self-claims a different
    // issuer, re-fetch at that issuer's origin and require *that* one
    // to self-identify. Anti-issuer-confusion is preserved by the
    // second-hop self-check.
    let raw = if raw.issuer == issuer {
        raw
    } else {
        chase_delegated_issuer(http, &issuer, raw).await?
    };
    let final_issuer = raw.issuer.clone();

    Ok(AsMetadata {
        issuer: final_issuer,
        authorization_endpoint: raw.authorization_endpoint,
        token_endpoint: raw.token_endpoint,
        registration_endpoint: raw.registration_endpoint,
        scopes_supported: raw.scopes_supported,
        token_endpoint_auth_methods_supported: raw.token_endpoint_auth_methods_supported,
    })
}

/// One-shot delegated-issuer chase. Given the first AS metadata document
/// claims a different `issuer` than the URL we fetched it from, re-fetch
/// the well-known at the claimed issuer's origin and require that one
/// to self-identify with the claimed issuer. No looping — exactly one
/// extra hop. Returns the re-fetched (self-consistent) metadata.
async fn chase_delegated_issuer(
    http: &Client,
    original_issuer: &str,
    first: AsMetadataJson,
) -> Result<AsMetadataJson, OAuthError> {
    let claimed = first.issuer.clone();
    tracing::info!(
        relay.oauth.discovered_issuer = %original_issuer,
        relay.oauth.claimed_issuer = %claimed,
        "mcp.oauth.discover.issuer_delegation_chase",
    );
    let claimed_url = Url::parse(&claimed)
        .map_err(|e| OAuthError::Discovery(format!("delegated issuer url: {e}")))?;
    let next_url = join_well_known(&claimed_url, ".well-known/oauth-authorization-server");
    let raw2 = fetch_json::<AsMetadataJson>(http, &next_url).await?;
    if raw2.issuer != claimed {
        return Err(OAuthError::Discovery(format!(
            "issuer mismatch after one delegation hop: \
             resource says {original_issuer}, first AS says {claimed}, \
             second AS says {}",
            raw2.issuer
        )));
    }
    Ok(raw2)
}

async fn fetch_protected_resource_issuer(http: &Client, url: &Url) -> Result<String, OAuthError> {
    let metadata = fetch_json::<ProtectedResourceMetadata>(http, url).await?;
    let issuers = metadata.authorization_servers.unwrap_or_default();
    issuers
        .into_iter()
        .next()
        .ok_or_else(|| OAuthError::Discovery("no authorization_servers advertised".into()))
}

async fn fetch_json<T: serde::de::DeserializeOwned>(
    http: &Client,
    url: &Url,
) -> Result<T, OAuthError> {
    let req = http.get(url.clone()).send();
    let resp = timeout(DISCOVERY_TIMEOUT, req)
        .await
        .map_err(|_| OAuthError::Discovery("timed out".into()))?
        .map_err(|e| OAuthError::Discovery(format!("http: {e}")))?;
    if !resp.status().is_success() {
        return Err(OAuthError::Discovery(format!(
            "{} {} {}",
            url,
            resp.status().as_u16(),
            resp.status().canonical_reason().unwrap_or("")
        )));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| OAuthError::Discovery(format!("body: {e}")))?;
    if bytes.len() > DISCOVERY_MAX_BYTES {
        return Err(OAuthError::Discovery(format!(
            "response exceeds {DISCOVERY_MAX_BYTES} bytes"
        )));
    }
    serde_json::from_slice::<T>(&bytes).map_err(|e| OAuthError::Discovery(format!("parse: {e}")))
}

/// Build a well-known URL for `base` per RFC 9728 §3.1 and RFC 8414
/// §3.1: the well-known segment is **inserted between the origin and the
/// path** of the resource/issuer identifier, not appended to its path.
///
/// For `base = https://host[:port]/p1/p2` and `well_known =
/// .well-known/oauth-protected-resource`, the result is
/// `https://host[:port]/.well-known/oauth-protected-resource/p1/p2`.
/// When `base.path()` is `/` or empty, no path is appended and the
/// result is `https://host[:port]/.well-known/oauth-protected-resource`.
/// Query and fragment on `base` are dropped — well-known fetches are
/// path-addressed only.
///
/// Append-style construction (`<base>/<path>`) is wrong on any
/// non-root-path resource and was the cause of Gmail discovery 404s
/// (Google's MCP server returns 405 on a `GET` of `/mcp/v1`, so the
/// RFC 9728 §5.1 401-probe fallback couldn't recover either).
///
/// Infallible: `Url::set_path` cannot fail on a hierarchical URL and the
/// caller has already parsed `base`, so the operation is total.
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

/// Probe the MCP server with an unauthenticated GET and pull the
/// `resource_metadata` URL out of the `WWW-Authenticate: Bearer` challenge
/// (RFC 9728 §5.1). One request, one timeout, no retries — the caller is
/// responsible for falling back exactly once.
async fn probe_resource_metadata_url(http: &Client, server_url: &Url) -> Result<Url, OAuthError> {
    let req = http.get(server_url.clone()).send();
    let resp = timeout(DISCOVERY_TIMEOUT, req)
        .await
        .map_err(|_| OAuthError::Discovery("probe timed out".into()))?
        .map_err(|e| OAuthError::Discovery(format!("probe http: {e}")))?;
    // RFC 9728 §5.1 ties the `resource_metadata` advertisement to a 401
    // challenge. Refusing other statuses rules out a stale or
    // intermediary-injected `WWW-Authenticate` on a 200/302 steering
    // discovery to an attacker-controlled URL.
    if resp.status() != reqwest::StatusCode::UNAUTHORIZED {
        return Err(OAuthError::Discovery(format!(
            "probe expected 401 challenge, got {}",
            resp.status().as_u16()
        )));
    }
    for value in resp.headers().get_all(WWW_AUTHENTICATE) {
        let Ok(raw) = value.to_str() else { continue };
        // `get(..N)` returns `None` when `N` falls on a non-char boundary;
        // fall back to the full string in that case rather than panicking
        // from a `&raw[..N]` slice.
        let bounded = raw.get(..DISCOVERY_HEADER_MAX_BYTES).unwrap_or(raw);
        if let Some(extracted) = parse_resource_metadata_param(bounded) {
            return Url::parse(&extracted)
                .map_err(|e| OAuthError::Discovery(format!("probe: resource_metadata url: {e}")));
        }
    }
    Err(OAuthError::Discovery(
        "no WWW-Authenticate Bearer resource_metadata in challenge".into(),
    ))
}

/// Scan one `WWW-Authenticate` header value for a `Bearer` challenge
/// containing `resource_metadata=<url>` and return the URL string.
///
/// Hand-rolled single-pass scanner — no regex, no allocations beyond the
/// returned `String`. Accepts quoted-string and token68 parameter forms
/// (RFC 7235 §2.1) and is case-insensitive on the scheme name (RFC 9110
/// §11.1). Multiple comma-separated challenges are handled by tracking
/// which scheme is currently "in scope".
fn parse_resource_metadata_param(header: &str) -> Option<String> {
    let bytes = header.as_bytes();
    let mut i = 0;
    let mut in_bearer = false;

    while i < bytes.len() {
        // Skip whitespace and commas between params/challenges.
        while i < bytes.len() && (bytes[i].is_ascii_whitespace() || bytes[i] == b',') {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        // Read a token (scheme name or param name).
        let tok_start = i;
        while i < bytes.len() && is_tchar(bytes[i]) {
            i += 1;
        }
        if i == tok_start {
            // Unexpected byte; skip it to make forward progress.
            i += 1;
            continue;
        }
        let tok = &header[tok_start..i];

        // Skip linear whitespace after the token.
        while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
            i += 1;
        }

        if i < bytes.len() && bytes[i] == b'=' {
            // It's a parameter: `name=value`.
            i += 1;
            while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
                i += 1;
            }
            let value = read_param_value(bytes, &mut i)?;
            if in_bearer && tok.eq_ignore_ascii_case("resource_metadata") {
                return Some(value);
            }
        } else {
            // It's a scheme name introducing a new challenge.
            in_bearer = tok.eq_ignore_ascii_case("Bearer");
        }
    }
    None
}

/// Consume a `quoted-string` or `token` starting at `*i`. Returns the
/// inner value (quotes stripped, backslash escapes resolved). Advances
/// `*i` past the value. Returns `None` on malformed input.
fn read_param_value(bytes: &[u8], i: &mut usize) -> Option<String> {
    if *i >= bytes.len() {
        return None;
    }
    if bytes[*i] == b'"' {
        *i += 1;
        let mut out = String::new();
        while *i < bytes.len() {
            let b = bytes[*i];
            if b == b'\\' && *i + 1 < bytes.len() {
                out.push(char::from(bytes[*i + 1]));
                *i += 2;
                continue;
            }
            if b == b'"' {
                *i += 1;
                return Some(out);
            }
            out.push(char::from(b));
            *i += 1;
        }
        // Unterminated quote.
        None
    } else {
        let start = *i;
        while *i < bytes.len() && is_token68_char(bytes[*i]) {
            *i += 1;
        }
        if *i == start {
            return None;
        }
        // token68 chars are all ASCII; from_utf8 is total here.
        core::str::from_utf8(&bytes[start..*i])
            .ok()
            .map(str::to_owned)
    }
}

/// RFC 7230 `tchar`: printable ASCII minus separators.
const fn is_tchar(b: u8) -> bool {
    matches!(b,
        b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*' | b'+' | b'-' | b'.'
        | b'^' | b'_' | b'`' | b'|' | b'~'
        | b'0'..=b'9' | b'A'..=b'Z' | b'a'..=b'z')
}

/// RFC 7235 token68: ALPHA / DIGIT / "-" / "." / "_" / "~" / "+" / "/" /
/// "=" with optional trailing "=" padding. We accept ":" too because real
/// URLs in token form include it (e.g. `https://...`).
const fn is_token68_char(b: u8) -> bool {
    matches!(b,
        b'-' | b'.' | b'_' | b'~' | b'+' | b'/' | b'=' | b':'
        | b'0'..=b'9' | b'A'..=b'Z' | b'a'..=b'z')
}

#[cfg(test)]
mod tests {
    use super::{join_well_known, parse_resource_metadata_param};
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

    #[test]
    fn parses_bare_resource_metadata_quoted() {
        let h = r#"Bearer resource_metadata="https://x.test/.well-known/oauth-protected-resource""#;
        assert_eq!(
            parse_resource_metadata_param(h).as_deref(),
            Some("https://x.test/.well-known/oauth-protected-resource"),
        );
    }

    #[test]
    fn parses_with_realm_and_other_params() {
        let h = r#"Bearer realm="x", error="invalid_token", resource_metadata="https://x.test/y""#;
        assert_eq!(
            parse_resource_metadata_param(h).as_deref(),
            Some("https://x.test/y"),
        );
    }

    #[test]
    fn parses_when_basic_challenge_precedes_bearer() {
        let h = r#"Basic realm="x", Bearer resource_metadata="https://x.test/y""#;
        assert_eq!(
            parse_resource_metadata_param(h).as_deref(),
            Some("https://x.test/y"),
        );
    }

    #[test]
    fn parses_token68_unquoted_value() {
        let h = "Bearer resource_metadata=https://x.test/y";
        assert_eq!(
            parse_resource_metadata_param(h).as_deref(),
            Some("https://x.test/y"),
        );
    }

    #[test]
    fn scheme_name_is_case_insensitive() {
        let h = r#"bearer resource_metadata="https://x.test/y""#;
        assert_eq!(
            parse_resource_metadata_param(h).as_deref(),
            Some("https://x.test/y"),
        );
    }

    #[test]
    fn param_name_is_case_insensitive() {
        let h = r#"Bearer Resource_Metadata="https://x.test/y""#;
        assert_eq!(
            parse_resource_metadata_param(h).as_deref(),
            Some("https://x.test/y"),
        );
    }

    #[test]
    fn returns_none_when_param_missing() {
        let h = r#"Bearer realm="x", error="invalid_token""#;
        assert!(parse_resource_metadata_param(h).is_none());
    }

    #[test]
    fn returns_none_when_only_basic_challenge() {
        let h = r#"Basic realm="x", charset="UTF-8""#;
        assert!(parse_resource_metadata_param(h).is_none());
    }

    #[test]
    fn ignores_resource_metadata_under_non_bearer_scheme() {
        // Param attached to Basic should not satisfy a Bearer match.
        let h = r#"Basic resource_metadata="https://nope.test/", Bearer realm="x""#;
        assert!(parse_resource_metadata_param(h).is_none());
    }

    #[test]
    fn returns_none_for_empty_and_garbage_input() {
        assert!(parse_resource_metadata_param("").is_none());
        assert!(parse_resource_metadata_param("   ").is_none());
        assert!(parse_resource_metadata_param("not a challenge").is_none());
    }

    #[test]
    fn handles_unterminated_quoted_value_without_panic() {
        let h = r#"Bearer resource_metadata="https://x.test/y"#;
        assert!(parse_resource_metadata_param(h).is_none());
    }

    #[test]
    fn resolves_backslash_escape_in_quoted_value() {
        let h = r#"Bearer resource_metadata="https://x.test/\"y\"""#;
        assert_eq!(
            parse_resource_metadata_param(h).as_deref(),
            Some(r#"https://x.test/"y""#),
        );
    }
}
