//! Module error type for the crypto envelope. CLAUDE.md §12: every variant
//! describes a distinct failure shape the caller can match on. Errors never
//! carry secret material in `Debug` / `Display`.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum CryptoError {
    /// The configured master KEK is the wrong size (must be 32 bytes after
    /// base64 decode). Detected once at startup.
    #[error("crypto: master kek must be {expected} bytes, got {got}")]
    MasterKekSize { expected: usize, got: usize },

    /// The configured master KEK is unparseable (not valid base64).
    #[error("crypto: master kek is not valid base64: {0}")]
    MasterKekDecode(String),

    /// HKDF expand failed. With our fixed 32-byte output size this is
    /// effectively unreachable; surfaced as an error rather than a panic so
    /// the caller can crash the process via §6 at the call site.
    #[error("crypto: per-org key derivation failed")]
    KeyDerivation,

    /// The ciphertext failed AEAD authentication. Either tampered, wrong
    /// key version, or the row belongs to a different org. Never leaks
    /// which of the three.
    #[error("crypto: decryption rejected (tampered or wrong key)")]
    DecryptRejected,

    /// The blob's `key_version` is one we no longer carry. Rotation removed
    /// the old key before the row was re-encrypted.
    #[error("crypto: unknown key version {0}")]
    UnknownKeyVersion(i16),
}
