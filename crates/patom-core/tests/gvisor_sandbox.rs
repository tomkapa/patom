//! #218 live-backend integration tests for the gVisor sandbox.
//!
//! These exercise the REAL `GvisorSandbox` against a running executor sibling —
//! so they need infrastructure CI gates do not have. They are doubly guarded:
//!
//! * `#[ignore]` — a plain `cargo test` skips them.
//! * `PATOM_RUN_GVISOR_TESTS=1` — even `cargo test -- --ignored` is a no-op
//!   unless this is set, and `PATOM_SANDBOX_EXECUTOR_URL` points at the
//!   executor.
//!
//! A post-Ansible infra lane sets both and runs:
//!   `PATOM_RUN_GVISOR_TESTS=1 cargo test --test gvisor_sandbox -- --ignored`

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::time::Duration;

use patom::clock::{SharedClock, SystemClock};
use patom::sandbox::{
    EgressPolicy, ExecutorUrl, GvisorSandbox, Language, RunRequest, RunTimeout, Sandbox,
    SandboxError, SourceCode,
};

/// Build the live backend, or `None` when the guards aren't satisfied (so the
/// test body becomes an inert pass). Returns the backend plus a real clock.
fn live_backend() -> Option<(GvisorSandbox, SharedClock)> {
    if std::env::var("PATOM_RUN_GVISOR_TESTS").ok().as_deref() != Some("1") {
        return None;
    }
    let url = std::env::var("PATOM_SANDBOX_EXECUTOR_URL").ok()?;
    let executor = ExecutorUrl::try_from(url).expect("PATOM_SANDBOX_EXECUTOR_URL must be valid");
    let http = reqwest::Client::builder()
        .timeout(Duration::from_mins(1))
        .build()
        .expect("http client");
    Some((GvisorSandbox::new(executor, http), SystemClock::shared()))
}

fn request(code: &str, egress: EgressPolicy, timeout: RunTimeout) -> RunRequest {
    RunRequest::new(
        Language::Python,
        SourceCode::try_from(code).expect("code"),
        Vec::new(),
        egress,
        timeout,
    )
    .expect("request")
}

#[tokio::test]
#[ignore = "needs a live gVisor executor; set PATOM_RUN_GVISOR_TESTS=1"]
async fn python_stdout_round_trips() {
    let Some((sandbox, clock)) = live_backend() else {
        return;
    };
    let out = sandbox
        .run(
            request(
                "print('hello from gvisor')",
                EgressPolicy::DenyAll,
                RunTimeout::default(),
            ),
            &clock,
        )
        .await
        .expect("run ok");
    assert!(out.exit_code().is_success(), "stderr: {}", out.stderr());
    assert!(
        out.stdout().contains("hello from gvisor"),
        "got: {}",
        out.stdout()
    );
}

#[tokio::test]
#[ignore = "needs a live gVisor executor; set PATOM_RUN_GVISOR_TESTS=1"]
async fn wall_clock_kill_is_reported() {
    let Some((sandbox, clock)) = live_backend() else {
        return;
    };
    let timeout = RunTimeout::try_from(Duration::from_secs(2)).expect("timeout");
    let err = sandbox
        .run(
            request("while True:\n    pass", EgressPolicy::DenyAll, timeout),
            &clock,
        )
        .await
        .expect_err("a spinning program must be killed");
    assert_eq!(err, SandboxError::Timeout);
}

#[tokio::test]
#[ignore = "needs a live gVisor executor; set PATOM_RUN_GVISOR_TESTS=1"]
async fn default_deny_blocks_network() {
    let Some((sandbox, clock)) = live_backend() else {
        return;
    };
    // With DenyAll the sandbox runs `--network=none`; an outbound connection must
    // fail. The program exits non-zero (a successful *run* reporting the failure)
    // — the point is the request never reaches the network.
    let code = "import socket\ntry:\n    socket.create_connection(('1.1.1.1', 80), timeout=3)\n    print('REACHED')\nexcept OSError as e:\n    print('blocked:', e)";
    let out = sandbox
        .run(
            request(code, EgressPolicy::DenyAll, RunTimeout::default()),
            &clock,
        )
        .await
        .expect("run ok");
    assert!(
        !out.stdout().contains("REACHED"),
        "network must be blocked under deny-all: {}",
        out.stdout()
    );
}

#[tokio::test]
#[ignore = "needs a live gVisor executor; set PATOM_RUN_GVISOR_TESTS=1"]
async fn openpyxl_artifact_is_harvested() {
    let Some((sandbox, clock)) = live_backend() else {
        return;
    };
    let code = "import openpyxl\nwb = openpyxl.Workbook()\nwb.active['A1'] = 'hi'\nwb.save('out.xlsx')\nprint('wrote out.xlsx')";
    let out = sandbox
        .run(
            request(code, EgressPolicy::DenyAll, RunTimeout::default()),
            &clock,
        )
        .await
        .expect("run ok");
    assert!(out.exit_code().is_success(), "stderr: {}", out.stderr());
    assert!(
        out.artifacts()
            .iter()
            .any(|a| a.name().as_str() == "out.xlsx"),
        "expected out.xlsx artifact, got: {:?}",
        out.artifacts()
            .iter()
            .map(|a| a.name().as_str())
            .collect::<Vec<_>>()
    );
}
