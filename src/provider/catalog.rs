//! Catalog of LLM models Relay knows how to route.
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

/// Every model Relay accepts on the chat path. Linear scan is fine — the slice
/// is bounded by the source file. Adding a new model = one row here.
pub const MODEL_CATALOG: &[CatalogEntry] = &[
    // Anthropic
    CatalogEntry {
        name: "claude-sonnet-4-5",
        provider: ProviderId::Anthropic,
    },
    CatalogEntry {
        name: "claude-haiku-4-5",
        provider: ProviderId::Anthropic,
    },
    CatalogEntry {
        name: "claude-opus-4-7",
        provider: ProviderId::Anthropic,
    },
    // OpenAI
    CatalogEntry {
        name: "gpt-4o",
        provider: ProviderId::Openai,
    },
    CatalogEntry {
        name: "gpt-4o-mini",
        provider: ProviderId::Openai,
    },
    CatalogEntry {
        name: "gpt-4.1",
        provider: ProviderId::Openai,
    },
    CatalogEntry {
        name: "gpt-4.1-mini",
        provider: ProviderId::Openai,
    },
    // DeepSeek
    CatalogEntry {
        name: "deepseek-chat",
        provider: ProviderId::Deepseek,
    },
    CatalogEntry {
        name: "deepseek-reasoner",
        provider: ProviderId::Deepseek,
    },
    // Test-only sentinels. Real model names would also work here, but
    // tests prefer to assert against a stable name that won't collide with
    // a real ChatRequest in production traces. Keeping them as catalog
    // entries (rather than gating with #[cfg(test)]) lets the
    // integration-tests crate construct `Model` values without separate
    // build-feature plumbing.
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
        MODEL_CATALOG.iter().map(|entry| Self { entry })
    }
}

impl TryFrom<&str> for Model {
    type Error = UnknownModel;
    fn try_from(raw: &str) -> Result<Self, Self::Error> {
        MODEL_CATALOG
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
        for entry in MODEL_CATALOG {
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
        for entry in MODEL_CATALOG {
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
