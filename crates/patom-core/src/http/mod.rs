//! HTTP surface (axum). Tower's `TraceLayer` opens a root span per request per
//! CLAUDE.md §2; handlers stay thin and call into `prompt::*` traits.

mod auth_layer;
mod csrf;
mod error;
mod limits;
mod membership_cache;
mod routes;
mod state;

pub use membership_cache::MembershipCache;

pub use error::HttpError;
pub use routes::router;
pub use state::AppState;
