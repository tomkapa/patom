use std::fmt;

use serde::{Deserialize, Deserializer};

use super::error::ParseError;

/// A string that must never appear in `Debug`, `Display`, or any default serializer output.
///
/// Construct via `TryFrom<String>` so empty values are rejected at the boundary. Read with
/// [`SecretString::expose`] only at the precise call site that needs the bytes (e.g.
/// outbound HTTP header construction).
#[derive(Clone)]
pub struct SecretString(String);

impl SecretString {
    /// Borrow the secret bytes. Avoid copying through `to_string` — that defeats the type.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Length of the underlying secret in bytes. Useful for assertions; reveals nothing.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl TryFrom<String> for SecretString {
    type Error = ParseError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.is_empty() {
            return Err(ParseError::Empty { field: "secret" });
        }
        Ok(Self(value))
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretString(***)")
    }
}

impl fmt::Display for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

impl<'de> Deserialize<'de> for SecretString {
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
    fn rejects_empty() {
        assert!(SecretString::try_from(String::new()).is_err());
    }

    #[test]
    fn redacts_in_debug() {
        let s = SecretString::try_from("hunter2".to_string()).expect("non-empty");
        assert_eq!(format!("{s:?}"), "SecretString(***)");
        assert_eq!(format!("{s}"), "***");
    }

    #[test]
    fn exposes_only_through_method() {
        let s = SecretString::try_from("hunter2".to_string()).expect("non-empty");
        assert_eq!(s.expose(), "hunter2");
    }
}
