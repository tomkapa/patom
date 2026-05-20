//! Per-agent view of the MCP tool catalogue.
//!
//! Strict-by-default: a catalog id absent from the allowlist exposes none
//! of that integration's tools. Within an allowed catalog, the value
//! carried by the [`AllowedMcpTools`] map decides whether every tool is
//! exposed ([`ToolScope::All`]) or only a named subset
//! ([`ToolScope::Some`]).
//!
//! Catalog → server resolution is supplied at construction (built once
//! per session in the composition root from the org's wired
//! `mcp_servers`). A catalog id present in the allowlist but absent from
//! the resolution map contributes zero tools — the recruiter is expected
//! to have asked the user to wire it via `request_user_wire_mcp` first.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use crate::agents::{AllowedMcpTools, ToolScope};
use crate::provider::ToolSpec;
use crate::tools::{DynamicToolSource, SharedTool};

use super::registry::McpRegistry;
use super::types::{McpCatalogId, McpServerId, McpToolRemoteName};

#[derive(Debug, Clone)]
pub struct ScopedMcpSource {
    registry: McpRegistry,
    /// Pre-resolved view: catalog ids in the allowlist that have a wired
    /// connection in this org, indexed by the concrete `McpServerId` the
    /// tool dispatcher matches against. Catalog ids without a wired
    /// connection drop out at construction (no entry here) so the per-tool
    /// permits() check stays O(log n) on a small map.
    by_server: BTreeMap<McpServerId, Option<BTreeSet<McpToolRemoteName>>>,
}

impl ScopedMcpSource {
    /// Build the per-agent filter view.
    ///
    /// `catalog_to_server` is the org's "this catalog is wired as this
    /// concrete server" map — built once per session from
    /// `McpServerStore::list_for_org`. Catalog ids in `allowed` that are
    /// absent from the map are silently dropped (see module docs).
    #[must_use]
    pub fn new(
        registry: McpRegistry,
        allowed: &AllowedMcpTools,
        catalog_to_server: &HashMap<McpCatalogId, McpServerId>,
    ) -> Self {
        let mut by_server: BTreeMap<McpServerId, Option<BTreeSet<McpToolRemoteName>>> =
            BTreeMap::new();
        for (catalog, scope) in allowed.iter() {
            let Some(server) = catalog_to_server.get(catalog) else {
                continue;
            };
            let value = match scope {
                ToolScope::None => continue,
                ToolScope::All => None,
                ToolScope::Some(set) => Some(set.clone()),
            };
            by_server.insert(*server, value);
        }
        Self {
            registry,
            by_server,
        }
    }

    fn permits(&self, server: McpServerId, remote: &McpToolRemoteName) -> bool {
        match self.by_server.get(&server) {
            None => false,
            Some(None) => true,
            Some(Some(set)) => set.contains(remote),
        }
    }

    fn is_empty(&self) -> bool {
        self.by_server.is_empty()
    }
}

impl DynamicToolSource for ScopedMcpSource {
    fn specs(&self) -> Arc<[ToolSpec]> {
        if self.is_empty() {
            return Arc::default();
        }
        let snapshot = self.registry.snapshot();
        let mut kept: Vec<ToolSpec> = Vec::with_capacity(snapshot.specs.len());
        for spec in snapshot.specs.iter() {
            if let Some(origin) = snapshot.tool_origins.get(spec.name.as_str())
                && self.permits(origin.server, &origin.remote_name)
            {
                kept.push(spec.clone());
            }
        }
        Arc::from(kept)
    }

    fn get(&self, name: &str) -> Option<SharedTool> {
        if self.is_empty() {
            return None;
        }
        let (tool, origin) = self.registry.lookup(name)?;
        self.permits(origin.server, &origin.remote_name)
            .then_some(tool)
    }

    fn server_id_for(&self, name: &str) -> Option<McpServerId> {
        if self.is_empty() {
            return None;
        }
        let (_tool, origin) = self.registry.lookup(name)?;
        self.permits(origin.server, &origin.remote_name)
            .then_some(origin.server)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use async_trait::async_trait;
    use serde_json::{Value, json};

    use super::*;
    use crate::tools::{Tool, ToolCallContext, ToolError};
    use crate::types::ToolName;

    #[derive(Debug)]
    struct FakeTool {
        name: ToolName,
    }

    impl FakeTool {
        fn new(name: &str) -> Self {
            Self {
                name: ToolName::try_from(name).expect("valid"),
            }
        }
    }

    #[async_trait]
    impl Tool for FakeTool {
        fn name(&self) -> &ToolName {
            &self.name
        }
        fn description(&self) -> &str {
            "fake"
        }
        fn input_schema(&self) -> Arc<Value> {
            Arc::new(json!({"type":"object"}))
        }
        async fn execute(
            &self,
            _input: Value,
            _ctx: &ToolCallContext,
        ) -> Result<String, ToolError> {
            Ok("ok".into())
        }
    }

    fn fake(name: &str) -> SharedTool {
        Arc::new(FakeTool::new(name))
    }

    fn spec_names(specs: &[crate::provider::ToolSpec]) -> Vec<String> {
        specs.iter().map(|s| s.name.as_str().to_owned()).collect()
    }

    fn cat(id: &str) -> McpCatalogId {
        McpCatalogId::try_from(id).expect("valid catalog id")
    }

    /// Build an [`AllowedMcpTools`] from a list of `(catalog_id, scope)`
    /// entries where `scope = None` means "all tools" and
    /// `scope = Some(&[..])` means "only these remote names."
    fn allow(entries: &[(&str, Option<&[&str]>)]) -> AllowedMcpTools {
        let mut raw: BTreeMap<String, Option<Vec<String>>> = BTreeMap::new();
        for (id, names) in entries {
            let v = names.map(|list| list.iter().map(|s| (*s).to_owned()).collect());
            raw.insert((*id).to_owned(), v);
        }
        AllowedMcpTools::try_from(raw).expect("valid scope")
    }

    /// Build the catalog → server resolution map the runtime composes from
    /// `McpServerStore::list_for_org` at session-build time.
    fn resolve(entries: &[(&str, McpServerId)]) -> HashMap<McpCatalogId, McpServerId> {
        entries
            .iter()
            .map(|(id, server)| (cat(id), *server))
            .collect()
    }

    #[test]
    fn empty_allowlist_hides_every_tool() {
        let s1 = McpServerId::new();
        let registry = McpRegistry::for_test_with_remote_names(vec![
            (s1, "alpha".into(), fake("mcp_one_alpha")),
            (s1, "beta".into(), fake("mcp_one_beta")),
        ]);
        let scoped = ScopedMcpSource::new(
            registry,
            &AllowedMcpTools::empty(),
            &resolve(&[("one", s1)]),
        );
        assert!(scoped.specs().is_empty());
        assert!(scoped.get("mcp_one_alpha").is_none());
    }

    #[test]
    fn allowed_catalog_with_none_scope_exposes_every_tool() {
        let s1 = McpServerId::new();
        let s2 = McpServerId::new();
        let registry = McpRegistry::for_test_with_remote_names(vec![
            (s1, "alpha".into(), fake("mcp_one_alpha")),
            (s2, "beta".into(), fake("mcp_two_beta")),
            (s2, "gamma".into(), fake("mcp_two_gamma")),
        ]);
        let scoped = ScopedMcpSource::new(
            registry,
            &allow(&[("two", None)]),
            &resolve(&[("one", s1), ("two", s2)]),
        );
        let names = spec_names(&scoped.specs());
        assert_eq!(names.len(), 2);
        assert!(names.iter().any(|n| n == "mcp_two_beta"));
        assert!(names.iter().any(|n| n == "mcp_two_gamma"));
        assert!(scoped.get("mcp_one_alpha").is_none());
        assert!(scoped.get("mcp_two_beta").is_some());
    }

    #[test]
    fn partial_scope_keeps_only_named_tools() {
        let s1 = McpServerId::new();
        let registry = McpRegistry::for_test_with_remote_names(vec![
            (s1, "alpha".into(), fake("mcp_one_alpha")),
            (s1, "beta".into(), fake("mcp_one_beta")),
            (s1, "gamma".into(), fake("mcp_one_gamma")),
        ]);
        let scoped = ScopedMcpSource::new(
            registry,
            &allow(&[("one", Some(&["alpha", "gamma"]))]),
            &resolve(&[("one", s1)]),
        );
        let names = spec_names(&scoped.specs());
        assert_eq!(names.len(), 2);
        assert!(names.iter().any(|n| n == "mcp_one_alpha"));
        assert!(names.iter().any(|n| n == "mcp_one_gamma"));
        assert!(scoped.get("mcp_one_beta").is_none());
    }

    #[test]
    fn empty_subset_locks_down_an_allowed_catalog() {
        let s1 = McpServerId::new();
        let registry = McpRegistry::for_test_with_remote_names(vec![
            (s1, "alpha".into(), fake("mcp_one_alpha")),
            (s1, "beta".into(), fake("mcp_one_beta")),
        ]);
        let scoped = ScopedMcpSource::new(
            registry,
            &allow(&[("one", Some(&[]))]),
            &resolve(&[("one", s1)]),
        );
        assert!(scoped.specs().is_empty());
        assert!(scoped.get("mcp_one_alpha").is_none());
    }

    #[test]
    fn unknown_tool_yields_none_even_when_allowlist_nonempty() {
        let s1 = McpServerId::new();
        let registry = McpRegistry::for_test_with_remote_names(vec![(
            s1,
            "alpha".into(),
            fake("mcp_one_alpha"),
        )]);
        let scoped =
            ScopedMcpSource::new(registry, &allow(&[("one", None)]), &resolve(&[("one", s1)]));
        assert!(scoped.get("mcp_one_does_not_exist").is_none());
    }

    #[test]
    fn allowing_multiple_catalogs_unions_their_tools() {
        let s1 = McpServerId::new();
        let s2 = McpServerId::new();
        let s3 = McpServerId::new();
        let registry = McpRegistry::for_test_with_remote_names(vec![
            (s1, "alpha".into(), fake("mcp_one_alpha")),
            (s2, "beta".into(), fake("mcp_two_beta")),
            (s3, "gamma".into(), fake("mcp_three_gamma")),
        ]);
        let scoped = ScopedMcpSource::new(
            registry,
            &allow(&[("one", None), ("three", None)]),
            &resolve(&[("one", s1), ("two", s2), ("three", s3)]),
        );
        let names = spec_names(&scoped.specs());
        assert_eq!(names.len(), 2);
        assert!(names.iter().any(|n| n == "mcp_one_alpha"));
        assert!(names.iter().any(|n| n == "mcp_three_gamma"));
        assert!(scoped.get("mcp_two_beta").is_none());
    }

    #[test]
    fn unwired_catalog_in_allowlist_is_inert() {
        // The recruiter assigned `two` to the agent, but the user never
        // wired it — `resolve` has no entry for `two`. The filter must
        // not error and must surface only tools whose catalog is both in
        // the allowlist *and* in the resolution map.
        let s1 = McpServerId::new();
        let registry = McpRegistry::for_test_with_remote_names(vec![(
            s1,
            "alpha".into(),
            fake("mcp_one_alpha"),
        )]);
        let scoped = ScopedMcpSource::new(
            registry,
            &allow(&[("one", None), ("two", None)]),
            &resolve(&[("one", s1)]),
        );
        let names = spec_names(&scoped.specs());
        assert_eq!(names, vec!["mcp_one_alpha".to_owned()]);
    }

    #[test]
    fn partial_scope_referencing_unknown_remote_name_keeps_zero() {
        // The agent asked for `phantom`, which the live server does not
        // expose. The known tools are not surfaced because they aren't on
        // the per-tool list either.
        let s1 = McpServerId::new();
        let registry = McpRegistry::for_test_with_remote_names(vec![
            (s1, "alpha".into(), fake("mcp_one_alpha")),
            (s1, "beta".into(), fake("mcp_one_beta")),
        ]);
        let scoped = ScopedMcpSource::new(
            registry,
            &allow(&[("one", Some(&["phantom"]))]),
            &resolve(&[("one", s1)]),
        );
        assert!(scoped.specs().is_empty());
        assert!(scoped.get("mcp_one_alpha").is_none());
    }
}
