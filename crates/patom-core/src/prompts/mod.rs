//! Per-language prompt registry.
//!
//! The composition root calls [`Prompts::load`] once at startup. It parses
//! three TOML files baked into the binary via `include_str!`:
//!
//! - `internal.toml` — single-language scaffolding the user never sees
//!   (`<core>`, `<reflection>`, `<resolution>`, `<scheduling>` bodies).
//! - `en.toml`, `vi.toml` — per-language user-visible bodies (the
//!   `<language>` directive body, the default `recruiter` agent's
//!   `system_prompt` + `description`).
//!
//! Adding a new language is a sibling `xx.toml` + one match arm in
//! [`Prompts::set`]; no callers change.
//!
//! Splitting "internal" from per-language files keeps translation churn
//! out of the prompt-mechanics bodies that operators do not see. If we
//! later choose to translate `<core>` etc., they migrate into the
//! per-language files and the internal file shrinks.

use std::sync::Arc;

use serde::Deserialize;

use crate::auth::Language;
use crate::runtime::RequestKind;

const INTERNAL_TOML: &str = include_str!("internal.toml");
const EN_TOML: &str = include_str!("en.toml");
const VI_TOML: &str = include_str!("vi.toml");

/// Internal `<core>`-family prompts; single language for now.
#[derive(Debug, Clone)]
pub struct CorePrompts {
    /// `core` + `\n\n` + `scheduling_supplement`, pre-joined so the
    /// `<core>` body for a `Normal` turn is one `Arc<str>` clone away.
    pub normal: Arc<str>,
    pub reflection: Arc<str>,
    pub resolution: Arc<str>,
}

impl CorePrompts {
    /// Pick the core body for a request kind. Exhaustive `match` so a
    /// new [`RequestKind`] variant lights up here at compile time.
    #[must_use]
    pub fn for_kind(&self, kind: RequestKind) -> Arc<str> {
        match kind {
            RequestKind::Normal => self.normal.clone(),
            RequestKind::Reflection => self.reflection.clone(),
            RequestKind::Resolution => self.resolution.clone(),
        }
    }
}

/// Per-language user-visible prompt bodies.
#[derive(Debug, Clone)]
pub struct PromptSet {
    /// Body of the `<language>` tag appended to every assembled system
    /// prompt. Tells the model which language to respond in.
    pub language_directive: Arc<str>,
    /// Seeded `system_prompt` for the per-org default `recruiter`. Owned
    /// by the DB after first insert.
    pub default_agent_role: Arc<str>,
    /// Seeded operator-facing description for the per-org default
    /// `recruiter`. Owned by the DB after first insert.
    pub default_agent_description: Arc<str>,
}

/// Process-wide registry. Cheap to clone (everything inside is
/// `Arc<str>`); wrap in `Arc<Prompts>` at the composition root and share.
#[derive(Debug, Clone)]
pub struct Prompts {
    /// Internal scaffolding (not user-visible, single language).
    pub cores: CorePrompts,
    en: PromptSet,
    vi: PromptSet,
}

impl Prompts {
    /// Parse the baked-in TOML and assemble the registry. Panics on
    /// malformed TOML or missing keys — CLAUDE.md §6: this runs once at
    /// startup; the only correct response to a malformed prompt file is
    /// to refuse to boot.
    #[must_use]
    pub fn load() -> Self {
        let internal: InternalToml = toml::from_str(INTERNAL_TOML)
            .expect("invariant: src/prompts/internal.toml must parse as the InternalToml shape");
        let en: PromptSetToml = toml::from_str(EN_TOML)
            .expect("invariant: src/prompts/en.toml must parse as the PromptSetToml shape");
        let vi: PromptSetToml = toml::from_str(VI_TOML)
            .expect("invariant: src/prompts/vi.toml must parse as the PromptSetToml shape");

        let normal = format!("{}\n\n{}", internal.core, internal.scheduling_supplement);
        let cores = CorePrompts {
            normal: Arc::from(normal),
            reflection: Arc::from(internal.reflection),
            resolution: Arc::from(internal.resolution),
        };

        Self {
            cores,
            en: en.into_set(),
            vi: vi.into_set(),
        }
    }

    /// Pick the per-language [`PromptSet`] for a language. Exhaustive
    /// `match` so adding a [`Language`] variant lights up here at compile
    /// time.
    #[must_use]
    pub fn set(&self, lang: Language) -> &PromptSet {
        match lang {
            Language::En => &self.en,
            Language::Vi => &self.vi,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct InternalToml {
    core: String,
    scheduling_supplement: String,
    reflection: String,
    resolution: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PromptSetToml {
    language_directive: String,
    default_agent_role: String,
    default_agent_description: String,
}

impl PromptSetToml {
    fn into_set(self) -> PromptSet {
        PromptSet {
            language_directive: Arc::from(self.language_directive),
            default_agent_role: Arc::from(self.default_agent_role),
            default_agent_description: Arc::from(self.default_agent_description),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_without_panic() {
        // §6: a panic here means a TOML file was malformed or missing a
        // required key — the boot path must never reach `load` in that
        // state. Bare assertion suffices; the test name is the contract.
        let _ = Prompts::load();
    }

    #[test]
    fn cores_join_with_blank_line_between() {
        let prompts = Prompts::load();
        // The `<core>` envelope must show identity/communication/chain_of_command
        // and the scheduling supplement separated by a blank line.
        let normal = prompts.cores.for_kind(RequestKind::Normal);
        assert!(
            normal.contains("</chain_of_command>\n\n<scheduling>"),
            "scheduling supplement should be joined onto core with a blank-line separator; got: {normal:?}",
        );
    }

    #[test]
    fn language_directive_differs_per_language() {
        let prompts = Prompts::load();
        let en = prompts.set(Language::En).language_directive.as_ref();
        let vi = prompts.set(Language::Vi).language_directive.as_ref();
        assert_ne!(en, vi);
        // §6: assert positive identifiers, not just inequality, so an
        // accidental swap of file contents fails loudly.
        assert!(
            en.contains("English"),
            "en language_directive should name English; got {en:?}",
        );
        assert!(
            vi.contains("tiếng Việt"),
            "vi language_directive should name tiếng Việt; got {vi:?}",
        );
    }

    #[test]
    fn default_agent_role_differs_per_language() {
        let prompts = Prompts::load();
        assert_ne!(
            prompts.set(Language::En).default_agent_role.as_ref(),
            prompts.set(Language::Vi).default_agent_role.as_ref(),
        );
    }

    #[test]
    fn reflection_and_resolution_cores_are_distinct() {
        let prompts = Prompts::load();
        let reflection = prompts.cores.for_kind(RequestKind::Reflection);
        let resolution = prompts.cores.for_kind(RequestKind::Resolution);
        let normal = prompts.cores.for_kind(RequestKind::Normal);
        assert_ne!(reflection.as_ref(), resolution.as_ref());
        assert_ne!(reflection.as_ref(), normal.as_ref());
        assert_ne!(resolution.as_ref(), normal.as_ref());
    }
}
