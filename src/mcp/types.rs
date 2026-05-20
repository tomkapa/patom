//! Domain types for the MCP subsystem.
//!
//! CLAUDE.md §1: every value carrying an invariant gets a newtype with a `TryFrom`
//! smart constructor. The HTTP boundary parses raw JSON into these types once; nothing
//! downstream constructs them directly.

use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use url::Url;

use crate::types::ParseError;

use super::limits::{
    MCP_ALIAS_MAX_LEN, MCP_CATALOG_ID_MAX_LEN, MCP_DESCRIPTION_MAX_LEN, MCP_HEADER_NAME_MAX_LEN,
    MCP_HEADER_VALUE_MAX_LEN, MCP_TOOL_REMOTE_NAME_MAX_LEN, MCP_URL_MAX_LEN,
};

crate::uuid_newtype! {
    /// Opaque identifier for a registered MCP server row.
    pub McpServerId
}

/// Stable, human-readable id of an entry in `mcp_catalog` (e.g. "notion").
///
/// Drives the tool prefix on every tool a wired server exposes
/// (`mcp_<catalog_id>_<remote_name>`) and is the key the recruiter agent
/// uses in `allowed_mcp_tools` when hiring a new agent.
///
/// Regex (mirrors the `mcp_catalog.id` CHECK): `^[a-z][a-z0-9_-]{0,39}$`.
/// Bounded so the prefixed tool name always fits within `ToolName`'s
/// 64-byte cap (4 + 40 + 1 + 19 = 64 bytes worst case before the cap
/// kicks in on the remote name).
///
/// Reference-counted (`Arc<str>`) so the runtime can clone the same key
/// through many layers (catalog store → resolver → scoped source) without
/// per-clone allocations.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct McpCatalogId(Arc<str>);

impl McpCatalogId {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<&str> for McpCatalogId {
    type Error = ParseError;

    fn try_from(raw: &str) -> Result<Self, Self::Error> {
        if raw.is_empty() {
            return Err(ParseError::Empty {
                field: "mcp_catalog_id",
            });
        }
        if raw.len() > MCP_CATALOG_ID_MAX_LEN {
            return Err(ParseError::TooLong {
                field: "mcp_catalog_id",
                max: MCP_CATALOG_ID_MAX_LEN,
                got: raw.len(),
            });
        }
        let mut chars = raw.chars();
        let first = chars.next().expect("invariant: non-empty by check above");
        if !first.is_ascii_lowercase() {
            return Err(ParseError::Malformed {
                field: "mcp_catalog_id",
                detail: "must start with a-z",
            });
        }
        let rest_ok =
            chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
        if !rest_ok {
            return Err(ParseError::Malformed {
                field: "mcp_catalog_id",
                detail: "allowed: a-z 0-9 _ -",
            });
        }
        Ok(Self(Arc::from(raw)))
    }
}

impl TryFrom<String> for McpCatalogId {
    type Error = ParseError;
    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::try_from(raw.as_str())
    }
}

impl fmt::Debug for McpCatalogId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("McpCatalogId").field(&&*self.0).finish()
    }
}

impl fmt::Display for McpCatalogId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for McpCatalogId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for McpCatalogId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::try_from(raw).map_err(serde::de::Error::custom)
    }
}

/// Operator-chosen short name. Drives the prefix on every tool the server exposes
/// (`mcp_<alias>_<remote_name>`); the `ToolName` cap leaves us 16 chars to play with.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct McpServerAlias(String);

impl McpServerAlias {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<&str> for McpServerAlias {
    type Error = ParseError;

    fn try_from(raw: &str) -> Result<Self, Self::Error> {
        if raw.is_empty() {
            return Err(ParseError::Empty {
                field: "mcp_server_alias",
            });
        }
        if raw.len() > MCP_ALIAS_MAX_LEN {
            return Err(ParseError::TooLong {
                field: "mcp_server_alias",
                max: MCP_ALIAS_MAX_LEN,
                got: raw.len(),
            });
        }
        // The alias is concatenated into a `ToolName`, which only allows
        // `[a-zA-Z0-9_-]`; we further restrict to lowercase + digits + `_`/`-` so the
        // wire format is predictable.
        let valid = raw
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
        if !valid {
            return Err(ParseError::Malformed {
                field: "mcp_server_alias",
                detail: "allowed: a-z 0-9 _ -",
            });
        }
        Ok(Self(raw.to_owned()))
    }
}

impl TryFrom<String> for McpServerAlias {
    type Error = ParseError;
    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::try_from(raw.as_str())
    }
}

impl fmt::Debug for McpServerAlias {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("McpServerAlias").field(&self.0).finish()
    }
}

impl fmt::Display for McpServerAlias {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for McpServerAlias {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for McpServerAlias {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::try_from(raw).map_err(serde::de::Error::custom)
    }
}

/// Operator-facing description. Plain free text, length-bounded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpDescription(String);

impl McpDescription {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for McpDescription {
    type Error = ParseError;
    fn try_from(raw: String) -> Result<Self, Self::Error> {
        if raw.len() > MCP_DESCRIPTION_MAX_LEN {
            return Err(ParseError::TooLong {
                field: "mcp_description",
                max: MCP_DESCRIPTION_MAX_LEN,
                got: raw.len(),
            });
        }
        Ok(Self(raw))
    }
}

/// Validated MCP transport configuration. Persisted as JSONB; round-trips through the
/// same `TryFrom<McpTransportInput>` so the storage and HTTP boundaries enforce the
/// same invariants.
///
/// **Non-sensitive only.** Sensitive material (bearer tokens, secret-bearing
/// headers) lives in [`crate::mcp::CredentialPayload`] under
/// `mcp_server_credentials` — never in this struct. The absence of a `headers`
/// field here is the enforcement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "McpTransportInput", into = "McpTransportInput")]
pub enum McpTransport {
    Http { url: McpHttpUrl },
}

impl McpTransport {
    /// URL we can drive an HTTP-flavored MCP discovery or connect against.
    /// Today every variant is HTTP; when a non-HTTP transport lands the
    /// match arm forces every caller to handle the negative case.
    #[must_use]
    pub const fn http_url(&self) -> &McpHttpUrl {
        match self {
            Self::Http { url } => url,
        }
    }
}

/// On-the-wire shape used for both deserialize-from and serialize-to. Adding a new
/// transport variant is a one-tag change here plus one match arm in `TryFrom`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum McpTransportInput {
    Http { url: String },
}

impl TryFrom<McpTransportInput> for McpTransport {
    type Error = ParseError;
    fn try_from(raw: McpTransportInput) -> Result<Self, Self::Error> {
        match raw {
            McpTransportInput::Http { url } => {
                let url = McpHttpUrl::try_from(url.as_str())?;
                Ok(Self::Http { url })
            }
        }
    }
}

impl From<McpTransport> for McpTransportInput {
    fn from(value: McpTransport) -> Self {
        match value {
            McpTransport::Http { url } => Self::Http {
                url: url.as_str().to_owned(),
            },
        }
    }
}

/// `https://...` (or `http://localhost...` for dev) URL pointing at an MCP server.
///
/// We do **not** apply the SSRF policy here — outbound MCP traffic is operator-trusted
/// (per design: SaaS operator registers servers explicitly), unlike `web_fetch`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpHttpUrl(Url);

impl McpHttpUrl {
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl TryFrom<&str> for McpHttpUrl {
    type Error = ParseError;
    fn try_from(raw: &str) -> Result<Self, Self::Error> {
        if raw.is_empty() {
            return Err(ParseError::Empty {
                field: "mcp_transport.url",
            });
        }
        if raw.len() > MCP_URL_MAX_LEN {
            return Err(ParseError::TooLong {
                field: "mcp_transport.url",
                max: MCP_URL_MAX_LEN,
                got: raw.len(),
            });
        }
        let url = Url::parse(raw).map_err(|_| ParseError::Malformed {
            field: "mcp_transport.url",
            detail: "not a valid URL",
        })?;
        match url.scheme() {
            "http" | "https" => {}
            _ => {
                return Err(ParseError::Malformed {
                    field: "mcp_transport.url",
                    detail: "scheme must be http or https",
                });
            }
        }
        if url.host().is_none() {
            return Err(ParseError::Malformed {
                field: "mcp_transport.url",
                detail: "host missing",
            });
        }
        Ok(Self(url))
    }
}

/// Custom HTTP header name.
///
/// We allow a permissive but bounded set so we don't need to mirror RFC-compliance
/// here — the upstream `http::HeaderName` parses the value when we hand it to rmcp,
/// and rejects anything illegal.
///
/// Stored lowercased: HTTP header names are case-insensitive (RFC 7230 §3.2), and
/// `http::HeaderName` lowercases on insert. Without normalising at parse time, an
/// operator who registered both `Authorization` and `authorization` would see two
/// distinct entries in the `BTreeMap` but only one would survive the trip through
/// rmcp's `HashMap<HeaderName, ...>` — the other silently disappears. Normalise here
/// so operator intent matches the wire and so `BTreeMap` keying is canonical.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct McpHeaderName(String);

impl McpHeaderName {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

impl TryFrom<String> for McpHeaderName {
    type Error = ParseError;
    fn try_from(raw: String) -> Result<Self, Self::Error> {
        if raw.is_empty() {
            return Err(ParseError::Empty {
                field: "mcp_transport.header_name",
            });
        }
        if raw.len() > MCP_HEADER_NAME_MAX_LEN {
            return Err(ParseError::TooLong {
                field: "mcp_transport.header_name",
                max: MCP_HEADER_NAME_MAX_LEN,
                got: raw.len(),
            });
        }
        // rfc7230 allows tchar; we restrict further to the printable ASCII subset
        // commonly seen in headers and avoid spaces / control chars / non-ASCII.
        let valid = raw.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(
                    c,
                    '!' | '#'
                        | '$'
                        | '%'
                        | '&'
                        | '\''
                        | '*'
                        | '+'
                        | '-'
                        | '.'
                        | '^'
                        | '_'
                        | '`'
                        | '|'
                        | '~'
                )
        });
        if !valid {
            return Err(ParseError::Malformed {
                field: "mcp_transport.header_name",
                detail: "rfc7230 token characters only",
            });
        }
        Ok(Self(raw.to_ascii_lowercase()))
    }
}

impl Serialize for McpHeaderName {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for McpHeaderName {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::try_from(raw).map_err(serde::de::Error::custom)
    }
}

/// Custom HTTP header value. Length-bounded, ASCII-printable.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct McpHeaderValue(String);

impl McpHeaderValue {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

impl TryFrom<String> for McpHeaderValue {
    type Error = ParseError;
    fn try_from(raw: String) -> Result<Self, Self::Error> {
        if raw.is_empty() {
            return Err(ParseError::Empty {
                field: "mcp_transport.header_value",
            });
        }
        if raw.len() > MCP_HEADER_VALUE_MAX_LEN {
            return Err(ParseError::TooLong {
                field: "mcp_transport.header_value",
                max: MCP_HEADER_VALUE_MAX_LEN,
                got: raw.len(),
            });
        }
        // rfc7230 field-value: visible ASCII + space/htab. Reject CR/LF (header smuggling)
        // and any non-ASCII.
        let valid = raw
            .chars()
            .all(|c| c == ' ' || c == '\t' || (c.is_ascii_graphic()));
        if !valid {
            return Err(ParseError::Malformed {
                field: "mcp_transport.header_value",
                detail: "must be ascii-printable, no control chars",
            });
        }
        Ok(Self(raw))
    }
}

impl Serialize for McpHeaderValue {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for McpHeaderValue {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::try_from(raw).map_err(serde::de::Error::custom)
    }
}

/// One row in `mcp_servers`. Returned by the store; consumed by `McpRegistry::refresh`.
///
/// `catalog_id` is the stable, human-readable id of the [`mcp_catalog`]
/// entry this server is wired against (e.g. `"notion"`). The per-tenant
/// uniqueness constraint (one wired connection per (org, catalog_id))
/// and the tool prefix (`mcp_<catalog_id>_<remote_name>`) both key off
/// this field.
#[derive(Debug, Clone)]
pub struct McpServerRecord {
    pub id: McpServerId,
    pub org_id: crate::auth::OrgId,
    pub catalog_id: McpCatalogId,
    pub enabled: bool,
    pub config: McpTransport,
    pub description: Option<McpDescription>,
    pub last_seen_at: Option<chrono::DateTime<chrono::Utc>>,
    pub last_error: Option<String>,
    pub discovered_tools: Option<Vec<DiscoveredTool>>,
    /// Audit field: which user created this row through the HTTP API.
    /// Non-optional because the column is `NOT NULL` (pre-launch: no
    /// nullable shim per `feedback_no_backcompat`).
    pub created_by_user_id: crate::auth::UserId,
    /// Connection status surfaced to the UI. A row created without
    /// credentials starts in `AuthPending` and the OAuth callback flips
    /// it to `Ok`; rows created with static-header creds start in `Ok`.
    /// The OAuth refresher flips a token-revoked row to
    /// `ReconnectRequired`; sticky connect failures land in `Error`.
    pub connection_status: ConnectionStatus,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

crate::str_enum! {
    /// Surfaced state of an MCP server's upstream connection. Each variant
    /// is wire-stable; the column `CHECK` constraint and JSON
    /// serialisation key off the same labels.
    pub enum ConnectionStatus {
        Ok                  => "ok",
        AuthPending         => "auth_pending",
        ReconnectRequired   => "reconnect_required",
        Error               => "error",
    }
}

/// Tool metadata cached on the row after a successful refresh, surfaced by the
/// list/read API so operators can see what a server is exposing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveredTool {
    pub remote_name: String,
    pub prefixed_name: String,
    pub description: Option<String>,
}

/// The unprefixed name a remote MCP server publishes for a tool (e.g.
/// `issues.create`).
///
/// Distinct from [`crate::types::ToolName`] — that one is the prefixed,
/// agent-facing name (`mcp_<alias>_<remote>`). The per-agent allowlist
/// keys on the remote name so a server rename (the alias changes) does
/// not silently drop every entry.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct McpToolRemoteName(std::sync::Arc<str>);

impl McpToolRemoteName {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<&str> for McpToolRemoteName {
    type Error = ParseError;

    fn try_from(raw: &str) -> Result<Self, Self::Error> {
        if raw.is_empty() {
            return Err(ParseError::Empty {
                field: "mcp_tool_remote_name",
            });
        }
        if raw.len() > MCP_TOOL_REMOTE_NAME_MAX_LEN {
            return Err(ParseError::TooLong {
                field: "mcp_tool_remote_name",
                max: MCP_TOOL_REMOTE_NAME_MAX_LEN,
                got: raw.len(),
            });
        }
        Ok(Self(std::sync::Arc::from(raw)))
    }
}

impl TryFrom<String> for McpToolRemoteName {
    type Error = ParseError;
    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::try_from(raw.as_str())
    }
}

impl fmt::Debug for McpToolRemoteName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("McpToolRemoteName").field(&&*self.0).finish()
    }
}

impl fmt::Display for McpToolRemoteName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for McpToolRemoteName {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for McpToolRemoteName {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::try_from(raw).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alias_rejects_uppercase_and_spaces() {
        assert!(McpServerAlias::try_from("hello").is_ok());
        assert!(McpServerAlias::try_from("h-1_2").is_ok());
        assert!(McpServerAlias::try_from("Hello").is_err());
        assert!(McpServerAlias::try_from("hi there").is_err());
        assert!(McpServerAlias::try_from("").is_err());
        assert!(McpServerAlias::try_from("a".repeat(MCP_ALIAS_MAX_LEN + 1).as_str()).is_err());
    }

    #[test]
    fn url_requires_http_scheme_and_host() {
        assert!(McpHttpUrl::try_from("https://example.com/mcp").is_ok());
        assert!(McpHttpUrl::try_from("http://localhost:9000").is_ok());
        assert!(McpHttpUrl::try_from("file:///etc/passwd").is_err());
        assert!(McpHttpUrl::try_from("not a url").is_err());
        assert!(McpHttpUrl::try_from("").is_err());
    }

    #[test]
    fn header_name_rejects_separators_and_controls() {
        assert!(McpHeaderName::try_from("Authorization".to_owned()).is_ok());
        assert!(McpHeaderName::try_from("X-Custom_Header".to_owned()).is_ok());
        assert!(McpHeaderName::try_from("bad header".to_owned()).is_err());
        assert!(McpHeaderName::try_from("bad/header".to_owned()).is_err());
        assert!(McpHeaderName::try_from(String::new()).is_err());
    }

    #[test]
    fn header_name_is_canonicalised_to_lowercase() {
        let upper = McpHeaderName::try_from("Authorization".to_owned()).expect("valid");
        let lower = McpHeaderName::try_from("authorization".to_owned()).expect("valid");
        assert_eq!(upper.as_str(), "authorization");
        assert_eq!(upper, lower);
    }

    #[test]
    fn header_value_rejects_crlf() {
        assert!(McpHeaderValue::try_from("Bearer abc.def".to_owned()).is_ok());
        assert!(McpHeaderValue::try_from("oops\r\nInjected: 1".to_owned()).is_err());
        assert!(McpHeaderValue::try_from("nul\0byte".to_owned()).is_err());
    }

    #[test]
    fn transport_round_trips_through_serde() {
        let original = McpTransport::Http {
            url: McpHttpUrl::try_from("https://example.com/mcp").expect("valid"),
        };
        let json = serde_json::to_value(&original).expect("serialize");
        let round: McpTransport = serde_json::from_value(json).expect("deserialize");
        assert_eq!(round, original);
    }

    #[test]
    fn transport_rejects_unknown_scheme_via_serde() {
        let json = serde_json::json!({"type": "http", "url": "ftp://x"});
        let res: Result<McpTransport, _> = serde_json::from_value(json);
        assert!(res.is_err());
    }
}
