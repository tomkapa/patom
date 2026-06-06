use std::borrow::Borrow;
use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::error::ParseError;

/// Maximum length of a tool name. Anthropic's hard cap is 64; we mirror it.
pub const TOOL_NAME_MAX_LEN: usize = 64;

/// A validated, low-cardinality identifier for a tool.
///
/// Reference-counted so that registry lookups, tracing fields, and provider serialization
/// all share the same heap allocation.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct ToolName(Arc<str>);

impl ToolName {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<&str> for ToolName {
    type Error = ParseError;

    fn try_from(raw: &str) -> Result<Self, Self::Error> {
        if raw.is_empty() {
            return Err(ParseError::Empty { field: "tool_name" });
        }
        if raw.len() > TOOL_NAME_MAX_LEN {
            return Err(ParseError::TooLong {
                field: "tool_name",
                max: TOOL_NAME_MAX_LEN,
                got: raw.len(),
            });
        }
        let valid = raw
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
        if !valid {
            return Err(ParseError::Malformed {
                field: "tool_name",
                detail: "allowed: a-z A-Z 0-9 _ -",
            });
        }
        Ok(Self(Arc::from(raw)))
    }
}

impl TryFrom<String> for ToolName {
    type Error = ParseError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::try_from(raw.as_str())
    }
}

impl Borrow<str> for ToolName {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ToolName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ToolName").field(&&*self.0).finish()
    }
}

impl fmt::Display for ToolName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for ToolName {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ToolName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::try_from(raw).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty_and_oversize_and_bad_chars() {
        assert!(ToolName::try_from("").is_err());
        assert!(ToolName::try_from("a".repeat(TOOL_NAME_MAX_LEN + 1).as_str()).is_err());
        assert!(ToolName::try_from("bad name").is_err());
        assert!(ToolName::try_from("bad/name").is_err());
    }

    #[test]
    fn accepts_alnum_and_dash_underscore() {
        assert!(ToolName::try_from("web_fetch").is_ok());
        assert!(ToolName::try_from("Web-Fetch-1").is_ok());
    }

    #[test]
    fn borrow_str_supports_hashmap_lookup() {
        use std::collections::HashMap;
        let name = ToolName::try_from("x").expect("valid");
        let mut m: HashMap<ToolName, i32> = HashMap::new();
        m.insert(name, 1);
        assert_eq!(m.get("x"), Some(&1));
    }
}
