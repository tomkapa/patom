//! Bounds for the provider subsystem (BYO credentials + per-org overlay).
//! CLAUDE.md §5: every limit is named, doc-commented, and exported so the
//! operator can audit them in one place.

/// Maximum byte length of a BYO provider API key accepted at the boundary.
///
/// Real provider keys are short (Anthropic `sk-ant-…`, OpenAI `sk-…`, DeepSeek
/// `sk-…` are well under 200 bytes). 1 KiB is a generous safety belt that traps
/// a paste of the wrong field (a whole config blob) before it reaches the AEAD
/// envelope (CLAUDE.md §5: every string crossing a trust boundary is capped).
pub const MAX_PROVIDER_API_KEY_BYTES: usize = 1024;

/// Maximum byte length of a BYO provider `base_url` override.
///
/// Sized for a fully-qualified endpoint with a path prefix; anything longer is
/// almost certainly malformed input rather than a real gateway URL.
pub const MAX_PROVIDER_BASE_URL_BYTES: usize = 512;

/// Upper bound on the number of orgs held in the in-memory
/// [`crate::provider::OrgProviderOverlay`] snapshot.
///
/// The overlay caches one decrypted client set per org that has at least one
/// BYO key. Bounded so a pathological tenant count cannot grow worker memory
/// without limit; a saturation counter watches it (CLAUDE.md §5). Sized well
/// above any realistic launch tenant count.
pub const MAX_ORGS_OVERLAY: usize = 4096;

/// Maximum number of attachments (images + files) carried by a single user
/// message.
///
/// A handful of references per turn is the realistic ceiling for a chat
/// composer; the cap bounds the per-message JSONB envelope and the number of
/// byte fetches / Office extractions a single turn can trigger (CLAUDE.md §5).
pub const MAX_ATTACHMENTS_PER_MESSAGE: usize = 8;

/// Maximum byte length of an attachment's original filename.
///
/// 255 is the de-facto filesystem ceiling; the name is display-only metadata
/// so the cap exists purely to bound the string crossing the trust boundary
/// (CLAUDE.md §5).
pub const MAX_FILENAME_LEN: usize = 255;

/// Maximum decoded size of an *image* attachment, in bytes.
///
/// Images ride inline (data-URI to OpenAI, URL to both providers). 10 MiB is
/// generous for screenshots/photos while staying well under provider
/// per-image limits.
pub const MAX_ATTACHMENT_IMAGE_BYTES: u64 = 10 * 1024 * 1024;

/// Maximum decoded size of a *file* attachment (PDF / xlsx / docx), in bytes.
///
/// Files are inlined as base64 to OpenAI, which caps a single file at 50 MB;
/// 32 MiB keeps the base64-expanded payload (~43 MiB) comfortably under that
/// while bounding the bytes a single turn fetches and re-encodes.
pub const MAX_ATTACHMENT_FILE_BYTES: u64 = 32 * 1024 * 1024;

/// Timeout for fetching one attachment's bytes from object storage during
/// dispatch materialization (CLAUDE.md §5: every I/O await is bounded).
///
/// Object storage is same-region and the body is capped at
/// [`MAX_ATTACHMENT_FILE_BYTES`]; 30s is generous headroom for the largest
/// allowed file on a slow link before we fail the turn rather than hang it.
pub const ATTACHMENT_FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Maximum bytes of extracted text kept from a single Office (xlsx/docx)
/// attachment on the Anthropic path.
///
/// Extraction can balloon (a large spreadsheet → megabytes of text); the cap
/// bounds what we inject into the prompt — and therefore token spend — per
/// attachment. Mirrors OpenAI's own server-side "first ~1000 rows" posture.
/// Expressed in bytes to match the crate-wide truncation convention
/// ([`crate::tools::truncate_to_char_boundary`]).
pub const MAX_OFFICE_EXTRACT_BYTES: usize = 200 * 1024;
