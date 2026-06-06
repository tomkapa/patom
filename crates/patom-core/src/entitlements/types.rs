//! Value types for the entitlement seam: the agent-count quota and the
//! boolean-feature catalog. Both are bare hand-written enums — they cross no
//! Postgres or serde boundary yet, so the `str_enum!` macro's sqlx/serde impls
//! would be dead surface against CLAUDE.md §8. When a future feature gate needs
//! to persist (billing, #131), promoting one of these to `str_enum!` is a
//! localized change.

/// How many agents an org may run, as resolved by an [`super::Entitlements`].
///
/// A sum type rather than a `bool` + `Option<u32>` (CLAUDE.md §1): the two
/// states — uncapped vs. a concrete ceiling — are exhaustively distinct.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentLimit {
    /// No ceiling. The OSS / self-host default (see
    /// [`super::UnlimitedEntitlements`]) and the Cloud Enterprise tier.
    Unlimited,
    /// At most `cap` agents may exist in the org. A would-be `cap+1`-th
    /// creation is refused with 402 by the agent store's in-tx cap gate (#131).
    Max(u32),
}

/// A boolean-gated capability.
///
/// The pricing model gates on agent *count*, not features (every feature is on
/// for every tier today), so this catalog has no real entries yet. The single
/// provisional [`Feature::Reserved`] variant keeps the
/// [`super::Entitlements::allows`] seam inhabited and testable until the first
/// real gate lands; replace or extend it then.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Feature {
    /// Placeholder exemplar — not a shipped gate. Holds the seam open.
    Reserved,
}

impl Feature {
    /// Stable, low-cardinality label for the 402 body and `patom.*` tracing
    /// attributes. The single source of truth for the wire name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Reserved => "reserved",
        }
    }
}

impl std::fmt::Display for Feature {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::Feature;

    #[test]
    fn feature_label_is_stable() {
        assert_eq!(Feature::Reserved.as_str(), "reserved");
        assert_eq!(Feature::Reserved.to_string(), "reserved");
    }
}
