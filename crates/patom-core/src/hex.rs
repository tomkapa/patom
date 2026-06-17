//! Lower-case hex decode/encode primitives for the chat-platform signed-token
//! modules.
//!
//! Shared by Slack `verify.rs` (inbound webhook signature), `oauth.rs` (install
//! state token), and the `connect_link.rs` MCP connect links for Slack, Lark,
//! and Discord. All have HMAC tails of exactly 64 hex chars / 32 bytes — wide
//! enough that one shared module is cleaner than a duplicated `hex_nibble` /
//! `write!("{byte:02x}")` loop in each consumer.
//
// `redundant_pub_crate`: the items are `pub(crate)` so consumers in sibling
// modules (slack/lark/discord) can reach them; clippy flags that as redundant
// against the `pub(crate) mod` declaration, but dropping to bare `pub` then
// trips `unreachable_pub`. Allow the nursery lint to keep the module crate-
// internal without churn.
#![allow(clippy::redundant_pub_crate)]

/// Decode a single ASCII hex digit. `None` on a non-hex byte.
#[must_use]
pub(crate) fn nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Decode exactly 64 hex chars into the 32-byte output buffer.
/// Returns `Err(())` for any malformed input.
pub(crate) fn decode_32(s: &str, out: &mut [u8; 32]) -> Result<(), ()> {
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

/// Encode a 32-byte buffer as 64 lowercase hex chars. Used by every
/// HMAC-SHA256 signer in the chat-platform token modules.
pub(crate) fn encode_32(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(64);
    for byte in bytes {
        write!(out, "{byte:02x}").expect("write to in-memory String is infallible");
    }
    out
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

    #[test]
    fn encode_32_lowercase_64_chars() {
        let s = encode_32(&[0xAB; 32]);
        assert_eq!(s.len(), 64);
        assert_eq!(s, "ab".repeat(32));
    }

    #[test]
    fn encode_decode_roundtrip() {
        let original = [0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0]
            .into_iter()
            .cycle()
            .take(32)
            .collect::<Vec<_>>();
        let encoded = encode_32(&original);
        let mut decoded = [0u8; 32];
        decode_32(&encoded, &mut decoded).expect("decode");
        assert_eq!(decoded.as_slice(), original.as_slice());
    }
}
