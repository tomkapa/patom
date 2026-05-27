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

/// One catalog row: model name + the provider that serves it.
#[derive(Debug, Clone, Copy)]
pub struct CatalogEntry {
    pub name: &'static str,
    pub provider: ProviderId,
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
    CatalogEntry {
        name: "claude-opus-4-7",
        provider: ProviderId::Anthropic,
    },
    CatalogEntry {
        name: "claude-sonnet-4-6",
        provider: ProviderId::Anthropic,
    },
    CatalogEntry {
        name: "claude-haiku-4-5",
        provider: ProviderId::Anthropic,
    },
    CatalogEntry {
        name: "claude-sonnet-4-5",
        provider: ProviderId::Anthropic,
    },
    // OpenAI — current generation per
    // https://developers.openai.com/api/docs/models/all (May 2026).
    // `gpt-4o-mini` is legacy but kept for cost-sensitive workloads still on
    // the older lineup.
    CatalogEntry {
        name: "gpt-5.5",
        provider: ProviderId::Openai,
    },
    CatalogEntry {
        name: "gpt-5.4",
        provider: ProviderId::Openai,
    },
    CatalogEntry {
        name: "gpt-5.4-mini",
        provider: ProviderId::Openai,
    },
    CatalogEntry {
        name: "gpt-5.4-nano",
        provider: ProviderId::Openai,
    },
    CatalogEntry {
        name: "gpt-4o-mini",
        provider: ProviderId::Openai,
    },
    // DeepSeek — current generation per
    // https://api-docs.deepseek.com/quick_start/pricing (May 2026). The old
    // `deepseek-chat` / `deepseek-reasoner` aliases retire 2026-07-24 and are
    // intentionally omitted; both modes are reachable via v4-flash + the
    // thinking/non-thinking switch.
    CatalogEntry {
        name: "deepseek-v4-pro",
        provider: ProviderId::Deepseek,
    },
    CatalogEntry {
        name: "deepseek-v4-flash",
        provider: ProviderId::Deepseek,
    },
];

/// Test-only sentinel rows, off by default. Enabled via the `test-catalog`
/// cargo feature, which is auto-enabled when integration tests pull in the
/// crate as a dev-dependency. Release builds never see this slice.
#[cfg(feature = "test-catalog")]
const TEST_CATALOG_EXTENSION: &[CatalogEntry] = &[
    CatalogEntry {
        name: "test-model",
        provider: ProviderId::Anthropic,
    },
    CatalogEntry {
        name: "test-model-openai",
        provider: ProviderId::Openai,
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
