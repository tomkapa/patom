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
