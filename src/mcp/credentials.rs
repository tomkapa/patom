//! Sensitive credential payloads for MCP servers.
//!
//! Lives outside `McpTransport` so the seam between "URL/config (plaintext)"
//! and "secret material (envelope-encrypted)" is enforced by construction.
//! The payload is JSON-encoded then sealed via [`crate::crypto::OrgEncryptor`]
//! before being written to `mcp_server_credentials`. Decryption is performed
//! only inside the registry's connect path; the HTTP response never echoes
//! plaintext back.
//!
//! Two variants share the same on-the-wire and on-disk envelope; the `kind`
//! column in `mcp_server_credentials` records which payload the ciphertext
//! decrypts to so a future variant is a `kind`-CHECK update and one match
//! arm here.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::auth::OrgId;
use crate::crypto::SharedOrgEncryptor;

use super::error::McpError;
use super::types::{McpHeaderName, McpHeaderValue, McpServerId};

/// Maximum byte length of an encoded credential payload before sealing.
///
/// A safety belt: each field is already length-capped by its `TryFrom` impl,
/// but if a future variant gains an unbounded field the cap traps the
/// regression at the boundary.
pub const MAX_CREDENTIAL_PLAINTEXT: usize = 64 * 1024;

/// Plaintext credential payload as it lives in memory after decrypt or just
/// before encrypt. Serialised to JSON inside the AEAD envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CredentialPayload {
    /// Static custom headers (e.g. `Authorization: Bearer <token>`) attached
    /// to every outbound MCP request. The map is itself capped at
    /// [`MCP_MAX_HEADERS`] entries by the boundary parser.
    StaticHeaders {
        headers: BTreeMap<McpHeaderName, McpHeaderValue>,
    },
    /// OAuth-issued access + refresh tokens. Populated by phase C; the
    /// variant exists in phase B so the `kind` column already knows about
    /// it and migrations don't churn.
    Oauth2(OAuth2Payload),
}

/// OAuth credential payload. Stored encrypted alongside `static_headers`.
///
/// Hand-rolled `Debug` that redacts `access_token` / `refresh_token` /
/// `dcr_client_secret`: the struct travels through tracing spans and panic
/// backtraces, and a stray `?payload` would otherwise leak the bearer
/// (CLAUDE.md §2).
///
/// DCR client material lives inside the same encrypted envelope so one
/// row per `(server_id, org_id)` holds everything needed to refresh —
/// access_token, refresh_token, and (for DCR vendors) the client_id /
/// client_secret the AS issued at registration. Platform-credential
/// entries (where the client comes from env) leave the `dcr_*` fields
/// `None`.
#[derive(Clone, Serialize, Deserialize)]
pub struct OAuth2Payload {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: chrono::DateTime<chrono::Utc>,
    pub scope: Option<String>,
    pub issuer: String,
    pub token_endpoint: String,
    /// DCR-issued client_id for this server. `None` for `client_source =
    /// Platform` entries (the resolver pulls those from env).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dcr_client_id: Option<String>,
    /// DCR-issued client_secret. Confidential; redacted in `Debug`.
    /// `None` when the AS issued a public (PKCE-only) client or when the
    /// entry uses platform credentials.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dcr_client_secret: Option<String>,
    /// Token-endpoint auth method picked at start time from
    /// `token_endpoint_auth_methods_supported`. Tracked for **both**
    /// Platform and DCR entries so the refresh path doesn't re-discover
    /// AS metadata to know whether the secret rides on the
    /// Authorization header or in the POST body. `None` only for
    /// pre-this-PR rows decoded via `#[serde(default)]` — the refresh
    /// path falls back to `ClientSecretBasic` (the RFC 7591 default)
    /// when that happens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_endpoint_auth_method: Option<super::types::TokenAuthMethod>,
}

impl fmt::Debug for OAuth2Payload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuth2Payload")
            .field("access_token", &"***")
            .field("refresh_token", &self.refresh_token.as_ref().map(|_| "***"))
            .field("expires_at", &self.expires_at)
            .field("scope", &self.scope)
            .field("issuer", &self.issuer)
            .field("token_endpoint", &self.token_endpoint)
            .field("dcr_client_id", &self.dcr_client_id)
            .field(
                "dcr_client_secret",
                &self.dcr_client_secret.as_ref().map(|_| "***"),
            )
            .field(
                "token_endpoint_auth_method",
                &self.token_endpoint_auth_method,
            )
            .finish()
    }
}

/// Stable label for the `static_headers` variant in the `kind` column.
pub const STATIC_HEADERS_KIND_LABEL: &str = "static_headers";
/// Stable label for the `oauth2` variant in the `kind` column.
pub const OAUTH2_KIND_LABEL: &str = "oauth2";

impl CredentialPayload {
    /// Stable kind label for the `mcp_server_credentials.kind` column.
    #[must_use]
    pub const fn kind_label(&self) -> &'static str {
        match self {
            Self::StaticHeaders { .. } => STATIC_HEADERS_KIND_LABEL,
            Self::Oauth2(_) => OAUTH2_KIND_LABEL,
        }
    }
}

/// Boundary type that captures a "set / replace credentials" request after
/// validation. Lives here rather than in HTTP so the future OAuth callback
/// path uses the same constructor.
#[derive(Debug, Clone)]
pub struct McpCredentialWrite {
    pub server_id: McpServerId,
    pub org_id: OrgId,
    pub payload: CredentialPayload,
}

/// Decrypted credential record returned by the store.
#[derive(Debug, Clone)]
pub struct McpCredentialRecord {
    pub server_id: McpServerId,
    pub org_id: OrgId,
    pub payload: CredentialPayload,
}

/// Storage trait for the credential seam.
#[async_trait]
pub trait McpCredentialStore: fmt::Debug + Send + Sync {
    /// Insert or replace the credential row for `server_id`. The previous
    /// ciphertext is overwritten in a single statement; replacement must
    /// not expose the old value, so we never read it back first.
    async fn upsert(&self, write: McpCredentialWrite) -> Result<(), McpError>;

    /// Delete the credential row for `server_id`. Idempotent — deleting a
    /// row that doesn't exist returns `Ok(())`.
    async fn delete(&self, server_id: McpServerId, org_id: OrgId) -> Result<(), McpError>;

    /// **PRIVILEGED — cross-tenant.** Load every credential row. The only
    /// legitimate caller is the registry refresher (one process-wide task
    /// that connects every enabled server). Never call from an HTTP
    /// handler. RLS is bypassed.
    async fn list_all(&self) -> Result<Vec<McpCredentialRecord>, McpError>;

    /// **PRIVILEGED — cross-tenant.** Load a single credential row by
    /// server id. Returns `None` when the server has no credentials. Same
    /// caveat as [`Self::list_all`].
    async fn read(
        &self,
        server_id: McpServerId,
        org_id: OrgId,
    ) -> Result<Option<McpCredentialRecord>, McpError>;
}

pub type SharedMcpCredentialStore = Arc<dyn McpCredentialStore>;

/// Seal a [`CredentialPayload`] under the org KEK and return `(kind, blob)`.
///
/// The intermediate JSON encoding is dropped (and so cleared from this stack
/// frame's allocator) before the function returns; the AEAD ciphertext is
/// the only post-seal artefact.
pub(super) fn seal_payload(
    enc: &SharedOrgEncryptor,
    org: OrgId,
    payload: &CredentialPayload,
) -> Result<(String, crate::crypto::EncryptedBlob), McpError> {
    let json = serde_json::to_vec(payload)
        .map_err(|e| McpError::Backend(format!("encode credential: {e}")))?;
    if json.len() > MAX_CREDENTIAL_PLAINTEXT {
        return Err(McpError::InvalidConfig(format!(
            "credential payload exceeds {MAX_CREDENTIAL_PLAINTEXT} bytes"
        )));
    }
    let blob = enc.seal(org, &json).map_err(McpError::from)?;
    Ok((payload.kind_label().to_owned(), blob))
}

/// Open a stored credential blob and decode it back into a typed payload.
///
/// Variant-mismatch (`kind` column claims `oauth2` but the JSON decodes as
/// `static_headers`) trips a typed `Backend` error so the boundary can map
/// it onto a 500 / log site rather than a silent fall-through.
pub fn open_payload(
    enc: &SharedOrgEncryptor,
    org: OrgId,
    kind: &str,
    blob: &crate::crypto::EncryptedBlob,
) -> Result<CredentialPayload, McpError> {
    let plaintext = enc.open(org, blob).map_err(McpError::from)?;
    let payload: CredentialPayload = serde_json::from_slice(plaintext.as_slice())
        .map_err(|e| McpError::Backend(format!("decode credential: {e}")))?;
    if payload.kind_label() != kind {
        return Err(McpError::Backend(format!(
            "credential kind/{kind} mismatches payload/{}",
            payload.kind_label()
        )));
    }
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::OrgId;
    use crate::crypto::OrgEncryptor;

    fn enc() -> SharedOrgEncryptor {
        Arc::new(OrgEncryptor::for_test([3u8; 32]))
    }

    #[test]
    fn static_headers_roundtrip() {
        let e = enc();
        let org = OrgId::new();
        let mut headers = BTreeMap::new();
        headers.insert(
            McpHeaderName::try_from("authorization".to_owned()).expect("valid"),
            McpHeaderValue::try_from("Bearer sk-test-xyz".to_owned()).expect("valid"),
        );
        let p = CredentialPayload::StaticHeaders { headers };
        let (kind, blob) = seal_payload(&e, org, &p).expect("seal");
        assert_eq!(kind, "static_headers");
        let back = open_payload(&e, org, &kind, &blob).expect("open");
        match back {
            CredentialPayload::StaticHeaders { headers } => {
                let v = headers
                    .get(&McpHeaderName::try_from("authorization".to_owned()).expect("valid"))
                    .expect("header preserved");
                assert_eq!(v.as_str(), "Bearer sk-test-xyz");
            }
            CredentialPayload::Oauth2(_) => panic!("variant mismatch"),
        }
    }

    #[test]
    fn kind_mismatch_is_rejected() {
        let e = enc();
        let org = OrgId::new();
        let p = CredentialPayload::StaticHeaders {
            headers: BTreeMap::new(),
        };
        let (_kind, blob) = seal_payload(&e, org, &p).expect("seal");
        let err = open_payload(&e, org, "oauth2", &blob).expect_err("kind mismatch");
        assert!(matches!(err, McpError::Backend(_)));
    }
}
