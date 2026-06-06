//! Patom cloud — paid-tier / billing.
//!
//! This crate holds commercial code (entitlement resolution, charging,
//! Lemon Squeezy integration per issue #131). It is compiled **only** under
//! `patom-server`'s `cloud` feature, so the default OSS / self-host binary
//! never links any of it.
//!
//! It is an empty scaffold today; the entitlement implementation arrives in
//! #134 and the billing payload in #131. See `README.md` for what belongs here
//! versus in `patom-core`.
