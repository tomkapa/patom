use std::fmt;

use super::error::ParseError;

/// Maximum number of bytes accepted in a single user prompt.
///
/// Sized to fit comfortably under typical model context windows even after history
/// accumulation. Bumping this requires re-checking session truncation logic.
pub const PROMPT_MAX_BYTES: usize = 64 * 1024;

/// A non-empty, length-capped user-provided prompt.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Prompt(String);

impl Prompt {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl TryFrom<String> for Prompt {
    type Error = ParseError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(ParseError::Empty { field: "prompt" });
        }
        if raw.len() > PROMPT_MAX_BYTES {
            return Err(ParseError::TooLong {
                field: "prompt",
                max: PROMPT_MAX_BYTES,
                got: raw.len(),
            });
        }
        Ok(Self(raw))
    }
}

impl TryFrom<&str> for Prompt {
    type Error = ParseError;

    fn try_from(raw: &str) -> Result<Self, Self::Error> {
        Self::try_from(raw.to_string())
    }
}

impl fmt::Debug for Prompt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Prompt").field(&self.0.len()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty_and_whitespace_only() {
        assert!(Prompt::try_from("").is_err());
        assert!(Prompt::try_from("   \n\t").is_err());
    }

    #[test]
    fn rejects_oversize() {
        let big = "a".repeat(PROMPT_MAX_BYTES + 1);
        assert!(Prompt::try_from(big).is_err());
    }

    #[test]
    fn accepts_normal() {
        let p = Prompt::try_from("hello").expect("non-empty");
        assert_eq!(p.as_str(), "hello");
    }
}
