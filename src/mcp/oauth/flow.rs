//! Browser flow: PKCE + DCR + authorize URL + code exchange.
//!
//! Uses the `oauth2` crate (already a dep) for the PKCE + token-exchange
//! pieces; DCR (RFC 7591) is a one-shot POST we render directly.

use std::time::Duration;

use oauth2::basic::BasicClient;
use oauth2::reqwest::Client as OAuthHttpClient;
use oauth2::{
    AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken, EndpointNotSet, EndpointSet,
    PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, RefreshToken, Scope, TokenResponse, TokenUrl,
};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::time::timeout;
use url::Url;

use crate::auth::OrgId;
use crate::types::SecretString;

use super::discovery::AsMetadata;
use super::errors::OAuthError;
use super::store::{NewOAuthClient, TokenAuthMethod};

const FLOW_HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const DCR_MAX_BYTES: usize = 32 * 1024;

/// Cheap-clone HTTP client wired with the oauth2 trait. Holds both a
/// plain reqwest::Client (for DCR) and the oauth2-specific one (for the
/// token exchange).
#[derive(Clone)]
pub struct OAuthFlowClient {
    pub(crate) http: Client,
    pub(crate) http_oauth: OAuthHttpClient,
}

impl std::fmt::Debug for OAuthFlowClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthFlowClient").finish_non_exhaustive()
    }
}

impl OAuthFlowClient {
    /// Construct from a shared `reqwest` client; spins up the oauth2
    /// crate's separate HTTP client (its trait is incompatible with our
    /// own `reqwest::Client` directly).
    pub fn new(http: Client) -> Result<Self, OAuthError> {
        let http_oauth = OAuthHttpClient::builder()
            .timeout(FLOW_HTTP_TIMEOUT)
            .connect_timeout(Duration::from_secs(5))
            .redirect(oauth2::reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| OAuthError::Misconfigured(format!("oauth http: {e}")))?;
        Ok(Self { http, http_oauth })
    }
}

/// Output of [`start_authorization`].
#[derive(Debug, Clone)]
pub struct AuthorizeStart {
    pub authorize_url: Url,
    pub state: String,
    pub pkce_verifier: String,
}

/// Output of [`exchange_code`]. The plaintext tokens live here briefly;
/// the caller is responsible for sealing them before they hit the DB.
#[derive(Debug, Clone)]
pub struct TokenExchangeResult {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: chrono::DateTime<chrono::Utc>,
    pub scope: Option<String>,
    pub issuer: String,
    pub token_endpoint: String,
}

pub type PendingAuthorization = super::store::PendingAuthorization;

/// RFC 7591 Dynamic Client Registration. POSTs the smallest viable
/// metadata document to `registration_endpoint`; refuses to proceed if
/// the AS metadata doesn't advertise one (the operator must provision a
/// client out-of-band in that case).
#[tracing::instrument(
    name = "mcp.oauth.dcr",
    skip_all,
    fields(
        relay.mcp.oauth.issuer = %as_metadata.issuer,
    ),
)]
pub async fn register_dynamic_client(
    flow: &OAuthFlowClient,
    as_metadata: &AsMetadata,
    org_id: OrgId,
    redirect_uri: &str,
    scope: Option<&str>,
) -> Result<NewOAuthClient, OAuthError> {
    let registration_endpoint = as_metadata
        .registration_endpoint
        .as_deref()
        .ok_or_else(|| OAuthError::DcrUnsupported {
            issuer: as_metadata.issuer.clone(),
        })?;

    // Pick the auth method: prefer `none` (PKCE-only public client) when
    // the AS advertises it, else `client_secret_basic`. RFC 7591 lets us
    // request a specific method; we read the supported list to avoid
    // negotiating one the AS will reject.
    let supported = as_metadata
        .token_endpoint_auth_methods_supported
        .as_deref()
        .unwrap_or(&[]);
    let pick = |method: TokenAuthMethod| supported.iter().any(|m| m == method.as_str());
    // RFC 7591 default is `client_secret_basic`. Prefer `none` (public
    // PKCE-only client) when the AS supports it.
    let auth_method = if pick(TokenAuthMethod::None) {
        TokenAuthMethod::None
    } else if pick(TokenAuthMethod::ClientSecretBasic) {
        TokenAuthMethod::ClientSecretBasic
    } else if pick(TokenAuthMethod::ClientSecretPost) {
        TokenAuthMethod::ClientSecretPost
    } else {
        TokenAuthMethod::ClientSecretBasic
    };

    let body = DcrRequest {
        client_name: "Relay",
        redirect_uris: vec![redirect_uri.to_owned()],
        grant_types: vec!["authorization_code".into(), "refresh_token".into()],
        response_types: vec!["code".into()],
        token_endpoint_auth_method: auth_method.as_str(),
        scope: scope.map(str::to_owned),
    };

    let resp = timeout(
        FLOW_HTTP_TIMEOUT,
        flow.http.post(registration_endpoint).json(&body).send(),
    )
    .await
    .map_err(|_| OAuthError::Dcr("registration timed out".into()))?
    .map_err(|e| OAuthError::Dcr(format!("http: {e}")))?;

    let status = resp.status();
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| OAuthError::Dcr(format!("body: {e}")))?;
    if !status.is_success() {
        return Err(OAuthError::Dcr(format!(
            "{} {} body={}",
            status.as_u16(),
            status.canonical_reason().unwrap_or(""),
            String::from_utf8_lossy(&bytes)
                .chars()
                .take(256)
                .collect::<String>()
        )));
    }
    if bytes.len() > DCR_MAX_BYTES {
        return Err(OAuthError::Dcr(format!(
            "response exceeds {DCR_MAX_BYTES} bytes"
        )));
    }
    let raw: DcrResponse =
        serde_json::from_slice(&bytes).map_err(|e| OAuthError::Dcr(format!("parse: {e}")))?;
    // Surface SecretString::try_from failures (empty / oversized) as a
    // typed DCR error rather than silently dropping the field — a half-
    // registered client where the secret was discarded would fail later
    // in non-obvious ways.
    let client_secret = raw
        .client_secret
        .map(SecretString::try_from)
        .transpose()
        .map_err(|e| OAuthError::Dcr(format!("invalid client_secret: {e}")))?;
    let registration_access_token = raw
        .registration_access_token
        .map(SecretString::try_from)
        .transpose()
        .map_err(|e| OAuthError::Dcr(format!("invalid registration_access_token: {e}")))?;
    let client_id = super::store::OAuthClientId::try_from(raw.client_id)
        .map_err(|e| OAuthError::Dcr(format!("invalid client_id: {e}")))?;
    Ok(NewOAuthClient {
        issuer: as_metadata.issuer.clone(),
        client_id,
        client_secret,
        authorization_endpoint: as_metadata.authorization_endpoint.clone(),
        token_endpoint: as_metadata.token_endpoint.clone(),
        token_endpoint_auth_method: auth_method,
        scope: scope.map(str::to_owned),
        provenance: super::store::ClientProvenance::Dcr {
            org_id,
            registration_client_uri: raw.registration_client_uri,
            registration_access_token,
        },
    })
}

#[derive(Debug, Serialize)]
struct DcrRequest<'a> {
    client_name: &'a str,
    redirect_uris: Vec<String>,
    grant_types: Vec<String>,
    response_types: Vec<String>,
    token_endpoint_auth_method: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DcrResponse {
    client_id: String,
    #[serde(default)]
    client_secret: Option<String>,
    #[serde(default)]
    registration_client_uri: Option<String>,
    #[serde(default)]
    registration_access_token: Option<String>,
}

/// Build the authorize URL the browser will be redirected to. PKCE +
/// state are minted here; the caller persists them in
/// `mcp_oauth_pending` for the callback to consume.
///
/// `extras` are non-standard query params the catalog row pins for
/// vendors whose AS rejects (or silently misbehaves on) a strict
/// RFC 6749 §4.1 redirect — promoted to catalog data in migration 39 so
/// new vendors are a row, not a code change. Order is preserved into
/// the URL: some ASes (Microsoft) care about repeat-key order.
pub fn build_authorize_url(
    client: &super::store::DcrClientRecord,
    redirect_uri: &str,
    requested_scope: Option<&str>,
    extras: &[(&str, &str)],
) -> Result<AuthorizeStart, OAuthError> {
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let oauth_client = build_basic_client(client, redirect_uri)?;
    let mut authorize = oauth_client
        .authorize_url(|| CsrfToken::new_random_len(32))
        .set_pkce_challenge(challenge);
    if let Some(s) = requested_scope {
        for scope in s.split_whitespace() {
            authorize = authorize.add_scope(Scope::new(scope.to_owned()));
        }
    }
    for (k, v) in extras {
        authorize = authorize.add_extra_param(*k, *v);
    }
    let (url, csrf) = authorize.url();
    Ok(AuthorizeStart {
        authorize_url: url,
        state: csrf.secret().clone(),
        pkce_verifier: verifier.secret().clone(),
    })
}

/// Exchange the callback code for tokens. The result is the plaintext
/// token payload; the caller seals it into the credentials seam.
#[tracing::instrument(
    name = "mcp.oauth.exchange",
    skip_all,
    fields(
        relay.mcp.oauth.issuer = %client.issuer,
    ),
)]
pub async fn exchange_code(
    flow: &OAuthFlowClient,
    client: &super::store::DcrClientRecord,
    redirect_uri: &str,
    code: &str,
    pkce_verifier: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<TokenExchangeResult, OAuthError> {
    let oauth_client = build_basic_client(client, redirect_uri)?;
    let token = oauth_client
        .exchange_code(AuthorizationCode::new(code.to_owned()))
        .set_pkce_verifier(PkceCodeVerifier::new(pkce_verifier.to_owned()))
        .request_async(&flow.http_oauth)
        .await
        .map_err(|e| OAuthError::TokenEndpoint(format!("exchange: {e}")))?;
    let access_token = token.access_token().secret().clone();
    let refresh_token = token.refresh_token().map(|t| t.secret().clone());
    // Default to a conservative 10-minute expiry when the server omits
    // `expires_in`. Vendors that don't return one are rare; the cap
    // means we refresh sooner rather than later, which is the safer
    // failure mode.
    let default_expiry = chrono::Duration::seconds(600);
    let expires_in = token.expires_in().map_or(default_expiry, |d| {
        chrono::Duration::from_std(d).unwrap_or(default_expiry)
    });
    let expires_at = now + expires_in;
    let scope = token.scopes().map(|ss| {
        ss.iter()
            .map(std::convert::AsRef::as_ref)
            .collect::<Vec<&str>>()
            .join(" ")
    });
    Ok(TokenExchangeResult {
        access_token,
        refresh_token,
        expires_at,
        scope,
        issuer: client.issuer.clone(),
        token_endpoint: client.token_endpoint.clone(),
    })
}

/// Authorize + exchange paths must agree byte-for-byte on the client
/// config, or PKCE silently fails the comparison; build the `oauth2`
/// `BasicClient` here so both call sites see the same shape.
fn build_basic_client(
    client: &super::store::DcrClientRecord,
    redirect_uri: &str,
) -> Result<
    BasicClient<EndpointSet, EndpointNotSet, EndpointNotSet, EndpointNotSet, EndpointSet>,
    OAuthError,
> {
    let auth_url = AuthUrl::new(client.authorization_endpoint.clone())
        .map_err(|e| OAuthError::Misconfigured(format!("authorization_endpoint: {e}")))?;
    let token_url = TokenUrl::new(client.token_endpoint.clone())
        .map_err(|e| OAuthError::Misconfigured(format!("token_endpoint: {e}")))?;
    let redirect = RedirectUrl::new(redirect_uri.to_owned())
        .map_err(|e| OAuthError::Misconfigured(format!("redirect_uri: {e}")))?;
    let mut b = BasicClient::new(ClientId::new(client.client_id.as_str().to_owned()))
        .set_auth_uri(auth_url);
    if let Some(secret) = &client.client_secret {
        b = b.set_client_secret(ClientSecret::new(secret.expose().to_owned()));
    }
    Ok(b.set_token_uri(token_url).set_redirect_uri(redirect))
}

/// Result of [`refresh_oauth_token`]. The caller decides what to do on
/// each variant — typically: `Refreshed` → seal + persist the new token;
/// `Revoked` → flip `connection_status = 'reconnect_required'`.
#[derive(Debug)]
pub enum RefreshOutcome {
    Refreshed(TokenExchangeResult),
    Revoked,
}

/// Exchange `refresh_token` for a fresh access token. The redirect_uri
/// isn't strictly required for the refresh grant by RFC 6749 §6, but
/// some ASes echo back redirect-URI checks; pass the same one the
/// authorization step used.
#[tracing::instrument(
    name = "mcp.oauth.refresh",
    skip_all,
    fields(
        relay.mcp.oauth.issuer = %client.issuer,
    ),
)]
pub async fn refresh_oauth_token(
    flow: &OAuthFlowClient,
    client: &super::store::DcrClientRecord,
    refresh_token: &str,
    redirect_uri: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<RefreshOutcome, OAuthError> {
    let oauth_client = build_basic_client(client, redirect_uri)?;
    let resp = oauth_client
        .exchange_refresh_token(&RefreshToken::new(refresh_token.to_owned()))
        .request_async(&flow.http_oauth)
        .await;
    let token = match resp {
        Ok(t) => t,
        Err(e) => {
            // The oauth2 crate buries the AS's `error` field inside a
            // crate-specific enum. Match on the textual form so we
            // don't tie this code to a `oauth2::RequestTokenError`
            // private layout. `invalid_grant` is the standard signal
            // for "refresh token revoked / expired" per RFC 6749
            // §5.2.
            let s = e.to_string();
            if s.contains("invalid_grant") {
                tracing::warn!(error = %e, "mcp.oauth.refresh.revoked");
                return Ok(RefreshOutcome::Revoked);
            }
            return Err(OAuthError::TokenEndpoint(format!("refresh: {e}")));
        }
    };
    let access_token = token.access_token().secret().clone();
    // Some ASes rotate the refresh token on each use; if so we take the
    // new one. Otherwise we keep the existing one — the caller carries
    // the prior value over when we return `None` here.
    let new_refresh = token.refresh_token().map(|t| t.secret().clone());
    let default_expiry = chrono::Duration::seconds(600);
    let expires_in = token.expires_in().map_or(default_expiry, |d| {
        chrono::Duration::from_std(d).unwrap_or(default_expiry)
    });
    let scope = token.scopes().map(|ss| {
        ss.iter()
            .map(std::convert::AsRef::as_ref)
            .collect::<Vec<&str>>()
            .join(" ")
    });
    Ok(RefreshOutcome::Refreshed(TokenExchangeResult {
        access_token,
        refresh_token: new_refresh,
        expires_at: now + expires_in,
        scope,
        issuer: client.issuer.clone(),
        token_endpoint: client.token_endpoint.clone(),
    }))
}

#[cfg(test)]
mod tests {
    use super::super::store::{DcrClientRecord, OAuthClientId, TokenAuthMethod};
    use super::build_authorize_url;

    fn client(issuer: &str) -> DcrClientRecord {
        DcrClientRecord {
            org_id: None,
            issuer: issuer.to_owned(),
            client_id: OAuthClientId::try_from("test-client-id".to_owned())
                .expect("invariant: literal client id is valid"),
            client_secret: None,
            authorization_endpoint: "https://example.test/authorize".to_owned(),
            token_endpoint: "https://example.test/token".to_owned(),
            token_endpoint_auth_method: TokenAuthMethod::None,
            scope: None,
        }
    }

    #[test]
    fn extras_are_appended_to_authorize_url_in_order() {
        let extras: &[(&str, &str)] = &[("access_type", "offline"), ("prompt", "consent")];
        let start = build_authorize_url(
            &client("https://accounts.google.com"),
            "https://relay.test/cb",
            Some("openid"),
            extras,
        )
        .expect("invariant: build_authorize_url with valid client + extras succeeds");
        let s = start.authorize_url.as_str();
        assert!(s.contains("access_type=offline"), "url={s}");
        assert!(s.contains("prompt=consent"), "url={s}");
        let access_pos = s
            .find("access_type=")
            .expect("invariant: access_type present");
        let prompt_pos = s.find("prompt=").expect("invariant: prompt present");
        assert!(access_pos < prompt_pos, "order preserved: url={s}");
    }

    #[test]
    fn empty_extras_produce_no_extra_params() {
        let start = build_authorize_url(
            &client("https://mcp.notion.com"),
            "https://relay.test/cb",
            None,
            &[],
        )
        .expect("invariant: build_authorize_url with empty extras succeeds");
        let s = start.authorize_url.as_str();
        assert!(!s.contains("access_type="), "url={s}");
        assert!(!s.contains("prompt="), "url={s}");
    }
}
