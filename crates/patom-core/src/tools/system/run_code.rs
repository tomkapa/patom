//! `run_code` — confined shell/Python execution (#218).
//!
//! The agent's first *general compute* tool. It runs untrusted shell or Python
//! behind a [`Sandbox`] backend (an external OS/VM boundary — CLAUDE.md §7) and
//! returns the program's stdout, a non-zero-exit notice, and any harvested
//! artifacts as tenant-private asset references.
//!
//! Data flow is **fetch-and-inject**: Patom (trusted) holds every credential and
//! does all I/O. Inputs are resolved and validated host-side, then written into
//! a credential-free scratch dir *before* exec; the program runs with no
//! network unless the org has opted into an egress allowlist; outputs are
//! validated and stored to a private `sandbox/{org}/{thread}/…` key on the way
//! back. The tool never hands the sandbox a secret and never trusts a byte it
//! produces without re-validating it.

use std::fmt::Write as _;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::assets::{AssetContentType, ObjectKey, SharedAssetStore, validate_attachment_bytes};
use crate::auth::OrgId;
use crate::clock::SharedClock;
use crate::sandbox::limits::{MAX_INPUT_FILE_BYTES, RUN_CODE_OUTER_TIMEOUT};
use crate::sandbox::{
    EgressPolicy, InputFile, Language, RunOutput, RunRequest, RunTimeout, SandboxError,
    ScratchFileName, SharedOrgEgressStore, SharedSandbox, SourceCode,
};
use crate::threads::ThreadId;
use crate::types::ToolName;

use super::super::limits::{TOOL_RESULT_MAX_BYTES, truncate_to_char_boundary};
use super::super::modes::RequestKindModes;
use super::super::traits::{Tool, ToolCallContext, ToolError};

const TOOL_NAME: &str = "run_code";

const TOOL_DESCRIPTION: &str = "Run a short shell or Python program in an isolated sandbox and \
    return its stdout. Use this to transform data, do calculations, or produce files \
    (spreadsheets, documents, charts) from data you already have. Python ships with pandas, \
    numpy, matplotlib, openpyxl, python-docx and python-pptx pre-installed.\n\
    \n\
    The sandbox has NO network access by default and starts empty. To work on an existing \
    file, pass it in `inputs` (each `{name, asset_key}` stages that stored asset into the \
    working directory under `name`). Any file your program writes to the working directory is \
    returned to chat as a downloadable artifact. A non-zero exit is not an error — read stderr \
    and try again.\n\
    \n\
    Arguments: `language` (\"python\" or \"shell\"), `code` (the program), optional `inputs` \
    (files to stage in), optional `timeout_secs` (1..=30).";

/// One input file the model asks Patom to stage into the scratch dir: a stored
/// asset (`asset_key`) made available under the leaf name `name`.
#[derive(Debug, Deserialize)]
struct InputRef {
    name: String,
    asset_key: String,
}

#[derive(Debug, Deserialize)]
struct Input {
    language: String,
    code: String,
    #[serde(default)]
    inputs: Vec<InputRef>,
    #[serde(default)]
    timeout_secs: Option<u64>,
}

/// Confined code-execution tool. Holds the backend plus the trusted I/O
/// dependencies it brokers on the program's behalf (CLAUDE.md §9 — built once,
/// threaded as handles).
pub struct RunCodeTool {
    name: ToolName,
    description: &'static str,
    input_schema: Arc<Value>,
    sandbox: SharedSandbox,
    assets: SharedAssetStore,
    egress: SharedOrgEgressStore,
    clock: SharedClock,
}

impl std::fmt::Debug for RunCodeTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunCodeTool").finish_non_exhaustive()
    }
}

impl RunCodeTool {
    #[must_use]
    pub fn new(
        sandbox: SharedSandbox,
        assets: SharedAssetStore,
        egress: SharedOrgEgressStore,
        clock: SharedClock,
    ) -> Self {
        let name = ToolName::try_from(TOOL_NAME).expect("invariant: run_code is a valid name");
        let input_schema = Arc::new(json!({
            "type": "object",
            "required": ["language", "code"],
            "properties": {
                "language": { "type": "string", "enum": ["python", "shell"] },
                "code": { "type": "string" },
                "inputs": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "required": ["name", "asset_key"],
                        "properties": {
                            "name": { "type": "string" },
                            "asset_key": { "type": "string" },
                        },
                        "additionalProperties": false,
                    },
                },
                "timeout_secs": { "type": "integer", "minimum": 1, "maximum": 30 },
            },
            "additionalProperties": false,
        }));
        Self {
            name,
            description: TOOL_DESCRIPTION,
            input_schema,
            sandbox,
            assets,
            egress,
            clock,
        }
    }

    /// Resolve the org's egress allowlist into the per-run policy (empty ⇒ deny).
    async fn resolve_egress(&self, org: OrgId) -> Result<EgressPolicy, ToolError> {
        let allowlist = self.egress.allowlist_for_org(org).await.map_err(|e| {
            tracing::error!(error = ?e, event = "run_code.egress_lookup.failed");
            ToolError::Backend(format!("run_code: egress lookup: {e}"))
        })?;
        Ok(allowlist.to_policy())
    }

    /// Fetch-and-inject: pull each referenced asset's bytes and pair it with a
    /// validated scratch name. All I/O happens here, host-side, before exec.
    async fn stage_inputs(&self, refs: Vec<InputRef>) -> Result<Vec<InputFile>, ToolError> {
        let mut staged = Vec::with_capacity(refs.len());
        for r in refs {
            let name = ScratchFileName::try_from(r.name)
                .map_err(|e| ToolError::InvalidInput(format!("run_code: input name: {e}")))?;
            let key = ObjectKey::try_from(r.asset_key.as_str())
                .map_err(|e| ToolError::InvalidInput(format!("run_code: input asset_key: {e}")))?;
            let bytes = self
                .assets
                .get(key, MAX_INPUT_FILE_BYTES)
                .await
                .map_err(|e| ToolError::InvalidInput(format!("run_code: input fetch: {e}")))?;
            let file = InputFile::new(name, bytes)
                .map_err(|e| ToolError::InvalidInput(format!("run_code: input: {e}")))?;
            staged.push(file);
        }
        Ok(staged)
    }

    /// Run the request behind the host-side wall-clock fence. The backend
    /// enforces the in-sandbox kill (precise [`SandboxError::Timeout`]); this
    /// outer fence only catches a backend that hangs past the harvest budget.
    async fn run_confined(&self, req: RunRequest) -> Result<RunOutput, ToolError> {
        let Ok(result) =
            tokio::time::timeout(RUN_CODE_OUTER_TIMEOUT, self.sandbox.run(req, &self.clock)).await
        else {
            return Err(ToolError::Backend(
                "run_code: sandbox did not return within the host time budget".to_owned(),
            ));
        };
        result.map_err(ToolError::from)
    }

    /// Validate and store one harvested artifact under a tenant-private key,
    /// returning its public reference. The bytes are re-validated against the
    /// claimed type (§5) — we never trust an output the program produced.
    async fn store_artifact(
        &self,
        org: OrgId,
        thread: Option<ThreadId>,
        name: &ScratchFileName,
        bytes: bytes::Bytes,
    ) -> Result<String, ToolError> {
        let content_type =
            AssetContentType::from_attachment_extension(name.as_str()).ok_or_else(|| {
                ToolError::InvalidInput(format!(
                    "run_code: artifact `{}` has an unsupported file type",
                    name.as_str()
                ))
            })?;
        validate_attachment_bytes(&bytes, content_type, content_type.attachment_max_bytes())
            .map_err(|e| ToolError::Backend(format!("run_code: artifact validate: {e}")))?;
        let key = sandbox_artifact_key(org, thread, content_type.extension())
            .map_err(|e| ToolError::Backend(format!("run_code: artifact key: {e}")))?;
        let url = self
            .assets
            .put(key, bytes, content_type)
            .await
            .map_err(|e| ToolError::Backend(format!("run_code: artifact store: {e}")))?;
        Ok(format!("{}: {}", name.as_str(), url.as_str()))
    }
}

/// Tenant-private object key for a sandbox artifact: `sandbox/{org}/{thread}/{uuid}.{ext}`.
///
/// Deliberately *not* the public `attachments/` prefix — sandbox output is
/// scoped to the org+thread and rendered through an authenticated read, never a
/// guessable public URL. The background path (no thread) buckets under
/// `_background` so the key is always well-formed.
fn sandbox_artifact_key(
    org: OrgId,
    thread: Option<ThreadId>,
    ext: &str,
) -> Result<ObjectKey, crate::types::ParseError> {
    let thread_seg = thread.map_or_else(|| "_background".to_owned(), |t| t.as_uuid().to_string());
    let raw = format!(
        "sandbox/{org}/{thread_seg}/{file}.{ext}",
        file = uuid::Uuid::new_v4()
    );
    ObjectKey::try_from(raw.as_str())
}

/// Map a backend failure onto the tool boundary error (§12). Model-actionable
/// failures (it can simplify code, trim output, or avoid a host) surface as
/// `InvalidInput` so the next turn can self-correct; genuine infrastructure
/// faults surface as `Backend`.
impl From<SandboxError> for ToolError {
    fn from(e: SandboxError) -> Self {
        match e {
            SandboxError::Timeout => {
                Self::InvalidInput("run_code: execution exceeded the wall-clock limit".to_owned())
            }
            SandboxError::EgressDenied { host } => Self::InvalidInput(format!(
                "run_code: network egress to `{host}` is not allowed for this org"
            )),
            SandboxError::OutputTooLarge(detail) => {
                Self::InvalidInput(format!("run_code: {detail}"))
            }
            SandboxError::Spawn(detail) => {
                Self::Backend(format!("run_code: sandbox failed to start: {detail}"))
            }
            SandboxError::Backend(detail) => Self::Backend(format!("run_code: {detail}")),
        }
    }
}

/// Render the run result the model reads: stdout, a non-zero-exit notice, any
/// stderr, and the harvested artifact references. Capped to the tool-result
/// byte budget; #185 offload recovers a body that overflows it.
fn render(output: &RunOutput, artifacts: &[String]) -> String {
    let mut out = String::with_capacity(output.stdout().len() + 256);
    out.push_str(output.stdout());
    if !output.exit_code().is_success() {
        let _ = write!(
            out,
            "\n\n[process exited with status {}]",
            output.exit_code().get()
        );
        if !output.stderr().is_empty() {
            let _ = write!(out, "\nstderr:\n{}", output.stderr());
        }
    }
    if !artifacts.is_empty() {
        out.push_str("\n\nArtifacts:\n");
        for a in artifacts {
            let _ = writeln!(out, "- {a}");
        }
    }
    truncate_to_char_boundary(&mut out, TOOL_RESULT_MAX_BYTES);
    out
}

impl RunCodeTool {
    #[tracing::instrument(
        skip_all,
        name = "tool.run_code",
        fields(
            patom.tool = "run_code",
            patom.org.id = %ctx.org_id,
            patom.run_code.language = tracing::field::Empty,
            patom.run_code.outcome = tracing::field::Empty,
        ),
    )]
    async fn handle(&self, input: Input, ctx: &ToolCallContext) -> Result<String, ToolError> {
        let language = Language::try_from(input.language.as_str())
            .map_err(|e| ToolError::InvalidInput(format!("run_code: language: {e}")))?;
        tracing::Span::current().record("patom.run_code.language", language.as_str());
        let code = SourceCode::try_from(input.code)
            .map_err(|e| ToolError::InvalidInput(format!("run_code: code: {e}")))?;
        let timeout = match input.timeout_secs {
            None => RunTimeout::default(),
            Some(secs) => RunTimeout::try_from(std::time::Duration::from_secs(secs))
                .map_err(|e| ToolError::InvalidInput(format!("run_code: timeout_secs: {e}")))?,
        };

        let egress = self.resolve_egress(ctx.org_id).await?;
        let inputs = self.stage_inputs(input.inputs).await?;
        let req = RunRequest::new(language, code, inputs, egress, timeout)
            .map_err(|e| ToolError::InvalidInput(format!("run_code: request: {e}")))?;

        let output = self.run_confined(req).await.inspect_err(|_| {
            set_outcome("run_failed");
        })?;

        let mut artifacts = Vec::with_capacity(output.artifacts().len());
        for artifact in output.artifacts() {
            let reference = self
                .store_artifact(
                    ctx.org_id,
                    ctx.thread_id,
                    artifact.name(),
                    artifact.bytes().clone(),
                )
                .await
                .inspect_err(|_| set_outcome("harvest_failed"))?;
            artifacts.push(reference);
        }

        set_outcome(if output.exit_code().is_success() {
            "ok"
        } else {
            "nonzero_exit"
        });
        Ok(render(&output, &artifacts))
    }
}

/// Record the `patom.run_code.outcome` span field. Variants: `ok`,
/// `nonzero_exit`, `run_failed`, `harvest_failed`.
fn set_outcome(label: &'static str) {
    tracing::Span::current().record("patom.run_code.outcome", label);
}

#[async_trait]
impl Tool for RunCodeTool {
    fn name(&self) -> &ToolName {
        &self.name
    }

    fn description(&self) -> &str {
        self.description
    }

    fn input_schema(&self) -> Arc<Value> {
        self.input_schema.clone()
    }

    /// Untrusted execution with side effects (stored artifacts) — never run two
    /// concurrently under one turn.
    fn concurrency_safe(&self) -> bool {
        false
    }

    /// Running code is a deliberate, user-facing action. Reflection and
    /// resolution turns reason over the agent's own memory and have no use for
    /// it — keep it off those seams.
    fn modes(&self) -> RequestKindModes {
        RequestKindModes::NORMAL
    }

    async fn execute(&self, input: Value, ctx: &ToolCallContext) -> Result<String, ToolError> {
        let parsed: Input = serde_json::from_value(input)?;
        self.handle(parsed, ctx).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    use crate::assets::InMemoryAssetStore;
    use crate::auth::UserId;
    use crate::clock::TestClock;
    use crate::runtime::{ClaimKey, PromptRequestId, RequestKindPayload};
    use crate::sandbox::{ExitCode, FakeSandbox, InMemoryOrgEgressStore, OutputFile};
    use crate::types::Participant;

    fn ctx(org: OrgId, thread: Option<ThreadId>) -> ToolCallContext {
        let rid = PromptRequestId::new();
        ToolCallContext {
            claim_key: ClaimKey::new(),
            thread_id: thread,
            state_id: None,
            viewer: Participant::system(),
            root_request_id: rid,
            request_id: rid,
            kind_payload: RequestKindPayload::Normal {},
            acting_user_id: UserId::new(),
            org_id: org,
        }
    }

    fn deps(sandbox: Arc<FakeSandbox>) -> (RunCodeTool, Arc<InMemoryAssetStore>) {
        let assets = Arc::new(InMemoryAssetStore::new("https://assets.test.invalid"));
        let egress = Arc::new(InMemoryOrgEgressStore::new());
        let clock = Arc::new(TestClock::default());
        let tool = RunCodeTool::new(sandbox, assets.clone(), egress, clock);
        (tool, assets)
    }

    #[tokio::test]
    async fn returns_stdout_through_the_tool_boundary() {
        let out = RunOutput::new(
            ExitCode::new(0),
            "42\n".to_owned(),
            String::new(),
            Vec::new(),
        );
        let fake = Arc::new(FakeSandbox::new().push_output(out));
        let (tool, _) = deps(fake);
        let res = tool
            .execute(
                json!({ "language": "python", "code": "print(42)" }),
                &ctx(OrgId::new(), None),
            )
            .await
            .expect("ok");
        assert!(res.contains("42"), "stdout missing: {res}");
    }

    #[tokio::test(start_paused = true)]
    async fn wall_clock_kill_surfaces_typed_error() {
        let fake = Arc::new(FakeSandbox::new().push_timeout());
        let (tool, _) = deps(fake);
        let err = tool
            .execute(
                json!({ "language": "python", "code": "x=1\nwhile True:\n  pass", "timeout_secs": 5 }),
                &ctx(OrgId::new(), None),
            )
            .await
            .expect_err("timeout");
        assert!(matches!(err, ToolError::InvalidInput(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn empty_allowlist_resolves_to_deny_all() {
        let fake = Arc::new(FakeSandbox::new());
        let (tool, _) = deps(fake.clone());
        let _ = tool
            .execute(
                json!({ "language": "shell", "code": "echo hi" }),
                &ctx(OrgId::new(), None),
            )
            .await
            .expect("ok");
        let reqs = fake.recorded_requests();
        assert_eq!(reqs.len(), 1);
        assert_eq!(*reqs[0].egress(), EgressPolicy::DenyAll);
    }

    #[tokio::test]
    async fn artifact_is_stored_under_tenant_private_key() {
        let name = ScratchFileName::try_from("out.csv").expect("name");
        let artifact = OutputFile::new(name, Bytes::from_static(b"a,b\n1,2\n")).expect("artifact");
        let out = RunOutput::new(
            ExitCode::new(0),
            "done\n".to_owned(),
            String::new(),
            vec![artifact],
        );
        let fake = Arc::new(FakeSandbox::new().push_output(out));
        let (tool, assets) = deps(fake);
        let org = OrgId::new();
        let thread = ThreadId::new();
        let res = tool
            .execute(
                json!({ "language": "python", "code": "open('out.csv','w')" }),
                &ctx(org, Some(thread)),
            )
            .await
            .expect("ok");
        assert!(
            res.contains(&format!("sandbox/{org}/{thread}")),
            "tenant-private key missing from result: {res}"
        );
        assert_eq!(assets.len().await, 1, "artifact not stored");
    }

    #[test]
    fn artifact_key_is_tenant_private_and_extensioned() {
        let org = OrgId::new();
        let thread = ThreadId::new();
        let key = sandbox_artifact_key(org, Some(thread), "png").expect("key");
        assert!(
            key.as_str()
                .starts_with(&format!("sandbox/{org}/{thread}/"))
        );
        assert!(key.as_str().ends_with(".png"));
    }

    #[test]
    fn artifact_key_background_path_is_well_formed() {
        let key = sandbox_artifact_key(OrgId::new(), None, "txt").expect("key");
        assert!(key.as_str().contains("/_background/"));
        assert!(key.as_str().ends_with(".txt"));
    }

    #[test]
    fn sandbox_error_maps_to_tool_error() {
        assert!(matches!(
            ToolError::from(SandboxError::Timeout),
            ToolError::InvalidInput(_)
        ));
        assert!(matches!(
            ToolError::from(SandboxError::Backend("boom".to_owned())),
            ToolError::Backend(_)
        ));
        let host = crate::sandbox::EgressHost::try_from("api.example.com").expect("host");
        assert!(matches!(
            ToolError::from(SandboxError::EgressDenied { host }),
            ToolError::InvalidInput(_)
        ));
    }
}
