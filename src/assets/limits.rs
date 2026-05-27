//! Asset-storage invariants. CLAUDE.md §5: every container, every I/O is bounded.

use std::time::Duration;

/// Upper bound on a single user avatar upload.
///
/// Sized for "modern phone photo" — a 4MP JPEG/PNG out of an iPhone camera
/// is comfortably under 2 MiB. Bigger inputs almost certainly mean the
/// user uploaded the raw file straight off a DSLR; rejecting at 2 MiB
/// nudges them to resize before we ever bill bytes to R2.
pub const MAX_AVATAR_BYTES: usize = 2 * 1024 * 1024;

/// Upper bound on a single workspace (organization) avatar upload.
///
/// Same 2 MiB ceiling as user avatars today; held as its own constant
/// so the cap can drift if workspace logos diverge from camera output
/// in product (e.g. designer drops a high-DPI export).
pub const MAX_WORKSPACE_AVATAR_BYTES: usize = 2 * 1024 * 1024;

/// Upper bound on an MCP-catalog tile icon upload.
///
/// Tile icons are SVGs or PNGs that render at ~48px in the grid. A
/// well-prepared SVG is single-digit KB; even an unoptimised PNG is
/// under 100 KB. The 256 KiB ceiling exists so a misclicked HD wallpaper
/// gets rejected at the boundary rather than wasting R2 Class A ops.
pub const MAX_MCP_ICON_BYTES: usize = 256 * 1024;

/// Cap on the bytes of magic-byte sniffing we feed `infer`.
///
/// `infer` only inspects the first dozens of bytes for any signature it
/// knows; 512 is generous headroom. Bounded so a multi-MB upload with a
/// malformed multipart frame can't trick us into hashing the whole body.
pub const SNIFF_PREFIX_BYTES: usize = 512;

/// Upper bound on a stored object key length.
///
/// Our keys follow `avatars/<user_id>.<ext>` (~50 bytes) or
/// `mcp/<catalog_id>.<ext>` (~50 bytes); 256 is comfortable headroom and
/// keeps the value short enough that S3 list-objects responses don't
/// balloon.
pub const OBJECT_KEY_MAX_LEN: usize = 256;

/// Cap on the persisted asset URL string length.
///
/// Matches the schema CHECK on `mcp_catalog.icon_url` and `users.avatar_url`
/// so the BE rejects a row the DB would also reject.
pub const ASSET_URL_MAX_LEN: usize = 2048;

/// How long we'll wait on a single R2 PutObject round-trip.
///
/// R2 is CDN-fronted and usually sub-second; the cap is generous so a
/// transient slow upload still completes, but tight enough that a stuck
/// connection doesn't hold an axum task indefinitely. CLAUDE.md §5: every
/// I/O await is wrapped in `tokio::time::timeout`.
pub const R2_PUT_TIMEOUT: Duration = Duration::from_secs(15);

/// How long we'll wait on a single R2 DeleteObject round-trip.
pub const R2_DELETE_TIMEOUT: Duration = Duration::from_secs(10);

/// Per-await cap on the multipart trust boundary.
///
/// `next_field()`, `field.bytes()`, etc. each get this budget so a
/// client trickling bytes can't hold the request task indefinitely.
/// The body itself is size-capped by `DefaultBodyLimit`. CLAUDE.md §5:
/// every I/O await is wrapped in `tokio::time::timeout`.
pub const MULTIPART_IO_TIMEOUT: Duration = Duration::from_secs(15);
