//! Browser flow: PKCE + DCR + authorize URL + code exchange + refresh.
//!
//! Uses the `oauth2` crate (already a dep) for the PKCE + token-exchange
//! pieces; DCR (RFC 7591) is a one-shot POST rendered directly.

use std::time::Duration;

use oauth2::basic::BasicClient;
use oauth2::reqwest::Client as OAuthHttpClient;
use oauth2::{
    AuthType, AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken, EndpointNotSet,
    EndpointSet, PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, RefreshToken, Scope,
    TokenResponse, TokenUrl,
};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::time::timeout;
use url::Url;

use crate::types::SecretString;

use super::discovery::AsMetadata;
use super::errors::OAuthError;
use super::store::{OAuthClientCreds, OAuthClientId, TokenAuthMethod};

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

/// Output of [`build_authorize_url`].
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

/// Minimal credential bundle [`refresh_oauth_token`] needs.
///
/// The refresh grant doesn't touch the authorization endpoint, so the
/// persisted `OAuth2Payload` carries only what's listed here (`issuer`,
/// `token_endpoint`, `token_endpoint_auth_method`, plus DCR client
/// material for `client_source = 'dcr'` entries — Platform entries
/// reconstruct credentials via env + catalog).
///
/// `redirect_uri` is mandatory: some ASes (Microsoft Azure AD with
/// strict redirect-URI validation) echo back the original redirect on
/// the refresh grant.
#[derive(Debug, Clone)]
pub struct RefreshCreds {
    pub issuer: String,
    pub token_endpoint: String,
    pub client_id: OAuthClientId,
    pub client_secret: Option<SecretString>,
    pub token_endpoint_auth_method: TokenAuthMethod,
    pub redirect_uri: String,
}

/// RFC 7591 Dynamic Client Registration. POSTs the smallest viable
/// metadata document to `registration_endpoint`; refuses to proceed if
/// the AS metadata doesn't advertise one (the catalog entry is then
/// either marked `client_source = 'platform'` or unsupported).
#[tracing::instrument(
    name = "mcp.oauth.dcr",
    skip_all,
    fields(
        patom.mcp.oauth.issuer = %as_metadata.issuer,
    ),
)]
pub(super) async fn register_dynamic_client(
    flow: &OAuthFlowClient,
    as_metadata: &AsMetadata,
    redirect_uri: &str,
    scope: Option<&str>,
) -> Result<OAuthClientCreds, OAuthError> {
    let registration_endpoint = as_metadata
        .registration_endpoint
        .as_deref()
        .ok_or_else(|| {
            OAuthError::Dcr(format!(
                "issuer {} does not advertise registration_endpoint; mark catalog \
             `client_source = 'platform'` and configure env credentials",
                as_metadata.issuer
            ))
        })?;

    let supported = as_metadata
        .token_endpoint_auth_methods_supported
        .as_deref()
        .unwrap_or(&[]);
    let pick = |method: TokenAuthMethod| supported.iter().any(|m| m == method.as_str());
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
        client_name: "Patom",
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
    let client_secret = raw
        .client_secret
        .map(SecretString::try_from)
        .transpose()
        .map_err(|e| OAuthError::Dcr(format!("invalid client_secret: {e}")))?;
    let client_id = OAuthClientId::try_from(raw.client_id)
        .map_err(|e| OAuthError::Dcr(format!("invalid client_id: {e}")))?;
    Ok(OAuthClientCreds {
        issuer: as_metadata.issuer.clone(),
        client_id,
        client_secret,
        authorization_endpoint: as_metadata.authorization_endpoint.clone(),
        token_endpoint: as_metadata.token_endpoint.clone(),
        token_endpoint_auth_method: auth_method,
        scope: scope.map(str::to_owned),
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
}

/// Build the authorize URL the browser will be redirected to. PKCE +
/// state are minted here; the caller persists them in
/// `mcp_oauth_pending` for the callback to consume.
pub fn build_authorize_url(
    client: &OAuthClientCreds,
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
        patom.mcp.oauth.issuer = %client.issuer,
    ),
)]
pub async fn exchange_code(
    flow: &OAuthFlowClient,
    client: &OAuthClientCreds,
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

/// Map our token-endpoint auth method to the `oauth2` crate's wire-side
/// `AuthType`. The crate defaults to `BasicAuth` if `set_auth_type` is
/// not called, so a `set_auth_type` call is mandatory for `_Post` ASes
/// (Google, some Notion-style DCR ASes) — otherwise the secret rides in
/// the `Authorization: Basic` header and the AS replies `invalid_client`.
const fn auth_type_for(method: TokenAuthMethod) -> AuthType {
    match method {
        TokenAuthMethod::ClientSecretPost => AuthType::RequestBody,
        // None has no secret to place anywhere; `BasicAuth` is harmless.
        TokenAuthMethod::ClientSecretBasic | TokenAuthMethod::None => AuthType::BasicAuth,
    }
}

/// Authorize + exchange paths must agree byte-for-byte on the client
/// config, or PKCE silently fails the comparison; build the `oauth2`
/// `BasicClient` here so both call sites see the same shape.
fn build_basic_client(
    client: &OAuthClientCreds,
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
        .set_auth_uri(auth_url)
        .set_auth_type(auth_type_for(client.token_endpoint_auth_method));
    if let Some(secret) = &client.client_secret {
        b = b.set_client_secret(ClientSecret::new(secret.expose().to_owned()));
    }
    Ok(b.set_token_uri(token_url).set_redirect_uri(redirect))
}

/// Refresh-only variant of [`build_basic_client`]. The refresh grant
/// doesn't touch the authorization endpoint, so we synthesize a dummy
/// `AuthUrl` from the same origin as the token endpoint to satisfy the
/// `oauth2` crate's type state (`set_auth_uri` is mandatory before
/// `exchange_refresh_token`). `redirect_uri` is still attached because
/// some ASes (notably Microsoft Azure AD with strict redirect
/// validation) echo back the original redirect on the refresh grant.
fn build_refresh_client(
    creds: &RefreshCreds,
) -> Result<
    BasicClient<EndpointSet, EndpointNotSet, EndpointNotSet, EndpointNotSet, EndpointSet>,
    OAuthError,
> {
    let token_url = TokenUrl::new(creds.token_endpoint.clone())
        .map_err(|e| OAuthError::Misconfigured(format!("token_endpoint: {e}")))?;
    let dummy_auth_url = AuthUrl::new(creds.token_endpoint.clone())
        .map_err(|e| OAuthError::Misconfigured(format!("token_endpoint as auth_url: {e}")))?;
    let redirect = RedirectUrl::new(creds.redirect_uri.clone())
        .map_err(|e| OAuthError::Misconfigured(format!("redirect_uri: {e}")))?;
    let mut b = BasicClient::new(ClientId::new(creds.client_id.as_str().to_owned()))
        .set_auth_uri(dummy_auth_url)
        .set_auth_type(auth_type_for(creds.token_endpoint_auth_method));
    if let Some(secret) = &creds.client_secret {
        b = b.set_client_secret(ClientSecret::new(secret.expose().to_owned()));
    }
    Ok(b.set_token_uri(token_url).set_redirect_uri(redirect))
}

/// Result of [`refresh_oauth_token`]. The caller decides what to do on
/// each variant — typically: `Refreshed` → seal + persist the new token;
/// `Revoked` → flip `connection_status = 'reconnect_required'`.
#[derive(Debug)]
pub(super) enum RefreshOutcome {
    Refreshed(TokenExchangeResult),
    Revoked,
}

/// Exchange `refresh_token` for a fresh access token. The redirect_uri
/// isn't strictly required for the refresh grant by RFC 6749 §6.
#[tracing::instrument(
    name = "mcp.oauth.refresh",
    skip_all,
    fields(
        patom.mcp.oauth.issuer = %creds.issuer,
    ),
)]
pub(super) async fn refresh_oauth_token(
    flow: &OAuthFlowClient,
    creds: &RefreshCreds,
    refresh_token: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<RefreshOutcome, OAuthError> {
    let oauth_client = build_refresh_client(creds)?;
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
            // for "refresh token revoked / expired" per RFC 6749 §5.2.
            let s = e.to_string();
            if s.contains("invalid_grant") {
                tracing::warn!(error = %e, "mcp.oauth.refresh.revoked");
                return Ok(RefreshOutcome::Revoked);
            }
            return Err(OAuthError::TokenEndpoint(format!("refresh: {e}")));
        }
    };
    let access_token = token.access_token().secret().clone();
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
        issuer: creds.issuer.clone(),
        token_endpoint: creds.token_endpoint.clone(),
    }))
}

#[cfg(test)]
mod tests {
    use super::super::store::{OAuthClientCreds, OAuthClientId, TokenAuthMethod};
    use super::build_authorize_url;

    fn client(issuer: &str) -> OAuthClientCreds {
        OAuthClientCreds {
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
            "https://patom.test/cb",
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
            "https://patom.test/cb",
            None,
            &[],
        )
        .expect("invariant: build_authorize_url with empty extras succeeds");
        let s = start.authorize_url.as_str();
        assert!(!s.contains("access_type="), "url={s}");
        assert!(!s.contains("prompt="), "url={s}");
    }
}
