//! pbbp2 fragment reassembly (CLAUDE.md §4 — explicit bounded loop, no
//! recursion; §5 — every buffer is bounded).
//!
//! Lark splits a large event into `sum` frames that share a `message_id`, each
//! carrying its `seq` (0-based) and a slice of the payload. The [`Reassembler`]
//! buffers fragments until every `seq` of a `message_id` has arrived, then
//! concatenates them in order. Unfragmented frames (`sum <= 1`) pass straight
//! through. The in-flight buffer is bounded by [`LARK_FRAME_REASSEMBLY_MAX`]
//! distinct messages (oldest evicted on overflow) and each message by
//! [`LARK_FRAME_MAX_FRAGMENTS`] fragments.

use std::collections::VecDeque;

use tracing::warn;

use super::limits::{LARK_FRAME_MAX_FRAGMENTS, LARK_FRAME_REASSEMBLY_MAX};
use super::pbbp2::{Frame, HEADER_SEQ, HEADER_SUM, Pbbp2Error};

/// One partially-received multi-fragment message.
#[derive(Debug)]
struct Partial {
    message_id: String,
    sum: usize,
    /// `fragments[seq]` is the payload slice for that index, or `None` if not
    /// yet received. Length is exactly `sum`.
    fragments: Vec<Option<Vec<u8>>>,
    /// Count of `Some` slots — the message is complete when `filled == sum`.
    filled: usize,
}

/// Bounded fragment reassembler. One per WS connection (fragment streams never
/// cross connections), driven by [`Reassembler::accept`].
#[derive(Debug, Default)]
pub struct Reassembler {
    /// FIFO of in-progress messages, oldest at the front. Bounded by
    /// [`LARK_FRAME_REASSEMBLY_MAX`].
    partials: VecDeque<Partial>,
}

impl Reassembler {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one decoded frame. Returns `Ok(Some(payload))` when a complete
    /// (possibly single-fragment) message is ready, `Ok(None)` while fragments
    /// are still outstanding, or an error for a malformed fragment header.
    pub fn accept(&mut self, frame: Frame) -> Result<Option<Vec<u8>>, Pbbp2Error> {
        // `sum` defaults to 1 (unfragmented) and `seq` to 0 when absent —
        // matching the Go SDK's `Headers.GetInt` zero default.
        let sum_i64 = frame.header_int(HEADER_SUM).unwrap_or(1);
        let seq_i64 = frame.header_int(HEADER_SEQ).unwrap_or(0);

        // Fast path: an unfragmented frame is its own complete payload.
        if sum_i64 <= 1 {
            return Ok(Some(frame.payload));
        }

        // Validate the fragment header before allocating anything. The i64
        // values are kept for error reporting; the usize narrowings below are
        // lossless because the range is now `[0, LARK_FRAME_MAX_FRAGMENTS]`
        // (§7 — no silent `as`).
        if sum_i64 > i64::from(LARK_FRAME_MAX_FRAGMENTS) || seq_i64 < 0 || seq_i64 >= sum_i64 {
            return Err(Pbbp2Error::BadFragment {
                sum: sum_i64,
                seq: seq_i64,
            });
        }
        let bad = || Pbbp2Error::BadFragment {
            sum: sum_i64,
            seq: seq_i64,
        };
        let sum = usize::try_from(sum_i64).map_err(|_| bad())?;
        let seq = usize::try_from(seq_i64).map_err(|_| bad())?;

        let message_id = frame
            .message_id()
            .ok_or(Pbbp2Error::MissingHeader(super::pbbp2::HEADER_MESSAGE_ID))?
            .to_owned();

        // Locate the in-progress message, or start a new one (evicting the
        // oldest if the buffer is full).
        let idx = if let Some(i) = self
            .partials
            .iter()
            .position(|p| p.message_id == message_id)
        {
            i
        } else {
            if self.partials.len() >= LARK_FRAME_REASSEMBLY_MAX
                && let Some(dropped) = self.partials.pop_front()
            {
                warn!(
                    event = "lark.codec.reassembly_evicted",
                    message_id = %dropped.message_id,
                    filled = dropped.filled,
                    sum = dropped.sum,
                    "evicted oldest incomplete message: reassembly buffer full",
                );
            }
            self.partials.push_back(Partial {
                message_id,
                sum,
                fragments: vec![None; sum],
                filled: 0,
            });
            self.partials.len() - 1
        };

        // A fragment count mismatch for the same message_id is a protocol
        // violation — treat as malformed rather than corrupt the buffer.
        if self.partials[idx].sum != sum {
            self.partials.remove(idx);
            return Err(bad());
        }

        // Record this slice (ignore an exact duplicate seq — at-least-once
        // delivery can re-send a fragment).
        if self.partials[idx].fragments[seq].is_none() {
            self.partials[idx].fragments[seq] = Some(frame.payload);
            self.partials[idx].filled += 1;
        }

        if self.partials[idx].filled < self.partials[idx].sum {
            return Ok(None);
        }

        // Complete: pull it out and concatenate in seq order.
        let done = self
            .partials
            .remove(idx)
            .expect("invariant: reassembly index just located is valid");
        let cap: usize = done.fragments.iter().flatten().map(Vec::len).sum();
        let mut payload = Vec::with_capacity(cap);
        // Every slot is `Some` because `filled == sum`; `flatten` drops the
        // `Option` wrapper in seq order.
        for bytes in done.fragments.into_iter().flatten() {
            payload.extend_from_slice(&bytes);
        }
        Ok(Some(payload))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lark::pbbp2::{HEADER_MESSAGE_ID, Header};

    fn frag(message_id: &str, sum: i64, seq: i64, body: &[u8]) -> Frame {
        Frame {
            headers: vec![
                Header::new(HEADER_MESSAGE_ID, message_id),
                Header::new(HEADER_SUM, sum.to_string()),
                Header::new(HEADER_SEQ, seq.to_string()),
            ],
            payload: body.to_vec(),
            ..Frame::default()
        }
    }

    #[test]
    fn single_fragment_passes_through() {
        let mut r = Reassembler::new();
        let f = Frame {
            payload: b"whole".to_vec(),
            ..Frame::default()
        };
        assert_eq!(r.accept(f).expect("ok"), Some(b"whole".to_vec()));
    }

    #[test]
    fn explicit_sum_one_passes_through() {
        let mut r = Reassembler::new();
        let f = frag("om_1", 1, 0, b"solo");
        assert_eq!(r.accept(f).expect("ok"), Some(b"solo".to_vec()));
    }

    #[test]
    fn three_fragments_in_order() {
        let mut r = Reassembler::new();
        assert_eq!(r.accept(frag("om_a", 3, 0, b"foo")).expect("ok"), None);
        assert_eq!(r.accept(frag("om_a", 3, 1, b"bar")).expect("ok"), None);
        assert_eq!(
            r.accept(frag("om_a", 3, 2, b"baz")).expect("ok"),
            Some(b"foobarbaz".to_vec())
        );
    }

    #[test]
    fn fragments_out_of_order() {
        let mut r = Reassembler::new();
        assert_eq!(r.accept(frag("om_b", 3, 2, b"baz")).expect("ok"), None);
        assert_eq!(r.accept(frag("om_b", 3, 0, b"foo")).expect("ok"), None);
        assert_eq!(
            r.accept(frag("om_b", 3, 1, b"bar")).expect("ok"),
            Some(b"foobarbaz".to_vec())
        );
    }

    #[test]
    fn interleaved_messages_are_independent() {
        let mut r = Reassembler::new();
        assert_eq!(r.accept(frag("om_x", 2, 0, b"x0")).expect("ok"), None);
        assert_eq!(r.accept(frag("om_y", 2, 0, b"y0")).expect("ok"), None);
        assert_eq!(
            r.accept(frag("om_x", 2, 1, b"x1")).expect("ok"),
            Some(b"x0x1".to_vec())
        );
        assert_eq!(
            r.accept(frag("om_y", 2, 1, b"y1")).expect("ok"),
            Some(b"y0y1".to_vec())
        );
    }

    #[test]
    fn duplicate_seq_is_ignored() {
        let mut r = Reassembler::new();
        assert_eq!(r.accept(frag("om_d", 2, 0, b"a")).expect("ok"), None);
        // Re-deliver seq 0 — must not double-count toward completion.
        assert_eq!(r.accept(frag("om_d", 2, 0, b"a")).expect("ok"), None);
        assert_eq!(
            r.accept(frag("om_d", 2, 1, b"b")).expect("ok"),
            Some(b"ab".to_vec())
        );
    }

    #[test]
    fn seq_out_of_range_is_rejected() {
        let mut r = Reassembler::new();
        let err = r.accept(frag("om_e", 2, 2, b"x")).expect_err("bad seq");
        assert!(matches!(err, Pbbp2Error::BadFragment { sum: 2, seq: 2 }));
    }

    #[test]
    fn sum_over_cap_is_rejected() {
        let mut r = Reassembler::new();
        let over = i64::from(LARK_FRAME_MAX_FRAGMENTS) + 1;
        let err = r.accept(frag("om_f", over, 0, b"x")).expect_err("too many");
        assert!(matches!(err, Pbbp2Error::BadFragment { .. }));
    }

    #[test]
    fn missing_message_id_on_fragment_is_rejected() {
        let mut r = Reassembler::new();
        let f = Frame {
            headers: vec![Header::new(HEADER_SUM, "2"), Header::new(HEADER_SEQ, "0")],
            payload: b"x".to_vec(),
            ..Frame::default()
        };
        let err = r.accept(f).expect_err("missing message_id");
        assert!(matches!(err, Pbbp2Error::MissingHeader(_)));
    }

    #[test]
    fn overflow_evicts_oldest_incomplete() {
        let mut r = Reassembler::new();
        // Fill the buffer with MAX distinct incomplete messages.
        for i in 0..LARK_FRAME_REASSEMBLY_MAX {
            let id = format!("om_{i}");
            assert_eq!(r.accept(frag(&id, 2, 0, b"a")).expect("ok"), None);
        }
        // One more distinct message evicts the oldest ("om_0").
        let overflow_id = format!("om_{LARK_FRAME_REASSEMBLY_MAX}");
        assert_eq!(r.accept(frag(&overflow_id, 2, 0, b"a")).expect("ok"), None);
        // "om_0" was evicted: completing it now would have to re-buffer, so its
        // second fragment alone does NOT complete it.
        assert_eq!(r.accept(frag("om_0", 2, 1, b"b")).expect("ok"), None);
    }
}
