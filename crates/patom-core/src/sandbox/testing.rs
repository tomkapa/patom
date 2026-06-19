//! A programmable [`Sandbox`] double for tests.
//!
//! Gated so the integration-test crate (compiled separately) can reach it via
//! the `test-catalog` feature, mirroring `InMemoryAssetStore`. Backends that hit
//! a real OS boundary can't run in CI gates; [`FakeSandbox`] lets the tool and
//! turn-loop tests drive every path — canned output, a queued error, a
//! wall-clock kill — deterministically under `tokio::time`'s paused clock.

use std::collections::VecDeque;
use std::sync::Mutex;

use async_trait::async_trait;

use crate::clock::SharedClock;
use crate::sandbox::error::SandboxError;
use crate::sandbox::traits::Sandbox;
use crate::sandbox::types::{ExitCode, RunOutput, RunRequest};

/// One programmed reaction to a `run` call.
#[derive(Debug, Clone)]
enum Programmed {
    /// Return this result immediately.
    Respond(Result<RunOutput, SandboxError>),
    /// Sleep for the request's own [`crate::sandbox::types::RunTimeout`], then
    /// report a kill — models a backend that hit its wall-clock budget. Under a
    /// paused tokio clock the sleep auto-advances, so the test is instant.
    SleepThenTimeout,
}

/// Programmable test double.
///
/// Push reactions in order; `run` consumes them front-to-back and records every
/// [`RunRequest`] it received for later assertions (e.g. that the tool resolved
/// `EgressPolicy::DenyAll`).
///
/// With no reactions queued, `run` returns a clean empty success — the inert
/// default for tests that don't care about the body.
#[derive(Debug, Default)]
pub struct FakeSandbox {
    programmed: Mutex<VecDeque<Programmed>>,
    recorded: Mutex<Vec<RunRequest>>,
}

impl FakeSandbox {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue a canned successful (or non-zero-exit) output.
    #[must_use]
    pub fn push_output(self, output: RunOutput) -> Self {
        self.enqueue(Programmed::Respond(Ok(output)));
        self
    }

    /// Queue a canned backend error.
    #[must_use]
    pub fn push_error(self, err: SandboxError) -> Self {
        self.enqueue(Programmed::Respond(Err(err)));
        self
    }

    /// Queue a wall-clock kill: the next `run` sleeps the request's timeout then
    /// returns [`SandboxError::Timeout`].
    #[must_use]
    pub fn push_timeout(self) -> Self {
        self.enqueue(Programmed::SleepThenTimeout);
        self
    }

    /// Every request `run` has seen, in order. Cloned out so the caller holds no
    /// lock.
    #[must_use]
    pub fn recorded_requests(&self) -> Vec<RunRequest> {
        self.recorded
            .lock()
            .expect("invariant: FakeSandbox recorded mutex not poisoned in tests")
            .clone()
    }

    fn enqueue(&self, p: Programmed) {
        self.programmed
            .lock()
            .expect("invariant: FakeSandbox programmed mutex not poisoned in tests")
            .push_back(p);
    }

    fn record(&self, req: &RunRequest) {
        self.recorded
            .lock()
            .expect("invariant: FakeSandbox recorded mutex not poisoned in tests")
            .push(req.clone());
    }

    fn next_reaction(&self) -> Option<Programmed> {
        self.programmed
            .lock()
            .expect("invariant: FakeSandbox programmed mutex not poisoned in tests")
            .pop_front()
    }
}

#[async_trait]
impl Sandbox for FakeSandbox {
    async fn run(&self, req: RunRequest, _clock: &SharedClock) -> Result<RunOutput, SandboxError> {
        self.record(&req);
        // Pop the lock-guarded reaction *before* any await — never hold a
        // std::sync::Mutex across the suspend point (§7).
        match self.next_reaction() {
            Some(Programmed::Respond(result)) => result,
            Some(Programmed::SleepThenTimeout) => {
                tokio::time::sleep(req.timeout().as_duration()).await;
                Err(SandboxError::Timeout)
            }
            None => Ok(RunOutput::new(
                ExitCode::new(0),
                String::new(),
                String::new(),
                Vec::new(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;
    use crate::clock::TestClock;
    use crate::sandbox::egress::EgressPolicy;
    use crate::sandbox::types::{Language, RunTimeout, SourceCode};

    fn clock() -> SharedClock {
        Arc::new(TestClock::default())
    }

    fn request(timeout: RunTimeout) -> RunRequest {
        RunRequest::new(
            Language::Python,
            SourceCode::try_from("print('hi')").expect("code"),
            Vec::new(),
            EgressPolicy::DenyAll,
            timeout,
        )
        .expect("request")
    }

    #[tokio::test]
    async fn returns_queued_output() {
        let out = RunOutput::new(
            ExitCode::new(0),
            "hello\n".to_owned(),
            String::new(),
            Vec::new(),
        );
        let fake = FakeSandbox::new().push_output(out.clone());
        let got = fake
            .run(request(RunTimeout::default()), &clock())
            .await
            .expect("queued output");
        assert_eq!(got.stdout(), "hello\n");
    }

    #[tokio::test]
    async fn records_received_request() {
        let fake = FakeSandbox::new();
        let _ = fake.run(request(RunTimeout::default()), &clock()).await;
        let seen = fake.recorded_requests();
        assert_eq!(seen.len(), 1);
        assert_eq!(*seen[0].egress(), EgressPolicy::DenyAll);
    }

    #[tokio::test(start_paused = true)]
    async fn sleep_then_timeout_yields_typed_timeout() {
        let timeout = RunTimeout::try_from(Duration::from_secs(5)).expect("timeout");
        let fake = FakeSandbox::new().push_timeout();
        // Paused clock auto-advances to the pending sleep, so this resolves
        // instantly with the typed kill.
        let err = fake
            .run(request(timeout), &clock())
            .await
            .expect_err("wall-clock kill");
        assert_eq!(err, SandboxError::Timeout);
    }
}
