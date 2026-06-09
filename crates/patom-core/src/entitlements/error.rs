//! One error type for the `entitlements` module boundary (CLAUDE.md §12).
//!
//! Maps to HTTP **402 Payment Required** — the caller's plan does not cover the
//! action. The HTTP mapping itself lives once, next to every other status, in
//! [`crate::http::HttpError`]'s `into_response` (this codebase has no
//! per-module `IntoResponse`); a `From<LicenseError>` bridge funnels it through
//! the `?` operator there.
//!
//! The agent-count cap is *not* a `LicenseError`: it's enforced in-tx by the
//! agent store and surfaced as `AgentStoreError::AgentLimitReached` (#131).
//! This type covers the boolean feature gate.

use thiserror::Error;

use super::Feature;

#[derive(Debug, Error)]
pub enum LicenseError {
    /// A boolean-gated [`Feature`] the org's plan does not license. Inert
    /// today (the OSS default licenses everything); the seam exists for the
    /// first real gate.
    #[error("feature not licensed: {feature}")]
    FeatureNotLicensed { feature: Feature },
}
