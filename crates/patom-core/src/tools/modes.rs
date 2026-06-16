//! Per-mode tool availability.
//!
//! Every [`Tool`](super::traits::Tool) declares which
//! [`RequestKind`](crate::runtime::RequestKind) modes it is available
//! in via [`Tool::modes`](super::traits::Tool::modes). The default is
//! [`RequestKindModes::ALL`] — most tools are usable in every mode.
//! Mode filtering is the seam that lets future submodes (Planning,
//! Researching, …) opt tools in or out without touching the agent
//! loop.
//!
//! Bitset-backed so a tool's `modes()` is `Copy` and trivially
//! composable: `RequestKindModes::NORMAL | RequestKindModes::REFLECTION`.

use crate::runtime::RequestKind;

/// Set of [`RequestKind`] modes a tool participates in.
///
/// The [`RequestKindModes::ALL`] default is the conservative choice for
/// existing tools — opt out only when a tool is genuinely meaningless
/// or unsafe in some mode. Adding a new variant to `RequestKind` means
/// adding a flag here and recompiling; existing tools whose `modes()`
/// is `ALL` automatically include the new mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestKindModes(u8);

impl RequestKindModes {
    pub const NONE: Self = Self(0);
    pub const NORMAL: Self = Self(1 << 0);
    pub const REFLECTION: Self = Self(1 << 1);
    pub const RESOLUTION: Self = Self(1 << 2);
    /// Every mode currently defined. Adding a `RequestKind` variant
    /// requires updating both [`Self::ALL`] and [`Self::for_kind`];
    /// `cargo check` flags the latter via `match` exhaustiveness.
    pub const ALL: Self = Self(0b0000_0111);

    /// Build a single-mode set for `kind`.
    #[must_use]
    pub const fn for_kind(kind: RequestKind) -> Self {
        match kind {
            RequestKind::Normal => Self::NORMAL,
            RequestKind::Reflection => Self::REFLECTION,
            RequestKind::Resolution => Self::RESOLUTION,
        }
    }

    /// Does this set include `kind`?
    #[must_use]
    pub const fn includes(self, kind: RequestKind) -> bool {
        (self.0 & Self::for_kind(kind).0) != 0
    }
}

impl Default for RequestKindModes {
    fn default() -> Self {
        Self::ALL
    }
}

impl std::ops::BitOr for RequestKindModes {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for RequestKindModes {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_includes_every_kind() {
        assert!(RequestKindModes::ALL.includes(RequestKind::Normal));
        assert!(RequestKindModes::ALL.includes(RequestKind::Reflection));
        assert!(RequestKindModes::ALL.includes(RequestKind::Resolution));
    }

    #[test]
    fn single_kind_only_includes_self() {
        let only_resolution = RequestKindModes::RESOLUTION;
        assert!(!only_resolution.includes(RequestKind::Normal));
        assert!(!only_resolution.includes(RequestKind::Reflection));
        assert!(only_resolution.includes(RequestKind::Resolution));
    }

    #[test]
    fn bitor_combines() {
        let normal_or_reflection = RequestKindModes::NORMAL | RequestKindModes::REFLECTION;
        assert!(normal_or_reflection.includes(RequestKind::Normal));
        assert!(normal_or_reflection.includes(RequestKind::Reflection));
        assert!(!normal_or_reflection.includes(RequestKind::Resolution));
    }
}
