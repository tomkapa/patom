//! One error type for the `entitlements` module boundary (CLAUDE.md §12).
//!
//! Both variants map to HTTP **402 Payment Required** — the caller's plan does
//! not cover the action. The HTTP mapping itself lives once, next to every
//! other status, in [`crate::http::HttpError`]'s `into_response` (this codebase
//! has no per-module `IntoResponse`); a `From<LicenseError>` bridge funnels
//! these through the `?` operator there.

use thiserror::Error;

use super::Feature;

#[derive(Debug, Error)]
pub enum LicenseError {
    /// A boolean-gated [`Feature`] the org's plan does not license. Inert
    /// today (the OSS default licenses everything); the seam exists for the
    /// first real gate.
    #[error("feature not licensed: {feature}")]
    FeatureNotLicensed { feature: Feature },

    /// The org is at its agent ceiling. `limit` is the cap that was hit, for
    /// the upgrade prompt the FE renders on the 402.
    #[error("agent limit reached: {limit}")]
    AgentLimitReached { limit: u32 },
}
