//! Boundary newtypes for the sandbox (CLAUDE.md §1).
//!
//! Every value that carries an invariant — the program text, the requested
//! timeout, a scratch file name — enters the typed world exactly once, here,
//! through `TryFrom`. Bare `String` / `Duration` never reach the backend.
//!
//! The scalars here ([`Language`], [`SourceCode`], [`RunTimeout`], [`ExitCode`],
//! [`ScratchFileName`]) are assembled into the bytes-in / bytes-out containers
//! [`RunRequest`] and [`RunOutput`] that cross the [`crate::sandbox::Sandbox`]
//! trait. Every container validates its own caps on construction.

use std::time::Duration;

use bytes::Bytes;

use crate::sandbox::egress::EgressPolicy;
use crate::sandbox::limits::{
    MAX_CODE_BYTES, MAX_INPUT_FILE_BYTES, MAX_INPUT_FILES, MAX_OUTPUT_FILE_BYTES,
    MAX_SCRATCH_FILE_NAME_LEN, RUN_CODE_WALL_TIMEOUT,
};
use crate::types::ParseError;

/// What interpreter runs the program. A closed sum (§1) so the backend matches
/// exhaustively and a new language is a compile error everywhere it matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Language {
    /// POSIX shell (`/bin/sh`).
    Shell,
    /// Python 3 with the pre-baked wheel set (pandas/numpy/matplotlib/openpyxl/…).
    Python,
}

impl Language {
    /// Stable wire token, used in tool-input parsing and on the executor wire.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Shell => "shell",
            Self::Python => "python",
        }
    }
}

impl TryFrom<&str> for Language {
    type Error = ParseError;

    fn try_from(raw: &str) -> Result<Self, Self::Error> {
        match raw {
            "shell" | "sh" | "bash" => Ok(Self::Shell),
            "python" | "py" => Ok(Self::Python),
            _ => Err(ParseError::Malformed {
                field: "language",
                detail: "expected one of: shell, python",
            }),
        }
    }
}

/// Validated source program. Non-empty and within [`MAX_CODE_BYTES`]; the only
/// way to construct one is through the boundary parse.
#[derive(Clone, PartialEq, Eq)]
pub struct SourceCode(String);

impl SourceCode {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for SourceCode {
    type Error = ParseError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        if raw.trim().is_empty() {
            return Err(ParseError::Empty { field: "code" });
        }
        if raw.len() > MAX_CODE_BYTES {
            return Err(ParseError::TooLong {
                field: "code",
                max: MAX_CODE_BYTES,
                got: raw.len(),
            });
        }
        Ok(Self(raw))
    }
}

impl TryFrom<&str> for SourceCode {
    type Error = ParseError;

    fn try_from(raw: &str) -> Result<Self, Self::Error> {
        Self::try_from(raw.to_owned())
    }
}

// The program text is untrusted and potentially large; keep it out of Debug so
// it never lands in a span or log line (§2 — code is not an attribute).
impl std::fmt::Debug for SourceCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SourceCode")
            .field("bytes", &self.0.len())
            .finish()
    }
}

/// Requested in-sandbox wall-clock budget. Always `> 0` and never above
/// [`RUN_CODE_WALL_TIMEOUT`] — the model may ask for less, never more (§5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct RunTimeout(Duration);

impl RunTimeout {
    /// The subsystem ceiling, used as the default when the caller omits one.
    pub const MAX: Self = Self(RUN_CODE_WALL_TIMEOUT);

    #[must_use]
    pub fn as_duration(self) -> Duration {
        self.0
    }
}

impl Default for RunTimeout {
    fn default() -> Self {
        Self::MAX
    }
}

impl TryFrom<Duration> for RunTimeout {
    type Error = ParseError;

    fn try_from(d: Duration) -> Result<Self, Self::Error> {
        if d.is_zero() {
            return Err(ParseError::OutOfRange {
                field: "timeout",
                detail: "must be greater than zero",
            });
        }
        if d > RUN_CODE_WALL_TIMEOUT {
            return Err(ParseError::OutOfRange {
                field: "timeout",
                detail: "exceeds the run_code wall-clock ceiling",
            });
        }
        Ok(Self(d))
    }
}

/// Process exit status reported by the backend.
///
/// A non-zero exit is a *successful run* (the model reads it alongside stderr),
/// not an error — see [`crate::sandbox::error::SandboxError`]. Any `i32` is a
/// valid status, so this newtype carries no invariant beyond type identity (§1
/// exception) and offers a free constructor for the backend that produces it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitCode(i32);

impl ExitCode {
    #[must_use]
    pub fn new(code: i32) -> Self {
        Self(code)
    }

    #[must_use]
    pub fn get(self) -> i32 {
        self.0
    }

    /// Convenience: did the program exit cleanly?
    #[must_use]
    pub fn is_success(self) -> bool {
        self.0 == 0
    }
}

/// A single file name inside the sandbox scratch directory.
///
/// For a staged input or a harvested output. Names cross a trust boundary, so
/// the parse rejects any path separator, any traversal component, and anything
/// empty or over [`MAX_SCRATCH_FILE_NAME_LEN`]. The result is a leaf name safe
/// to join to the scratch root host-side (§1, §5).
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ScratchFileName(String);

impl ScratchFileName {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ScratchFileName {
    type Error = ParseError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        if raw.is_empty() {
            return Err(ParseError::Empty {
                field: "scratch_file_name",
            });
        }
        if raw.len() > MAX_SCRATCH_FILE_NAME_LEN {
            return Err(ParseError::TooLong {
                field: "scratch_file_name",
                max: MAX_SCRATCH_FILE_NAME_LEN,
                got: raw.len(),
            });
        }
        // No path separators (POSIX or Windows) — a name is a single leaf.
        if raw.contains('/') || raw.contains('\\') {
            return Err(ParseError::Malformed {
                field: "scratch_file_name",
                detail: "must not contain a path separator",
            });
        }
        // No traversal, no relative anchors, no NUL.
        if raw == "." || raw == ".." || raw.contains("..") {
            return Err(ParseError::Malformed {
                field: "scratch_file_name",
                detail: "must not contain a traversal component",
            });
        }
        if raw.contains('\0') {
            return Err(ParseError::Malformed {
                field: "scratch_file_name",
                detail: "must not contain a NUL byte",
            });
        }
        Ok(Self(raw))
    }
}

impl TryFrom<&str> for ScratchFileName {
    type Error = ParseError;

    fn try_from(raw: &str) -> Result<Self, Self::Error> {
        Self::try_from(raw.to_owned())
    }
}

impl std::fmt::Debug for ScratchFileName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("ScratchFileName").field(&self.0).finish()
    }
}

/// A file staged into the sandbox scratch dir before exec, or harvested after.
///
/// `name` is a validated leaf; `bytes` is capped at [`MAX_INPUT_FILE_BYTES`]
/// (inputs) on construction. Fields are read-only — the only way to build one is
/// the cap-checked [`InputFile::new`] (§1).
#[derive(Clone, PartialEq, Eq)]
pub struct InputFile {
    name: ScratchFileName,
    bytes: Bytes,
}

impl InputFile {
    /// Validate the byte cap and pair the name with its bytes. Inputs are
    /// resolved host-side (fetch-and-inject), so an oversize input is a caller
    /// error surfaced as [`ParseError`].
    pub fn new(name: ScratchFileName, bytes: Bytes) -> Result<Self, ParseError> {
        if bytes.len() > MAX_INPUT_FILE_BYTES {
            return Err(ParseError::TooLong {
                field: "input_file",
                max: MAX_INPUT_FILE_BYTES,
                got: bytes.len(),
            });
        }
        Ok(Self { name, bytes })
    }

    #[must_use]
    pub fn name(&self) -> &ScratchFileName {
        &self.name
    }

    #[must_use]
    pub fn bytes(&self) -> &Bytes {
        &self.bytes
    }
}

impl std::fmt::Debug for InputFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InputFile")
            .field("name", &self.name)
            .field("bytes", &self.bytes.len())
            .finish()
    }
}

/// A file produced by the program and harvested from the scratch dir.
///
/// Identical shape to [`InputFile`] but capped at [`MAX_OUTPUT_FILE_BYTES`]; the
/// backend maps an over-cap output onto
/// [`crate::sandbox::error::SandboxError::OutputTooLarge`] rather than returning
/// a truncated artifact.
#[derive(Clone, PartialEq, Eq)]
pub struct OutputFile {
    name: ScratchFileName,
    bytes: Bytes,
}

impl OutputFile {
    /// Validate the byte cap. Returns [`ParseError::TooLong`] so the backend can
    /// translate it to the typed sandbox error at the harvest seam.
    pub fn new(name: ScratchFileName, bytes: Bytes) -> Result<Self, ParseError> {
        if bytes.len() > MAX_OUTPUT_FILE_BYTES {
            return Err(ParseError::TooLong {
                field: "output_file",
                max: MAX_OUTPUT_FILE_BYTES,
                got: bytes.len(),
            });
        }
        Ok(Self { name, bytes })
    }

    #[must_use]
    pub fn name(&self) -> &ScratchFileName {
        &self.name
    }

    #[must_use]
    pub fn bytes(&self) -> &Bytes {
        &self.bytes
    }
}

impl std::fmt::Debug for OutputFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutputFile")
            .field("name", &self.name)
            .field("bytes", &self.bytes.len())
            .finish()
    }
}

/// One confined execution: what to run, with what inputs, under what network
/// policy and time budget.
///
/// Bytes-in: the trust boundary is here, so the count of inputs is capped at
/// [`MAX_INPUT_FILES`] on construction and every part is already a validated
/// newtype. Read-only fields; build via [`RunRequest::new`].
#[derive(Debug, Clone)]
pub struct RunRequest {
    language: Language,
    code: SourceCode,
    inputs: Vec<InputFile>,
    egress: EgressPolicy,
    timeout: RunTimeout,
}

impl RunRequest {
    /// Assemble a request, enforcing the input-count cap (§5).
    pub fn new(
        language: Language,
        code: SourceCode,
        inputs: Vec<InputFile>,
        egress: EgressPolicy,
        timeout: RunTimeout,
    ) -> Result<Self, ParseError> {
        if inputs.len() > MAX_INPUT_FILES {
            return Err(ParseError::TooLong {
                field: "inputs",
                max: MAX_INPUT_FILES,
                got: inputs.len(),
            });
        }
        Ok(Self {
            language,
            code,
            inputs,
            egress,
            timeout,
        })
    }

    #[must_use]
    pub fn language(&self) -> Language {
        self.language
    }

    #[must_use]
    pub fn code(&self) -> &SourceCode {
        &self.code
    }

    #[must_use]
    pub fn inputs(&self) -> &[InputFile] {
        &self.inputs
    }

    #[must_use]
    pub fn egress(&self) -> &EgressPolicy {
        &self.egress
    }

    #[must_use]
    pub fn timeout(&self) -> RunTimeout {
        self.timeout
    }
}

/// The result of a confined run.
///
/// Bytes-out: a non-zero [`ExitCode`] is still a success here (the model reads
/// it with stderr). stdout/stderr are already byte-capped by the backend before
/// they reach this struct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOutput {
    exit_code: ExitCode,
    stdout: String,
    stderr: String,
    artifacts: Vec<OutputFile>,
}

impl RunOutput {
    #[must_use]
    pub fn new(
        exit_code: ExitCode,
        stdout: String,
        stderr: String,
        artifacts: Vec<OutputFile>,
    ) -> Self {
        Self {
            exit_code,
            stdout,
            stderr,
            artifacts,
        }
    }

    #[must_use]
    pub fn exit_code(&self) -> ExitCode {
        self.exit_code
    }

    #[must_use]
    pub fn stdout(&self) -> &str {
        &self.stdout
    }

    #[must_use]
    pub fn stderr(&self) -> &str {
        &self.stderr
    }

    #[must_use]
    pub fn artifacts(&self) -> &[OutputFile] {
        &self.artifacts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_parses_known_aliases() {
        assert_eq!(
            Language::try_from("python").expect("python"),
            Language::Python
        );
        assert_eq!(Language::try_from("py").expect("py"), Language::Python);
        assert_eq!(Language::try_from("bash").expect("bash"), Language::Shell);
    }

    #[test]
    fn language_rejects_unknown() {
        let err = Language::try_from("ruby").expect_err("unknown language rejected");
        assert!(matches!(
            err,
            ParseError::Malformed {
                field: "language",
                ..
            }
        ));
    }

    #[test]
    fn source_code_rejects_blank() {
        let err = SourceCode::try_from("   \n\t ").expect_err("blank code rejected");
        assert!(matches!(err, ParseError::Empty { field: "code" }));
    }

    #[test]
    fn source_code_rejects_oversize() {
        let big = "a".repeat(MAX_CODE_BYTES + 1);
        let err = SourceCode::try_from(big).expect_err("oversize code rejected");
        assert!(matches!(err, ParseError::TooLong { field: "code", .. }));
    }

    #[test]
    fn source_code_accepts_at_cap() {
        let at = "a".repeat(MAX_CODE_BYTES);
        assert!(SourceCode::try_from(at).is_ok());
    }

    #[test]
    fn run_timeout_rejects_zero() {
        let err = RunTimeout::try_from(Duration::ZERO).expect_err("zero rejected");
        assert!(matches!(
            err,
            ParseError::OutOfRange {
                field: "timeout",
                ..
            }
        ));
    }

    #[test]
    fn run_timeout_rejects_above_ceiling() {
        let over = RUN_CODE_WALL_TIMEOUT + Duration::from_secs(1);
        let err = RunTimeout::try_from(over).expect_err("over ceiling rejected");
        assert!(matches!(
            err,
            ParseError::OutOfRange {
                field: "timeout",
                ..
            }
        ));
    }

    #[test]
    fn run_timeout_default_is_the_ceiling() {
        assert_eq!(RunTimeout::default().as_duration(), RUN_CODE_WALL_TIMEOUT);
    }

    #[test]
    fn exit_code_success_only_for_zero() {
        assert!(ExitCode::new(0).is_success());
        assert!(!ExitCode::new(1).is_success());
        assert_eq!(ExitCode::new(137).get(), 137);
    }

    #[test]
    fn scratch_name_rejects_separator() {
        let err = ScratchFileName::try_from("sub/dir.txt").expect_err("separator rejected");
        assert!(matches!(
            err,
            ParseError::Malformed {
                field: "scratch_file_name",
                ..
            }
        ));
    }

    #[test]
    fn scratch_name_rejects_traversal() {
        for bad in ["..", "../etc", "a..b", "..\\win"] {
            let err = ScratchFileName::try_from(bad).expect_err("traversal rejected");
            assert!(matches!(
                err,
                ParseError::Malformed {
                    field: "scratch_file_name",
                    ..
                }
            ));
        }
    }

    #[test]
    fn scratch_name_rejects_empty() {
        let err = ScratchFileName::try_from("").expect_err("empty rejected");
        assert!(matches!(
            err,
            ParseError::Empty {
                field: "scratch_file_name"
            }
        ));
    }

    #[test]
    fn scratch_name_accepts_plain_leaf() {
        let name = ScratchFileName::try_from("report.xlsx").expect("plain leaf");
        assert_eq!(name.as_str(), "report.xlsx");
    }
}
