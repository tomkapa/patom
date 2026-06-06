//! Bounds for the Lemon Squeezy subsystem (CLAUDE.md §5). Named, exported,
//! with a note on *why* each number.

/// Max byte length accepted for any Lemon Squeezy id.
///
/// Covers customer / subscription / order / variant / event ids. LS ids are
/// short numeric strings today; 128 bytes is generous headroom while still
/// capping anything that crosses the trust boundary into a `TEXT` column.
pub const MAX_LS_ID_BYTES: usize = 128;
