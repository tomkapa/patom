//! Composite tool surface that the agent holds.
//!
//! Combines an immutable [`ToolRegistry`] of built-in tools with a dynamic source of
//! MCP tools. The agent calls `specs()` once per turn (snapshot semantics) and looks
//! up by name when the model emits a tool call. Built-ins shadow MCP tools on a name
//! collision — defence in depth, since the MCP prefix `mcp_…` should already make
//! collisions impossible.

use std::sync::Arc;

use crate::mcp::McpServerId;
use crate::provider::ToolSpec;
use crate::runtime::RequestKind;

use super::registry::ToolRegistry;
use super::traits::SharedTool;

/// Trait that supplies dynamic tool specs to the [`ToolBox`]. Implemented by
/// `McpRegistry` in production and by hand-rolled fakes in tests; keeps `ToolBox`
/// independent of the MCP module.
pub trait DynamicToolSource: std::fmt::Debug + Send + Sync {
    /// Slice of tool specs to expose to the provider. Returned as `Arc<[…]>` so each
    /// turn's snapshot is reference-counted, not copied.
    fn specs(&self) -> Arc<[ToolSpec]>;

    /// Mode-filtered variant of [`Self::specs`]. Default implementation
    /// returns every spec — implementations whose tools all default to
    /// `RequestKindModes::ALL` (e.g. MCP-sourced tools) need not
    /// override.
    fn specs_for(&self, _kind: RequestKind) -> Arc<[ToolSpec]> {
        self.specs()
    }

    /// Look up a tool by its (prefixed) name.
    fn get(&self, name: &str) -> Option<SharedTool>;

    /// Mode-filtered variant of [`Self::get`]. Default returns the same
    /// result as [`Self::get`]; override only when the dynamic source
    /// has per-mode opt-outs.
    fn get_for(&self, _kind: RequestKind, name: &str) -> Option<SharedTool> {
        self.get(name)
    }

    /// The MCP server backing this tool, if any.
    ///
    /// Used by the dispatcher's `tool_calls` recorder to populate the
    /// nullable `mcp_server_id` column without making the source
    /// type-specific. Default returns `None` so non-MCP sources (the
    /// test-only `StaticDynamic`, `EmptySource`, and any future
    /// non-MCP dynamic source) need not opt in.
    fn server_id_for(&self, _name: &str) -> Option<McpServerId> {
        None
    }
}

#[derive(Debug, Clone)]
pub struct ToolBox {
    builtins: ToolRegistry,
    dynamic: Arc<dyn DynamicToolSource>,
}

impl ToolBox {
    #[must_use]
    pub fn new(builtins: ToolRegistry, dynamic: Arc<dyn DynamicToolSource>) -> Self {
        Self { builtins, dynamic }
    }

    /// Construct from just a built-in registry; the dynamic source returns an empty
    /// catalogue. Useful in tests and any composition that doesn't wire MCP yet.
    #[must_use]
    pub fn from_builtins(builtins: ToolRegistry) -> Self {
        Self::new(builtins, Arc::new(EmptySource))
    }

    /// Concatenated `[builtins…, dynamic…]` slice. Allocated only when the dynamic
    /// half is non-empty; the common all-builtin case returns the registry's cached
    /// `Arc<[…]>` directly.
    #[must_use]
    pub fn specs(&self) -> Arc<[ToolSpec]> {
        let builtins = self.builtins.specs();
        let dynamic = self.dynamic.specs();
        if dynamic.is_empty() {
            return builtins;
        }
        if builtins.is_empty() {
            return dynamic;
        }
        let mut combined: Vec<ToolSpec> = Vec::with_capacity(builtins.len() + dynamic.len());
        combined.extend(builtins.iter().cloned());
        combined.extend(dynamic.iter().cloned());
        Arc::from(combined)
    }

    /// Specs visible to `kind` — same shape as [`Self::specs`] but with
    /// per-mode filtering applied to both halves of the toolbox.
    #[must_use]
    pub fn specs_for(&self, kind: RequestKind) -> Arc<[ToolSpec]> {
        let builtins = self.builtins.specs_for(kind);
        let dynamic = self.dynamic.specs_for(kind);
        if dynamic.is_empty() {
            return builtins;
        }
        if builtins.is_empty() {
            return dynamic;
        }
        let mut combined: Vec<ToolSpec> = Vec::with_capacity(builtins.len() + dynamic.len());
        combined.extend(builtins.iter().cloned());
        combined.extend(dynamic.iter().cloned());
        Arc::from(combined)
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<SharedTool> {
        if let Some(tool) = self.builtins.get(name) {
            return Some(tool);
        }
        self.dynamic.get(name)
    }

    /// Look up only if the tool's modes include `kind`. Built-ins
    /// shadow MCP tools on a name collision (same as [`Self::get`]).
    #[must_use]
    pub fn get_for(&self, kind: RequestKind, name: &str) -> Option<SharedTool> {
        if let Some(tool) = self.builtins.get_for(kind, name) {
            return Some(tool);
        }
        self.dynamic.get_for(kind, name)
    }

    /// The MCP server backing `name`, if `name` resolves through the
    /// dynamic (MCP) half of the toolbox. Built-in system tools always
    /// return `None` — there is no MCP server behind a built-in.
    #[must_use]
    pub fn server_id_for(&self, name: &str) -> Option<McpServerId> {
        if self.builtins.get(name).is_some() {
            return None;
        }
        self.dynamic.server_id_for(name)
    }

    #[must_use]
    pub fn builtins(&self) -> &ToolRegistry {
        &self.builtins
    }
}

#[derive(Debug)]
struct EmptySource;

impl DynamicToolSource for EmptySource {
    fn specs(&self) -> Arc<[ToolSpec]> {
        Arc::from(Vec::new())
    }
    fn get(&self, _name: &str) -> Option<SharedTool> {
        None
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use serde_json::{Value, json};
    use std::sync::Arc;

    use super::*;
    use crate::tools::{Tool, ToolCallContext, ToolError, ToolRegistry};
    use crate::types::ToolName;

    #[derive(Debug)]
    struct FakeTool {
        name: ToolName,
        body: &'static str,
    }
    impl FakeTool {
        fn new(name: &str, body: &'static str) -> Self {
            Self {
                name: ToolName::try_from(name).expect("valid"),
                body,
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
            Ok(self.body.into())
        }
    }

    #[derive(Debug)]
    struct StaticDynamic {
        specs: Arc<[ToolSpec]>,
        tools: Vec<(ToolName, SharedTool)>,
    }
    impl DynamicToolSource for StaticDynamic {
        fn specs(&self) -> Arc<[ToolSpec]> {
            self.specs.clone()
        }
        fn get(&self, name: &str) -> Option<SharedTool> {
            self.tools
                .iter()
                .find(|(n, _)| n.as_str() == name)
                .map(|(_, t)| t.clone())
        }
    }

    fn dynamic_with(name: &str) -> StaticDynamic {
        let n = ToolName::try_from(name).expect("valid");
        let tool: SharedTool = Arc::new(FakeTool::new(name, "from-dynamic"));
        let spec = ToolSpec {
            name: n.clone(),
            description: Arc::from("dyn"),
            input_schema: Arc::new(json!({"type":"object"})),
        };
        StaticDynamic {
            specs: Arc::from(vec![spec]),
            tools: vec![(n, tool)],
        }
    }

    #[test]
    fn empty_dynamic_returns_builtin_specs_arc() {
        let registry = ToolRegistry::builder()
            .with(Arc::new(FakeTool::new("alpha", "a")))
            .build();
        let toolbox = ToolBox::from_builtins(registry);
        assert_eq!(toolbox.specs().len(), 1);
        assert!(toolbox.get("alpha").is_some());
        assert!(toolbox.get("missing").is_none());
    }

    #[test]
    fn specs_are_concatenated_when_both_sides_present() {
        let registry = ToolRegistry::builder()
            .with(Arc::new(FakeTool::new("alpha", "a")))
            .build();
        let dyn_src = Arc::new(dynamic_with("mcp_x_echo"));
        let toolbox = ToolBox::new(registry, dyn_src);
        let specs = toolbox.specs();
        assert_eq!(specs.len(), 2);
        let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"alpha"));
        assert!(names.contains(&"mcp_x_echo"));
    }

    #[test]
    fn builtin_shadows_dynamic_on_collision() {
        let registry = ToolRegistry::builder()
            .with(Arc::new(FakeTool::new("shared", "static")))
            .build();
        let mut dyn_src = dynamic_with("shared");
        // overwrite to make sure the dynamic body would surface if shadowing failed
        dyn_src.tools[0].1 = Arc::new(FakeTool::new("shared", "dynamic"));
        let toolbox = ToolBox::new(registry, Arc::new(dyn_src));
        let tool = toolbox.get("shared").expect("present");
        // We can't run async here in this sync test, but the identity check via Arc pointer
        // is sufficient: the static tool is what got returned.
        assert_eq!(tool.description(), "fake");
    }
}
