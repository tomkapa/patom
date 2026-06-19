//! gVisor sibling-pod [`Sandbox`] backend (#218).
//!
//! The Patom process never runs `runsc` itself — that needs root and would
//! co-locate untrusted code with `master_kek` in-process (CLAUDE.md §7). Instead
//! this backend POSTs the [`RunRequest`] (code + base64 inputs + the resolved
//! egress policy) over in-cluster HTTP to a small **executor** Deployment that
//! launches the real gVisor pod (`runtimeClassName: gvisor`, `--network=none`
//! unless the allowlist proxy is wired) and streams back stdout/stderr/exit plus
//! harvested artifacts. The bytes-in / bytes-out trait makes this a clean
//! `tokio::time::timeout`-wrapped HTTP boundary — no `kube`/`k8s-openapi`
//! dependency, just the `reqwest` client already in the tree (§8).

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::clock::SharedClock;
use crate::sandbox::EgressPolicy;
use crate::sandbox::error::SandboxError;
use crate::sandbox::limits::{MAX_OUTPUT_FILES, RUN_CODE_OUTER_TIMEOUT};
use crate::sandbox::traits::Sandbox;
use crate::sandbox::types::{ExitCode, OutputFile, RunOutput, RunRequest, ScratchFileName};
use crate::types::ParseError;

/// Validated in-cluster URL of the sandbox executor service.
///
/// Plain HTTP is permitted: the executor is a sibling service reached over the
/// cluster network (a `NetworkPolicy` fences it), so TLS would only add a
/// mutual-cert burden without a trust gain. Construction enforces an http(s)
/// scheme and a present host so a typo can't silently become a relative path.
#[derive(Clone, PartialEq, Eq)]
pub struct ExecutorUrl(String);

impl ExecutorUrl {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ExecutorUrl {
    type Error = ParseError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        let url = url::Url::parse(&raw).map_err(|_| ParseError::Malformed {
            field: "sandbox_executor_url",
            detail: "not a valid URL",
        })?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(ParseError::Malformed {
                field: "sandbox_executor_url",
                detail: "scheme must be http or https",
            });
        }
        if url.host_str().is_none() {
            return Err(ParseError::Malformed {
                field: "sandbox_executor_url",
                detail: "host is required",
            });
        }
        Ok(Self(raw.trim_end_matches('/').to_owned()))
    }
}

impl std::fmt::Debug for ExecutorUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("ExecutorUrl").field(&self.0).finish()
    }
}

/// gVisor backend over the executor HTTP boundary.
#[derive(Debug, Clone)]
pub struct GvisorSandbox {
    executor: ExecutorUrl,
    http: reqwest::Client,
}

impl GvisorSandbox {
    #[must_use]
    pub fn new(executor: ExecutorUrl, http: reqwest::Client) -> Self {
        Self { executor, http }
    }
}

// ---- wire types ------------------------------------------------------------

#[derive(Serialize)]
struct WireRequest {
    language: &'static str,
    code: String,
    timeout_secs: u64,
    /// Resolved policy: `None` ⇒ `--network=none`; a non-empty list ⇒ proxy.
    egress: Vec<String>,
    inputs: Vec<WireFile>,
}

#[derive(Serialize, Deserialize)]
struct WireFile {
    name: String,
    /// Standard base64 of the file bytes.
    content_b64: String,
}

/// Tagged executor response. The executor reports a kill / egress denial / size
/// overflow as a distinct status so they map onto the typed [`SandboxError`]
/// rather than a generic non-zero exit.
#[derive(Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum WireResponse {
    Ok {
        exit_code: i32,
        stdout: String,
        stderr: String,
        #[serde(default)]
        artifacts: Vec<WireFile>,
    },
    Timeout,
    EgressDenied {
        host: String,
    },
    OutputTooLarge {
        detail: String,
    },
    Error {
        detail: String,
    },
}

impl GvisorSandbox {
    fn build_wire(req: &RunRequest) -> WireRequest {
        let egress = match req.egress() {
            EgressPolicy::DenyAll => Vec::new(),
            EgressPolicy::Allow(hosts) => hosts.iter().map(|h| h.as_str().to_owned()).collect(),
        };
        let inputs = req
            .inputs()
            .iter()
            .map(|f| WireFile {
                name: f.name().as_str().to_owned(),
                content_b64: BASE64.encode(f.bytes()),
            })
            .collect();
        WireRequest {
            language: req.language().as_str(),
            code: req.code().as_str().to_owned(),
            timeout_secs: req.timeout().as_duration().as_secs(),
            egress,
            inputs,
        }
    }

    fn decode_response(resp: WireResponse) -> Result<RunOutput, SandboxError> {
        match resp {
            WireResponse::Ok {
                exit_code,
                stdout,
                stderr,
                artifacts,
            } => {
                let harvested = decode_artifacts(artifacts)?;
                Ok(RunOutput::new(
                    ExitCode::new(exit_code),
                    stdout,
                    stderr,
                    harvested,
                ))
            }
            WireResponse::Timeout => Err(SandboxError::Timeout),
            WireResponse::EgressDenied { host } => {
                // Re-validate the host through the same newtype so a malformed
                // executor reply can't smuggle junk into the error.
                let host = crate::sandbox::EgressHost::try_from(host)
                    .map_err(|e| SandboxError::Backend(format!("bad egress host in reply: {e}")))?;
                Err(SandboxError::EgressDenied { host })
            }
            WireResponse::OutputTooLarge { detail } => Err(SandboxError::OutputTooLarge(detail)),
            WireResponse::Error { detail } => Err(SandboxError::Backend(detail)),
        }
    }
}

/// Decode the executor's base64 artifacts into validated [`OutputFile`]s.
fn decode_artifacts(wire: Vec<WireFile>) -> Result<Vec<OutputFile>, SandboxError> {
    // §5: bound the batch on entry — never trust the executor's count, even
    // though the executor caps it too.
    if wire.len() > MAX_OUTPUT_FILES {
        return Err(SandboxError::OutputTooLarge(format!(
            "executor returned {} artifacts (max {MAX_OUTPUT_FILES})",
            wire.len()
        )));
    }
    let mut out = Vec::with_capacity(wire.len());
    for f in wire {
        let bytes = BASE64
            .decode(f.content_b64.as_bytes())
            .map(Bytes::from)
            .map_err(|e| SandboxError::Backend(format!("artifact base64: {e}")))?;
        let name = ScratchFileName::try_from(f.name)
            .map_err(|e| SandboxError::Backend(format!("artifact name: {e}")))?;
        let file = OutputFile::new(name, bytes)
            .map_err(|e| SandboxError::OutputTooLarge(e.to_string()))?;
        out.push(file);
    }
    Ok(out)
}

#[async_trait]
impl Sandbox for GvisorSandbox {
    #[tracing::instrument(
        name = "sandbox.gvisor.run",
        skip_all,
        fields(patom.sandbox.language = req.language().as_str()),
    )]
    async fn run(&self, req: RunRequest, _clock: &SharedClock) -> Result<RunOutput, SandboxError> {
        let wire = Self::build_wire(&req);
        let endpoint = format!("{}/run", self.executor.as_str());
        let send = self.http.post(&endpoint).json(&wire).send();
        // §5: the executor enforces the in-sandbox kill; this outer fence guards
        // against an unresponsive executor (the tool wraps the whole call again).
        let resp = match tokio::time::timeout(RUN_CODE_OUTER_TIMEOUT, send).await {
            Err(_) => {
                let e = SandboxError::Backend("executor request timed out".to_owned());
                tracing::error!(error = ?e, event = "sandbox.gvisor.send.timeout");
                return Err(e);
            }
            Ok(Err(e)) => {
                let e = SandboxError::Spawn(format!("executor unreachable: {e}"));
                tracing::error!(error = ?e, event = "sandbox.gvisor.send.failed");
                return Err(e);
            }
            Ok(Ok(r)) => r,
        };
        if !resp.status().is_success() {
            let e = SandboxError::Backend(format!("executor returned status {}", resp.status()));
            tracing::error!(error = ?e, event = "sandbox.gvisor.status");
            return Err(e);
        }
        // §5: the send timeout covers headers only — fence the body read too so a
        // wedged executor that stops mid-stream can't pin the task.
        let body: WireResponse =
            match tokio::time::timeout(RUN_CODE_OUTER_TIMEOUT, resp.json()).await {
                Err(_) => {
                    let e = SandboxError::Backend("executor reply decode timed out".to_owned());
                    tracing::error!(error = ?e, event = "sandbox.gvisor.decode.timeout");
                    return Err(e);
                }
                Ok(Err(e)) => {
                    let e = SandboxError::Backend(format!("executor reply decode: {e}"));
                    tracing::error!(error = ?e, event = "sandbox.gvisor.decode.failed");
                    return Err(e);
                }
                Ok(Ok(body)) => body,
            };
        Self::decode_response(body).inspect_err(|e| {
            tracing::error!(error = ?e, event = "sandbox.gvisor.decode_response.failed");
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn executor_url_accepts_in_cluster_http() {
        let u = ExecutorUrl::try_from("http://sandbox-executor.patom.svc:8080".to_owned())
            .expect("valid");
        assert_eq!(u.as_str(), "http://sandbox-executor.patom.svc:8080");
    }

    #[test]
    fn executor_url_strips_trailing_slash() {
        let u = ExecutorUrl::try_from("http://exec.local/".to_owned()).expect("valid");
        assert_eq!(u.as_str(), "http://exec.local");
    }

    #[test]
    fn executor_url_rejects_non_http_scheme() {
        assert!(ExecutorUrl::try_from("ftp://exec.local".to_owned()).is_err());
        assert!(ExecutorUrl::try_from("not a url".to_owned()).is_err());
    }

    #[test]
    fn decode_ok_response_builds_run_output() {
        let resp = WireResponse::Ok {
            exit_code: 0,
            stdout: "hi\n".to_owned(),
            stderr: String::new(),
            artifacts: vec![WireFile {
                name: "out.txt".to_owned(),
                content_b64: BASE64.encode(b"data"),
            }],
        };
        let out = GvisorSandbox::decode_response(resp).expect("ok");
        assert_eq!(out.stdout(), "hi\n");
        assert_eq!(out.artifacts().len(), 1);
        assert_eq!(out.artifacts()[0].bytes().as_ref(), b"data");
    }

    #[test]
    fn decode_timeout_and_egress_map_to_typed_errors() {
        assert_eq!(
            GvisorSandbox::decode_response(WireResponse::Timeout).expect_err("timeout"),
            SandboxError::Timeout
        );
        let err = GvisorSandbox::decode_response(WireResponse::EgressDenied {
            host: "api.example.com".to_owned(),
        })
        .expect_err("egress denied");
        assert!(matches!(err, SandboxError::EgressDenied { .. }));
    }
}
