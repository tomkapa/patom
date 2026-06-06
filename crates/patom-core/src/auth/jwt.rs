//! HS256 JWT signer for session cookies. Per CLAUDE.md §11 time enters
//! through [`SharedClock`]; never `Utc::now`.

use std::collections::HashSet;
use std::sync::LazyLock;
use std::time::{Duration, UNIX_EPOCH};

use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, encode};
use serde::{Deserialize, Serialize};

use crate::clock::SharedClock;
use crate::types::SecretString;

use super::error::AuthError;
use super::limits::{JWT_SECRET_MIN_BYTES, JWT_TTL};
use super::types::{OrgId, UserId};

/// `exp` and `iat` are the only claims we require on every verify; built once so
/// each `verify` call doesn't reallocate the set.
static REQUIRED_CLAIMS: LazyLock<HashSet<String>> =
    LazyLock::new(|| ["exp", "iat"].iter().map(|s| (*s).to_owned()).collect());

/// Clock-skew tolerance for the `exp` check on verify. Matches the
/// previous `Validation::leeway` value.
const EXP_LEEWAY_SECS: i64 = 5;

/// Claims minted on login and consumed by [`crate::http::auth_layer`].
/// `sub` = user id, `org` = active org id, `exp/iat` = epoch seconds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JwtClaims {
    pub sub: UserId,
    pub org: OrgId,
    pub iat: i64,
    pub exp: i64,
}

/// Process-wide signer. Cheap to clone — `EncodingKey` and `DecodingKey`
/// are reference-counted internally by `jsonwebtoken`.
#[derive(Clone)]
pub struct JwtSigner {
    encoding: EncodingKey,
    decoding: DecodingKey,
    ttl: Duration,
    clock: SharedClock,
}

impl std::fmt::Debug for JwtSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JwtSigner")
            .field("ttl_seconds", &self.ttl.as_secs())
            .finish_non_exhaustive()
    }
}

impl JwtSigner {
    /// Construct a signer from a shared secret. Fails fast if the
    /// secret is too short to be safe for HS256.
    pub fn new(secret: &SecretString, clock: SharedClock) -> Result<Self, AuthError> {
        let bytes = secret.expose().as_bytes();
        if bytes.len() < JWT_SECRET_MIN_BYTES {
            return Err(AuthError::Internal(
                "jwt secret too short (need >= 32 bytes)",
            ));
        }
        Ok(Self {
            encoding: EncodingKey::from_secret(bytes),
            decoding: DecodingKey::from_secret(bytes),
            ttl: JWT_TTL,
            clock,
        })
    }

    /// Override the default TTL — for tests that want short-lived tokens.
    #[must_use]
    pub fn with_ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }

    /// TTL in seconds as an `i64`. Centralised so the `Set-Cookie` `Max-Age`
    /// stays in lockstep with the JWT `exp`. Asserts the TTL fits in `i64`
    /// (any conceivable session TTL does — `i64` seconds covers ~292 billion
    /// years).
    #[must_use]
    pub fn ttl_secs(&self) -> i64 {
        i64::try_from(self.ttl.as_secs())
            .expect("invariant: JWT_TTL fits in i64 (config-controlled, weeks at most)")
    }

    /// Mint a JWT for `(user, org)` with the configured TTL.
    pub fn mint(&self, user: UserId, org: OrgId) -> Result<String, AuthError> {
        let now = self.now_epoch();
        let claims = JwtClaims {
            sub: user,
            org,
            iat: now,
            exp: now.saturating_add(self.ttl_secs()),
        };
        encode(&Header::new(Algorithm::HS256), &claims, &self.encoding)
            .map_err(|e| AuthError::Jwt(e.to_string()))
    }

    /// Verify signature + expiry; return the decoded claims. `exp` is checked
    /// against [`SharedClock`] (not `SystemTime::now`) so tests using a
    /// [`crate::clock::TestClock`] can drive expiry deterministically per
    /// CLAUDE.md §11.
    pub fn verify(&self, token: &str) -> Result<JwtClaims, AuthError> {
        let mut validation = Validation::new(Algorithm::HS256);
        validation.validate_exp = false;
        validation.required_spec_claims.clone_from(&REQUIRED_CLAIMS);
        let data = decode::<JwtClaims>(token, &self.decoding, &validation)
            .map_err(|e| AuthError::Jwt(e.to_string()))?;
        if data.claims.exp + EXP_LEEWAY_SECS < self.now_epoch() {
            return Err(AuthError::Jwt("token expired".to_owned()));
        }
        Ok(data.claims)
    }

    fn now_epoch(&self) -> i64 {
        // Both unwraps are assertions per CLAUDE.md §6: `duration_since(UNIX_EPOCH)`
        // only fails if the wall clock is before 1970 (impossible in any deployed
        // environment), and an i64 of seconds-since-epoch overflows ~year 292277025020.
        let secs = self
            .clock
            .now_wall()
            .duration_since(UNIX_EPOCH)
            .expect("invariant: system clock not before 1970")
            .as_secs();
        i64::try_from(secs).expect("invariant: seconds-since-epoch fits in i64")
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use crate::clock::{SystemClock, TestClock};
    use crate::types::SecretString;

    use super::super::types::{OrgId, UserId};
    use super::JwtSigner;

    fn signer() -> JwtSigner {
        let secret = SecretString::try_from("a".repeat(64)).expect("non-empty");
        JwtSigner::new(&secret, SystemClock::shared()).expect("signer")
    }

    #[test]
    fn rejects_short_secret() {
        let secret = SecretString::try_from("short".to_owned()).expect("non-empty");
        assert!(JwtSigner::new(&secret, SystemClock::shared()).is_err());
    }

    #[test]
    fn round_trip() {
        let s = signer();
        let user = UserId::new();
        let org = OrgId::new();
        let token = s.mint(user, org).expect("mint");
        let claims = s.verify(&token).expect("verify");
        assert_eq!(claims.sub.as_uuid(), user.as_uuid());
        assert_eq!(claims.org.as_uuid(), org.as_uuid());
        assert!(claims.exp > claims.iat);
    }

    #[test]
    fn tampered_signature_rejected() {
        let s = signer();
        let token = s.mint(UserId::new(), OrgId::new()).expect("mint");
        let mut bytes: Vec<u8> = token.into_bytes();
        // Flip a bit deep in the signature.
        let last = bytes.len() - 1;
        bytes[last] ^= 0x01;
        let tampered = String::from_utf8(bytes).expect("valid utf8");
        assert!(s.verify(&tampered).is_err());
    }

    #[test]
    fn expired_token_rejected() {
        let clock = Arc::new(TestClock::new());
        let secret = SecretString::try_from("a".repeat(64)).expect("non-empty");
        let s = JwtSigner::new(&secret, clock.clone())
            .expect("signer")
            .with_ttl(Duration::from_secs(1));
        let token = s.mint(UserId::new(), OrgId::new()).expect("mint");
        // Past the leeway window (Validation::leeway = 5s above).
        clock.advance(Duration::from_secs(7));
        assert!(s.verify(&token).is_err());
    }
}
