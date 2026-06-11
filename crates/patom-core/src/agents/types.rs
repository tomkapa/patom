//! Domain types for the agents subsystem.
//!
//! CLAUDE.md §1: every value carrying an invariant gets a newtype with a `TryFrom`
//! smart constructor. The HTTP boundary parses raw JSON into these types once;
//! nothing downstream constructs them directly.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::auth::OrgId;
use crate::mcp::{McpCatalogId, McpToolRemoteName};
use crate::types::{AvatarUrl, ParseError};

use super::limits::{
    AGENT_DESCRIPTION_MAX_LEN, AGENT_NAME_MAX_LEN, AGENT_SYSTEM_PROMPT_MAX_LEN,
    MAX_ALLOWED_MCP_CATALOGS_PER_AGENT, MAX_ALLOWED_MCP_TOOLS_PER_CATALOG_PER_AGENT,
};

crate::uuid_newtype! {
    /// Opaque identifier for a registered agent row. Wire format and DB column both
    /// use `agent_id`; this is the typed handle.
    pub AgentId
}

/// Role-shaped agent name (doc/agent_discovery_plan.md §6).
///
/// Globally unique on `lower(name)`; the model addresses peers by this name
/// in `send_message` and `search_agents`, and the renderer surfaces it in
/// the `<colleagues>` block and in `<memory>` Collaborator entries. The wire
/// label is preserved as-is; case-insensitivity is enforced by the
/// `agents_name_lower_unique` index.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct AgentName(Arc<str>);

impl AgentName {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<&str> for AgentName {
    type Error = ParseError;

    fn try_from(raw: &str) -> Result<Self, Self::Error> {
        if raw.is_empty() {
            return Err(ParseError::Empty {
                field: "agent_name",
            });
        }
        if raw.len() > AGENT_NAME_MAX_LEN {
            return Err(ParseError::TooLong {
                field: "agent_name",
                max: AGENT_NAME_MAX_LEN,
                got: raw.len(),
            });
        }
        Ok(Self(Arc::from(raw)))
    }
}

impl TryFrom<String> for AgentName {
    type Error = ParseError;
    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::try_from(raw.as_str())
    }
}

impl fmt::Debug for AgentName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("AgentName").field(&&*self.0).finish()
    }
}

impl fmt::Display for AgentName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for AgentName {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for AgentName {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::try_from(raw).map_err(serde::de::Error::custom)
    }
}

/// Validated, non-empty role-specific system prompt. Reference-counted so the
/// memory layer can hand the same allocation to the provider without copying.
#[derive(Clone, PartialEq, Eq)]
pub struct AgentSystemPrompt(Arc<str>);

impl AgentSystemPrompt {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn into_arc(self) -> Arc<str> {
        self.0
    }
}

impl TryFrom<&str> for AgentSystemPrompt {
    type Error = ParseError;

    fn try_from(raw: &str) -> Result<Self, Self::Error> {
        if raw.trim().is_empty() {
            return Err(ParseError::Empty {
                field: "agent_system_prompt",
            });
        }
        if raw.len() > AGENT_SYSTEM_PROMPT_MAX_LEN {
            return Err(ParseError::TooLong {
                field: "agent_system_prompt",
                max: AGENT_SYSTEM_PROMPT_MAX_LEN,
                got: raw.len(),
            });
        }
        Ok(Self(Arc::from(raw)))
    }
}

impl TryFrom<String> for AgentSystemPrompt {
    type Error = ParseError;
    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::try_from(raw.as_str())
    }
}

impl fmt::Debug for AgentSystemPrompt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Length-only Debug — full prompts are large and pollute logs.
        f.debug_tuple("AgentSystemPrompt")
            .field(&self.0.len())
            .finish()
    }
}

/// Operator-curated, model-facing one-sentence blurb describing what the
/// agent is for (doc/agent_discovery_plan.md §5).
///
/// Required, non-empty. Distinct from [`AgentSystemPrompt`]: this is for
/// *being found* (embedded for `search_agents`); the system prompt is for
/// *being the agent*. The two surfaces evolve for different reasons —
/// description is a clean positive statement of role; the system prompt
/// can carry negations, examples, style guidance that hurt embedding
/// quality.
#[derive(Clone, PartialEq, Eq)]
pub struct AgentDescription(Arc<str>);

impl AgentDescription {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn into_arc(self) -> Arc<str> {
        self.0
    }
}

impl TryFrom<&str> for AgentDescription {
    type Error = ParseError;

    fn try_from(raw: &str) -> Result<Self, Self::Error> {
        if raw.trim().is_empty() {
            return Err(ParseError::Empty {
                field: "agent_description",
            });
        }
        if raw.len() > AGENT_DESCRIPTION_MAX_LEN {
            return Err(ParseError::TooLong {
                field: "agent_description",
                max: AGENT_DESCRIPTION_MAX_LEN,
                got: raw.len(),
            });
        }
        Ok(Self(Arc::from(raw)))
    }
}

impl TryFrom<String> for AgentDescription {
    type Error = ParseError;
    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::try_from(raw.as_str())
    }
}

impl fmt::Debug for AgentDescription {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("AgentDescription").field(&&*self.0).finish()
    }
}

impl fmt::Display for AgentDescription {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for AgentDescription {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for AgentDescription {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::try_from(raw).map_err(serde::de::Error::custom)
    }
}

/// Slim `(id, name, description)` projection for the `search_agents` tool.
///
/// Distinct from [`AgentRecord`] so similarity search does not pay the
/// round-trip / decode cost for `system_prompt` (up to 64 KiB), the MCP
/// allowlist, and the timestamp columns.
#[derive(Debug, Clone)]
pub struct AgentCard {
    pub id: AgentId,
    pub name: AgentName,
    pub description: AgentDescription,
}

/// Snapshot of a single row in the `agents` table.
///
/// `allowed_mcp_tools` is the per-agent MCP allowlist with per-catalog tool
/// granularity: every catalog id present grants the agent visibility to the
/// tenant's wired connection for that catalog, with the value (`None` = all
/// tools, `Some(set)` = only these remote tool names) narrowing what
/// surfaces. Strict semantics: an absent catalog id means **zero** tools
/// for that integration, not "all of them". The recruiter must explicitly
/// opt an agent in to each catalog.
#[derive(Debug, Clone)]
pub struct AgentRecord {
    pub id: AgentId,
    /// Owning organisation. Set at insert time from the request principal
    /// (HTTP create) or the calling agent's org (tool-driven create);
    /// required because `agents.org_id` is `NOT NULL`.
    pub org_id: OrgId,
    pub name: AgentName,
    /// Resolved by joining `agent_prompt_versions` at read time on the
    /// agent's MAX(version) — the dual storage is gone (migration 45).
    /// "Current" is structural, not a stored pointer: restore is
    /// append-only (`max + 1`), so `MAX(version) WHERE agent_id = X` IS
    /// the current row by construction.
    pub system_prompt: AgentSystemPrompt,
    pub description: AgentDescription,
    pub allowed_mcp_tools: AllowedMcpTools,
    /// Per-agent LLM model selection. `None` means "use the workspace default"
    /// (`Settings::model`); the [`crate::agents::ModelResolver`] is the single
    /// chokepoint that turns `Option<Model>` into the effective `Model` at
    /// agent-build time. Resolved through the same JOIN as `system_prompt`.
    pub model: Option<crate::provider::Model>,
    /// Optional per-agent avatar image URL. `None` → Slack falls back to
    /// the app's default bot avatar and the FE renders the name monogram;
    /// `Some(url)` is the public assets-origin URL set via
    /// `/api/uploads/agent-avatar/{agent_id}` and passed through as the
    /// Slack `icon_url` on outbound agent posts (issue #43). Reuses the
    /// shared [`AvatarUrl`] newtype — same validation as user/org avatars.
    pub avatar_url: Option<AvatarUrl>,
    /// Id of the current (= MAX(version)) prompt-version row. Surfaced
    /// here so the turn-metrics writer can attribute each turn to the
    /// version the worker actually ran without re-querying. Derived from
    /// the same join that populates `system_prompt` and `model`.
    pub current_prompt_version_id: super::prompt_versions::PromptVersionId,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Per-catalog tool-allowlist view used by the runtime filter to decide
/// whether a single tool surfaces to the agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolScope<'a> {
    /// Catalog is not in the allowlist — every tool the wired connection
    /// for it exposes is hidden.
    None,
    /// Catalog is allowed and every tool the wired connection exposes is
    /// exposed.
    All,
    /// Catalog is allowed; only the listed remote names are exposed. May
    /// be empty, which is a valid "catalog present, lockdown all its
    /// tools" state — distinct from [`ToolScope::None`] only in that the
    /// recruiter has explicitly opted in but listed zero tools.
    Some(&'a BTreeSet<McpToolRemoteName>),
}

/// Per-agent MCP allowlist with per-catalog tool granularity.
///
/// Storage: `BTreeMap<McpCatalogId, Option<BTreeSet<McpToolRemoteName>>>`.
/// `None` value = "all tools from this catalog's wired connection";
/// `Some(set)` = "only these remote names." An absent catalog id = no
/// access to that integration (strict).
///
/// Resolution from catalog id to the tenant's wired `McpServerId` happens
/// at session-build time in [`crate::mcp::ScopedMcpSource::new`]; catalog
/// ids that have no wired connection in the org contribute zero tools
/// (silent) and the recruiter is expected to have asked the user to wire
/// first via `request_user_wire_mcp`.
///
/// The newtype enforces both caps on every construction path (HTTP, store
/// reload, factory wiring):
/// - at most [`MAX_ALLOWED_MCP_CATALOGS_PER_AGENT`] keys
/// - at most [`MAX_ALLOWED_MCP_TOOLS_PER_CATALOG_PER_AGENT`] entries per
///   value list (and each remote name is itself bounded via
///   `McpToolRemoteName::try_from`).
///
/// Empty (`{}`) is a legitimate value — the "no MCP tools" lockdown — and
/// is the default for a freshly minted agent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AllowedMcpTools(BTreeMap<McpCatalogId, Option<BTreeSet<McpToolRemoteName>>>);

impl AllowedMcpTools {
    #[must_use]
    pub fn empty() -> Self {
        Self(BTreeMap::new())
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Number of distinct catalog ids in the allowlist.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Iterator over `(catalog_id, ToolScope)` for the runtime filter.
    pub fn iter(&self) -> impl Iterator<Item = (&McpCatalogId, ToolScope<'_>)> {
        self.0.iter().map(|(id, v)| {
            let scope = v.as_ref().map_or(ToolScope::All, ToolScope::Some);
            (id, scope)
        })
    }

    /// Look up the scope for `catalog`. Returns [`ToolScope::None`] for a
    /// catalog that's absent from the allowlist.
    #[must_use]
    pub fn tools_for_catalog(&self, catalog: &McpCatalogId) -> ToolScope<'_> {
        match self.0.get(catalog) {
            None => ToolScope::None,
            Some(None) => ToolScope::All,
            Some(Some(set)) => ToolScope::Some(set),
        }
    }

    /// True iff this allowlist mentions `catalog` at all (regardless of
    /// whether the tool subset is `None` or `Some`).
    #[must_use]
    pub fn contains_catalog(&self, catalog: &McpCatalogId) -> bool {
        self.0.contains_key(catalog)
    }
}

impl Serialize for AllowedMcpTools {
    // Delegates to the inner `BTreeMap`'s own `Serialize`. `Option`,
    // `BTreeSet`, and `McpToolRemoteName` / `McpCatalogId` all already
    // implement `Serialize`, so this emits the wire-shaped JSONB without
    // copying any `Arc<str>` into a `String`.
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for AllowedMcpTools {
    // Parses the wire-shaped `BTreeMap<String, Option<Vec<String>>>` and
    // funnels through `TryFrom` so the caps + per-name + uniqueness +
    // catalog-id-regex checks fire on every boundary cross (HTTP, sqlx
    // JSONB, tool input). Boundary error → serde error → HTTP 400 /
    // store backend error, same as every other newtype in this crate.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = <BTreeMap<String, Option<Vec<String>>>>::deserialize(deserializer)?;
        Self::try_from(raw).map_err(serde::de::Error::custom)
    }
}

impl TryFrom<BTreeMap<String, Option<Vec<String>>>> for AllowedMcpTools {
    type Error = ParseError;

    fn try_from(raw: BTreeMap<String, Option<Vec<String>>>) -> Result<Self, Self::Error> {
        if raw.len() > MAX_ALLOWED_MCP_CATALOGS_PER_AGENT {
            return Err(ParseError::OutOfRange {
                field: "allowed_mcp_tools",
                detail: "too many catalogs",
            });
        }
        let mut out: BTreeMap<McpCatalogId, Option<BTreeSet<McpToolRemoteName>>> = BTreeMap::new();
        for (raw_id, names) in raw {
            let catalog = McpCatalogId::try_from(raw_id)?;
            let scope = match names {
                None => None,
                Some(list) => {
                    if list.len() > MAX_ALLOWED_MCP_TOOLS_PER_CATALOG_PER_AGENT {
                        return Err(ParseError::OutOfRange {
                            field: "allowed_mcp_tools",
                            detail: "too many tools for one catalog",
                        });
                    }
                    let mut set: BTreeSet<McpToolRemoteName> = BTreeSet::new();
                    for raw_name in list {
                        let name = McpToolRemoteName::try_from(raw_name)?;
                        if !set.insert(name) {
                            return Err(ParseError::Malformed {
                                field: "allowed_mcp_tools",
                                detail: "duplicate tool name in catalog list",
                            });
                        }
                    }
                    Some(set)
                }
            };
            if out.insert(catalog, scope).is_some() {
                // BTreeMap<String, _> already dedup'd by string key; an
                // insert collision here would mean two catalog ids
                // serialised to the same id after parse — impossible.
                return Err(ParseError::Malformed {
                    field: "allowed_mcp_tools",
                    detail: "duplicate catalog id",
                });
            }
        }
        Ok(Self(out))
    }
}

/// Seed payload used by the init function to insert the default agent row when
/// none exists. Every field is a pre-validated newtype so the inserter cannot
/// land malformed data.
#[derive(Debug, Clone)]
pub struct AgentSeed {
    pub name: AgentName,
    pub system_prompt: AgentSystemPrompt,
    pub description: AgentDescription,
    /// Optional default avatar for the seeded agent. `Some` when the asset
    /// CDN origin is configured (the recruiter takes `agent-1.png`); `None`
    /// in deployments without object storage, where the FE renders the name
    /// monogram. Built via [`crate::agents::preset_agent_avatar_url`].
    pub avatar_url: Option<AvatarUrl>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_name_rejects_empty_and_oversize() {
        assert!(AgentName::try_from("").is_err());
        let big = "a".repeat(AGENT_NAME_MAX_LEN + 1);
        assert!(AgentName::try_from(big.as_str()).is_err());
    }

    #[test]
    fn agent_name_accepts_normal() {
        let n = AgentName::try_from("assistant").expect("valid");
        assert_eq!(n.as_str(), "assistant");
    }

    #[test]
    fn agent_system_prompt_rejects_empty_and_whitespace() {
        assert!(AgentSystemPrompt::try_from("").is_err());
        assert!(AgentSystemPrompt::try_from("   \n\t").is_err());
    }

    #[test]
    fn agent_system_prompt_rejects_oversize() {
        let big = "a".repeat(AGENT_SYSTEM_PROMPT_MAX_LEN + 1);
        assert!(AgentSystemPrompt::try_from(big.as_str()).is_err());
    }

    #[test]
    fn agent_system_prompt_accepts_normal() {
        let p = AgentSystemPrompt::try_from("be helpful").expect("valid");
        assert_eq!(p.as_str(), "be helpful");
    }

    #[test]
    fn allowed_mcp_tools_default_is_empty() {
        let a = AllowedMcpTools::default();
        assert!(a.is_empty());
        assert_eq!(a.len(), 0);
    }

    fn cat(id: &str) -> McpCatalogId {
        McpCatalogId::try_from(id).expect("valid catalog id")
    }

    #[test]
    fn allowed_mcp_tools_rejects_too_many_catalogs() {
        let mut raw: BTreeMap<String, Option<Vec<String>>> = BTreeMap::new();
        for i in 0..=MAX_ALLOWED_MCP_CATALOGS_PER_AGENT {
            raw.insert(format!("c{i}"), None);
        }
        let err = AllowedMcpTools::try_from(raw).expect_err("over catalog cap");
        assert!(matches!(
            err,
            ParseError::OutOfRange {
                field: "allowed_mcp_tools",
                detail: "too many catalogs",
            }
        ));
    }

    #[test]
    fn allowed_mcp_tools_rejects_too_many_tools_per_catalog() {
        let mut list: Vec<String> = Vec::new();
        for i in 0..=MAX_ALLOWED_MCP_TOOLS_PER_CATALOG_PER_AGENT {
            list.push(format!("t{i}"));
        }
        let mut raw: BTreeMap<String, Option<Vec<String>>> = BTreeMap::new();
        raw.insert("notion".into(), Some(list));
        let err = AllowedMcpTools::try_from(raw).expect_err("over tools cap");
        assert!(matches!(
            err,
            ParseError::OutOfRange {
                field: "allowed_mcp_tools",
                detail: "too many tools for one catalog",
            }
        ));
    }

    #[test]
    fn allowed_mcp_tools_rejects_empty_tool_name() {
        let mut raw: BTreeMap<String, Option<Vec<String>>> = BTreeMap::new();
        raw.insert("notion".into(), Some(vec![String::new()]));
        let err = AllowedMcpTools::try_from(raw).expect_err("empty name");
        assert!(matches!(err, ParseError::Empty { .. }));
    }

    #[test]
    fn allowed_mcp_tools_rejects_malformed_catalog_id() {
        // Uppercase + spaces + leading digit all fail McpCatalogId::try_from.
        for bad in ["NOTION", "no tion", "9notion", ""] {
            let mut raw: BTreeMap<String, Option<Vec<String>>> = BTreeMap::new();
            raw.insert(bad.into(), None);
            let err = AllowedMcpTools::try_from(raw).expect_err("bad id");
            assert!(matches!(
                err,
                ParseError::Empty { .. }
                    | ParseError::Malformed { .. }
                    | ParseError::TooLong { .. }
            ));
        }
    }

    #[test]
    fn allowed_mcp_tools_rejects_duplicate_tool_in_one_catalog_list() {
        let mut raw: BTreeMap<String, Option<Vec<String>>> = BTreeMap::new();
        raw.insert("notion".into(), Some(vec!["a".into(), "a".into()]));
        let err = AllowedMcpTools::try_from(raw).expect_err("dup");
        assert!(matches!(
            err,
            ParseError::Malformed {
                field: "allowed_mcp_tools",
                ..
            }
        ));
    }

    #[test]
    fn allowed_mcp_tools_distinguishes_all_versus_some_empty() {
        let all = cat("notion");
        let some_empty = cat("linear");
        let unknown = cat("slack");
        let mut raw: BTreeMap<String, Option<Vec<String>>> = BTreeMap::new();
        raw.insert("notion".into(), None);
        raw.insert("linear".into(), Some(Vec::new()));
        let allowed = AllowedMcpTools::try_from(raw).expect("valid");
        assert!(matches!(allowed.tools_for_catalog(&all), ToolScope::All));
        let empty_set = match allowed.tools_for_catalog(&some_empty) {
            ToolScope::Some(set) => set,
            other => panic!("expected Some(empty), got {other:?}"),
        };
        assert!(empty_set.is_empty());
        assert!(matches!(
            allowed.tools_for_catalog(&unknown),
            ToolScope::None
        ));
    }

    #[test]
    fn allowed_mcp_tools_accepts_at_caps() {
        let mut raw: BTreeMap<String, Option<Vec<String>>> = BTreeMap::new();
        for i in 0..MAX_ALLOWED_MCP_CATALOGS_PER_AGENT {
            // Catalog ids must match `^[a-z][a-z0-9_-]{0,39}$` — synthesise
            // unique ones by appending a digit suffix.
            let id = format!("cat{i:02}");
            let tools: Vec<String> = (0..MAX_ALLOWED_MCP_TOOLS_PER_CATALOG_PER_AGENT)
                .map(|t| format!("s{i}_t{t}"))
                .collect();
            raw.insert(id, Some(tools));
        }
        let allowed = AllowedMcpTools::try_from(raw).expect("at caps");
        assert_eq!(allowed.len(), MAX_ALLOWED_MCP_CATALOGS_PER_AGENT);
    }
}
