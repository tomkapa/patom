//! Standards-compliant OpenID Connect provider (ADR-0011).
//!
//! Wraps the `openidconnect` crate so the rest of `auth/` sees a small,
//! testable surface. Endpoints are discovered once at startup from
//! `{issuer}/.well-known/openid-configuration` (JWKS pulled alongside);
//! `start()` mints the authorize URL + the `(state, verifier, nonce)`
//! the callback must store; `exchange()` swaps the callback `code` for a
//! signature- and nonce-verified id_token and reads the claims into an
//! [`OidcProfile`].
//!
//! Google is not special-cased here — it is one issuer
//! (`https://accounts.google.com`) wired in at the composition root
//! (ADR-0011 "Google is one preset"). The `OidcAuth` trait is the test
//! seam: production stores an [`OidcProvider`]; tests inject a fake
//! without driving discovery or a real IdP.

use std::time::Duration;

use async_trait::async_trait;
use openidconnect::core::{CoreAuthenticationFlow, CoreClient, CoreProviderMetadata};
use openidconnect::reqwest as oidc_http;
use openidconnect::{
    AuthorizationCode, ClientId, ClientSecret, CsrfToken, EndpointMaybeSet, EndpointNotSet,
    EndpointSet, IssuerUrl as OidcIssuerUrl, Nonce, PkceCodeChallenge, PkceCodeVerifier,
    RedirectUrl, Scope, TokenResponse,
};
use tracing::warn;
use url::Url;

use crate::types::SecretString;

use super::error::AuthError;
use super::limits::{OAUTH_HTTP_TIMEOUT, OIDC_DISCOVERY_TIMEOUT};
use super::locale_hint::LocaleHint;
use super::types::{
    Email, IssuerUrl, OAuthState, OidcNonce, OidcProfile, OidcSubject, PkceVerifier,
};

/// `openidconnect`'s reqwest-backed HTTP client (its `AsyncHttpClient`).
/// Re-exported through `openidconnect`/`oauth2` so we share one TLS
/// stack and don't mix it with the project's top-level `reqwest`.
type HttpClient = oidc_http::Client;

/// A `CoreClient` configured from discovery: the auth endpoint is set,
/// the token + userinfo endpoints are "maybe set" (present iff the
/// discovery document advertised them). This is exactly the type
/// `CoreClient::from_provider_metadata` returns.
type ConfiguredClient = CoreClient<
    EndpointSet,      // auth
    EndpointNotSet,   // device authorization
    EndpointNotSet,   // introspection
    EndpointNotSet,   // revocation
    EndpointMaybeSet, // token
    EndpointMaybeSet, // userinfo
>;

/// Configured OIDC provider. Cheap to clone (the inner client + reqwest
/// client are both `Arc`-backed).
#[derive(Clone)]
pub struct OidcProvider {
    client: ConfiguredClient,
    http: HttpClient,
    issuer: IssuerUrl,
}

impl std::fmt::Debug for OidcProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OidcProvider")
            .field("issuer", &self.issuer)
            .finish_non_exhaustive()
    }
}

impl OidcProvider {
    /// Discover `issuer`'s endpoints + JWKS and build a provider bound to
    /// `client_id` / `client_secret` / `redirect_url`.
    ///
    /// Discovery is the single network round-trip, timeout-bounded
    /// (§5) and fail-closed (§6): a provider we can't discover yields
    /// [`AuthError::DiscoveryFailed`] and the caller refuses to boot
    /// rather than fall back to an insecure path.
    pub async fn discover(
        issuer: &IssuerUrl,
        client_id: &SecretString,
        client_secret: &SecretString,
        redirect_url: &str,
    ) -> Result<Self, AuthError> {
        let http = HttpClient::builder()
            .timeout(OAUTH_HTTP_TIMEOUT)
            .connect_timeout(Duration::from_secs(5))
            // Never auto-follow redirects on the OAuth/discovery calls —
            // a redirect off the configured issuer would defeat the
            // point of discovery. Matches Google's OAuth guidance.
            .redirect(oidc_http::redirect::Policy::none())
            .build()
            .map_err(|e| AuthError::OAuthProvider(format!("http client: {e}")))?;

        let oidc_issuer = OidcIssuerUrl::new(issuer.as_str().to_owned())
            .map_err(|e| AuthError::DiscoveryFailed(format!("issuer url: {e}")))?;
        let metadata = tokio::time::timeout(
            OIDC_DISCOVERY_TIMEOUT,
            CoreProviderMetadata::discover_async(oidc_issuer, &http),
        )
        .await
        .map_err(|_| AuthError::DiscoveryFailed("discovery timed out".to_owned()))?
        .map_err(|e| AuthError::DiscoveryFailed(format!("{e}")))?;

        let redirect = RedirectUrl::new(redirect_url.to_owned())
            .map_err(|e| AuthError::Misconfigured(format!("redirect url: {e}")))?;
        let client = CoreClient::from_provider_metadata(
            metadata,
            ClientId::new(client_id.expose().to_owned()),
            Some(ClientSecret::new(client_secret.expose().to_owned())),
        )
        .set_redirect_uri(redirect);

        Ok(Self {
            client,
            http,
            issuer: issuer.clone(),
        })
    }

    fn build_start(&self) -> Result<AuthStart, AuthError> {
        let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
        let (url, csrf, nonce) = self
            .client
            // 32 random bytes → 43-char base64url state, inside both the
            // `OAuthState` newtype bounds and the DB CHECK (32..=128).
            .authorize_url(
                CoreAuthenticationFlow::AuthorizationCode,
                || CsrfToken::new_random_len(32),
                Nonce::new_random,
            )
            // `openid` is added by the client; request the claims we read.
            .add_scope(Scope::new("email".to_owned()))
            .add_scope(Scope::new("profile".to_owned()))
            .set_pkce_challenge(challenge)
            .url();
        Ok(AuthStart {
            authorize_url: url,
            state: OAuthState::try_from(csrf.secret().as_str())?,
            pkce_verifier: PkceVerifier::try_from(verifier.secret().as_str())?,
            nonce: OidcNonce::try_from(nonce.secret().as_str())?,
        })
    }

    async fn exchange_code(
        &self,
        code: &str,
        verifier: &PkceVerifier,
        nonce: &OidcNonce,
    ) -> Result<OidcProfile, AuthError> {
        let request = self
            .client
            .exchange_code(AuthorizationCode::new(code.to_owned()))
            .map_err(|e| AuthError::Misconfigured(format!("token endpoint: {e}")))?
            .set_pkce_verifier(PkceCodeVerifier::new(verifier.as_str().to_owned()));
        let token = tokio::time::timeout(OAUTH_HTTP_TIMEOUT, request.request_async(&self.http))
            .await
            .map_err(|_| AuthError::OAuthProvider("token exchange timed out".to_owned()))?
            .map_err(|e| AuthError::OAuthProvider(format!("token exchange: {e}")))?;

        let id_token = token
            .id_token()
            .ok_or_else(|| AuthError::OAuthProvider("response carried no id_token".to_owned()))?;
        let expected = Nonce::new(nonce.as_str().to_owned());
        let claims = id_token
            .claims(&self.client.id_token_verifier(), &expected)
            .map_err(|e| AuthError::OAuthProvider(format!("id_token verification: {e}")))?;
        self.profile_from_claims(claims)
    }

    fn profile_from_claims(
        &self,
        claims: &openidconnect::IdTokenClaims<
            openidconnect::EmptyAdditionalClaims,
            openidconnect::core::CoreGenderClaim,
        >,
    ) -> Result<OidcProfile, AuthError> {
        if !claims.email_verified().unwrap_or(false) {
            // PII (the email) is debug-tier and stripped by production
            // exporters (§2); the WARN only records the event so the rate
            // of unverified attempts is observable without leaking subjects.
            warn!(event = "oauth.email_unverified");
            return Err(AuthError::EmailUnverified);
        }
        let email_raw = claims
            .email()
            .ok_or_else(|| AuthError::OAuthProvider("id_token missing email".to_owned()))?;
        let email = Email::try_from(email_raw.as_str())?;
        let subject = OidcSubject::try_from(claims.subject().as_str())?;
        let display_name = claims
            .name()
            .and_then(|n| n.get(None))
            .map(|n| n.as_str().to_owned());
        let avatar_url = claims
            .picture()
            .and_then(|p| p.get(None))
            .map(|p| p.as_str().to_owned());
        // Locale is a hint; a misshapen tag is dropped rather than
        // failing sign-in (we fall back to Accept-Language / DEFAULT).
        let locale = claims
            .locale()
            .and_then(|l| LocaleHint::try_from(l.as_str()).ok());
        Ok(OidcProfile {
            issuer: self.issuer.clone(),
            subject,
            email,
            email_verified: true,
            display_name,
            avatar_url,
            locale,
        })
    }
}

#[async_trait]
impl OidcAuth for OidcProvider {
    fn start(&self) -> Result<AuthStart, AuthError> {
        self.build_start()
    }

    async fn exchange(
        &self,
        code: &str,
        verifier: &PkceVerifier,
        nonce: &OidcNonce,
    ) -> Result<OidcProfile, AuthError> {
        self.exchange_code(code, verifier, nonce).await
    }
}

/// Output of [`OidcAuth::start`]: the browser redirect plus the three
/// secrets the caller stores in `oauth_login_states` for the callback.
#[derive(Debug, Clone)]
pub struct AuthStart {
    pub authorize_url: Url,
    pub state: OAuthState,
    pub pkce_verifier: PkceVerifier,
    pub nonce: OidcNonce,
}

/// Identity-establishment seam. Production uses [`OidcProvider`]; the
/// `AppState` holds a [`SharedOidcAuth`] so tests can inject a fake
/// without discovery or a live IdP.
#[async_trait]
pub trait OidcAuth: Send + Sync + std::fmt::Debug + 'static {
    /// Mint the authorize URL + the `(state, verifier, nonce)` triple to
    /// persist for the upcoming callback.
    fn start(&self) -> Result<AuthStart, AuthError>;

    /// Exchange the callback `code` for a verified profile. `verifier`
    /// and `nonce` are the values [`Self::start`] minted and the caller
    /// stored.
    async fn exchange(
        &self,
        code: &str,
        verifier: &PkceVerifier,
        nonce: &OidcNonce,
    ) -> Result<OidcProfile, AuthError>;
}

/// Shared, dyn-dispatched [`OidcAuth`] held by the `AppState`.
pub type SharedOidcAuth = std::sync::Arc<dyn OidcAuth>;
