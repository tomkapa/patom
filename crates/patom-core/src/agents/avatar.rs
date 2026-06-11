//! Default avatar assignment for agents.
//!
//! Preset and freshly-minted agents are decorated with one of a fixed set
//! of app-bundled avatar images hosted on the asset CDN at
//! `<public_host>/agents/agent-{n}.png` (`n` in `1..=PRESET_AVATAR_COUNT`).
//! The recruiter always takes index 1; preset team members take the next
//! indices in order; a freshly-created agent takes a random index. The
//! `/agents/agent-{n}.png` URL convention lives here and nowhere else, so a
//! caller that wants a default avatar goes through
//! [`preset_agent_avatar_url`] rather than hand-rolling the path.

use rand::Rng;

use super::limits::PRESET_AVATAR_COUNT;
use crate::types::{AvatarUrl, ParseError};

/// A 1-based index into the fixed set of bundled agent avatars
/// (`agent-1.png` .. `agent-{PRESET_AVATAR_COUNT}.png`).
///
/// CLAUDE.md §1: the `TryFrom` smart constructor is the only way in, so an
/// out-of-range index can never reach URL construction. Bounds mirror the
/// `1..=PRESET_AVATAR_COUNT` set of assets uploaded to the CDN.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct AvatarIndex(u8);

impl AvatarIndex {
    /// The recruiter's fixed avatar — `agent-1.png`.
    pub const RECRUITER: Self = Self(1);

    /// The wrapped 1-based index.
    #[must_use]
    pub fn get(self) -> u8 {
        self.0
    }

    /// Pick a uniformly-random avatar index in `1..=PRESET_AVATAR_COUNT`,
    /// used when a freshly-created agent carries no explicit avatar.
    ///
    /// Non-time randomness through the same `rand::thread_rng()` seam as
    /// invite-token minting (`orgs::pg_store`). CLAUDE.md §11 governs the
    /// clock, not the RNG, so a direct `thread_rng()` is fine here.
    #[must_use]
    pub fn random() -> Self {
        let n = rand::thread_rng().gen_range(1..=PRESET_AVATAR_COUNT);
        Self(n)
    }
}

impl TryFrom<u8> for AvatarIndex {
    type Error = ParseError;

    fn try_from(n: u8) -> Result<Self, Self::Error> {
        if n < 1 {
            return Err(ParseError::OutOfRange {
                field: "avatar_index",
                detail: "must be >= 1",
            });
        }
        if n > PRESET_AVATAR_COUNT {
            return Err(ParseError::OutOfRange {
                field: "avatar_index",
                detail: "exceeds bundled avatar count",
            });
        }
        Ok(Self(n))
    }
}

/// Build the absolute avatar URL for a bundled agent avatar:
/// `{host}/agents/agent-{n}.png`.
///
/// `host` is the asset store's already-validated public origin (no trailing
/// slash — see [`crate::assets::AssetStore::public_host`]) and `index` is
/// bounded by construction, so the joined string is always a valid
/// [`AvatarUrl`]. The `.expect` is therefore a named assertion (CLAUDE.md
/// §6), not error handling: it can only fire if the public-host invariant
/// is broken upstream.
#[must_use]
pub fn preset_agent_avatar_url(host: &str, index: AvatarIndex) -> AvatarUrl {
    let raw = format!("{host}/agents/agent-{n}.png", n = index.get());
    AvatarUrl::try_from(raw.as_str())
        .expect("invariant: validated host + bounded index yields a valid AvatarUrl")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn try_from_accepts_full_range() {
        assert_eq!(AvatarIndex::try_from(1).expect("1 valid").get(), 1);
        assert_eq!(
            AvatarIndex::try_from(PRESET_AVATAR_COUNT)
                .expect("max valid")
                .get(),
            PRESET_AVATAR_COUNT
        );
    }

    #[test]
    fn try_from_rejects_zero() {
        assert!(matches!(
            AvatarIndex::try_from(0),
            Err(ParseError::OutOfRange { .. })
        ));
    }

    #[test]
    fn try_from_rejects_above_count() {
        assert!(matches!(
            AvatarIndex::try_from(PRESET_AVATAR_COUNT + 1),
            Err(ParseError::OutOfRange { .. })
        ));
    }

    #[test]
    fn random_is_always_in_range() {
        // Sample enough draws that an off-by-one bound would almost surely
        // surface at least once.
        for _ in 0..1_000 {
            let n = AvatarIndex::random().get();
            assert!(n >= 1, "random index below 1: {n}");
            assert!(n <= PRESET_AVATAR_COUNT, "random index above cap: {n}");
        }
    }

    #[test]
    fn recruiter_is_index_one() {
        assert_eq!(AvatarIndex::RECRUITER.get(), 1);
    }

    #[test]
    fn builds_absolute_url_from_host_and_index() {
        let url = preset_agent_avatar_url("https://cdn.example", AvatarIndex::RECRUITER);
        assert_eq!(url.as_str(), "https://cdn.example/agents/agent-1.png");
    }

    #[test]
    fn builds_url_for_path_style_host() {
        // Path-style MinIO/R2 host carries a bucket segment; the join must
        // append, not replace, the path.
        let idx = AvatarIndex::try_from(5).expect("5 valid");
        let url = preset_agent_avatar_url("http://minio:9000/patom-assets", idx);
        assert_eq!(
            url.as_str(),
            "http://minio:9000/patom-assets/agents/agent-5.png"
        );
    }
}
