//! Envelope encryption for org-scoped credentials.
//!
//! Layered key model:
//!
//! ```text
//!   PATOM_MASTER_KEK (32 bytes, env)
//!         │
//!         │  HKDF-SHA256 (salt = org_id.as_bytes(), info = b"patom-mcp-cred-v1")
//!         ▼
//!   per-org KEK (32 bytes, cached in process memory, Zeroized on drop)
//!         │
//!         │  AES-256-GCM (random 12-byte nonce per record)
//!         ▼
//!   ciphertext  (= AES-GCM seal(plaintext))
//! ```
//!
//! `key_version` is stamped on every record so a future master-KEK rotation
//! can carry the old key alongside the new one and re-encrypt rows lazily.
//! v1 is "the master KEK in `PATOM_MASTER_KEK`"; future versions add more
//! entries to the [`Versions`] map.
//!
//! Threat model. The DB-only attacker who reads `mcp_server_credentials` rows
//! and `mcp_oauth_clients.secret_ciphertext` sees opaque AEAD ciphertexts; the
//! per-org KEK lives only in process memory and the master KEK lives only in
//! the environment. A process-memory attacker can still recover plaintexts —
//! `Zeroize` on the temporary `Vec<u8>` shrinks the window, but the goal is
//! defence-in-depth at rest, not memory-safe enclaves.
//!
//! Each `seal` / `open` runs in constant time relative to plaintext length
//! (modulo allocator behaviour); AES-GCM authentication tag compare is
//! constant-time by construction in the `aes-gcm` crate.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use aes_gcm::aead::{Aead, AeadCore, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use hkdf::Hkdf;
use sha2::Sha256;
use zeroize::Zeroizing;

use crate::auth::OrgId;
use crate::types::SecretString;

mod error;
pub use error::CryptoError;

/// Size of an AES-256 key (master KEK + every per-org KEK).
pub const KEY_BYTES: usize = 32;

/// Size of an AES-GCM nonce, in bytes.
///
/// `aes-gcm` defaults to a 96-bit nonce; randomly chosen from `OsRng` per
/// record (CLAUDE.md §6 invariant: never reuse a nonce under the same key —
/// random 96-bit space is large enough at our expected message volume).
pub const NONCE_BYTES: usize = 12;

/// The active key version for newly-sealed records. Bumped via configuration
/// during rotation; the prior version stays in [`OrgEncryptor::versions`] so
/// `open` continues to decrypt old rows.
pub const CURRENT_KEY_VERSION: i16 = 1;

/// HKDF info parameter for per-org KEK derivation. Bound to v1 of the key
/// schedule; if the derivation rule changes (e.g. add a tenant-tier byte) we
/// rotate to `patom-mcp-cred-v2` so old rows cannot accidentally decrypt
/// under a new schedule.
const KEK_INFO: &[u8] = b"patom-mcp-cred-v1";

/// One sealed record. Encoded inline into BYTEA columns or split into
/// `(ciphertext, nonce, key_version)` triples depending on the table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncryptedBlob {
    pub key_version: i16,
    pub nonce: [u8; NONCE_BYTES],
    pub ciphertext: Vec<u8>,
}

/// Process-wide envelope encryptor. Holds the master KEK(s), caches per-org
/// KEKs derived from them, and exposes the only seal/open path used by every
/// store layer.
pub struct OrgEncryptor {
    /// `key_version → master_kek_bytes`. v1 is required; other versions are
    /// added during rotation. Mapped behind an `Arc` so cloning the handle
    /// is cheap — the bytes themselves are `Zeroizing` so a drop of the
    /// final reference wipes the master keys from memory.
    versions: Arc<HashMap<i16, Zeroizing<[u8; KEY_BYTES]>>>,
    /// Per-org KEK cache. Derived lazily on first use, retained for the
    /// process lifetime. `Zeroizing` ensures the bytes are scrubbed when the
    /// map is dropped (e.g. at shutdown).
    cache: Arc<RwLock<KekCache>>,
}

/// Cached per-(version, org) KEK bytes. The map is small (at most one entry
/// per active org per key version) so a `HashMap` is the right shape.
type KekCache = HashMap<(i16, OrgId), Zeroizing<[u8; KEY_BYTES]>>;

impl std::fmt::Debug for OrgEncryptor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OrgEncryptor")
            .field("versions", &self.versions.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl OrgEncryptor {
    /// Construct from the base64-encoded master KEK. Only call this from the
    /// composition root (`main` / `app::build`); tests use [`Self::for_test`].
    pub fn from_settings(master_kek_b64: &SecretString) -> Result<Self, CryptoError> {
        use base64::Engine as _;
        let raw = base64::engine::general_purpose::STANDARD
            .decode(master_kek_b64.expose().as_bytes())
            .map_err(|e| CryptoError::MasterKekDecode(e.to_string()))?;
        if raw.len() != KEY_BYTES {
            return Err(CryptoError::MasterKekSize {
                expected: KEY_BYTES,
                got: raw.len(),
            });
        }
        let mut bytes = Zeroizing::new([0u8; KEY_BYTES]);
        bytes.copy_from_slice(&raw);
        // Wipe the intermediate `Vec` too.
        drop(Zeroizing::new(raw));
        let mut versions = HashMap::with_capacity(1);
        versions.insert(CURRENT_KEY_VERSION, bytes);
        Ok(Self {
            versions: Arc::new(versions),
            cache: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    /// Test-only constructor with a fixed 32-byte master KEK. Available
    /// outside `#[cfg(test)]` because integration tests in `tests/` are
    /// separate crates and cannot reach cfg-test items.
    #[must_use]
    pub fn for_test(master_kek: [u8; KEY_BYTES]) -> Self {
        let mut versions = HashMap::with_capacity(1);
        versions.insert(CURRENT_KEY_VERSION, Zeroizing::new(master_kek));
        Self {
            versions: Arc::new(versions),
            cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Seal `plaintext` under the per-org KEK derived from the current master
    /// KEK. The returned [`EncryptedBlob`] carries the random nonce + version
    /// alongside the ciphertext.
    pub fn seal(&self, org: OrgId, plaintext: &[u8]) -> Result<EncryptedBlob, CryptoError> {
        let kek = self.org_kek(CURRENT_KEY_VERSION, org)?;
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(kek.as_slice()));
        let nonce_arr = Aes256Gcm::generate_nonce(&mut OsRng);
        let ciphertext = cipher
            .encrypt(&nonce_arr, plaintext)
            // The aes-gcm encrypt error is opaque (no useful diagnostic) and
            // only fires on >64GB plaintexts or arithmetic overflow — neither
            // is reachable at our call sizes. Surface as a typed error rather
            // than panic so the boundary chooses how to respond.
            .map_err(|_| CryptoError::DecryptRejected)?;
        let mut nonce = [0u8; NONCE_BYTES];
        nonce.copy_from_slice(nonce_arr.as_slice());
        Ok(EncryptedBlob {
            key_version: CURRENT_KEY_VERSION,
            nonce,
            ciphertext,
        })
    }

    /// Open a previously-sealed blob. Fails with [`CryptoError::DecryptRejected`]
    /// for any tamper / wrong-key / cross-org attempt — never reveals which.
    pub fn open(
        &self,
        org: OrgId,
        blob: &EncryptedBlob,
    ) -> Result<Zeroizing<Vec<u8>>, CryptoError> {
        let kek = self.org_kek(blob.key_version, org)?;
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(kek.as_slice()));
        let nonce = Nonce::from_slice(&blob.nonce);
        let plaintext = cipher
            .decrypt(nonce, blob.ciphertext.as_slice())
            .map_err(|_| CryptoError::DecryptRejected)?;
        Ok(Zeroizing::new(plaintext))
    }

    /// Cached per-(version, org) KEK lookup. Derived once via HKDF on first
    /// miss and held for the process lifetime; rotation invalidates by
    /// adding a new version, never by editing an existing entry.
    fn org_kek(&self, version: i16, org: OrgId) -> Result<Zeroizing<[u8; KEY_BYTES]>, CryptoError> {
        // Fast path: read-lock and clone the bytes.
        if let Some(k) = self
            .cache
            .read()
            .expect("invariant: org-kek cache rwlock poisoned")
            .get(&(version, org))
        {
            let mut out = Zeroizing::new([0u8; KEY_BYTES]);
            out.copy_from_slice(k.as_slice());
            return Ok(out);
        }
        // Miss: derive once and insert.
        let master = self
            .versions
            .get(&version)
            .ok_or(CryptoError::UnknownKeyVersion(version))?;
        let salt = org.as_uuid();
        let hk = Hkdf::<Sha256>::new(Some(salt.as_bytes()), master.as_slice());
        let mut derived = Zeroizing::new([0u8; KEY_BYTES]);
        hk.expand(KEK_INFO, derived.as_mut_slice())
            .map_err(|_| CryptoError::KeyDerivation)?;
        let mut guard = self
            .cache
            .write()
            .expect("invariant: org-kek cache rwlock poisoned");
        guard.entry((version, org)).or_insert_with(|| {
            let mut copy = Zeroizing::new([0u8; KEY_BYTES]);
            copy.copy_from_slice(derived.as_slice());
            copy
        });
        Ok(derived)
    }
}

/// Convenience newtype mirroring `Arc<OrgEncryptor>` so subsystem signatures
/// stay short — see the `SharedClock` precedent.
pub type SharedOrgEncryptor = Arc<OrgEncryptor>;

// Compile-time check: every public type implements the standard auto traits
// we expect (no surprise non-Send/Sync ever creeps in).
#[cfg(test)]
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<OrgEncryptor>();
    assert_send_sync::<EncryptedBlob>();
};

#[cfg(test)]
mod tests {
    use super::*;

    fn enc() -> OrgEncryptor {
        OrgEncryptor::for_test([7u8; KEY_BYTES])
    }

    #[test]
    fn roundtrip_recovers_plaintext() {
        let e = enc();
        let org = OrgId::new();
        let payload = b"hunter2-bearer-token";
        let blob = e.seal(org, payload).expect("seal");
        let out = e.open(org, &blob).expect("open");
        assert_eq!(out.as_slice(), payload);
    }

    #[test]
    fn tampering_with_ciphertext_is_rejected() {
        let e = enc();
        let org = OrgId::new();
        let mut blob = e.seal(org, b"secret").expect("seal");
        // Flip a bit in the ciphertext.
        blob.ciphertext[0] ^= 0x01;
        let err = e.open(org, &blob).expect_err("open should fail");
        assert!(matches!(err, CryptoError::DecryptRejected));
    }

    #[test]
    fn tampering_with_nonce_is_rejected() {
        let e = enc();
        let org = OrgId::new();
        let mut blob = e.seal(org, b"secret").expect("seal");
        blob.nonce[0] ^= 0x01;
        let err = e.open(org, &blob).expect_err("open should fail");
        assert!(matches!(err, CryptoError::DecryptRejected));
    }

    #[test]
    fn cross_org_open_is_rejected() {
        let e = enc();
        let alice = OrgId::new();
        let bob = OrgId::new();
        let sealed = e.seal(alice, b"alice-only").expect("seal");
        let err = e
            .open(bob, &sealed)
            .expect_err("cross-org open should fail");
        assert!(matches!(err, CryptoError::DecryptRejected));
    }

    #[test]
    fn unknown_key_version_surfaces_typed_error() {
        let e = enc();
        let org = OrgId::new();
        let mut blob = e.seal(org, b"x").expect("seal");
        blob.key_version = 99;
        let err = e.open(org, &blob).expect_err("unknown version");
        assert!(matches!(err, CryptoError::UnknownKeyVersion(99)));
    }

    #[test]
    fn nonces_are_distinct_across_seals() {
        let e = enc();
        let org = OrgId::new();
        let a = e.seal(org, b"same").expect("seal");
        let b = e.seal(org, b"same").expect("seal");
        // Random 96-bit nonces — equal pairs have probability 2^-96.
        assert_ne!(a.nonce, b.nonce);
        // Same plaintext under different nonces → different ciphertexts.
        assert_ne!(a.ciphertext, b.ciphertext);
    }

    #[test]
    fn master_kek_wrong_size_rejected_at_boundary() {
        use base64::Engine as _;
        let too_short = base64::engine::general_purpose::STANDARD.encode([0u8; 16]);
        let s = SecretString::try_from(too_short).expect("non-empty");
        let err = OrgEncryptor::from_settings(&s).expect_err("short kek");
        assert!(matches!(err, CryptoError::MasterKekSize { .. }));
    }

    #[test]
    fn master_kek_correct_size_accepted() {
        use base64::Engine as _;
        let exact = base64::engine::general_purpose::STANDARD.encode([0u8; KEY_BYTES]);
        let s = SecretString::try_from(exact).expect("non-empty");
        OrgEncryptor::from_settings(&s).expect("32-byte kek accepted");
    }
}
