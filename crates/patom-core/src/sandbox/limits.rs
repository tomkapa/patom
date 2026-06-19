//! Sandbox-subsystem invariants. Per CLAUDE.md §5: every magic number is named,
//! exported, and doc-commented with the *why*.
//!
//! These bound the untrusted code path end-to-end: the size of the program we
//! accept, how long it may run, how much it may emit, and how many bytes cross
//! in and out. Nothing here is configurable at runtime — a looser bound is a
//! code change that goes through review.

use std::time::Duration;

use crate::agent_core::TOOL_CALL_TIMEOUT;
use crate::tools::limits::TOOL_RESULT_MAX_BYTES;

/// Hard ceiling on the source program a single `run_code` call may carry.
///
/// Agent-authored snippets and bundled skill code are small; 256 KiB is already
/// generous (a large `pandas` transform is a few KiB). The cap crosses a trust
/// boundary (the model supplies the code) so it is enforced in
/// [`crate::sandbox::types::SourceCode`]'s `TryFrom` (§5).
pub const MAX_CODE_BYTES: usize = 256 * 1024;

/// Wall-clock budget *inside* the sandbox. The backend kills the program after
/// this and reports [`crate::sandbox::error::SandboxError::Timeout`].
///
/// 30 s covers a chart render or a spreadsheet build with headroom; anything
/// longer is almost always a runaway loop in untrusted code. This is the bound
/// the typed [`crate::sandbox::types::RunTimeout`] caps against, so the model
/// can request *less* but never more.
pub const RUN_CODE_WALL_TIMEOUT: Duration = Duration::from_secs(30);

/// Host-side fence around the whole backend call (spawn + run + harvest),
/// wrapping `sandbox.run` in `tokio::time::timeout`.
///
/// Must sit strictly above [`RUN_CODE_WALL_TIMEOUT`] so the backend's own
/// in-sandbox kill surfaces a precise `SandboxError::Timeout` *before* this
/// coarser fence trips; the 15 s gap absorbs spawn latency and output harvest.
pub const RUN_CODE_OUTER_TIMEOUT: Duration = Duration::from_secs(45);

/// Hard ceiling on captured stdout returned to the model.
///
/// Kept at or below the global tool-result cap so the agent boundary never has
/// to re-truncate something we already bounded. Oversized stdout still flows
/// through #185 offload via the tool's `result_policy`, but the backend refuses
/// to buffer more than this from the untrusted process in the first place.
pub const MAX_STDOUT_BYTES: usize = TOOL_RESULT_MAX_BYTES;

/// Hard ceiling on captured stderr. Smaller than stdout — stderr is diagnostics
/// (a traceback, a warning), not payload; 64 KiB holds a deep Python traceback.
pub const MAX_STDERR_BYTES: usize = 64 * 1024;

/// Maximum number of input files Patom may stage into the scratch dir per call.
///
/// Fetch-and-inject resolves each input host-side before exec; 16 covers
/// multi-file analyses (a CSV plus a template) without unbounded fan-in.
pub const MAX_INPUT_FILES: usize = 16;

/// Maximum number of output files harvested from the scratch dir per call.
///
/// A run typically emits one or two artifacts (a workbook, a chart). 16 leaves
/// room for a multi-sheet export without letting a runaway program flood R2.
pub const MAX_OUTPUT_FILES: usize = 16;

/// Per-file byte cap on a single staged input.
///
/// 16 MiB holds a sizeable spreadsheet or document; larger inputs are an
/// upload-then-reference workflow, not an inline `run_code` argument.
pub const MAX_INPUT_FILE_BYTES: usize = 16 * 1024 * 1024;

/// Per-file byte cap on a single harvested output, enforced before R2 storage.
///
/// Mirrors [`MAX_INPUT_FILE_BYTES`]; an artifact larger than this is a
/// programming error in the skill, surfaced as
/// [`crate::sandbox::error::SandboxError::OutputTooLarge`].
pub const MAX_OUTPUT_FILE_BYTES: usize = 16 * 1024 * 1024;

/// Maximum number of hosts an org may allowlist for sandbox egress.
///
/// Default-deny means most orgs carry zero; a handful of API endpoints covers
/// the opt-in case. 64 caps the JSONB column and the per-run policy without a
/// second thought (§5).
pub const MAX_EGRESS_HOSTS_PER_ORG: usize = 64;

/// Maximum byte length of a single allowlisted egress host.
///
/// A DNS name is at most 253 bytes; 255 covers it with the nearest round cap.
pub const MAX_EGRESS_HOST_LEN: usize = 255;

/// Maximum length, in bytes, of a scratch-dir file name.
///
/// Names cross a trust boundary (the program chooses output names; the model
/// chooses input names), so they are validated in
/// [`crate::sandbox::types::ScratchFileName`] — no path separators, no
/// traversal, bounded length. 255 matches the common POSIX `NAME_MAX`.
pub const MAX_SCRATCH_FILE_NAME_LEN: usize = 255;

// §5: the in-sandbox kill must fire before the outer host fence, and the outer
// fence must sit within the agent's generic per-tool-call timeout — otherwise a
// timeout surfaces as the wrong (coarser) error and the typed `Timeout` variant
// the model reads is never produced.
const _: () = assert!(RUN_CODE_WALL_TIMEOUT.as_millis() < RUN_CODE_OUTER_TIMEOUT.as_millis());
const _: () = assert!(RUN_CODE_OUTER_TIMEOUT.as_millis() <= TOOL_CALL_TIMEOUT.as_millis());

// §5: captured stdout must fit the global tool-result budget so the agent
// boundary never re-truncates a body the backend already bounded.
const _: () = assert!(MAX_STDOUT_BYTES <= TOOL_RESULT_MAX_BYTES);
