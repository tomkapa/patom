//! MCP catalog: the system's known integrations (notion, linear, …)
//! plus any tenant-custom entries.
//!
//! Three layers describe a single integration. The catalog answers
//! "what could exist"; `mcp_servers` answers "what this tenant wired";
//! `McpRegistry` answers "what's live right now."

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use crate::auth::OrgId;
use crate::crypto::{EncryptedBlob, SharedOrgEncryptor};
use crate::types::ParseError;

use super::error::McpError;
use super::limits::{
    MCP_CATALOG_AUTHORIZE_EXTRA_PARAM_BYTES_MAX, MCP_CATALOG_AUTHORIZE_EXTRA_PARAMS_MAX,
    MCP_CATALOG_DESCRIPTION_MAX_LEN, MCP_CATALOG_DISPLAY_NAME_MAX_LEN, MCP_OAUTH_CLIENT_ID_MAX_LEN,
    MCP_OAUTH_CLIENT_SECRET_MAX_LEN,
};
use super::types::{McpCatalogId, McpTransport};

crate::str_enum! {
    /// Authentication mechanism the catalog entry expects when wiring.
    ///
    /// Wire-stable labels backed by the same string as the DB CHECK; the
    /// UI keys its render of the wire-card off these.
    pub enum McpAuthKind {
        OAuth2          => "oauth2",
        StaticHeaders   => "static_headers",
        None            => "none",
    }
}

crate::str_enum! {
    /// Where the OAuth client_id / client_secret for this catalog entry
    /// come from. Drives [`crate::mcp::oauth`]'s client resolver.
    ///
    /// * `Platform` — Patom-supported vendor. Credentials read from env
    ///   via [`platform_env_keys`]; the resolver short-circuits DCR.
    /// * `Dcr` — BYO vendor that supports RFC 7591 Dynamic Client
    ///   Registration. The resolver registers a fresh client on the first
    ///   `/oauth/start` and persists it inside the same encrypted
    ///   `mcp_server_credentials` envelope as the access token.
    /// * `UserSupplied` — BYO vendor where the operator pastes their own
    ///   `client_id` (+ optional `client_secret`) for a custom server URL.
    ///   The id lives plaintext and the secret sealed on the org-scoped
    ///   `mcp_catalog` row (NOT in `mcp_server_credentials`, which the
    ///   callback overwrites with the token). Read back via
    ///   [`McpCatalogStore::oauth_client`] in the start/callback paths.
    /// * `None` — no OAuth (`auth_kind = 'none'` rows). Only here so the
    ///   resolver's `match` is exhaustive.
    pub enum ClientSource {
        Platform     => "platform",
        Dcr          => "dcr",
        UserSupplied => "user_supplied",
        None         => "none",
    }
}

/// A user-supplied OAuth `client_id` for a custom catalog entry. Bounded
/// at the parse boundary; stored plaintext (it is not a secret) in
/// `mcp_catalog.oauth_client_id`.
#[derive(Clone, PartialEq, Eq)]
pub struct OAuthClientId(Arc<str>);

impl OAuthClientId {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<&str> for OAuthClientId {
    type Error = ParseError;
    fn try_from(raw: &str) -> Result<Self, Self::Error> {
        if raw.is_empty() {
            return Err(ParseError::Empty {
                field: "mcp_catalog.oauth_client_id",
            });
        }
        if raw.len() > MCP_OAUTH_CLIENT_ID_MAX_LEN {
            return Err(ParseError::TooLong {
                field: "mcp_catalog.oauth_client_id",
                max: MCP_OAUTH_CLIENT_ID_MAX_LEN,
                got: raw.len(),
            });
        }
        Ok(Self(Arc::from(raw)))
    }
}

impl TryFrom<String> for OAuthClientId {
    type Error = ParseError;
    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::try_from(raw.as_str())
    }
}

impl fmt::Debug for OAuthClientId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("OAuthClientId").field(&&*self.0).finish()
    }
}

/// A user-supplied OAuth `client_secret`.
///
/// Redacts in `Debug`/`Display` (§2 — it travels through tracing spans) and
/// exposes its bytes only via [`OAuthClientSecret::expose`] at the precise
/// call site that seals it or hands it to rmcp's `OAuthClientConfig`.
#[derive(Clone)]
pub struct OAuthClientSecret(String);

impl OAuthClientSecret {
    /// Borrow the secret bytes. Avoid copying through `to_string` — that
    /// defeats the type.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for OAuthClientSecret {
    type Error = ParseError;
    fn try_from(raw: String) -> Result<Self, Self::Error> {
        if raw.is_empty() {
            return Err(ParseError::Empty {
                field: "mcp_catalog.oauth_client_secret",
            });
        }
        if raw.len() > MCP_OAUTH_CLIENT_SECRET_MAX_LEN {
            return Err(ParseError::TooLong {
                field: "mcp_catalog.oauth_client_secret",
                max: MCP_OAUTH_CLIENT_SECRET_MAX_LEN,
                got: raw.len(),
            });
        }
        Ok(Self(raw))
    }
}

impl fmt::Debug for OAuthClientSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("OAuthClientSecret(***)")
    }
}

impl fmt::Display for OAuthClientSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

/// A user-supplied OAuth client for a custom catalog entry.
///
/// The public `client_id`, plus an optional `client_secret` (absent →
/// public/PKCE client). Read out of `mcp_catalog` only in the OAuth
/// start/callback paths via [`McpCatalogStore::oauth_client`]; never
/// surfaced on the `Serialize`-derived [`McpCatalogEntry`].
#[derive(Debug, Clone)]
pub struct UserOAuthClient {
    pub client_id: OAuthClientId,
    pub client_secret: Option<OAuthClientSecret>,
}

/// Env-var names that source a `ClientSource::Platform` catalog entry's
/// OAuth client. The convention is `PATOM_<UPPER>_CLIENT_ID/SECRET` where
/// `<UPPER>` is the catalog id upper-cased with `-` → `_`.
///
/// The transform is total: [`McpCatalogId`] is restricted to
/// `[a-z][a-z0-9_-]{0,39}` so `to_ascii_uppercase` + `replace('-', "_")`
/// always yields a valid env-var name.
#[must_use]
pub fn platform_env_keys(catalog_id: &McpCatalogId) -> (String, String) {
    let upper = catalog_id.as_str().to_ascii_uppercase().replace('-', "_");
    (
        format!("PATOM_{upper}_CLIENT_ID"),
        format!("PATOM_{upper}_CLIENT_SECRET"),
    )
}

/// Env-var middle used to look up a platform OAuth client in
/// [`crate::config::AuthSettings::platform_oauth_clients`].
///
/// Same transform as [`platform_env_keys`] but returns the lowercased
/// middle (matching the `HashMap` key the env parser emits).
#[must_use]
pub fn platform_env_middle(catalog_id: &McpCatalogId) -> String {
    catalog_id.as_str().replace('-', "_")
}

/// Display name for a catalog entry. Bounded so list views stay readable.
#[derive(Clone, PartialEq, Eq)]
pub struct McpCatalogDisplayName(Arc<str>);

impl McpCatalogDisplayName {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<&str> for McpCatalogDisplayName {
    type Error = ParseError;
    fn try_from(raw: &str) -> Result<Self, Self::Error> {
        if raw.is_empty() {
            return Err(ParseError::Empty {
                field: "mcp_catalog.display_name",
            });
        }
        if raw.len() > MCP_CATALOG_DISPLAY_NAME_MAX_LEN {
            return Err(ParseError::TooLong {
                field: "mcp_catalog.display_name",
                max: MCP_CATALOG_DISPLAY_NAME_MAX_LEN,
                got: raw.len(),
            });
        }
        Ok(Self(Arc::from(raw)))
    }
}

impl TryFrom<String> for McpCatalogDisplayName {
    type Error = ParseError;
    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::try_from(raw.as_str())
    }
}

impl fmt::Debug for McpCatalogDisplayName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("McpCatalogDisplayName")
            .field(&&*self.0)
            .finish()
    }
}

impl fmt::Display for McpCatalogDisplayName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for McpCatalogDisplayName {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(&self.0)
    }
}

/// Recruiter-facing summary of the integration: what it's for, what
/// general capability it brings. Distinct from `McpDescription` (which is
/// the *server*'s operator-facing notes).
#[derive(Clone, PartialEq, Eq)]
pub struct McpCatalogDescription(Arc<str>);

impl McpCatalogDescription {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<&str> for McpCatalogDescription {
    type Error = ParseError;
    fn try_from(raw: &str) -> Result<Self, Self::Error> {
        if raw.is_empty() {
            return Err(ParseError::Empty {
                field: "mcp_catalog.description",
            });
        }
        if raw.len() > MCP_CATALOG_DESCRIPTION_MAX_LEN {
            return Err(ParseError::TooLong {
                field: "mcp_catalog.description",
                max: MCP_CATALOG_DESCRIPTION_MAX_LEN,
                got: raw.len(),
            });
        }
        Ok(Self(Arc::from(raw)))
    }
}

impl TryFrom<String> for McpCatalogDescription {
    type Error = ParseError;
    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::try_from(raw.as_str())
    }
}

impl fmt::Debug for McpCatalogDescription {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("McpCatalogDescription")
            .field(&&*self.0)
            .finish()
    }
}

impl Serialize for McpCatalogDescription {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(&self.0)
    }
}

/// One `{key, value}` pair in [`OAuthAuthorizeExtras`]. Wire shape
/// mirrors the JSONB stored in `mcp_catalog.authorize_extra_params`.
///
/// `Deserialize` is the inbound seam from the DB row decode only —
/// the validated `OAuthAuthorizeExtras::try_from` wraps the parsed
/// `Vec<Self>` and is the only constructor exposed to the rest of the
/// codebase. No HTTP route deserialises this type today.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthAuthorizeExtra {
    pub key: String,
    pub value: String,
}

/// Validated catalog-row `authorize_extra_params`.
///
/// Bounded on every axis (item count + per-item key/value bytes) so a
/// poisoned DB row can't make the authorize URL unboundedly large.
/// Order-preserving: some ASes care about param order in the redirect
/// (Microsoft's `prompt` list is one).
///
/// Empty list and absent column are distinguished at the storage layer
/// (`Option<OAuthAuthorizeExtras>` field on the entry); this type only
/// represents a non-empty, validated list.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct OAuthAuthorizeExtras(Vec<OAuthAuthorizeExtra>);

impl OAuthAuthorizeExtras {
    #[must_use]
    pub fn as_slice(&self) -> &[OAuthAuthorizeExtra] {
        &self.0
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl TryFrom<Vec<OAuthAuthorizeExtra>> for OAuthAuthorizeExtras {
    type Error = ParseError;
    fn try_from(raw: Vec<OAuthAuthorizeExtra>) -> Result<Self, Self::Error> {
        if raw.len() > MCP_CATALOG_AUTHORIZE_EXTRA_PARAMS_MAX {
            return Err(ParseError::TooLong {
                field: "mcp_catalog.authorize_extra_params",
                max: MCP_CATALOG_AUTHORIZE_EXTRA_PARAMS_MAX,
                got: raw.len(),
            });
        }
        for item in &raw {
            if item.key.is_empty() {
                return Err(ParseError::Empty {
                    field: "mcp_catalog.authorize_extra_params.key",
                });
            }
            if item.key.len() > MCP_CATALOG_AUTHORIZE_EXTRA_PARAM_BYTES_MAX {
                return Err(ParseError::TooLong {
                    field: "mcp_catalog.authorize_extra_params.key",
                    max: MCP_CATALOG_AUTHORIZE_EXTRA_PARAM_BYTES_MAX,
                    got: item.key.len(),
                });
            }
            if item.value.len() > MCP_CATALOG_AUTHORIZE_EXTRA_PARAM_BYTES_MAX {
                return Err(ParseError::TooLong {
                    field: "mcp_catalog.authorize_extra_params.value",
                    max: MCP_CATALOG_AUTHORIZE_EXTRA_PARAM_BYTES_MAX,
                    got: item.value.len(),
                });
            }
        }
        Ok(Self(raw))
    }
}

/// A row in `mcp_catalog`. Wire-shape via `Serialize` is what the
/// `GET /mcp-catalog` endpoint and the `search_tools` system tool both
/// emit.
#[derive(Debug, Clone, Serialize)]
pub struct McpCatalogEntry {
    pub id: McpCatalogId,
    /// `None` = built-in (global, visible to every tenant); `Some` =
    /// tenant-custom.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub org_id: Option<OrgId>,
    pub display_name: McpCatalogDisplayName,
    pub description: McpCatalogDescription,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub homepage_url: Option<String>,
    /// Public URL of the tile icon (R2-hosted). Built-ins seed via
    /// migration 33; org-scoped entries upload via the asset module.
    /// `None` falls back to the FE's `Monogram` rendering.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon_url: Option<String>,
    pub default_transport: McpTransport,
    pub auth_kind: McpAuthKind,
    /// Space-separated OAuth scope list applied when the user clicks
    /// "Connect" without an explicit override. `None` for DCR vendors
    /// whose AS supplies its own default scope set at registration time
    /// (Notion, Linear, Slack, Jira); `Some` for vendors that require
    /// the client to declare scopes on every authorize request (Google
    /// — `Missing required parameter: scope` otherwise). RFC 6749 §3.3
    /// wire shape, forwarded verbatim into the authorize URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_scope: Option<String>,
    /// Non-standard query params appended verbatim to the OAuth
    /// authorize URL when the user clicks "Connect". `None` (and the
    /// empty list) suppress the param entirely; both serialize-skip so
    /// the API surface stays clean for the common case. See the column
    /// comment in migration 39 for the vendor rationale (Google's
    /// `access_type=offline` + `prompt=consent`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authorize_extra_params: Option<OAuthAuthorizeExtras>,
    /// Where the OAuth client_id/secret for this entry come from. Defaults
    /// to `Dcr` at the DB level — built-in entries that diverge (Google,
    /// GitHub, Gmail, Calendar) are set by migration 50.
    pub client_source: ClientSource,
    /// Optional alias to another platform catalog id whose env-var
    /// credentials this entry shares. `gmail` and `gcal` both point at
    /// `google` so one Google OAuth app covers both products. `None` when
    /// the entry uses its own credentials (or isn't `client_source =
    /// Platform`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform_client_alias: Option<McpCatalogId>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Storage trait for the catalog. Two reads cover today's needs:
///   * `list_for_org` — recruiter discovery, frontend connections page.
///   * `get_for_org`  — resolution from a catalog_id assignment to the
///     concrete entry (and its default transport) when the UI clicks
///     "wire this".
///
/// Writes (tenant-custom CRUD) are deliberately absent for v1 — built-ins
/// ship via migration, custom support arrives when there's a real ask.
#[async_trait]
pub trait McpCatalogStore: fmt::Debug + Send + Sync {
    /// Every global row plus every row owned by `org_id`. Ordered by
    /// `id` for stable iteration in `search_tools` output.
    async fn list_for_org(&self, org_id: OrgId) -> Result<Vec<McpCatalogEntry>, McpError>;

    /// Resolve `(org_id, id)`. Prefers a tenant-custom row over a global
    /// row of the same id (a tenant id legitimately *shadows* a built-in,
    /// e.g. for a self-hosted Notion mirror). `Ok(None)` for an unknown id.
    async fn get_for_org(
        &self,
        org_id: OrgId,
        id: &McpCatalogId,
    ) -> Result<Option<McpCatalogEntry>, McpError>;

    /// Set the `icon_url` on an org-scoped catalog row. Refuses to touch
    /// global (built-in) rows — those are migration-only. Returns
    /// [`McpError::CatalogIdUnknown`] if no row matches `(org_id, id)`.
    ///
    /// The caller (HTTP upload handler) is responsible for length
    /// validation; this call relies on the schema CHECK as a last-line
    /// guard.
    async fn set_icon_url(
        &self,
        org_id: OrgId,
        id: &McpCatalogId,
        icon_url: &str,
        now: DateTime<Utc>,
    ) -> Result<(), McpError>;

    /// Ensure a tenant-custom catalog row exists for `(org_id, id)`.
    /// Drives the "Custom URL" connection flow: an operator supplies
    /// the integration's transport + auth shape directly, without an
    /// admin pre-seeding a row by migration.
    ///
    /// Insert-or-fetch semantics: when the row is absent, inserts it
    /// with the provided metadata; when present, leaves the row
    /// unchanged. Returns the `auth_kind` actually in the DB (the
    /// caller uses it to derive `connection_status`). Re-posts cannot
    /// mutate the catalog row — that would silently leak state when
    /// the parallel server-row insert returns 409, since the catalog
    /// upsert lives in its own transaction.
    ///
    /// Rejects with [`McpError::CatalogIdShadowsGlobal`] if a built-in
    /// row already owns `id` — we refuse to silently let a tenant
    /// shadow a global. (The DB schema permits it, but the product
    /// rule does not.)
    ///
    /// `homepage_url` and `icon_url` are deliberately not part of this
    /// surface: the former has no operator-facing place to set it
    /// today; the latter goes through the dedicated icon upload route.
    async fn ensure_org_scoped(&self, payload: CatalogUpsert<'_>) -> Result<McpAuthKind, McpError>;

    /// Read back the user-supplied OAuth client (`client_id` + optional
    /// decrypted `client_secret`) for `(org_id, id)`. Returns `Ok(None)`
    /// when the resolved row carries no `oauth_client_id` (i.e. it is not a
    /// `UserSupplied` connector).
    ///
    /// Called only from the OAuth start/callback paths — the secret is
    /// **never** surfaced on [`McpCatalogEntry`] (which flows to HTTP
    /// responses); this dedicated reader is the only decrypt seam.
    async fn oauth_client(
        &self,
        org_id: OrgId,
        id: &McpCatalogId,
    ) -> Result<Option<UserOAuthClient>, McpError>;
}

/// Borrowed input for [`McpCatalogStore::ensure_org_scoped`]. Bundles
/// the request so the trait stays under clippy's argument-count cap
/// and so call sites read top-down at the seams.
#[derive(Debug)]
pub struct CatalogUpsert<'a> {
    pub org_id: OrgId,
    pub id: &'a McpCatalogId,
    pub display_name: &'a McpCatalogDisplayName,
    pub description: &'a McpCatalogDescription,
    pub default_transport: &'a McpTransport,
    pub auth_kind: McpAuthKind,
    /// Where this entry's OAuth client comes from. Non-OAuth entries pass
    /// [`ClientSource::Dcr`] to preserve the historical DB default (the
    /// resolver never reads it for `static_headers`/`none` auth kinds); a
    /// BYO-OAuth custom entry passes [`ClientSource::UserSupplied`].
    pub client_source: ClientSource,
    /// The user-supplied OAuth client to seal onto the row. `Some` only on
    /// the `UserSupplied` path; the secret (if any) is sealed with the org
    /// KEK before it touches the DB.
    pub oauth_client: Option<UserOAuthClient>,
    pub now: DateTime<Utc>,
}

pub type SharedMcpCatalogStore = Arc<dyn McpCatalogStore>;

/// Postgres-backed implementation. RLS already filters reads to the
/// caller's visible rows (global + their org); the queries themselves do
/// not need to add `WHERE org_id …` clauses.
pub struct PgMcpCatalogStore {
    pool: PgPool,
    /// Seals/opens the user-supplied OAuth client secret stored on
    /// org-scoped rows. Shared with the credential store so both encrypt
    /// under the same per-org KEK.
    enc: SharedOrgEncryptor,
}

impl PgMcpCatalogStore {
    #[must_use]
    pub fn new(pool: PgPool, enc: SharedOrgEncryptor) -> Self {
        Self { pool, enc }
    }
}

impl fmt::Debug for PgMcpCatalogStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgMcpCatalogStore").finish_non_exhaustive()
    }
}

/// `sqlx` row reader. Kept private; the trait surface returns the
/// validated [`McpCatalogEntry`] only.
#[derive(sqlx::FromRow)]
struct CatalogRow {
    id: String,
    org_id: Option<sqlx::types::Uuid>,
    display_name: String,
    description: String,
    homepage_url: Option<String>,
    icon_url: Option<String>,
    default_transport: serde_json::Value,
    auth_kind: String,
    default_scope: Option<String>,
    authorize_extra_params: Option<serde_json::Value>,
    client_source: ClientSource,
    platform_client_alias: Option<String>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl TryFrom<CatalogRow> for McpCatalogEntry {
    type Error = McpError;
    fn try_from(row: CatalogRow) -> Result<Self, Self::Error> {
        let id = McpCatalogId::try_from(row.id)?;
        let display_name = McpCatalogDisplayName::try_from(row.display_name)?;
        let description = McpCatalogDescription::try_from(row.description)?;
        let default_transport = serde_json::from_value::<McpTransport>(row.default_transport)
            .map_err(|e| McpError::Backend(format!("decode default_transport: {e}")))?;
        let auth_kind = McpAuthKind::parse(row.auth_kind.as_str()).ok_or_else(|| {
            McpError::Backend(format!("unknown auth_kind: {raw}", raw = row.auth_kind))
        })?;
        let org_id = row.org_id.map(OrgId::from);
        let authorize_extra_params = decode_authorize_extra_params(row.authorize_extra_params)?;
        let platform_client_alias = row
            .platform_client_alias
            .map(McpCatalogId::try_from)
            .transpose()?;
        Ok(Self {
            id,
            org_id,
            display_name,
            description,
            homepage_url: row.homepage_url,
            icon_url: row.icon_url,
            default_transport,
            auth_kind,
            default_scope: row.default_scope,
            authorize_extra_params,
            client_source: row.client_source,
            platform_client_alias,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }
}

/// Decode the JSONB column and validate via [`OAuthAuthorizeExtras`].
/// An absent or empty list maps to `None` so callers can branch with
/// `Option::as_ref()` without a separate emptiness check.
fn decode_authorize_extra_params(
    raw: Option<serde_json::Value>,
) -> Result<Option<OAuthAuthorizeExtras>, McpError> {
    let Some(value) = raw else { return Ok(None) };
    let parsed: Vec<OAuthAuthorizeExtra> = serde_json::from_value(value)
        .map_err(|e| McpError::Backend(format!("decode authorize_extra_params: {e}")))?;
    if parsed.is_empty() {
        return Ok(None);
    }
    let extras = OAuthAuthorizeExtras::try_from(parsed)
        .map_err(|e| McpError::Backend(format!("authorize_extra_params: {e}")))?;
    Ok(Some(extras))
}

/// `sqlx` reader for the four user-supplied-OAuth columns. Kept private
/// and separate from [`CatalogRow`] so the sealed secret can never ride the
/// `Serialize`-derived [`McpCatalogEntry`].
// Field names mirror the `mcp_catalog` column names exactly so
// `sqlx::FromRow` maps them; the shared `oauth_client` prefix is the
// schema's, not a smell.
#[allow(clippy::struct_field_names)]
#[derive(sqlx::FromRow)]
struct OAuthClientRow {
    oauth_client_id: Option<String>,
    oauth_client_secret_ciphertext: Option<Vec<u8>>,
    oauth_client_secret_nonce: Option<Vec<u8>>,
    oauth_client_secret_key_version: Option<i16>,
}

impl OAuthClientRow {
    /// Decode + decrypt into a [`UserOAuthClient`]. `None` when the row has
    /// no `oauth_client_id`. The secret triple is all-or-none (migration
    /// 67's CHECK); a partial triple is a corrupted row and crashes the
    /// request with a backend error rather than guessing.
    fn into_client(
        self,
        enc: &SharedOrgEncryptor,
        org_id: OrgId,
    ) -> Result<Option<UserOAuthClient>, McpError> {
        let Some(client_id_raw) = self.oauth_client_id else {
            return Ok(None);
        };
        let client_id = OAuthClientId::try_from(client_id_raw)?;
        let client_secret = match (
            self.oauth_client_secret_ciphertext,
            self.oauth_client_secret_nonce,
            self.oauth_client_secret_key_version,
        ) {
            (Some(ciphertext), Some(nonce_bytes), Some(key_version)) => {
                let nonce: [u8; crate::crypto::NONCE_BYTES] =
                    nonce_bytes.as_slice().try_into().map_err(|_| {
                        McpError::Backend("oauth secret nonce: wrong byte length".into())
                    })?;
                let blob = EncryptedBlob {
                    key_version,
                    nonce,
                    ciphertext,
                };
                let plaintext = enc.open(org_id, &blob)?;
                let raw = String::from_utf8(plaintext.as_slice().to_vec())
                    .map_err(|e| McpError::Backend(format!("oauth secret not utf8: {e}")))?;
                Some(OAuthClientSecret::try_from(raw)?)
            }
            (None, None, None) => None,
            _ => {
                return Err(McpError::Backend(
                    "oauth_client secret triple partially populated".into(),
                ));
            }
        };
        Ok(Some(UserOAuthClient {
            client_id,
            client_secret,
        }))
    }
}

#[async_trait]
impl McpCatalogStore for PgMcpCatalogStore {
    async fn list_for_org(&self, org_id: OrgId) -> Result<Vec<McpCatalogEntry>, McpError> {
        // RLS already restricts to (org_id IS NULL OR member); the explicit
        // `WHERE` is belt-and-braces against the rare case the caller
        // forgets to wrap in `begin_as`. Order by id for stable iteration.
        let rows: Vec<CatalogRow> = sqlx::query_as::<_, CatalogRow>(
            "SELECT id, org_id, display_name, description, homepage_url, icon_url, \
                    default_transport, auth_kind, default_scope, authorize_extra_params, \
                    client_source, platform_client_alias, created_at, updated_at \
               FROM mcp_catalog \
              WHERE org_id IS NULL OR org_id = $1 \
              ORDER BY id ASC",
        )
        .bind(org_id.as_uuid())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(McpCatalogEntry::try_from).collect()
    }

    async fn set_icon_url(
        &self,
        org_id: OrgId,
        id: &McpCatalogId,
        icon_url: &str,
        now: DateTime<Utc>,
    ) -> Result<(), McpError> {
        // Org-scoped only — global rows have org_id IS NULL and the
        // UPDATE policy rejects them anyway, but we lead with an
        // explicit predicate so the error is clear instead of a 0-row
        // surprise.
        let updated: u64 = sqlx::query(
            "UPDATE mcp_catalog \
                SET icon_url = $3, updated_at = $4 \
              WHERE id = $1 AND org_id = $2",
        )
        .bind(id.as_str())
        .bind(org_id.as_uuid())
        .bind(icon_url)
        .bind(now)
        .execute(&self.pool)
        .await?
        .rows_affected();
        if updated == 0 {
            return Err(McpError::CatalogIdUnknown(id.clone()));
        }
        Ok(())
    }

    async fn ensure_org_scoped(&self, payload: CatalogUpsert<'_>) -> Result<McpAuthKind, McpError> {
        // Run privileged so the shadow check sees global rows and so
        // the INSERT can target an org other than the caller's tx
        // session role (the HTTP layer already authorised `org_id`
        // via `Principal::active_org_id`). The shadow check + insert
        // must share a tx — otherwise a concurrent migration could
        // insert a global between the two statements.
        //
        // Semantics: insert-if-absent, then fetch. `DO NOTHING` is
        // critical — a `DO UPDATE` would silently mutate the existing
        // row when the caller's parallel `mcp_servers` insert is about
        // to fail with `CatalogIdTaken`, leaking display_name /
        // description / default_transport / auth_kind changes on a
        // request that returns 409.
        let CatalogUpsert {
            org_id,
            id,
            display_name,
            description,
            default_transport,
            auth_kind,
            client_source,
            oauth_client,
            now,
        } = payload;
        let transport_json = serde_json::to_value(default_transport)
            .map_err(|e| McpError::Backend(format!("serialize transport: {e}")))?;
        // Seal the secret (if any) before opening the tx — the AEAD op needs
        // no DB and an oversized plaintext was already rejected at the parse
        // boundary (§5). `oauth_client_id` rides plaintext; the secret triple
        // is all-or-none, matching migration 67's CHECK.
        let oauth_client_id: Option<String> = oauth_client
            .as_ref()
            .map(|c| c.client_id.as_str().to_owned());
        let sealed: Option<EncryptedBlob> =
            match oauth_client.as_ref().and_then(|c| c.client_secret.as_ref()) {
                Some(secret) => Some(self.enc.seal(org_id, secret.expose().as_bytes())?),
                None => None,
            };
        let id_for_err = id.clone();
        crate::auth::run_privileged::<McpAuthKind, McpError>(&self.pool, async |tx| {
            let global_exists: Option<(String,)> =
                sqlx::query_as("SELECT id FROM mcp_catalog WHERE id = $1 AND org_id IS NULL")
                    .bind(id_for_err.as_str())
                    .fetch_optional(&mut **tx)
                    .await?;
            if global_exists.is_some() {
                return Err(McpError::CatalogIdShadowsGlobal(id_for_err.clone()));
            }
            sqlx::query(
                "INSERT INTO mcp_catalog \
                    (id, org_id, display_name, description, default_transport, \
                     auth_kind, client_source, oauth_client_id, \
                     oauth_client_secret_ciphertext, oauth_client_secret_nonce, \
                     oauth_client_secret_key_version, created_at, updated_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $12) \
                 ON CONFLICT (org_id, id) WHERE org_id IS NOT NULL DO NOTHING",
            )
            .bind(id_for_err.as_str())
            .bind(org_id.as_uuid())
            .bind(display_name.as_str())
            .bind(description.as_str())
            .bind(&transport_json)
            .bind(auth_kind.as_str())
            .bind(client_source.as_str())
            .bind(oauth_client_id.as_deref())
            .bind(sealed.as_ref().map(|b| b.ciphertext.as_slice()))
            .bind(sealed.as_ref().map(|b| &b.nonce[..]))
            .bind(sealed.as_ref().map(|b| b.key_version))
            .bind(now)
            .execute(&mut **tx)
            .await?;
            // Read back the row's stored auth_kind. On insert this
            // matches the request; on conflict it reflects whatever the
            // original creator set, which is what `connection_status`
            // should be derived from.
            let row: (McpAuthKind,) =
                sqlx::query_as("SELECT auth_kind FROM mcp_catalog WHERE org_id = $1 AND id = $2")
                    .bind(org_id.as_uuid())
                    .bind(id_for_err.as_str())
                    .fetch_one(&mut **tx)
                    .await?;
            Ok(row.0)
        })
        .await
    }

    async fn get_for_org(
        &self,
        org_id: OrgId,
        id: &McpCatalogId,
    ) -> Result<Option<McpCatalogEntry>, McpError> {
        // Resolution rule: tenant-custom shadows global. `ORDER BY
        // org_id IS NULL` sorts FALSE (org-scoped) before TRUE (global);
        // LIMIT 1 picks the preferred row.
        let row: Option<CatalogRow> = sqlx::query_as::<_, CatalogRow>(
            "SELECT id, org_id, display_name, description, homepage_url, icon_url, \
                    default_transport, auth_kind, default_scope, authorize_extra_params, \
                    client_source, platform_client_alias, created_at, updated_at \
               FROM mcp_catalog \
              WHERE id = $1 \
                AND (org_id IS NULL OR org_id = $2) \
              ORDER BY (org_id IS NULL) ASC \
              LIMIT 1",
        )
        .bind(id.as_str())
        .bind(org_id.as_uuid())
        .fetch_optional(&self.pool)
        .await?;
        row.map(McpCatalogEntry::try_from).transpose()
    }

    async fn oauth_client(
        &self,
        org_id: OrgId,
        id: &McpCatalogId,
    ) -> Result<Option<UserOAuthClient>, McpError> {
        // Resolve the same row `get_for_org` would (tenant shadows global),
        // then decrypt the sealed secret. Only org-scoped `UserSupplied`
        // rows carry an `oauth_client_id`; global rows yield `None`.
        let row: Option<OAuthClientRow> = sqlx::query_as::<_, OAuthClientRow>(
            "SELECT oauth_client_id, oauth_client_secret_ciphertext, \
                    oauth_client_secret_nonce, oauth_client_secret_key_version \
               FROM mcp_catalog \
              WHERE id = $1 \
                AND (org_id IS NULL OR org_id = $2) \
              ORDER BY (org_id IS NULL) ASC \
              LIMIT 1",
        )
        .bind(id.as_str())
        .bind(org_id.as_uuid())
        .fetch_optional(&self.pool)
        .await?;
        row.map_or_else(|| Ok(None), |row| row.into_client(&self.enc, org_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::limits::MCP_CATALOG_AUTHORIZE_EXTRA_PARAMS_DB_BYTES_MAX;

    /// Worst-case in-memory value of [`OAuthAuthorizeExtras`] must
    /// serialize to under migration 39's 2048-byte JSONB CHECK. Guards
    /// the constants in `limits.rs` against drifting past the DB cap —
    /// a runtime CHECK violation would be a much worse failure mode
    /// than a parse-side rejection at the type boundary.
    #[test]
    fn authorize_extras_worst_case_serializes_under_db_cap() {
        let key = "k".repeat(MCP_CATALOG_AUTHORIZE_EXTRA_PARAM_BYTES_MAX);
        let value = "v".repeat(MCP_CATALOG_AUTHORIZE_EXTRA_PARAM_BYTES_MAX);
        let items: Vec<OAuthAuthorizeExtra> = (0..MCP_CATALOG_AUTHORIZE_EXTRA_PARAMS_MAX)
            .map(|_| OAuthAuthorizeExtra {
                key: key.clone(),
                value: value.clone(),
            })
            .collect();
        let extras = OAuthAuthorizeExtras::try_from(items)
            .expect("invariant: worst-case bounds are accepted by the validator");
        let json = serde_json::to_string(&extras)
            .expect("invariant: OAuthAuthorizeExtras serializes to JSON");
        assert!(
            json.len() <= MCP_CATALOG_AUTHORIZE_EXTRA_PARAMS_DB_BYTES_MAX,
            "worst-case JSON is {} bytes; DB CHECK allows {}",
            json.len(),
            MCP_CATALOG_AUTHORIZE_EXTRA_PARAMS_DB_BYTES_MAX,
        );
    }

    /// The secret must never leak through `Debug`/`Display` — it is
    /// carried on `StartCtx` and logged-adjacent in the OAuth spans (§2).
    #[test]
    fn oauth_client_secret_redacts() {
        let s = OAuthClientSecret::try_from("super-secret-value".to_string())
            .expect("non-empty under cap");
        assert_eq!(format!("{s:?}"), "OAuthClientSecret(***)");
        assert_eq!(format!("{s}"), "***");
        assert_eq!(s.expose(), "super-secret-value");
    }

    /// A `UserOAuthClient`'s `Debug` (derived) must inherit the secret's
    /// redaction while still showing the non-secret `client_id`.
    #[test]
    fn user_oauth_client_debug_redacts_secret_keeps_id() {
        let client = UserOAuthClient {
            client_id: OAuthClientId::try_from("client-abc").expect("valid id"),
            client_secret: Some(
                OAuthClientSecret::try_from("hunter2".to_string()).expect("non-empty"),
            ),
        };
        let rendered = format!("{client:?}");
        assert!(
            rendered.contains("client-abc"),
            "client_id should show: {rendered}"
        );
        assert!(
            !rendered.contains("hunter2"),
            "secret must not leak: {rendered}"
        );
    }

    #[test]
    fn oauth_client_id_rejects_empty_and_oversized() {
        assert!(OAuthClientId::try_from("").is_err());
        let oversized = "a".repeat(MCP_OAUTH_CLIENT_ID_MAX_LEN + 1);
        assert!(OAuthClientId::try_from(oversized.as_str()).is_err());
        assert!(OAuthClientId::try_from("a").is_ok());
    }

    #[test]
    fn oauth_client_secret_rejects_empty_and_oversized() {
        assert!(OAuthClientSecret::try_from(String::new()).is_err());
        let oversized = "a".repeat(MCP_OAUTH_CLIENT_SECRET_MAX_LEN + 1);
        assert!(OAuthClientSecret::try_from(oversized).is_err());
    }
}
