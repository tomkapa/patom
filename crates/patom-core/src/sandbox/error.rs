//! Sandbox backend error type. CLAUDE.md §12: one error per module boundary.
//!
//! A non-zero process exit is **not** an error — it is a successful run whose
//! [`crate::sandbox::types::ExitCode`] and stderr the model reads. `SandboxError`
//! is reserved for the cases where the run itself could not be trusted to have
//! produced a faithful result: it was killed, it tried to reach a denied host,
//! it could not be launched, it emitted more than we will carry, or the backend
//! broke.

use thiserror::Error;

use crate::sandbox::egress::EgressHost;

/// Every failure a [`crate::sandbox::Sandbox`] backend may surface.
///
/// The `run_code` tool maps these onto a [`crate::tools::ToolError`] via a local
/// `From` impl so the HTTP/agent boundary stays one hop away.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SandboxError {
    /// The program exceeded its wall-clock budget and was killed. The model
    /// gets a typed timeout rather than a truncated, possibly-partial result.
    #[error("sandbox run exceeded its wall-clock budget")]
    Timeout,

    /// The program attempted to reach a host outside the per-org allowlist.
    /// Default-deny: with an empty allowlist every host lands here.
    #[error("egress to `{host}` denied by org allowlist")]
    EgressDenied { host: EgressHost },

    /// The backend could not launch the sandbox at all (image pull, runtime
    /// registration, executor unreachable). Distinct from [`Self::Backend`] so
    /// dashboards separate "never started" from "started then failed".
    #[error("sandbox failed to start: {0}")]
    Spawn(String),

    /// The program produced more output than the subsystem caps allow, on
    /// stdout/stderr or in a harvested artifact. The run is discarded rather
    /// than returning a silently truncated payload.
    #[error("sandbox output exceeded its byte cap: {0}")]
    OutputTooLarge(String),

    /// The backend itself failed mid-run (executor 5xx, malformed response,
    /// transport error). The string is a short reason; full context is logged
    /// via the §2 tracing handler.
    #[error("sandbox backend failure: {0}")]
    Backend(String),
}
