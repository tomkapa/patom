//! Storage trait for the MCP-server registry.
//!
//! The HTTP CRUD handlers and the dynamic [`McpRegistry`](super::registry::McpRegistry)
//! both go through this trait — production deployments wire in [`PgMcpServerStore`],
//! tests can wire a fake without touching Postgres.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;

use crate::auth::{OrgId, UserId};

use super::error::McpError;
use super::types::{
    ConnectionStatus, DiscoveredTool, McpCatalogId, McpDescription, McpServerId, McpServerRecord,
    McpTransport,
};

/// Boundary type that captures a CRUD-create request after validation. Lives here (not
/// in HTTP) because the registry's own bootstrapping reaches the same trait method.
#[derive(Debug, Clone)]
pub struct McpServerCreate {
    /// Owning organisation. Set by the HTTP handler from the request
    /// principal; required because `mcp_servers.org_id` is `NOT NULL`.
    pub org_id: OrgId,
    /// Audit field: the user that initiated the create through the HTTP API.
    /// Required because `mcp_servers.created_by_user_id` is `NOT NULL`.
    pub created_by_user_id: UserId,
    pub catalog_id: McpCatalogId,
    pub config: McpTransport,
    pub description: Option<McpDescription>,
    pub enabled: bool,
    /// Initial lifecycle state. `AuthPending` parks a row so the
    /// registry refresher skips it until OAuth credentials land —
    /// otherwise the first connect runs without a Bearer token and the
    /// upstream's 401 lands as a misleading `last_error`.
    pub connection_status: ConnectionStatus,
}

/// Update payload. `None` fields keep the current value; `Some` replaces it.
#[derive(Debug, Clone, Default)]
pub struct McpServerUpdate {
    pub config: Option<McpTransport>,
    pub description: Option<Option<McpDescription>>,
    pub enabled: Option<bool>,
}

/// Health update emitted by the registry after a refresh. Stored verbatim.
#[derive(Debug, Clone)]
pub struct McpHealthUpdate {
    pub last_seen_at: Option<chrono::DateTime<chrono::Utc>>,
    pub last_error: Option<String>,
    pub discovered_tools: Option<Vec<DiscoveredTool>>,
}

#[async_trait]
pub trait McpServerStore: fmt::Debug + Send + Sync {
    async fn create(&self, payload: McpServerCreate) -> Result<McpServerRecord, McpError>;

    /// **PRIVILEGED — cross-tenant.** Returns every row across every org.
    /// The only legitimate caller is the registry refresher (one process-
    /// wide task that scans every enabled server). Never call from an
    /// HTTP handler; use a tenant-scoped raw query inside
    /// [`crate::auth::begin_as`] instead. RLS is bypassed here.
    async fn list(&self) -> Result<Vec<McpServerRecord>, McpError>;

    /// **PRIVILEGED — cross-tenant.** Same caveat as [`Self::list`].
    async fn list_enabled(&self) -> Result<Vec<McpServerRecord>, McpError>;

    /// Tenant-scoped list of every row in `org_id`'s slice. Used by the
    /// session-build wire-up (catalog_id → server_id resolution map) and
    /// by the `search_tools` system tool (wired vs unwired flag). Safe to
    /// call from a tenant-scoped HTTP handler — RLS would already cut to
    /// the caller's slice; the explicit `WHERE org_id` is belt-and-braces.
    async fn list_for_org(&self, org_id: OrgId) -> Result<Vec<McpServerRecord>, McpError>;

    async fn read(&self, id: McpServerId, org_id: OrgId) -> Result<McpServerRecord, McpError>;

    async fn update(
        &self,
        id: McpServerId,
        org_id: OrgId,
        payload: McpServerUpdate,
    ) -> Result<McpServerRecord, McpError>;

    async fn delete(&self, id: McpServerId, org_id: OrgId) -> Result<(), McpError>;

    async fn update_health(
        &self,
        id: McpServerId,
        org_id: OrgId,
        health: McpHealthUpdate,
    ) -> Result<(), McpError>;
}

pub type SharedMcpServerStore = Arc<dyn McpServerStore>;

// `pub(crate)` is the accurate visibility — the module is reachable from any
// `#[cfg(test)]` site in the crate — and `unreachable_pub` would otherwise
// trip if we used a plain `pub` on items inside a private module.
#[cfg(test)]
#[allow(clippy::redundant_pub_crate)]
pub(crate) mod test_support {
    //! Null-object [`McpServerStore`] used only by the `McpRegistry::for_test`
    //! constructor in this crate's unit tests. Every method panics — calling
    //! it would mean a test path that should have been allocation-free went
    //! through `refresh`, which is the bug.
    use super::*;
    use crate::mcp::types::{McpServerId, McpServerRecord};

    #[derive(Debug)]
    struct NullStore;

    #[async_trait]
    impl McpServerStore for NullStore {
        async fn create(&self, _: McpServerCreate) -> Result<McpServerRecord, McpError> {
            panic!("invariant: McpRegistry::for_test must not consult the store");
        }
        async fn list(&self) -> Result<Vec<McpServerRecord>, McpError> {
            panic!("invariant: McpRegistry::for_test must not consult the store");
        }
        async fn list_enabled(&self) -> Result<Vec<McpServerRecord>, McpError> {
            panic!("invariant: McpRegistry::for_test must not consult the store");
        }
        async fn list_for_org(&self, _: OrgId) -> Result<Vec<McpServerRecord>, McpError> {
            panic!("invariant: McpRegistry::for_test must not consult the store");
        }
        async fn read(&self, _: McpServerId, _: OrgId) -> Result<McpServerRecord, McpError> {
            panic!("invariant: McpRegistry::for_test must not consult the store");
        }
        async fn update(
            &self,
            _: McpServerId,
            _: OrgId,
            _: McpServerUpdate,
        ) -> Result<McpServerRecord, McpError> {
            panic!("invariant: McpRegistry::for_test must not consult the store");
        }
        async fn delete(&self, _: McpServerId, _: OrgId) -> Result<(), McpError> {
            panic!("invariant: McpRegistry::for_test must not consult the store");
        }
        async fn update_health(
            &self,
            _: McpServerId,
            _: OrgId,
            _: McpHealthUpdate,
        ) -> Result<(), McpError> {
            panic!("invariant: McpRegistry::for_test must not consult the store");
        }
    }

    pub(crate) fn null_store() -> SharedMcpServerStore {
        Arc::new(NullStore)
    }
}
