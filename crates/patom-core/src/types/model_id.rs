use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Deserializer};

use super::error::ParseError;

/// Maximum length of a model identifier. Generous; provider names are short.
const MODEL_ID_MAX_LEN: usize = 128;

/// Provider-agnostic handle to a model.
///
/// The `LlmProvider` impl interprets this string against its own model catalogue and
/// returns an error if it does not recognise the value. The agent layer never inspects
/// the contents.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct ModelId(Arc<str>);

impl ModelId {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<&str> for ModelId {
    type Error = ParseError;

    fn try_from(raw: &str) -> Result<Self, Self::Error> {
        if raw.is_empty() {
            return Err(ParseError::Empty { field: "model_id" });
        }
        if raw.len() > MODEL_ID_MAX_LEN {
            return Err(ParseError::TooLong {
                field: "model_id",
                max: MODEL_ID_MAX_LEN,
                got: raw.len(),
            });
        }
        Ok(Self(Arc::from(raw)))
    }
}

impl TryFrom<String> for ModelId {
    type Error = ParseError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::try_from(raw.as_str())
    }
}

impl fmt::Debug for ModelId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ModelId").field(&&*self.0).finish()
    }
}

impl fmt::Display for ModelId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ModelId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::try_from(raw).map_err(serde::de::Error::custom)
    }
}
