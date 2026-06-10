//! Shared integration-test plumbing. `mod common;` from each integration test file.
//!
//! `tests/common/` is a directory cargo skips when building per-file test binaries,
//! so its contents are only compiled when a sibling test references them.

#![allow(dead_code, unreachable_pub, clippy::expect_used, clippy::unwrap_used)]

pub mod auth;
pub mod billing;
pub mod embedding;
pub mod harness;
pub mod lang;
pub mod pg;
pub mod rule;
