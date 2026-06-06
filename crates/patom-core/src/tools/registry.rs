use std::collections::HashMap;
use std::sync::Arc;

use crate::provider::ToolSpec;
use crate::runtime::RequestKind;
use crate::types::ToolName;

use super::modes::RequestKindModes;
use super::traits::SharedTool;

/// Registry built once at startup, then frozen.
///
/// Stores each tool together with its [`RequestKindModes`] so per-turn
/// dispatch can filter by the active [`RequestKind`] without consulting
/// the tool again. Specs are cached as a single `Arc<[ToolSpec]>` slice
/// for the all-modes case (the common path); per-kind filters allocate
/// only when a tool's `modes()` actually excludes a kind.
#[derive(Clone, Debug)]
pub struct ToolRegistry {
    by_name: Arc<HashMap<ToolName, ToolEntry>>,
    /// All tools regardless of mode. Used by callers that don't care
    /// about mode filtering (e.g. operator audit endpoints).
    specs: Arc<[ToolSpec]>,
}

#[derive(Clone, Debug)]
struct ToolEntry {
    tool: SharedTool,
    spec: ToolSpec,
    modes: RequestKindModes,
}

impl ToolRegistry {
    #[must_use]
    pub fn empty() -> Self {
        Self {
            by_name: Arc::new(HashMap::new()),
            specs: Arc::from(Vec::new()),
        }
    }

    #[must_use]
    pub fn builder() -> ToolRegistryBuilder {
        ToolRegistryBuilder::default()
    }

    /// Look up regardless of mode. Most call sites should prefer
    /// [`Self::get_for`] so a tool whose `modes()` excludes the active
    /// kind is rejected at dispatch time.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<SharedTool> {
        self.by_name.get(name).map(|e| e.tool.clone())
    }

    /// Look up only if the tool's `modes()` includes `kind`. Returns
    /// `None` for both "not registered" and "registered but excluded
    /// from this mode" — the dispatcher treats those identically (the
    /// model issued an unknown-or-unauthorised tool name).
    #[must_use]
    pub fn get_for(&self, kind: RequestKind, name: &str) -> Option<SharedTool> {
        self.by_name
            .get(name)
            .filter(|e| e.modes.includes(kind))
            .map(|e| e.tool.clone())
    }

    /// All specs regardless of mode.
    #[must_use]
    pub fn specs(&self) -> Arc<[ToolSpec]> {
        self.specs.clone()
    }

    /// Specs visible to `kind`. When every tool participates in `kind`
    /// (the all-`ALL`-modes default case), returns the cached slice
    /// without allocating.
    #[must_use]
    pub fn specs_for(&self, kind: RequestKind) -> Arc<[ToolSpec]> {
        let any_filtered = self.by_name.values().any(|e| !e.modes.includes(kind));
        if !any_filtered {
            return self.specs.clone();
        }
        let mut filtered: Vec<ToolSpec> = self
            .by_name
            .values()
            .filter(|e| e.modes.includes(kind))
            .map(|e| e.spec.clone())
            .collect();
        filtered.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
        Arc::from(filtered)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }
}

#[derive(Default, Debug)]
pub struct ToolRegistryBuilder {
    tools: Vec<SharedTool>,
}

impl ToolRegistryBuilder {
    pub fn register(&mut self, tool: SharedTool) -> &mut Self {
        self.tools.push(tool);
        self
    }

    #[must_use]
    pub fn with(mut self, tool: SharedTool) -> Self {
        self.register(tool);
        self
    }

    #[must_use]
    pub fn build(self) -> ToolRegistry {
        let mut by_name: HashMap<ToolName, ToolEntry> = HashMap::with_capacity(self.tools.len());
        let mut specs: Vec<ToolSpec> = Vec::with_capacity(self.tools.len());
        for tool in self.tools {
            let name = tool.name().clone();
            let spec = ToolSpec {
                name: name.clone(),
                description: Arc::from(tool.description()),
                input_schema: tool.input_schema(),
            };
            specs.push(spec.clone());
            let modes = tool.modes();
            let entry = ToolEntry { tool, spec, modes };
            let prev = by_name.insert(name.clone(), entry);
            // §6: registering two tools under the same name is a programmer error
            // (a typo at composition root). Crash now rather than silently shadowing.
            assert!(
                prev.is_none(),
                "duplicate tool registration: {}",
                name.as_str()
            );
        }
        specs.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
        ToolRegistry {
            by_name: Arc::new(by_name),
            specs: Arc::from(specs),
        }
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use serde_json::{Value, json};

    use super::*;
    use crate::tools::traits::{Tool, ToolCallContext, ToolError};

    #[derive(Debug)]
    struct FakeTool {
        name: ToolName,
        schema: Arc<Value>,
        modes: RequestKindModes,
    }

    impl FakeTool {
        fn new(n: &str) -> Self {
            Self {
                name: ToolName::try_from(n).expect("valid name"),
                schema: Arc::new(json!({"type": "object"})),
                modes: RequestKindModes::ALL,
            }
        }

        fn with_modes(mut self, modes: RequestKindModes) -> Self {
            self.modes = modes;
            self
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
            self.schema.clone()
        }
        async fn execute(
            &self,
            _input: Value,
            _ctx: &ToolCallContext,
        ) -> Result<String, ToolError> {
            Ok("ok".into())
        }
        fn modes(&self) -> RequestKindModes {
            self.modes
        }
    }

    #[test]
    fn build_sorts_specs_and_caches_lookup() {
        let registry = ToolRegistry::builder()
            .with(Arc::new(FakeTool::new("zeta")))
            .with(Arc::new(FakeTool::new("alpha")))
            .build();
        let specs = registry.specs();
        assert_eq!(specs[0].name.as_str(), "alpha");
        assert_eq!(specs[1].name.as_str(), "zeta");
        assert!(registry.get("alpha").is_some());
    }

    #[test]
    fn specs_for_excludes_tools_outside_kind() {
        let registry = ToolRegistry::builder()
            .with(Arc::new(
                FakeTool::new("normal_only").with_modes(RequestKindModes::NORMAL),
            ))
            .with(Arc::new(FakeTool::new("everywhere")))
            .build();
        let normal_specs = registry.specs_for(RequestKind::Normal);
        let reflection_specs = registry.specs_for(RequestKind::Reflection);
        assert_eq!(normal_specs.len(), 2);
        assert_eq!(reflection_specs.len(), 1);
        assert_eq!(reflection_specs[0].name.as_str(), "everywhere");
    }

    #[test]
    fn get_for_returns_none_when_excluded() {
        let registry = ToolRegistry::builder()
            .with(Arc::new(
                FakeTool::new("normal_only").with_modes(RequestKindModes::NORMAL),
            ))
            .build();
        assert!(
            registry
                .get_for(RequestKind::Normal, "normal_only")
                .is_some()
        );
        assert!(
            registry
                .get_for(RequestKind::Reflection, "normal_only")
                .is_none()
        );
    }

    #[test]
    #[should_panic(expected = "duplicate tool registration")]
    fn duplicate_registration_panics() {
        let _ = ToolRegistry::builder()
            .with(Arc::new(FakeTool::new("dup")))
            .with(Arc::new(FakeTool::new("dup")))
            .build();
    }
}
