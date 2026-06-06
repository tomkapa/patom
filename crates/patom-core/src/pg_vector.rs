//! Wire format for `pgvector` columns.
//!
//! pgvector accepts a textual representation `'[1.0,2.0,...]'` on insert
//! and emits the same on read. We encode through this textual form so the
//! store does not depend on the third-party `pgvector` crate (CLAUDE.md
//! §8 — zero-dep bias). pgvector's operators (`<=>` cosine distance,
//! `<->` L2) handle similarity math server-side, so we don't need a
//! parallel cosine implementation in Rust.
//!
//! Lives at the crate root because two unrelated subsystems (`agents`
//! and `memory`) both write pgvector columns — keeping the encoder under
//! `memory` would leak that module's name into every Postgres adapter.

use std::fmt::Write;

/// Encode a `&[f32]` as a pgvector literal — `[v0,v1,...]`.
///
/// Returns the rendered string ready to bind as a `TEXT` parameter; the
/// pg server casts it to `vector` automatically when the target column
/// is typed.
#[must_use]
pub fn encode(v: &[f32]) -> String {
    let mut out = String::with_capacity(v.len() * 12 + 2);
    out.push('[');
    for (i, x) in v.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(&mut out, "{x}");
    }
    out.push(']');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_vec_encodes_to_brackets() {
        assert_eq!(encode(&[]), "[]");
    }

    #[test]
    fn simple_vec_encodes_with_commas() {
        let s = encode(&[0.1, -0.2, 1.5, 0.0]);
        assert_eq!(s, "[0.1,-0.2,1.5,0]");
    }
}
