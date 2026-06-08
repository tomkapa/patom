//! Sizing constants for the channels subsystem (CLAUDE.md §5).
//!
//! Every bound lives here, named and doc-commented with *why this number*, so
//! the magic-number ban holds and the typed caps cannot drift from the DB
//! column `CHECK`s they mirror.

/// Max bytes of a channel name. Mirrors the `channels.name` CHECK regex
/// (`^[a-z0-9][a-z0-9-]{0,62}$`, i.e. at most 63 bytes) and
/// [`super::ChannelName::MAX_BYTES`].
pub const CHANNEL_NAME_MAX_LEN: usize = 63;

/// Hard cap on active channels a single org may hold (§5).
///
/// Channels are cheap rows but unbounded creation would let one org balloon the
/// sidebar and the per-request channel list. Sized generously — far above any
/// realistic team's channel count — and enforced in [`super::ChannelStore`] on
/// create, which emits a saturation signal when it trips.
pub const MAX_CHANNELS_PER_ORG: i64 = 500;

/// Hard cap on rows a single channel-list / member-list read pulls (§5).
///
/// Bounds the wire transfer for `GET /channels` and `GET /channels/{id}/members`
/// independently of [`MAX_CHANNELS_PER_ORG`], so a future raise of that cap
/// cannot silently unbound a list query.
pub const CHANNEL_LIST_FETCH_MAX: i64 = 1024;
