//! BYO (bring-your-own) provider credentials (#141).
//!
//! A workspace stores its own LLM provider API key so that provider's turns
//! route to a per-org client and stop debiting platform credit (#154). This
//! module owns the typed boundary (newtypes + parse), the seal/open seam over
//! [`crate::crypto::OrgEncryptor`], and the storage trait. The Postgres impl
//! lives in [`super::pg_credentials`].
//!
//! Mirrors the `mcp/credentials.rs` seam, simplified: there is a single payload
//! shape (the API key), so no `kind` discriminator. The non-secret `base_url`
//! override is carried alongside the sealed key but never enters the AEAD
//! envelope — it lives in a plaintext column.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use thiserror::Error;

use crate::auth::OrgId;
use crate::crypto::{CryptoError, EncryptedBlob, SharedOrgEncryptor};
use crate::types::{ParseError, SecretString};

use super::catalog::Model;
use super::id::ProviderId;
use super::limits::{MAX_PROVIDER_API_KEY_BYTES, MAX_PROVIDER_BASE_URL_BYTES};

/// A BYO provider API key. Sealed before storage; only ever exposed at the
/// precise call site that builds the provider client.
///
/// Wraps [`SecretString`] so it can never land in `Debug`/`Display` output
/// (CLAUDE.md §2). The boundary parser caps length on top of the non-empty
/// invariant `SecretString` already enforces.
#[derive(Clone)]
pub struct ProviderApiKey(SecretString);

impl ProviderApiKey {
    /// Borrow the secret bytes. Call only when constructing the outbound
    /// provider client.
    #[must_use]
    pub fn expose(&self) -> &str {
        self.0.expose()
    }

    /// Borrow as a [`SecretString`] — the shape the provider client
    /// constructors ([`crate::provider::anthropic`] / `openai`) accept.
    #[must_use]
    pub fn as_secret(&self) -> &SecretString {
        &self.0
    }

    /// A masked rendering for UI/logs: keeps a short suffix so an operator can
    /// recognise *which* key is stored without revealing it. Never the full
    /// secret (CLAUDE.md §2).
    #[must_use]
    pub fn masked(&self) -> String {
        let raw = self.0.expose();
        // Show at most the last 4 bytes; mask the rest. Sub-4-char keys (only
        // possible in tests) mask entirely.
        let keep = raw.len().min(4);
        if raw.len() <= keep {
            return "•".repeat(raw.len());
        }
        let tail = &raw[raw.len() - keep..];
        format!("{}{tail}", "•".repeat(8))
    }
}

impl fmt::Debug for ProviderApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ProviderApiKey(***)")
    }
}

impl TryFrom<String> for ProviderApiKey {
    type Error = ParseError;
    fn try_from(raw: String) -> Result<Self, Self::Error> {
        if raw.len() > MAX_PROVIDER_API_KEY_BYTES {
            return Err(ParseError::TooLong {
                field: "provider_api_key",
                max: MAX_PROVIDER_API_KEY_BYTES,
                got: raw.len(),
            });
        }
        // `SecretString::try_from` rejects empty.
        let secret = SecretString::try_from(raw).map_err(|_| ParseError::Empty {
            field: "provider_api_key",
        })?;
        Ok(Self(secret))
    }
}

/// A non-secret endpoint override for a provider (proxy / compatible gateway).
/// `None` everywhere means "the provider's public default".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderBaseUrl(String);

impl ProviderBaseUrl {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume into the owned `String` the provider constructors accept.
    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

impl TryFrom<String> for ProviderBaseUrl {
    type Error = ParseError;
    fn try_from(raw: String) -> Result<Self, Self::Error> {
        if raw.is_empty() {
            return Err(ParseError::Empty {
                field: "provider_base_url",
            });
        }
        if raw.len() > MAX_PROVIDER_BASE_URL_BYTES {
            return Err(ParseError::TooLong {
                field: "provider_base_url",
                max: MAX_PROVIDER_BASE_URL_BYTES,
                got: raw.len(),
            });
        }
        // Minimal shape check: an absolute http(s) URL with a non-empty host.
        // We don't pull in a URL parser (CLAUDE.md §8) — the provider SDK does
        // the real connecting, this just traps obvious garbage at the boundary.
        let rest = raw
            .strip_prefix("https://")
            .or_else(|| raw.strip_prefix("http://"))
            .ok_or(ParseError::Malformed {
                field: "provider_base_url",
                detail: "must start with http:// or https://",
            })?;
        if rest.is_empty() || rest.starts_with('/') {
            return Err(ParseError::Malformed {
                field: "provider_base_url",
                detail: "missing host",
            });
        }
        Ok(Self(raw))
    }
}

/// A "set / replace credentials" request after parsing. The store seals the
/// key and writes the row.
#[derive(Debug, Clone)]
pub struct ProviderCredentialWrite {
    pub org_id: OrgId,
    pub provider: ProviderId,
    pub api_key: ProviderApiKey,
    pub base_url: Option<ProviderBaseUrl>,
}

/// A decrypted credential record returned by the store.
#[derive(Debug, Clone)]
pub struct ProviderCredentialRecord {
    pub org_id: OrgId,
    pub provider: ProviderId,
    pub api_key: ProviderApiKey,
    pub base_url: Option<ProviderBaseUrl>,
    pub last_validated_at: Option<DateTime<Utc>>,
}

/// One error type for the BYO credential boundary (CLAUDE.md §12).
#[derive(Debug, Error)]
pub enum ProviderCredentialError {
    /// A stored value failed to parse back into its newtype — schema/code
    /// drift, surfaced rather than silently coerced (CLAUDE.md §6).
    #[error("stored credential malformed: {0}")]
    Corrupt(&'static str),
    /// Seal/open failure from the envelope encryptor.
    #[error(transparent)]
    Crypto(#[from] CryptoError),
    /// Postgres failure.
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// Storage trait for the BYO credential seam.
///
/// All methods are **privileged / cross-tenant** (RLS bypassed): the only
/// callers are the per-org overlay refresher (process-wide) and the HTTP
/// handlers that have already gated the caller's org membership at the
/// boundary.
#[async_trait]
pub trait OrgProviderCredentialStore: fmt::Debug + Send + Sync {
    /// Insert or replace the row for `(org, provider)`. The previous ciphertext
    /// is overwritten in a single statement; we never read it back first.
    async fn upsert(&self, write: ProviderCredentialWrite) -> Result<(), ProviderCredentialError>;

    /// Delete the row for `(org, provider)`. Idempotent.
    async fn delete(
        &self,
        org_id: OrgId,
        provider: ProviderId,
    ) -> Result<(), ProviderCredentialError>;

    /// Load every credential row across all orgs. The legitimate caller is the
    /// overlay refresher.
    async fn list_all(&self) -> Result<Vec<ProviderCredentialRecord>, ProviderCredentialError>;

    /// Load every credential row for one org (masked-list / status read).
    async fn list_for_org(
        &self,
        org_id: OrgId,
    ) -> Result<Vec<ProviderCredentialRecord>, ProviderCredentialError>;

    /// Stamp `last_validated_at = now` for `(org, provider)`. No-op if the row
    /// is absent.
    async fn mark_validated(
        &self,
        org_id: OrgId,
        provider: ProviderId,
        now: DateTime<Utc>,
    ) -> Result<(), ProviderCredentialError>;

    /// Set the per-org default model. Called when the first BYO key is entered.
    async fn set_default_model(
        &self,
        org_id: OrgId,
        model: Model,
    ) -> Result<(), ProviderCredentialError>;

    /// Load every org that has a default model set. The legitimate caller is
    /// the overlay refresher (process-wide). Orgs with a NULL or unparseable
    /// model are omitted.
    async fn list_default_models(&self) -> Result<Vec<(OrgId, Model)>, ProviderCredentialError>;
}

pub type SharedOrgProviderCredentialStore = Arc<dyn OrgProviderCredentialStore>;

/// Seal a provider API key under the org KEK. The plaintext is dropped before
/// return; the AEAD ciphertext is the only post-seal artefact.
pub(super) fn seal_key(
    enc: &SharedOrgEncryptor,
    org: OrgId,
    key: &ProviderApiKey,
) -> Result<EncryptedBlob, ProviderCredentialError> {
    let blob = enc.seal(org, key.expose().as_bytes())?;
    Ok(blob)
}

/// Open a sealed key blob back into a typed [`ProviderApiKey`].
///
/// A decrypt success that yields non-UTF-8 or an over-length string means the
/// row was written under a different schema — a `Corrupt` invariant error, not
/// an operating failure.
pub(super) fn open_key(
    enc: &SharedOrgEncryptor,
    org: OrgId,
    blob: &EncryptedBlob,
) -> Result<ProviderApiKey, ProviderCredentialError> {
    let plaintext = enc.open(org, blob)?;
    let raw = std::str::from_utf8(plaintext.as_slice())
        .map_err(|_| ProviderCredentialError::Corrupt("api key not utf-8"))?;
    ProviderApiKey::try_from(raw.to_owned())
        .map_err(|_| ProviderCredentialError::Corrupt("api key fails boundary parse"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::OrgEncryptor;

    fn enc() -> SharedOrgEncryptor {
        Arc::new(OrgEncryptor::for_test([5u8; 32]))
    }

    #[test]
    fn api_key_rejects_empty_and_overlong() {
        assert!(ProviderApiKey::try_from(String::new()).is_err());
        let too_long = "x".repeat(MAX_PROVIDER_API_KEY_BYTES + 1);
        assert!(ProviderApiKey::try_from(too_long).is_err());
        assert!(ProviderApiKey::try_from("sk-ant-abc123".to_owned()).is_ok());
    }

    #[test]
    fn api_key_masked_hides_body_keeps_short_suffix() {
        let k = ProviderApiKey::try_from("sk-ant-secret-tail".to_owned()).expect("valid");
        let masked = k.masked();
        assert!(masked.ends_with("tail"), "keeps suffix: {masked}");
        assert!(!masked.contains("secret"), "hides body: {masked}");
    }

    #[test]
    fn api_key_debug_is_redacted() {
        let k = ProviderApiKey::try_from("sk-very-secret".to_owned()).expect("valid");
        assert_eq!(format!("{k:?}"), "ProviderApiKey(***)");
    }

    #[test]
    fn base_url_parses_absolute_http_only() {
        assert!(ProviderBaseUrl::try_from("https://api.example.test/v1".to_owned()).is_ok());
        assert!(ProviderBaseUrl::try_from("http://localhost:8080".to_owned()).is_ok());
        assert!(ProviderBaseUrl::try_from("ftp://x".to_owned()).is_err());
        assert!(ProviderBaseUrl::try_from("api.example.test".to_owned()).is_err());
        assert!(ProviderBaseUrl::try_from("https://".to_owned()).is_err());
        assert!(ProviderBaseUrl::try_from(String::new()).is_err());
    }

    #[test]
    fn seal_open_roundtrips_the_key() {
        let e = enc();
        let org = OrgId::new();
        let key = ProviderApiKey::try_from("sk-ant-roundtrip-xyz".to_owned()).expect("valid");
        let blob = seal_key(&e, org, &key).expect("seal");
        let back = open_key(&e, org, &blob).expect("open");
        assert_eq!(back.expose(), "sk-ant-roundtrip-xyz");
    }

    #[test]
    fn cross_org_open_is_rejected() {
        let e = enc();
        let alice = OrgId::new();
        let bob = OrgId::new();
        let key = ProviderApiKey::try_from("sk-alice-only".to_owned()).expect("valid");
        let sealed = seal_key(&e, alice, &key).expect("seal");
        let err = open_key(&e, bob, &sealed).expect_err("cross-org rejected");
        assert!(matches!(err, ProviderCredentialError::Crypto(_)));
    }
}
