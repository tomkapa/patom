//! Re-export of the crate-shared hex primitives, kept at `slack::hex` so
//! the Slack signed-token modules (`verify.rs`, `oauth.rs`, `link_token.rs`,
//! `connect_link.rs`) can keep referring to `super::hex::{encode_32,
//! decode_32}`. The implementation lives in [`crate::hex`], shared with the
//! Lark and Discord connect-link modules.

pub(super) use crate::hex::{decode_32, encode_32};
