//! Closed enum of LLM providers Patom can talk to.
//!
//! Used as the routing key inside [`crate::provider::ProviderRegistry`] and as
//! the `provider` discriminator on the per-agent [`Model`](super::catalog::Model)
//! catalog. Adding a fourth backend = one variant here + one factory arm in
//! `app::build_provider_registry` + new catalog entries in
//! [`crate::provider::catalog`].

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::types::ParseError;

/// Low-cardinality, stable identifier for a provider. Persisted as the catalog
/// discriminator and used in tracing fields (`patom.provider`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderId {
    Anthropic,
    Openai,
    Deepseek,
}

impl ProviderId {
    /// Every variant in declaration order. Bounded by the enum (CLAUDE.md §5).
    pub const ALL: &'static [Self] = &[Self::Anthropic, Self::Openai, Self::Deepseek];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::Openai => "openai",
            Self::Deepseek => "deepseek",
        }
    }
}

impl fmt::Display for ProviderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl TryFrom<&str> for ProviderId {
    type Error = ParseError;
    fn try_from(raw: &str) -> Result<Self, Self::Error> {
        match raw {
            "anthropic" => Ok(Self::Anthropic),
            "openai" => Ok(Self::Openai),
            "deepseek" => Ok(Self::Deepseek),
            _ => Err(ParseError::Malformed {
                field: "provider_id",
                detail: "unknown provider",
            }),
        }
    }
}

impl sqlx::Type<sqlx::Postgres> for ProviderId {
    fn type_info() -> sqlx::postgres::PgTypeInfo {
        <&str as sqlx::Type<sqlx::Postgres>>::type_info()
    }
    fn compatible(ty: &sqlx::postgres::PgTypeInfo) -> bool {
        <&str as sqlx::Type<sqlx::Postgres>>::compatible(ty)
    }
}

impl<'r> sqlx::Decode<'r, sqlx::Postgres> for ProviderId {
    fn decode(value: sqlx::postgres::PgValueRef<'r>) -> Result<Self, sqlx::error::BoxDynError> {
        let raw = <&str as sqlx::Decode<'r, sqlx::Postgres>>::decode(value)?;
        // §6: schema CHECK (provider IN ('anthropic','openai','deepseek'))
        // forbids any other value; observing one means schema and code disagree.
        Self::try_from(raw).map_err(|_| format!("invariant: unknown ProviderId {raw:?}").into())
    }
}

impl<'q> sqlx::Encode<'q, sqlx::Postgres> for ProviderId {
    fn encode_by_ref(
        &self,
        buf: &mut sqlx::postgres::PgArgumentBuffer,
    ) -> Result<sqlx::encode::IsNull, sqlx::error::BoxDynError> {
        <&str as sqlx::Encode<'q, sqlx::Postgres>>::encode(self.as_str(), buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_as_str() {
        for &id in ProviderId::ALL {
            assert_eq!(ProviderId::try_from(id.as_str()).expect("known"), id);
        }
    }

    #[test]
    fn rejects_unknown() {
        assert!(ProviderId::try_from("made-up").is_err());
        assert!(ProviderId::try_from("").is_err());
    }
}
