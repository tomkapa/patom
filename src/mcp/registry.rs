//! Dynamic MCP tool registry.
//!
//! Holds the live MCP client connections and the tool specs derived from them. Refresh
//! is the only mutator; reads (`specs`, `get`) are lock-free in steady state because
//! the inner state is replaced as a whole `Arc`.
//!
//! [`McpRegistry`] is a cheap-clone newtype around `Arc<McpRegistryInner>` —
//! every consumer (worker, HTTP handlers, refresher, scoped sources) holds
//! its own clone of the registry handle without needing to wrap it in an
//! outer `Arc<...>` each time.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use serde_json::Value;
use tracing::{error, instrument, warn};

use crate::clock::SharedClock;
use crate::provider::ToolSpec;
use crate::tools::{DynamicToolSource, SharedTool};
use crate::types::{ParseError, ToolName};

use super::client::McpClient;
use super::credentials::{CredentialPayload, SharedMcpCredentialStore};
use super::error::McpError;
use super::limits::{MAX_MCP_SERVERS, MAX_TOOLS_PER_SERVER};
use super::store::{McpHealthUpdate, SharedMcpServerStore};
use super::tool::McpTool;
use super::types::{
    DiscoveredTool, McpCatalogId, McpServerId, McpServerRecord, McpToolRemoteName, McpTransport,
};

/// Cheap-clone handle to the live MCP tool catalogue.
///
/// Wraps an `Arc<McpRegistryInner>`: the composition root constructs one and
/// every consumer (worker, refresher, HTTP CRUD, scoped sources) just
/// clones it. Reads (`specs`, `get`) are lock-free in steady state because
/// the inner state is replaced wholesale on each refresh.
#[derive(Clone, Debug)]
pub struct McpRegistry(Arc<McpRegistryInner>);

/// Point-in-time read view returned by [`McpRegistry::snapshot`].
///
/// Holding both `Arc`s lets a caller iterate specs and look up source
/// servers without re-entering the registry lock per spec. `tool_origins`
/// carries the source server **and** the remote (unprefixed) name so the
/// per-agent scope filter can match tool subsets without re-parsing the
/// prefixed name.
#[derive(Debug, Clone)]
pub struct RegistrySnapshot {
    pub specs: Arc<[ToolSpec]>,
    pub tool_origins: Arc<HashMap<ToolName, ToolOrigin>>,
}

/// Source-server back-pointer for a single tool in the registry.
#[derive(Debug, Clone)]
pub struct ToolOrigin {
    pub server: McpServerId,
    pub remote_name: McpToolRemoteName,
}

/// Inner state — held behind an `Arc` by [`McpRegistry`]. Holds the live
/// MCP clients and the spec snapshot; mutated only by `refresh`.
struct McpRegistryInner {
    inner: RwLock<McpState>,
    store: SharedMcpServerStore,
    /// Optional credentials store. When present (production), `refresh`
    /// loads decrypted credentials and threads them into each `connect`.
    /// Tests that don't exercise credentials wire `None`.
    credentials: Option<SharedMcpCredentialStore>,
    clock: SharedClock,
    server_cap: usize,
}

impl std::fmt::Debug for McpRegistryInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let snapshot = self.inner.read().expect("registry lock poisoned");
        f.debug_struct("McpRegistry")
            .field("server_cap", &self.server_cap)
            .field("connected_servers", &snapshot.servers.len())
            .field("total_tools", &snapshot.by_name.len())
            .finish_non_exhaustive()
    }
}

#[derive(Default)]
struct McpState {
    by_name: HashMap<ToolName, SharedTool>,
    /// Per-tool back-pointer to (server, remote_name). Wrapped in `Arc` so
    /// [`ScopedMcpSource`] can pull a single snapshot under one read lock
    /// alongside `specs` instead of looking each name up through a fresh
    /// acquisition.
    tool_origins: Arc<HashMap<ToolName, ToolOrigin>>,
    specs: Arc<[ToolSpec]>,
    /// Indexed by server id so we can reuse a client across refreshes when its config
    /// hasn't changed (avoids re-running the MCP `initialize` handshake on every CRUD
    /// edit to an unrelated row).
    servers: HashMap<McpServerId, ConnectedServer>,
    /// Per-org catalog→server resolution snapshot. Populated by
    /// [`McpRegistry::refresh`] from the same `mcp_servers` rows; consumed
    /// (cheaply, by `Arc` clone) by the composition root when building a
    /// session's [`super::ScopedMcpSource`] without needing a DB round-trip
    /// on every turn.
    org_catalog_servers: Arc<HashMap<crate::auth::OrgId, Arc<HashMap<McpCatalogId, McpServerId>>>>,
}

struct ConnectedServer {
    config: McpTransport,
    client: Arc<McpClient>,
}

impl McpRegistry {
    /// Build an empty registry. Call [`refresh`](Self::refresh) once at startup before
    /// the worker pool begins claiming sessions, so the agent's first turn already sees
    /// the registered MCP tools.
    #[must_use]
    pub fn new(store: SharedMcpServerStore, clock: SharedClock) -> Self {
        Self::with_credentials(store, None, clock)
    }

    /// Construct with a credentials store wired in. The composition root
    /// uses this in production; tests that don't exercise the encrypted
    /// path stick with [`Self::new`].
    #[must_use]
    pub fn with_credentials(
        store: SharedMcpServerStore,
        credentials: Option<SharedMcpCredentialStore>,
        clock: SharedClock,
    ) -> Self {
        Self(Arc::new(McpRegistryInner {
            inner: RwLock::new(McpState::default()),
            store,
            credentials,
            clock,
            server_cap: MAX_MCP_SERVERS,
        }))
    }

    /// Read-side handle: the (possibly-empty) flat slice of tool specs the agent
    /// concatenates with the static built-in registry every turn.
    #[must_use]
    pub fn specs(&self) -> Arc<[ToolSpec]> {
        self.0.specs()
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<SharedTool> {
        self.0.get(name)
    }

    /// Snapshot of the read state the per-agent scope filter needs — both
    /// the spec slice and the tool→server map — in **one** lock acquisition.
    /// The returned `Arc`s share state with the registry; subsequent refreshes
    /// replace them wholesale, so the caller gets a consistent point-in-time
    /// view without holding the lock while it filters.
    #[must_use]
    pub fn snapshot(&self) -> RegistrySnapshot {
        self.0.snapshot()
    }

    /// Single-lock-acquisition lookup of a tool and its origin (server id +
    /// remote name). Used by [`ScopedMcpSource::get`] so dispatch never
    /// pays two round-trips through the read lock.
    #[must_use]
    pub fn lookup(&self, name: &str) -> Option<(SharedTool, ToolOrigin)> {
        self.0.lookup(name)
    }

    /// Snapshot of `org_id`'s catalog → server resolution. Returns the
    /// per-org `Arc` directly so the composition root pays a single
    /// pointer clone per session-build. Empty when the org has no wired
    /// MCPs (and when called before the first [`refresh`](Self::refresh)).
    #[must_use]
    pub fn catalog_to_server_for_org(
        &self,
        org_id: crate::auth::OrgId,
    ) -> Arc<HashMap<McpCatalogId, McpServerId>> {
        self.0.catalog_to_server_for_org(org_id)
    }

    /// Re-read enabled servers, refresh tool lists, atomically swap state.
    /// Per-server failures are isolated; one bad server doesn't abort the
    /// whole refresh.
    pub async fn refresh(&self) -> Result<(), McpError> {
        self.0.refresh().await
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Project this registry as the trait object the [`crate::tools::ToolBox`]
    /// expects on its dynamic side. The returned `Arc` shares state with `self`
    /// (no extra allocation), so the toolbox sees the same live catalogue
    /// every refresh writes into.
    #[must_use]
    pub fn as_dynamic_source(&self) -> Arc<dyn DynamicToolSource> {
        self.0.clone()
    }

    /// Test seam: bypass the refresh path by installing a synthetic catalogue
    /// directly. Each entry is `(server_id, tool)`; the tool's own name is
    /// used as the registry key (no `mcp_<alias>_` prefixing — tests build
    /// whatever names they need). Lives behind `#[cfg(test)]` so it never
    /// reaches release artifacts.
    /// Test seam: bypass the refresh path by installing a synthetic catalogue
    /// directly. Each entry is `(server_id, remote_name, tool)`. Lives behind
    /// `#[cfg(test)]` so it never reaches release artifacts.
    #[cfg(test)]
    pub(crate) fn for_test_with_remote_names(
        entries: Vec<(McpServerId, String, SharedTool)>,
    ) -> Self {
        let mut by_name: HashMap<ToolName, SharedTool> = HashMap::new();
        let mut tool_origins: HashMap<ToolName, ToolOrigin> = HashMap::new();
        let mut specs: Vec<ToolSpec> = Vec::with_capacity(entries.len());
        for (server, remote, tool) in entries {
            let name = tool.name().clone();
            let spec = ToolSpec {
                name: name.clone(),
                description: Arc::from(tool.description()),
                input_schema: tool.input_schema(),
            };
            specs.push(spec);
            let remote_name = McpToolRemoteName::try_from(remote.as_str())
                .expect("invariant: test remote_name within cap");
            tool_origins.insert(
                name.clone(),
                ToolOrigin {
                    server,
                    remote_name,
                },
            );
            by_name.insert(name, tool);
        }
        specs.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
        Self(Arc::new(McpRegistryInner {
            inner: RwLock::new(McpState {
                by_name,
                tool_origins: Arc::new(tool_origins),
                specs: Arc::from(specs),
                servers: HashMap::new(),
                org_catalog_servers: Arc::new(HashMap::new()),
            }),
            // The store and clock are only consulted by `refresh`; tests
            // that build via `for_test` never call refresh, so the
            // never-touched fields can hold null-object stand-ins.
            store: crate::mcp::store::test_support::null_store(),
            credentials: None,
            clock: crate::clock::SystemClock::shared(),
            server_cap: MAX_MCP_SERVERS,
        }))
    }
}

impl McpRegistryInner {
    fn specs(&self) -> Arc<[ToolSpec]> {
        self.inner
            .read()
            .expect("registry lock poisoned")
            .specs
            .clone()
    }

    fn get(&self, name: &str) -> Option<SharedTool> {
        self.inner
            .read()
            .expect("registry lock poisoned")
            .by_name
            .get(name)
            .cloned()
    }

    fn snapshot(&self) -> RegistrySnapshot {
        let guard = self.inner.read().expect("registry lock poisoned");
        RegistrySnapshot {
            specs: guard.specs.clone(),
            tool_origins: guard.tool_origins.clone(),
        }
    }

    fn lookup(&self, name: &str) -> Option<(SharedTool, ToolOrigin)> {
        let guard = self.inner.read().expect("registry lock poisoned");
        let origin = guard.tool_origins.get(name).cloned()?;
        let tool = guard.by_name.get(name).cloned()?;
        Some((tool, origin))
    }

    fn catalog_to_server_for_org(
        &self,
        org_id: crate::auth::OrgId,
    ) -> Arc<HashMap<McpCatalogId, McpServerId>> {
        let guard = self.inner.read().expect("registry lock poisoned");
        guard
            .org_catalog_servers
            .get(&org_id)
            .cloned()
            .unwrap_or_else(|| Arc::new(HashMap::new()))
    }

    fn len(&self) -> usize {
        self.inner
            .read()
            .expect("registry lock poisoned")
            .by_name
            .len()
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[instrument(name = "mcp.registry.refresh", skip_all, err)]
    async fn refresh(&self) -> Result<(), McpError> {
        let rows = self.store.list_enabled().await?;
        // §6 / §5: cap is enforced at create-time, but we belt-and-brace at refresh too
        // — a misconfigured deploy or a DB hand-edit shouldn't blow the budget here.
        if rows.len() > self.server_cap {
            warn!(
                relay.mcp.rows = rows.len(),
                relay.mcp.cap = self.server_cap,
                "mcp.refresh.cap_exceeded",
            );
        }
        let rows: Vec<McpServerRecord> = rows.into_iter().take(self.server_cap).collect();

        // Snapshot existing connections so we can reuse / drop them.
        let mut prior: HashMap<McpServerId, ConnectedServer> = {
            let mut guard = self.inner.write().expect("registry lock poisoned");
            std::mem::take(&mut guard.servers)
        };

        let mut builder = McpStateBuilder::with_capacity(rows.len());
        for row in rows {
            self.refresh_one(row, &mut prior, &mut builder).await;
        }

        *self.inner.write().expect("registry lock poisoned") = builder.finish();
        // `prior` (any servers no longer present) is dropped here, which terminates
        // their rmcp worker tasks.
        drop(prior);
        Ok(())
    }

    /// Per-server work for [`refresh`]. Connect (or reuse), list tools, populate the
    /// new state buffers. Failures are logged and reflected in `last_error`; a single
    /// bad server never aborts the whole refresh.
    async fn refresh_one(
        &self,
        row: McpServerRecord,
        prior: &mut HashMap<McpServerId, ConnectedServer>,
        builder: &mut McpStateBuilder,
    ) {
        let McpServerRecord {
            id,
            org_id,
            catalog_id,
            config,
            ..
        } = row;

        // Always record the catalog → server resolution for this org —
        // even if the connect fails — so the recruiter's allowlist still
        // resolves while one server is in `Error` / `ReconnectRequired`.
        // ScopedMcpSource will safely surface zero tools when the
        // registry's tool map is empty for that server.
        builder
            .org_catalog_servers
            .entry(org_id)
            .or_default()
            .insert(catalog_id.clone(), id);

        // Load credentials before connecting. Servers without a row decrypt
        // to `None` and connect with no auth; the path is unchanged for
        // them. A decrypt failure is recorded against the server's
        // `last_error` and the row is skipped this refresh.
        let credentials = match self.load_credentials(id, org_id, &catalog_id).await {
            Ok(c) => c,
            Err(()) => return,
        };

        let client = match self
            .connect_or_reuse(
                id,
                org_id,
                &catalog_id,
                &config,
                credentials.as_ref(),
                prior,
            )
            .await
        {
            Some(c) => c,
            None => return,
        };

        let remote_tools = match client.list_tools().await {
            Ok(t) => t,
            Err(e) => {
                warn!(
                    relay.mcp.server.id = %id,
                    relay.mcp.server.catalog_id = %catalog_id,
                    error = %e,
                    "mcp.refresh.list_tools_failed",
                );
                let _ = self
                    .store
                    .update_health(
                        id,
                        org_id,
                        McpHealthUpdate {
                            last_seen_at: None,
                            last_error: Some(format!("list_tools: {e}")),
                            discovered_tools: None,
                        },
                    )
                    .await;
                return;
            }
        };

        let now = self.clock.now_utc();
        let discovered = ingest_tools(id, &catalog_id, &client, remote_tools, builder);

        let _ = self
            .store
            .update_health(
                id,
                org_id,
                McpHealthUpdate {
                    last_seen_at: Some(now),
                    last_error: None,
                    discovered_tools: Some(discovered),
                },
            )
            .await;

        builder
            .servers
            .insert(id, ConnectedServer { config, client });
    }

    /// Load credentials for a server. Returns `Ok(None)` for servers with
    /// no credential row (public MCP endpoints), `Ok(Some(_))` after a
    /// successful decrypt, or `Err(())` (and records `last_error` against
    /// the server row) on a decrypt failure. The decrypt failure path
    /// surfaces in `last_error` so operators see why the server stopped
    /// connecting after a master-KEK rotation gone wrong.
    async fn load_credentials(
        &self,
        id: McpServerId,
        org_id: crate::auth::OrgId,
        catalog_id: &McpCatalogId,
    ) -> Result<Option<CredentialPayload>, ()> {
        let Some(store) = self.credentials.as_ref() else {
            return Ok(None);
        };
        match store.read(id, org_id).await {
            Ok(Some(rec)) => Ok(Some(rec.payload)),
            Ok(None) => Ok(None),
            Err(e) => {
                // `error!` so the refresh span's status flips to ERROR
                // via the OTel bridge; credential decrypt failure here
                // is unexpected (master KEK rotation gone wrong, or
                // schema drift) and an operator needs to see it.
                error!(
                    relay.mcp.server.id = %id,
                    relay.mcp.server.catalog_id = %catalog_id,
                    error = ?e,
                    "mcp.refresh.credential_load_failed",
                );
                let _ = self
                    .store
                    .update_health(
                        id,
                        org_id,
                        McpHealthUpdate {
                            last_seen_at: None,
                            last_error: Some(format!("credentials: {e}")),
                            discovered_tools: None,
                        },
                    )
                    .await;
                Err(())
            }
        }
    }

    /// Reuse an existing client when its config hasn't changed; otherwise open a new
    /// one. `None` on connect failure (the row has had its `last_error` updated).
    ///
    /// Note: when credentials change we always reconnect — the bearer token
    /// is baked into the rmcp transport on `connect`, so a rotated token
    /// has no effect on an already-open client. Comparing the cached
    /// `config` alone wouldn't catch that, so the credentials parameter is
    /// part of the cache-key in spirit: callers that fetch fresh
    /// credentials and pass them in will trigger a reconnect via the path
    /// in phase D when the registered payload changes.
    async fn connect_or_reuse(
        &self,
        id: McpServerId,
        org_id: crate::auth::OrgId,
        catalog_id: &McpCatalogId,
        config: &McpTransport,
        credentials: Option<&CredentialPayload>,
        prior: &mut HashMap<McpServerId, ConnectedServer>,
    ) -> Option<Arc<McpClient>> {
        if let Some(prev) = prior.remove(&id)
            && &prev.config == config
            && credentials.is_none()
        {
            // Reuse is safe only when the transport config matches *and*
            // there are no credentials to refresh. With credentials we
            // conservatively reconnect each refresh so a rotated token
            // takes effect within one tick.
            return Some(prev.client);
        }
        match McpClient::connect(config, credentials).await {
            Ok(c) => Some(Arc::new(c)),
            Err(e) => {
                warn!(
                    relay.mcp.server.id = %id,
                    relay.mcp.server.catalog_id = %catalog_id,
                    error = %e,
                    "mcp.refresh.connect_failed",
                );
                let _ = self
                    .store
                    .update_health(
                        id,
                        org_id,
                        McpHealthUpdate {
                            last_seen_at: None,
                            last_error: Some(format!("connect: {e}")),
                            discovered_tools: None,
                        },
                    )
                    .await;
                None
            }
        }
    }
}

/// Build [`McpTool`] / [`ToolSpec`] entries for one server's remote tool list. Returns
/// the per-row `discovered_tools` snapshot that the store records.
fn ingest_tools(
    server_id: McpServerId,
    catalog_id: &McpCatalogId,
    client: &Arc<McpClient>,
    remote_tools: Vec<rmcp::model::Tool>,
    builder: &mut McpStateBuilder,
) -> Vec<DiscoveredTool> {
    let mut discovered: Vec<DiscoveredTool> = Vec::with_capacity(remote_tools.len());
    for remote in remote_tools.into_iter().take(MAX_TOOLS_PER_SERVER) {
        let prefixed = match prefixed_name(catalog_id, remote.name.as_ref()) {
            Ok(n) => n,
            Err(e) => {
                warn!(
                    relay.mcp.server.catalog_id = %catalog_id,
                    relay.mcp.tool.remote = %remote.name,
                    error = %e,
                    "mcp.tool.name_too_long",
                );
                continue;
            }
        };
        if builder.by_name.contains_key(&prefixed) {
            // A different server already produced this prefixed name (alias collision
            // or duplicated remote name); skip the duplicate so registry building stays
            // total. Operators see the survivor in `discovered_tools`.
            warn!(
                relay.tool = %prefixed,
                "mcp.tool.duplicate_skipped",
            );
            continue;
        }
        let description: Arc<str> = Arc::from(
            remote
                .description
                .as_deref()
                .unwrap_or("(no description provided)"),
        );
        let schema = Arc::new(Value::Object((*remote.input_schema).clone()));
        // The remote name has already been validated by the registry
        // when the prefixed name fit within `ToolName`'s 64-byte cap;
        // failing to parse here would mean we built a longer prefixed
        // name from a shorter remote name, which is impossible. Skip the
        // tool (with a warn) rather than panic on the path so a single
        // misbehaving server can't take the whole refresh down.
        let remote_name = match McpToolRemoteName::try_from(remote.name.as_ref()) {
            Ok(n) => n,
            Err(e) => {
                warn!(
                    relay.mcp.server.catalog_id = %catalog_id,
                    relay.mcp.tool.remote = %remote.name,
                    error = %e,
                    "mcp.tool.remote_name_invalid",
                );
                continue;
            }
        };
        discovered.push(DiscoveredTool {
            remote_name: remote.name.to_string(),
            prefixed_name: prefixed.as_str().to_owned(),
            description: remote.description.as_deref().map(str::to_owned),
        });
        let tool = Arc::new(McpTool::new(
            prefixed.clone(),
            remote.name.to_string(),
            description.clone(),
            schema.clone(),
            client.clone(),
        ));
        builder.specs.push(ToolSpec {
            name: prefixed.clone(),
            description,
            input_schema: schema,
        });
        builder.tool_origins.insert(
            prefixed.clone(),
            ToolOrigin {
                server: server_id,
                remote_name,
            },
        );
        builder.by_name.insert(prefixed, tool);
    }
    discovered
}

/// Accumulator for the in-progress next [`McpState`] across the
/// per-server refresh loop. Owning the four parallel collections in one
/// struct keeps the per-call signature of [`refresh_one`] / `ingest_tools`
/// to a single `&mut` borrow.
struct McpStateBuilder {
    by_name: HashMap<ToolName, SharedTool>,
    tool_origins: HashMap<ToolName, ToolOrigin>,
    specs: Vec<ToolSpec>,
    servers: HashMap<McpServerId, ConnectedServer>,
    /// Mirror of the per-org catalog → server map populated by
    /// [`refresh_one`] before connect — included regardless of connect
    /// outcome so the recruiter's allowlist still resolves while a single
    /// server is temporarily in `Error` / `ReconnectRequired`. Frozen into
    /// per-org `Arc`s on `finish`.
    org_catalog_servers: HashMap<crate::auth::OrgId, HashMap<McpCatalogId, McpServerId>>,
}

impl McpStateBuilder {
    fn with_capacity(server_count: usize) -> Self {
        Self {
            by_name: HashMap::new(),
            tool_origins: HashMap::new(),
            specs: Vec::new(),
            servers: HashMap::with_capacity(server_count),
            org_catalog_servers: HashMap::new(),
        }
    }

    fn finish(mut self) -> McpState {
        self.specs
            .sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
        let org_catalog_servers: HashMap<_, _> = self
            .org_catalog_servers
            .into_iter()
            .map(|(org, inner)| (org, Arc::new(inner)))
            .collect();
        McpState {
            by_name: self.by_name,
            tool_origins: Arc::new(self.tool_origins),
            specs: Arc::from(self.specs),
            servers: self.servers,
            org_catalog_servers: Arc::new(org_catalog_servers),
        }
    }
}

fn prefixed_name(catalog_id: &McpCatalogId, remote: &str) -> Result<ToolName, ParseError> {
    let raw = format!("mcp_{}_{}", catalog_id.as_str(), remote);
    ToolName::try_from(raw.as_str())
}

/// `DynamicToolSource` lives on the inner so `Arc<McpRegistryInner>` upcasts
/// straight to `Arc<dyn DynamicToolSource>` — [`McpRegistry::as_dynamic_source`]
/// just returns that upcast, no extra wrapping.
impl DynamicToolSource for McpRegistryInner {
    fn specs(&self) -> Arc<[ToolSpec]> {
        Self::specs(self)
    }
    fn get(&self, name: &str) -> Option<SharedTool> {
        Self::get(self, name)
    }
    fn server_id_for(&self, name: &str) -> Option<McpServerId> {
        self.inner
            .read()
            .expect("registry lock poisoned")
            .tool_origins
            .get(name)
            .map(|o| o.server)
    }
}
