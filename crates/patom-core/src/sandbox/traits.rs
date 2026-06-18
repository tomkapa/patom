//! The [`Sandbox`] backend trait.
//!
//! Bytes-in / bytes-out, Clock-injected, no credentials in the type. The
//! `run_code` tool does all I/O — it resolves inputs, holds every credential,
//! and harvests outputs; the backend only runs confined code and reports what
//! happened. That keeps untrusted code on the far side of an OS/VM boundary from
//! anything sensitive (CLAUDE.md §7).

use std::sync::Arc;

use async_trait::async_trait;

use crate::clock::SharedClock;
use crate::sandbox::error::SandboxError;
use crate::sandbox::types::{RunOutput, RunRequest};

/// A backend that runs one confined program and returns its result.
///
/// Implementors enforce the request's [`crate::sandbox::types::RunTimeout`]
/// in-sandbox and surface a kill as [`SandboxError::Timeout`]; the tool wraps
/// the call in an outer host-side fence as defence in depth. The `clock` is the
/// injected time source (§11) — a backend that needs to stamp or measure uses it
/// rather than `Instant::now`.
#[async_trait]
pub trait Sandbox: std::fmt::Debug + Send + Sync + 'static {
    /// Run `req` to completion (or until killed) and return the captured result.
    /// A non-zero exit is `Ok` — only an untrustworthy run is `Err`.
    async fn run(&self, req: RunRequest, clock: &SharedClock) -> Result<RunOutput, SandboxError>;
}

/// Reference-counted backend handle, threaded through the tool without a generic
/// parameter (mirrors [`crate::clock::SharedClock`] / `SharedAssetStore`).
pub type SharedSandbox = Arc<dyn Sandbox>;
