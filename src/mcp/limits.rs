//! MCP-subsystem invariants. CLAUDE.md §5: every container, every I/O is bounded.

use std::time::Duration;

/// Cap on how many MCP servers we let an operator register.
///
/// Each server holds a long-lived rmcp client + a `tokio` worker task; this is the
/// budget for that resource. Sized for the foreseeable per-tenant deployment, not a
/// single user's experimental sandbox.
pub const MAX_MCP_SERVERS: usize = 32;

/// Cap on tools exposed by a single MCP server. A misbehaving (or compromised) server
/// could otherwise flood the agent's tool list and balloon every provider request.
pub const MAX_TOOLS_PER_SERVER: usize = 64;

/// Length cap on a remote MCP tool name (the name the server publishes
/// before relay prefixes it with `mcp_<alias>_`).
///
/// Set to the `ToolName` cap so it can never exceed what `prefixed_name`
/// would tolerate even if alias-prefix overhead were zero. The runtime
/// rejects (and warns) on a longer prefixed name in `ingest_tools`; this
/// cap guards the per-agent allowlist input path before any registry
/// lookup happens.
pub const MCP_TOOL_REMOTE_NAME_MAX_LEN: usize = 64;

/// How long the alias may be. Matches the schema CHECK constraint and reserves enough
/// budget that `mcp_<alias>_<remote>` always fits in the 64-char `ToolName` cap.
/// `4 + 16 + 1 + 43 = 64`.
pub const MCP_ALIAS_MAX_LEN: usize = 16;

/// Cap on a catalog id (e.g. `"notion"`). Matches the regex in
/// `mcp_catalog.id`'s CHECK constraint (`^[a-z][a-z0-9_-]{0,39}$`) so the
/// HTTP boundary and the DB enforce the same length.
///
/// Sized so the prefixed tool name `mcp_<catalog_id>_<remote_name>` stays
/// within the 64-byte `ToolName` cap when the remote name is up to
/// `MCP_TOOL_REMOTE_NAME_MAX_LEN`: `4 + 40 + 1 + 19 = 64`. A longer remote
/// name is rejected at registry-ingest time with a warn.
pub const MCP_CATALOG_ID_MAX_LEN: usize = 40;

/// Cap on `mcp_catalog.display_name` ("Notion", "Linear", …). Matches the
/// schema CHECK.
pub const MCP_CATALOG_DISPLAY_NAME_MAX_LEN: usize = 100;

/// Cap on `mcp_catalog.description` (recruiter-facing summary).
///
/// Matches the schema CHECK. Larger than `MCP_DESCRIPTION_MAX_LEN` because
/// the catalog description carries the "what is this for" framing the
/// recruiter reasons over.
pub const MCP_CATALOG_DESCRIPTION_MAX_LEN: usize = 1024;

/// Description (operator-facing notes) cap; matches the schema CHECK.
pub const MCP_DESCRIPTION_MAX_LEN: usize = 512;

/// URL length cap on the `http` transport.
pub const MCP_URL_MAX_LEN: usize = 2048;

/// Maximum number of custom headers the operator may set on a server.
pub const MCP_MAX_HEADERS: usize = 16;

/// Maximum length of a single header name.
pub const MCP_HEADER_NAME_MAX_LEN: usize = 64;

/// Maximum length of a single header value. Bearer tokens fit comfortably; opaque blobs
/// past this size are a sign of misuse, not a real header.
pub const MCP_HEADER_VALUE_MAX_LEN: usize = 4096;

/// How long we'll wait while connecting + initializing one MCP server during refresh.
/// Tighter than the per-call timeout so a dead server can't stall the refresh task.
pub const MCP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long we'll wait for a single `tools/list` round-trip during refresh.
pub const MCP_LIST_TOOLS_TIMEOUT: Duration = Duration::from_secs(10);

/// Per-call timeout for `tools/call`. Honoured inside `McpTool::execute` in addition to
/// the agent's outer `tool_timeout`.
pub const MCP_CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// Bound on the single-server credential decrypt performed by the HTTP read
/// handler to surface `token_expires_at`. AEAD open is in-memory today; the
/// timeout is headroom for a future KMS-backed encryptor.
pub const MCP_CREDENTIAL_READ_TIMEOUT: Duration = Duration::from_secs(5);

/// How many bytes of an MCP tool's stringified result we'll keep before truncating.
/// Independent of (and always less than or equal to) `tools::TOOL_RESULT_MAX_BYTES`,
/// which is the agent-side cap.
pub const MCP_RESULT_RENDER_CAP: usize = 128 * 1024;

/// Cap on the number of catalog-driven authorize-URL extra params per
/// integration. Google needs two (`access_type`, `prompt`); the worst
/// realistic case we've sketched (Microsoft tenant-specific +
/// `prompt=consent`+`prompt=login`+`domain_hint` + Atlassian
/// `audience`+`prompt`) tops out at 6. Eight gives 2× headroom while
/// keeping the row payload short enough to inline in every catalog
/// list response. CLAUDE.md §5: every collection is bounded on entry.
pub(super) const MCP_CATALOG_AUTHORIZE_EXTRA_PARAMS_MAX: usize = 8;

/// Cap on one entry's `key` or `value` in `authorize_extra_params`.
/// `key` realistically maxes out around 32 chars (`access_type`,
/// `prompt`, `audience`, `domain_hint`); `value` carries short URLs
/// for `audience`-style params. 256 is a comfortable bound that still
/// fits the 2048-byte JSONB total enforced at the DB CHECK.
pub(super) const MCP_CATALOG_AUTHORIZE_EXTRA_PARAM_BYTES_MAX: usize = 256;

/// Per-user cap on `POST /mcp-servers/test-connect` calls per rolling minute.
///
/// Sized for legitimate "click test, fix the URL, click test" UX without
/// letting a curious user turn the endpoint into an SSRF probe. Enforced by a
/// bounded in-memory token-bucket map (CLAUDE.md §5).
pub const MCP_TEST_CONNECT_PER_MIN: usize = 10;

/// Cap on the in-memory rate-limiter map entries.
///
/// Older entries are evicted LRU when the cap is hit so a flood of one-shot
/// user ids can't grow the map unboundedly (CLAUDE.md §5).
pub const MCP_TEST_CONNECT_BUCKETS_MAX: usize = 4096;
