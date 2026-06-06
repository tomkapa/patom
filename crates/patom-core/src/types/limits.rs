use super::error::ParseError;

/// Hard ceiling on model output tokens per turn. Above this, providers reject the request
/// or silently truncate; we want to fail loudly at the boundary instead.
pub const MAX_OUTPUT_TOKENS_CAP: u32 = 200_000;

/// Hard ceiling on conversation turns per `Agent::reply` invocation. Defends against tool
/// loops where the model never converges on a final answer.
pub const MAX_TURNS_CAP: u32 = 64;

/// Output token budget for a single model call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MaxOutputTokens(u32);

impl MaxOutputTokens {
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl TryFrom<u32> for MaxOutputTokens {
    type Error = ParseError;

    fn try_from(n: u32) -> Result<Self, Self::Error> {
        if n == 0 {
            return Err(ParseError::OutOfRange {
                field: "max_output_tokens",
                detail: "must be > 0",
            });
        }
        if n > MAX_OUTPUT_TOKENS_CAP {
            return Err(ParseError::OutOfRange {
                field: "max_output_tokens",
                detail: "exceeds ceiling",
            });
        }
        Ok(Self(n))
    }
}

/// Maximum tool/turn iterations per `Agent::reply` invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MaxTurns(u32);

impl MaxTurns {
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl TryFrom<u32> for MaxTurns {
    type Error = ParseError;

    fn try_from(n: u32) -> Result<Self, Self::Error> {
        if n == 0 {
            return Err(ParseError::OutOfRange {
                field: "max_turns",
                detail: "must be > 0",
            });
        }
        if n > MAX_TURNS_CAP {
            return Err(ParseError::OutOfRange {
                field: "max_turns",
                detail: "exceeds ceiling",
            });
        }
        Ok(Self(n))
    }
}

/// Zero-based index of a turn within a single `Agent::reply` invocation. Bounded by
/// [`MAX_TURNS_CAP`] so any value carried on a hook context is provably legal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TurnIndex(u32);

impl TurnIndex {
    /// First turn in a reply.
    pub const ZERO: Self = Self(0);

    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }

    /// Step to the next turn. Returns `None` when the next value would exceed the cap so
    /// the agent loop terminates explicitly instead of producing an out-of-range index.
    #[must_use]
    pub const fn next(self) -> Option<Self> {
        if self.0 + 1 >= MAX_TURNS_CAP {
            None
        } else {
            Some(Self(self.0 + 1))
        }
    }
}

impl TryFrom<u32> for TurnIndex {
    type Error = ParseError;

    fn try_from(n: u32) -> Result<Self, Self::Error> {
        if n >= MAX_TURNS_CAP {
            return Err(ParseError::OutOfRange {
                field: "turn_index",
                detail: "exceeds ceiling",
            });
        }
        Ok(Self(n))
    }
}

impl std::fmt::Display for TurnIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_zero_and_overflow() {
        assert!(MaxOutputTokens::try_from(0).is_err());
        assert!(MaxOutputTokens::try_from(MAX_OUTPUT_TOKENS_CAP + 1).is_err());
        assert!(MaxTurns::try_from(0).is_err());
        assert!(MaxTurns::try_from(MAX_TURNS_CAP + 1).is_err());
    }

    #[test]
    fn accepts_in_range() {
        assert_eq!(MaxOutputTokens::try_from(4096).expect("valid").get(), 4096);
        assert_eq!(MaxTurns::try_from(8).expect("valid").get(), 8);
    }

    #[test]
    fn turn_index_rejects_at_or_above_cap() {
        assert!(TurnIndex::try_from(MAX_TURNS_CAP).is_err());
        assert!(TurnIndex::try_from(MAX_TURNS_CAP + 1).is_err());
    }

    #[test]
    fn turn_index_zero_is_constructible() {
        assert_eq!(TurnIndex::ZERO.get(), 0);
        assert_eq!(TurnIndex::try_from(0).expect("valid").get(), 0);
    }

    #[test]
    fn turn_index_next_stops_at_cap() {
        let last = TurnIndex::try_from(MAX_TURNS_CAP - 1).expect("valid");
        assert!(last.next().is_none());
        let first = TurnIndex::ZERO;
        assert_eq!(first.next().expect("under cap").get(), 1);
    }
}
