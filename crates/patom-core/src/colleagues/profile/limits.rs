//! Sizing constants for the colleague profile board (CLAUDE.md §5).
//!
//! Every cap lives here, named and doc-commented with *why this number*, so the
//! typed `TryFrom` bounds cannot drift from the `colleague_profiles` column
//! `CHECK`s they mirror (migration 84).

/// Max bytes of a `role` ("Product Manager"). Roles are short labels, not prose;
/// mirrors the `role` column `CHECK (... BETWEEN 1 AND 200)`.
pub const MAX_ROLE: usize = 200;

/// Max bytes of free-text `expertise`. Generous enough for a sentence or two of
/// what someone is good at; mirrors the `expertise` column `CHECK (1..2048)`.
pub const MAX_EXPERTISE: usize = 2048;

/// Max bytes of free-text `preferences` ("call me Pa; async-first"). Same budget
/// as expertise; mirrors the `preferences` column `CHECK (1..2048)`.
pub const MAX_PREFERENCES: usize = 2048;

/// Max bytes of the composed `profile_text` embedding source.
///
/// The three field caps plus their labels sum to ~4.3 KiB, so this leaves
/// headroom while still bounding the embedding input; mirrors the `profile_text`
/// column `CHECK (1..5120)`.
pub const MAX_PROFILE_TEXT: usize = 5120;

/// Hard cap on colleague ids a single `get_many` fetch resolves (§5).
///
/// The `<participants>` block (the caller) is itself capped well below this, so
/// this just bounds the `WHERE colleague_id = ANY($1)` array if a caller passes
/// more.
pub const MAX_PROFILE_FETCH: usize = 256;

/// Max characters of the per-person `search_colleague` / `<participants>` snippet.
///
/// Long enough to convey a role + a clause of context, short enough that a
/// result list of [`SEARCH_COLLEAGUE_K`] stays compact in the prompt.
pub const PROFILE_SNIPPET_LEN: usize = 160;

/// Max people named inline in the `<participants>` block before it is truncated.
///
/// A thread with more than this many distinct speakers is rare; past it, naming
/// everyone costs more per-turn tokens than it informs, and the roster /
/// `search_colleague` cover the rest. Saturation is emitted as a field.
pub const MAX_PARTICIPANTS_INLINE: usize = 32;

/// Result cap for `search_colleague`, reused as the tool's `limit` upper bound.
///
/// Matches the agent-search bound (1..=8) so the unified search returns a
/// focused, actable shortlist rather than a roster dump.
pub const SEARCH_COLLEAGUE_K: u8 = 8;

/// Default `search_colleague` result count when the model omits `limit`.
///
/// Four is enough to compare a shortlist without flooding the turn; the model
/// raises it (up to [`SEARCH_COLLEAGUE_K`]) when it wants a wider net.
pub const DEFAULT_SEARCH_COLLEAGUE_K: u8 = 4;
