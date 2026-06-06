//! Patom cloud — paid-tier / billing.
//!
//! This crate holds commercial code (entitlement resolution, charging,
//! Lemon Squeezy integration per issue #131). It is compiled **only** under
//! `patom-server`'s `cloud` feature, so the default OSS / self-host binary
//! never links any of it.
//!
//! Billing state lives in its own `cloud` Postgres schema with its own
//! migration stream ([`run_migrations`]) — see `README.md` for what belongs
//! here versus in `patom-core`.

mod migrate;

pub use migrate::run_migrations;
