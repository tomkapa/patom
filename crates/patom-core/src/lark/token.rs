//! `tenant_access_token` provider.
//!
//! A self-built ("internal") app mints a `tenant_access_token` from its
//! `app_id`/`app_secret` via `POST /open-apis/auth/v3/tenant_access_token/
//! internal`. The token is cached per app and re-minted
//! [`LARK_TOKEN_REFRESH_SKEW`] before its reported `expire` (Clock-driven, §11)
//! so a request never races the expiry boundary.
//!
//! The per-app secret is resolved through an [`AppSecretSource`] (implemented by
//! the `lark_apps` store) so this module stays free of DB types. Outbound code
//! depends on the [`TokenProvider`] trait, so tests inject [`FakeTokenProvider`]
//! and never hit the network.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::clock::SharedClock;

use super::error::LarkError;
use super::limits::{LARK_TOKEN_REFRESH_SKEW, LARK_TOKEN_TIMEOUT};
use super::types::{LarkAppId, LarkAppSecret};

const TOKEN_PATH: &str = "/open-apis/auth/v3/tenant_access_token/internal";

/// A minted tenant access token plus the deadline after which it must be
/// re-minted. `Debug` redacts the secret bytes.
#[derive(Clone)]
pub struct TenantAccessToken {
    secret: String,
    /// `clock.now()` instant at/after which this token is considered stale
    /// (already discounted by [`LARK_TOKEN_REFRESH_SKEW`]).
    refresh_after: Instant,
}

impl TenantAccessToken {
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.secret
    }

    /// Fresh iff `now` is before the (skew-adjusted) refresh deadline.
    #[must_use]
    pub fn is_fresh(&self, now: Instant) -> bool {
        now < self.refresh_after
    }
}

impl std::fmt::Debug for TenantAccessToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TenantAccessToken")
            .field("secret", &"***")
            .field("refresh_after", &self.refresh_after)
            .finish()
    }
}

/// Resolves the (decrypted) `app_secret` for an app id. Implemented by the
/// `lark_apps` store; kept as a trait so the token provider has no DB types.
#[async_trait]
pub trait AppSecretSource: std::fmt::Debug + Send + Sync {
    async fn secret(&self, app_id: &LarkAppId) -> Result<LarkAppSecret, LarkError>;
}

/// Hands out a fresh `tenant_access_token` for an app id.
#[async_trait]
pub trait TokenProvider: std::fmt::Debug + Send + Sync {
    async fn token(&self, app_id: &LarkAppId) -> Result<TenantAccessToken, LarkError>;
}

/// Shared handle to a [`TokenProvider`].
pub type SharedTokenProvider = std::sync::Arc<dyn TokenProvider>;

/// Production provider: mint-on-miss + per-app cache, refreshed by [`SharedClock`].
#[derive(Debug)]
pub struct CachingTokenProvider {
    http: reqwest::Client,
    api_base: String,
    source: std::sync::Arc<dyn AppSecretSource>,
    clock: SharedClock,
    cache: Mutex<HashMap<LarkAppId, TenantAccessToken>>,
}

impl CachingTokenProvider {
    #[must_use]
    pub fn new(
        http: reqwest::Client,
        api_base: String,
        source: std::sync::Arc<dyn AppSecretSource>,
        clock: SharedClock,
    ) -> Self {
        Self {
            http,
            api_base,
            source,
            clock,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Cached token if still fresh. Lock is taken and released synchronously
    /// (no `await` held) so it never straddles I/O.
    fn cached_fresh(&self, app_id: &LarkAppId, now: Instant) -> Option<TenantAccessToken> {
        let guard = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.get(app_id).filter(|t| t.is_fresh(now)).cloned()
    }

    fn store(&self, app_id: &LarkAppId, token: &TenantAccessToken) {
        let mut guard = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.insert(app_id.clone(), token.clone());
    }
}

#[async_trait]
impl TokenProvider for CachingTokenProvider {
    async fn token(&self, app_id: &LarkAppId) -> Result<TenantAccessToken, LarkError> {
        if let Some(t) = self.cached_fresh(app_id, self.clock.now()) {
            return Ok(t);
        }
        let secret = self.source.secret(app_id).await?;
        let (raw, expire_secs) = mint(&self.http, &self.api_base, app_id, &secret).await?;
        let token = TenantAccessToken {
            secret: raw,
            refresh_after: refresh_deadline(self.clock.now(), expire_secs),
        };
        self.store(app_id, &token);
        Ok(token)
    }
}

/// The instant at which a token minted `now` with TTL `expire_secs` should be
/// re-minted: `now + (expire - skew)`, never in the past.
#[must_use]
pub fn refresh_deadline(now: Instant, expire_secs: i64) -> Instant {
    let skew = LARK_TOKEN_REFRESH_SKEW.as_secs();
    let usable = u64::try_from(expire_secs).unwrap_or(0).saturating_sub(skew);
    now + std::time::Duration::from_secs(usable)
}

/// One timed HTTP mint. Returns `(token, expire_secs)`.
async fn mint(
    http: &reqwest::Client,
    api_base: &str,
    app_id: &LarkAppId,
    secret: &LarkAppSecret,
) -> Result<(String, i64), LarkError> {
    let url = format!("{api_base}{TOKEN_PATH}");
    let body = MintRequest {
        app_id: app_id.as_str(),
        app_secret: secret.expose(),
    };
    let send = http.post(&url).json(&body).send();
    let resp = tokio::time::timeout(LARK_TOKEN_TIMEOUT, send)
        .await
        .map_err(|_| LarkError::TokenMint("token mint timed out".to_owned()))??;
    let bytes = resp.bytes().await?;
    parse_token_response(&bytes)
}

/// Pure parse of the mint response into `(token, expire_secs)`.
fn parse_token_response(body: &[u8]) -> Result<(String, i64), LarkError> {
    let resp: MintResponse = serde_json::from_slice(body)?;
    if resp.code != 0 {
        let msg = if resp.msg.is_empty() {
            format!("token mint code {}", resp.code)
        } else {
            resp.msg
        };
        return Err(LarkError::TokenMint(msg));
    }
    let token = resp
        .tenant_access_token
        .filter(|t| !t.is_empty())
        .ok_or_else(|| LarkError::TokenMint("token mint ok but token empty".to_owned()))?;
    // A non-positive `expire` would make the token instantly stale (re-mint on
    // every call). Treat it as a malformed upstream response and fail fast.
    if resp.expire <= 0 {
        return Err(LarkError::TokenMint(
            "token mint ok but expire <= 0".to_owned(),
        ));
    }
    Ok((token, resp.expire))
}

#[derive(Serialize)]
struct MintRequest<'a> {
    app_id: &'a str,
    app_secret: &'a str,
}

#[derive(Deserialize)]
struct MintResponse {
    code: i32,
    #[serde(default)]
    msg: String,
    #[serde(default)]
    tenant_access_token: Option<String>,
    #[serde(default)]
    expire: i64,
}

/// Test-only provider that returns a fixed never-expiring token. Not
/// `#[cfg(test)]` so integration tests in `tests/` can inject it.
#[derive(Debug)]
pub struct FakeTokenProvider {
    token: String,
}

impl FakeTokenProvider {
    #[must_use]
    pub fn new(token: impl Into<String>) -> Self {
        Self {
            token: token.into(),
        }
    }
}

#[async_trait]
impl TokenProvider for FakeTokenProvider {
    async fn token(&self, _app_id: &LarkAppId) -> Result<TenantAccessToken, LarkError> {
        // `refresh_after` far in the future so `is_fresh` is always true.
        Ok(TenantAccessToken {
            secret: self.token.clone(),
            refresh_after: Instant::now() + std::time::Duration::from_hours(1),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ok_token() {
        let body = br#"{"code":0,"msg":"ok","tenant_access_token":"t-abc","expire":7200}"#;
        let (token, expire) = parse_token_response(body).expect("ok");
        assert_eq!(token, "t-abc");
        assert_eq!(expire, 7200);
    }

    #[test]
    fn non_zero_code_is_mint_error() {
        let body = br#"{"code":99991663,"msg":"app ticket invalid"}"#;
        let err = parse_token_response(body).expect_err("err");
        assert!(matches!(err, LarkError::TokenMint(m) if m == "app ticket invalid"));
    }

    #[test]
    fn ok_with_empty_token_is_error() {
        let body = br#"{"code":0,"tenant_access_token":"","expire":7200}"#;
        assert!(parse_token_response(body).is_err());
    }

    #[test]
    fn ok_with_nonpositive_expire_is_error() {
        // A success code with a missing / zero / negative expiry is malformed —
        // accepting it would re-mint on every call. Reject it instead.
        for body in [
            &br#"{"code":0,"tenant_access_token":"t-abc","expire":0}"#[..],
            &br#"{"code":0,"tenant_access_token":"t-abc","expire":-1}"#[..],
            &br#"{"code":0,"tenant_access_token":"t-abc"}"#[..],
        ] {
            assert!(
                matches!(parse_token_response(body), Err(LarkError::TokenMint(_))),
                "non-positive expire must be a mint error"
            );
        }
    }

    #[test]
    fn refresh_deadline_discounts_skew() {
        let now = Instant::now();
        let skew = LARK_TOKEN_REFRESH_SKEW.as_secs();
        // A 7200s token refreshes after 7200 - skew seconds.
        let d = refresh_deadline(now, 7200);
        let expected = now + std::time::Duration::from_secs(7200 - skew);
        assert_eq!(d, expected);
    }

    #[test]
    fn refresh_deadline_never_before_now() {
        let now = Instant::now();
        // A TTL below the skew window clamps to `now` (re-mint immediately).
        let d = refresh_deadline(now, 10);
        assert_eq!(d, now);
    }

    #[test]
    fn token_freshness_tracks_deadline() {
        let now = Instant::now();
        let token = TenantAccessToken {
            secret: "t".to_owned(),
            refresh_after: now + std::time::Duration::from_secs(100),
        };
        assert!(token.is_fresh(now));
        assert!(token.is_fresh(now + std::time::Duration::from_secs(99)));
        assert!(!token.is_fresh(now + std::time::Duration::from_secs(100)));
    }

    #[test]
    fn token_debug_redacts_secret() {
        let token = TenantAccessToken {
            secret: "super-secret".to_owned(),
            refresh_after: Instant::now(),
        };
        let dbg = format!("{token:?}");
        assert!(!dbg.contains("super-secret"));
    }
}
