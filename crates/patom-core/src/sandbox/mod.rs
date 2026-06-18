//! Confined code execution for the agent `run_code` tool.
//!
//! Untrusted shell/Python runs behind an external OS/VM boundary (CLAUDE.md §7
//! forbids in-process `unsafe` tricks), never co-located with credentials. The
//! [`Sandbox`] trait is bytes-in / bytes-out: Patom (trusted) does all I/O,
//! resolves and validates inputs, then hands confined code a credential-free
//! scratch dir; outputs are harvested and validated on the way back out.
//!
//! This module owns the boundary types, the trait, and the egress policy.
//! Backends live beside it: a programmable [`testing::FakeSandbox`] for tests
//! and (later) a `GvisorSandbox` that drives a sibling pod.

pub mod egress;
pub mod error;
pub mod gvisor;
pub mod limits;
pub mod traits;
pub mod types;

#[cfg(any(test, feature = "test-catalog"))]
pub mod testing;

pub use egress::{
    EgressAllowlist, EgressHost, EgressPolicy, EgressStoreError, OrgEgressStore, PgOrgEgressStore,
    SharedOrgEgressStore,
};

#[cfg(any(test, feature = "test-catalog"))]
pub use egress::InMemoryOrgEgressStore;
pub use error::SandboxError;
pub use gvisor::{ExecutorUrl, GvisorSandbox};
#[cfg(any(test, feature = "test-catalog"))]
pub use testing::FakeSandbox;
pub use traits::{Sandbox, SharedSandbox};
pub use types::{
    ExitCode, InputFile, Language, OutputFile, RunOutput, RunRequest, RunTimeout, ScratchFileName,
    SourceCode,
};
