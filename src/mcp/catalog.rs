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
use serde::Serialize;
use sqlx::PgPool;

use crate::auth::OrgId;
use crate::types::ParseError;

use super::error::McpError;
use super::limits::{MCP_CATALOG_DESCRIPTION_MAX_LEN, MCP_CATALOG_DISPLAY_NAME_MAX_LEN};
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
}

pub type SharedMcpCatalogStore = Arc<dyn McpCatalogStore>;

/// Postgres-backed implementation. RLS already filters reads to the
/// caller's visible rows (global + their org); the queries themselves do
/// not need to add `WHERE org_id …` clauses.
pub struct PgMcpCatalogStore {
    pool: PgPool,
}

impl PgMcpCatalogStore {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
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
        Ok(Self {
            id,
            org_id,
            display_name,
            description,
            homepage_url: row.homepage_url,
            icon_url: row.icon_url,
            default_transport,
            auth_kind,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
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
                    default_transport, auth_kind, created_at, updated_at \
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
                    default_transport, auth_kind, created_at, updated_at \
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
}
