//! Catalog of LLM models Patom knows how to route.
//!
//! Parse-don't-validate per CLAUDE.md §1: holding a [`Model`] proves the value
//! is one of the entries in [`MODEL_CATALOG`] and that its provider is known.
//! The smart constructor ([`Model::try_from`]) is the only way in; once you
//! have a `Model`, [`Model::provider`] and [`Model::as_str`] are pure getters,
//! no further lookup or fallibility downstream.
//!
//! The catalog is intentionally hardcoded today. Future evolution
//! (DB-backed, per-org filter, model aliases) replaces the constructor body
//! without touching call-sites.

use std::fmt;

use serde::{Serialize, Serializer};
use thiserror::Error;

use super::id::ProviderId;

/// Maximum prompt tokens a model accepts, from the catalog.
///
/// A newtype (CLAUDE.md §1) so the context-compaction budget can't be confused
/// with an output cap or a raw token count. Pure data — the value is whatever
/// the provider documents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ContextWindow(u32);

impl ContextWindow {
    /// Window size in tokens.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// A catalog window must be a positive token count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("context window must be > 0")]
pub struct InvalidContextWindow;

impl TryFrom<u32> for ContextWindow {
    type Error = InvalidContextWindow;
    fn try_from(raw: u32) -> Result<Self, Self::Error> {
        if raw == 0 {
            return Err(InvalidContextWindow);
        }
        Ok(Self(raw))
    }
}

/// What multimodal input a model accepts, after Patom's per-provider adaptation.
///
/// This is the capability gate that keeps a text-only backend on the shared
/// OpenAI-compatible wire path (DeepSeek) from ever receiving image or file
/// content parts (issue #187). A flag being `true` means "we can deliver this
/// content to the model" — whether natively (image URL, PDF as an OpenAI
/// `file` part / Anthropic `document` block) or via server-side adaptation
/// (Office → text on both providers). *How* is the converter's concern;
/// *whether* is this flag.
// One independent yes/no per content class is the natural shape for a
// capability set; a bitflag/enum would be less readable for four orthogonal
// flags consulted individually by `accepts`.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelCapabilities {
    /// PNG/JPEG/WebP/GIF image input.
    pub images: bool,
    /// PDF document input.
    pub pdf: bool,
    /// Office (xlsx/docx) input.
    pub office: bool,
    /// Plain-text file input (md/json/toml/…), delivered as decoded text.
    /// True even for text-only models — a text file is just text.
    pub text: bool,
}

impl ModelCapabilities {
    /// Text-only model: rejects image/PDF/Office parts but still accepts
    /// plain-text file attachments (they ride as ordinary text). This is the
    /// DeepSeek posture — its official API is text-only, so no `image_url` /
    /// `file` part may reach it, but a `.md`/`.json` file is fine.
    pub const TEXT_ONLY: Self = Self {
        images: false,
        pdf: false,
        office: false,
        text: true,
    };

    /// Images + PDF + Office + text. Both Anthropic and OpenAI reach this
    /// today (Office/text are delivered as extracted/decoded text).
    pub const ALL: Self = Self {
        images: true,
        pdf: true,
        office: true,
        text: true,
    };

    /// Whether the given mime is accepted as input by this model.
    #[must_use]
    pub const fn accepts(self, mime: super::attachment::AttachmentMime) -> bool {
        if mime.is_image() {
            self.images
        } else if mime.is_pdf() {
            self.pdf
        } else if mime.is_text() {
            self.text
        } else {
            self.office
        }
    }
}

/// One catalog row: model name + the provider that serves it + its context
/// window + the multimodal input it accepts.
#[derive(Debug, Clone, Copy)]
pub struct CatalogEntry {
    pub name: &'static str,
    pub provider: ProviderId,
    /// Maximum prompt tokens the model accepts (the compaction budget is a
    /// fraction of this — see `agent_core::compaction`). The inner field is
    /// private, so a [`ContextWindow`] outside this module can only be built via
    /// the validating [`TryFrom`]; the trusted in-tree catalog below constructs
    /// it directly.
    pub context_window: ContextWindow,
    pub capabilities: ModelCapabilities,
}

/// Every model Patom accepts on the chat path. Linear scan is fine — the slice
/// is bounded by the source file. Adding a new model = one row here.
///
/// Test-only sentinel models (`test-model`, `test-model-openai`) live in
/// [`TEST_CATALOG_EXTENSION`] and are spliced into this slice **only** when
/// the `test-catalog` cargo feature is enabled (auto-enabled in `[dev-
/// dependencies]`, never in `cargo build --release`). Production builds never
/// expose them, so a release HTTP boundary cannot accept a sentinel name.
pub const MODEL_CATALOG: &[CatalogEntry] = &[
    // Anthropic — current generation per
    // https://github.com/anthropics/skills/blob/main/skills/claude-api/shared/models.md
    // (May 2026). `claude-sonnet-4-5` is legacy but still active; included for
    // operators on the previous Sonnet generation.
    // Claude 4.x ships a 200k-token standard window (the 1M beta is opt-in and
    // not what the chat path requests, so we budget against the standard size).
    CatalogEntry {
        name: "claude-opus-4-7",
        provider: ProviderId::Anthropic,
        context_window: ContextWindow(200_000),
        capabilities: ModelCapabilities::ALL,
    },
    CatalogEntry {
        name: "claude-sonnet-4-6",
        provider: ProviderId::Anthropic,
        context_window: ContextWindow(200_000),
        capabilities: ModelCapabilities::ALL,
    },
    CatalogEntry {
        name: "claude-haiku-4-5",
        provider: ProviderId::Anthropic,
        context_window: ContextWindow(200_000),
        capabilities: ModelCapabilities::ALL,
    },
    CatalogEntry {
        name: "claude-sonnet-4-5",
        provider: ProviderId::Anthropic,
        context_window: ContextWindow(200_000),
        capabilities: ModelCapabilities::ALL,
    },
    // OpenAI — current generation per
    // https://developers.openai.com/api/docs/models/all (May 2026).
    // `gpt-4o-mini` is legacy but kept for cost-sensitive workloads still on
    // the older lineup.
    // GPT-5 generation ships a 400k-token window; gpt-4o-mini is the older
    // 128k lineup.
    CatalogEntry {
        name: "gpt-5.5",
        provider: ProviderId::Openai,
        context_window: ContextWindow(400_000),
        capabilities: ModelCapabilities::ALL,
    },
    CatalogEntry {
        name: "gpt-5.4",
        provider: ProviderId::Openai,
        context_window: ContextWindow(400_000),
        capabilities: ModelCapabilities::ALL,
    },
    CatalogEntry {
        name: "gpt-5.4-mini",
        provider: ProviderId::Openai,
        context_window: ContextWindow(400_000),
        capabilities: ModelCapabilities::ALL,
    },
    CatalogEntry {
        name: "gpt-5.4-nano",
        provider: ProviderId::Openai,
        context_window: ContextWindow(400_000),
        capabilities: ModelCapabilities::ALL,
    },
    CatalogEntry {
        name: "gpt-4o-mini",
        provider: ProviderId::Openai,
        context_window: ContextWindow(128_000),
        capabilities: ModelCapabilities::ALL,
    },
    // DeepSeek — current generation per
    // https://api-docs.deepseek.com/quick_start/pricing (May 2026). The old
    // `deepseek-chat` / `deepseek-reasoner` aliases retire 2026-07-24 and are
    // intentionally omitted; both modes are reachable via v4-flash + the
    // thinking/non-thinking switch.
    // DeepSeek v4 ships a 1M-token window (both pro and flash).
    CatalogEntry {
        name: "deepseek-v4-pro",
        provider: ProviderId::Deepseek,
        context_window: ContextWindow(1_000_000),
        capabilities: ModelCapabilities::TEXT_ONLY,
    },
    CatalogEntry {
        name: "deepseek-v4-flash",
        provider: ProviderId::Deepseek,
        context_window: ContextWindow(1_000_000),
        capabilities: ModelCapabilities::TEXT_ONLY,
    },
];

/// Test-only sentinel rows, off by default. Enabled via the `test-catalog`
/// cargo feature, which is auto-enabled when integration tests pull in the
/// crate as a dev-dependency. Release builds never see this slice.
#[cfg(feature = "test-catalog")]
const TEST_CATALOG_EXTENSION: &[CatalogEntry] = &[
    // Deliberately tiny windows so compaction-overflow paths are reachable in a
    // test with a handful of short messages, not a 100k-token fixture.
    CatalogEntry {
        name: "test-model",
        provider: ProviderId::Anthropic,
        context_window: ContextWindow(2_000),
        capabilities: ModelCapabilities::ALL,
    },
    CatalogEntry {
        name: "test-model-openai",
        provider: ProviderId::Openai,
        context_window: ContextWindow(2_000),
        capabilities: ModelCapabilities::ALL,
    },
];

/// Parse failure when an inbound string does not match any catalog entry.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[error("unknown model `{raw}` — not in the catalog")]
pub struct UnknownModel {
    pub raw: String,
}

/// Catalog-resolved model handle. `Copy` because it carries only a static
/// reference into [`MODEL_CATALOG`].
#[derive(Clone, Copy)]
pub struct Model {
    entry: &'static CatalogEntry,
}

impl PartialEq for Model {
    fn eq(&self, other: &Self) -> bool {
        // Catalog names are unique (asserted by a `#[test]` in this module),
        // so equality on the name is equivalent to equality on the entry
        // pointer — and the name version survives a hypothetical future
        // catalog reload that re-creates entries at fresh addresses.
        self.entry.name == other.entry.name
    }
}

impl Eq for Model {}

impl std::hash::Hash for Model {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.entry.name.hash(state);
    }
}

impl Model {
    /// Provider that serves this model. Pure getter — no runtime lookup.
    #[must_use]
    pub const fn provider(self) -> ProviderId {
        self.entry.provider
    }

    /// Catalog name. The provider client sends this string verbatim.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        self.entry.name
    }

    /// Maximum prompt tokens this model accepts. Pure getter — no runtime
    /// lookup. The compaction token budget is a fraction of this.
    #[must_use]
    pub const fn context_window(self) -> ContextWindow {
        self.entry.context_window
    }

    /// Multimodal input this model accepts. Consulted by each provider's
    /// converter to gate (or down-convert) attachment content before dispatch.
    #[must_use]
    pub const fn capabilities(self) -> ModelCapabilities {
        self.entry.capabilities
    }

    /// Every catalog entry, in declaration order. Bounded by the catalog.
    pub fn all() -> impl Iterator<Item = Self> {
        all_entries().iter().map(|entry| Self { entry })
    }
}

/// Active catalog slice. In release builds this is just [`MODEL_CATALOG`];
/// when the `test-catalog` cargo feature is enabled (auto-enabled by
/// `[dev-dependencies]`) the test sentinel rows are spliced in via a
/// per-call static lazy join. Keeping the join out of `MODEL_CATALOG` itself
/// means a release-mode HTTP boundary cannot accept the sentinel names.
#[cfg(feature = "test-catalog")]
fn all_entries() -> &'static [CatalogEntry] {
    use std::sync::OnceLock;
    static JOINED: OnceLock<Vec<CatalogEntry>> = OnceLock::new();
    JOINED.get_or_init(|| {
        let mut v = Vec::with_capacity(MODEL_CATALOG.len() + TEST_CATALOG_EXTENSION.len());
        v.extend_from_slice(MODEL_CATALOG);
        v.extend_from_slice(TEST_CATALOG_EXTENSION);
        v
    })
}

#[cfg(not(feature = "test-catalog"))]
const fn all_entries() -> &'static [CatalogEntry] {
    MODEL_CATALOG
}

impl TryFrom<&str> for Model {
    type Error = UnknownModel;
    fn try_from(raw: &str) -> Result<Self, Self::Error> {
        all_entries()
            .iter()
            .find(|entry| entry.name == raw)
            .map(|entry| Self { entry })
            .ok_or_else(|| UnknownModel {
                raw: raw.to_owned(),
            })
    }
}

impl TryFrom<String> for Model {
    type Error = UnknownModel;
    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::try_from(raw.as_str())
    }
}

impl fmt::Debug for Model {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Model")
            .field("name", &self.entry.name)
            .field("provider", &self.entry.provider)
            .finish()
    }
}

impl fmt::Display for Model {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.entry.name)
    }
}

// Boundary deserialise funnels through the smart constructor (CLAUDE.md §1).
impl<'de> serde::Deserialize<'de> for Model {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Self::try_from(raw.as_str()).map_err(serde::de::Error::custom)
    }
}

impl Serialize for Model {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

// sqlx round-trip: encode = catalog name; decode = smart-constructor parse.
impl sqlx::Type<sqlx::Postgres> for Model {
    fn type_info() -> sqlx::postgres::PgTypeInfo {
        <&str as sqlx::Type<sqlx::Postgres>>::type_info()
    }
}

impl<'q> sqlx::Encode<'q, sqlx::Postgres> for Model {
    fn encode_by_ref(
        &self,
        buf: &mut sqlx::postgres::PgArgumentBuffer,
    ) -> Result<sqlx::encode::IsNull, Box<dyn std::error::Error + Send + Sync>> {
        <&str as sqlx::Encode<'q, sqlx::Postgres>>::encode_by_ref(&self.as_str(), buf)
    }
}

impl<'r> sqlx::Decode<'r, sqlx::Postgres> for Model {
    fn decode(
        value: sqlx::postgres::PgValueRef<'r>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let raw = <&str as sqlx::Decode<'r, sqlx::Postgres>>::decode(value)?;
        Self::try_from(raw).map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn every_catalog_entry_round_trips() {
        for entry in all_entries() {
            let parsed = Model::try_from(entry.name).expect("catalog name parses");
            assert_eq!(parsed.as_str(), entry.name);
            assert_eq!(parsed.provider(), entry.provider);
        }
    }

    #[test]
    fn rejects_unknown_model() {
        let err = Model::try_from("not-a-real-model").expect_err("unknown");
        assert_eq!(err.raw, "not-a-real-model");
    }

    #[test]
    fn rejects_empty() {
        assert!(Model::try_from("").is_err());
    }

    #[test]
    fn every_model_has_a_positive_context_window() {
        for model in Model::all() {
            assert!(
                model.context_window().get() > 0,
                "model {model} has a zero context window",
            );
        }
    }

    #[test]
    fn context_window_is_the_catalog_value() {
        let haiku = Model::try_from("claude-haiku-4-5").expect("known");
        assert_eq!(haiku.context_window().get(), 200_000);
        let mini = Model::try_from("gpt-4o-mini").expect("known");
        assert_eq!(mini.context_window().get(), 128_000);
    }

    #[test]
    fn context_window_try_from_rejects_zero() {
        assert!(ContextWindow::try_from(0).is_err());
        assert_eq!(ContextWindow::try_from(1).expect("valid").get(), 1);
    }

    #[test]
    fn catalog_has_no_duplicate_names() {
        let mut seen = HashSet::new();
        for entry in all_entries() {
            assert!(
                seen.insert(entry.name),
                "duplicate catalog entry: {}",
                entry.name
            );
        }
    }

    #[test]
    fn deepseek_models_reject_binary_but_accept_text() {
        // The DeepSeek caveat (issue #187): official API is text-only, and it
        // shares the OpenAI-compatible wire path. No image/PDF/Office part may
        // be emitted to it — but a plain-text file is just text, so it passes.
        for name in ["deepseek-v4-pro", "deepseek-v4-flash"] {
            let caps = Model::try_from(name).expect("known").capabilities();
            assert_eq!(caps, ModelCapabilities::TEXT_ONLY);
            assert!(!caps.images && !caps.pdf && !caps.office);
            assert!(caps.text, "{name} should accept text files");
        }
    }

    #[test]
    fn anthropic_and_openai_models_accept_multimodal() {
        for name in ["claude-opus-4-7", "gpt-5.5", "gpt-4o-mini"] {
            let caps = Model::try_from(name).expect("known").capabilities();
            assert!(caps.images, "{name} should accept images");
            assert!(caps.pdf, "{name} should accept pdf");
            assert!(caps.office, "{name} should accept office");
            assert!(caps.text, "{name} should accept text");
        }
    }

    #[test]
    fn capabilities_accepts_maps_mime_class() {
        use crate::provider::attachment::AttachmentMime;
        let text_only = ModelCapabilities::TEXT_ONLY;
        assert!(!text_only.accepts(AttachmentMime::Png));
        assert!(!text_only.accepts(AttachmentMime::Pdf));
        assert!(!text_only.accepts(AttachmentMime::Xlsx));
        assert!(text_only.accepts(AttachmentMime::Text));
        let all = ModelCapabilities::ALL;
        assert!(all.accepts(AttachmentMime::Png));
        assert!(all.accepts(AttachmentMime::Pdf));
        assert!(all.accepts(AttachmentMime::Docx));
        assert!(all.accepts(AttachmentMime::Text));
    }

    #[test]
    fn json_round_trip_via_serde() {
        let model = Model::try_from("gpt-4o-mini").expect("known");
        let json = serde_json::to_string(&model).expect("ser");
        assert_eq!(json, "\"gpt-4o-mini\"");
        let back: Model = serde_json::from_str(&json).expect("de");
        assert_eq!(back, model);

        let err = serde_json::from_str::<Model>("\"not-a-model\"").expect_err("unknown rejects");
        assert!(err.to_string().contains("unknown model"));
    }
}
