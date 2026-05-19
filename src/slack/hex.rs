//! Lower-case hex decode/encode primitives shared by `verify.rs`
//! (inbound webhook signature) and `oauth.rs` (outbound state token).
//! Both have HMAC tails of exactly 64 hex chars / 32 bytes — wide
//! enough that a tiny shared module is cleaner than a duplicated
//! `hex_nibble` in each consumer.

/// Decode a single ASCII hex digit. `None` on a non-hex byte.
#[must_use]
pub(super) fn nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Decode exactly 64 hex chars into the 32-byte output buffer.
/// Returns `Err(())` for any malformed input.
pub(super) fn decode_32(s: &str, out: &mut [u8; 32]) -> Result<(), ()> {
    if s.len() != 64 {
        return Err(());
    }
    let bytes = s.as_bytes();
    for (i, slot) in out.iter_mut().enumerate() {
        let hi = nibble(bytes[i * 2]).ok_or(())?;
        let lo = nibble(bytes[i * 2 + 1]).ok_or(())?;
        *slot = (hi << 4) | lo;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nibble_handles_all_three_alphabets() {
        assert_eq!(nibble(b'0'), Some(0));
        assert_eq!(nibble(b'9'), Some(9));
        assert_eq!(nibble(b'a'), Some(10));
        assert_eq!(nibble(b'f'), Some(15));
        assert_eq!(nibble(b'A'), Some(10));
        assert_eq!(nibble(b'F'), Some(15));
        assert_eq!(nibble(b'z'), None);
        assert_eq!(nibble(b' '), None);
    }

    #[test]
    fn decode_32_roundtrip() {
        let mut out = [0u8; 32];
        decode_32(&"aa".repeat(32), &mut out).expect("ok");
        assert_eq!(out, [0xAA; 32]);
    }

    #[test]
    fn decode_32_rejects_short_and_non_hex() {
        let mut out = [0u8; 32];
        assert!(decode_32("short", &mut out).is_err());
        assert!(decode_32(&"zz".repeat(32), &mut out).is_err());
    }
}
