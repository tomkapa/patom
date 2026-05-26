// Wire types mirror src/runtime/response.rs and the route handlers.
// Keep in sync with: src/http/routes/threads.rs, src/runtime/response.rs.

export type Role = "owner" | "admin" | "member";

/** Mirrors `src/auth/language.rs` — kept narrow so the i18n layer can
 *  exhaustive-match on it. Adding a language here requires a paired
 *  backend change (new TOML + migration CHECK update). */
export type Language = "en" | "vi";

export type Org = {
  id: string;
  name: string;
  slug: string;
  role: Role;
  /** Org-wide language driving the agent's `<language>` directive and
   *  the web app's i18n. Mutated via `PATCH /me/org/language` for
   *  owner/admin members. */
  default_language: Language;
};

export type User = {
  id: string;
  email: string;
  display_name: string | null;
  avatar_url: string | null;
};

export type Me = {
  user: User;
  orgs: Org[];
  active_org_id: string;
  role: Role;
};

export type AgentRef = { id: string; name: string };

export type Agent = {
  id: string;
  name: string;
  /** Operator-curated, model-facing one-sentence blurb. Always present on
   *  every read; older list-only consumers may treat it as optional. */
  description?: string;
  /** Free-form system prompt. Present on every read; only the agent-detail
   *  page uses it today. */
  system_prompt?: string;
  is_default: boolean;
  /** Per-server tool allowlist. Keys are MCP server ids the agent may
   *  reach; the value is `null` (= every tool from that server) or an
   *  array of remote tool names (= only those tools). A server id that
   *  is absent from the object grants the agent no access to that
   *  server. Always present on every read; an empty object means the
   *  agent has no MCP access. Mirrors
   *  `src/http/routes/agents.rs::AgentResponse.allowed_mcp_tools`. */
  allowed_mcp_tools?: Record<string, string[] | null>;
  /** Pinned catalog model id (e.g. `"claude-sonnet-4-6"`) or `null` when
   *  the agent inherits the workspace default. Mirrors
   *  `src/http/routes/agents.rs::AgentResponse.model`. */
  model?: string | null;
  created_at?: string;
  updated_at?: string;
};

/** PUT /agents/{id}. Every field is a discrete patch: `undefined` leaves
 *  the column untouched; `null`-meaning omissions follow the backend
 *  contract in `src/http/routes/agents.rs::UpdateAgentRequest`. The
 *  allowlist replaces atomically when present — `Some({})` is the
 *  explicit "lockdown" shape that revokes every server. The `model` field
 *  is tri-state: omitted = leave untouched, `null` = clear to workspace
 *  default, string = pin to that catalog model. */
export type UpdateAgentRequest = {
  name?: string;
  system_prompt?: string;
  description?: string;
  is_default?: boolean;
  allowed_mcp_tools?: Record<string, string[] | null>;
  model?: string | null;
};

/** One row of `GET /models`. Mirrors
 *  `src/http/routes/models.rs::ModelEntry`. */
export type ModelEntry = {
  id: string;
  provider: string;
};

export type RequestStatus = "pending" | "processing" | "done" | "failed";

export type ThreadSummary = {
  root_request_id: string;
  root_session_id: string;
  first_agent: AgentRef;
  preview: string;
  reply_count: number;
  last_activity_at: string;
  status: RequestStatus;
  created_at: string;
};

export type Participant =
  | { kind: "human" }
  | { kind: "agent"; agent_id: string }
  | { kind: "system" };

// Mirrors src/provider/chat.rs `ChatMessage` + UserContent / AssistantContent.
// Wire shape is `{role, contents: [{kind, value}]}`; the demo fixtures tolerate
// the legacy `{role, content: string}` form too.
export type ContentBlock =
  | { kind: "text"; value: string }
  | { kind: "reasoning"; value: string }
  | {
      kind: "tool_call";
      value: { id: string; name: string; input: unknown };
    }
  | {
      kind: "tool_result";
      value: { call_id: string; output: string; is_error?: boolean };
    };

export type ChatMessageBody = {
  role?: "user" | "assistant" | "system" | "tool";
  contents?: ContentBlock[];
  /** Legacy / demo shorthand. */
  content?: string;
  [k: string]: unknown;
};

export type ThreadMessage = {
  session_id: string;
  seq: number;
  sender: Participant;
  receiver: Participant;
  body: ChatMessageBody;
  created_at: string;
  /** The prompt request that produced this row. The thread panel uses it to
   *  reconcile optimistic / live / persisted bubbles by identity instead of
   *  by text matching. */
  request_id: string;
};

// ─── ResponseChunk wire shapes ──────────────────────────────────────────

export type ToolCallPayload = {
  id: string;
  name: string;
  input: unknown;
};

export type ToolResultPayload = {
  call_id: string;
  output: string;
  is_error?: boolean;
};

export type ResponseChunk =
  | { kind: "text"; value: string }
  | { kind: "reasoning"; value: string }
  | { kind: "tool_call"; id: string; name: string; input: unknown }
  | { kind: "tool_result"; call_id: string; output: string; is_error?: boolean }
  | { kind: "agent_message"; from: string; content: string }
  /** Interactive prompt: the agent is asking the user to wire an MCP
   *  integration from inside the chat. Rendered as a click-to-wire card
   *  (`WireMcpRequestCard`) inline with the agent's other turn output.
   *  Non-terminal — the agent's turn continues; the user wires the MCP
   *  via a follow-up flow and the agent sees the new state on its next
   *  turn. */
  | ({ kind: "wire_mcp_request"; from: string } & McpWireRequest)
  | { kind: "done"; final_text: string }
  | { kind: "error"; reason: string }
  | { kind: "stalled" };

export type ToolCallEntry = {
  call_id: string;
  name: string;
  input?: unknown;
  output?: string;
  is_error?: boolean;
  status: "running" | "ok" | "error";
};

export type ThreadStreamEnvelope = {
  request_id: string | null;
  from_agent: string | null;
  chunk_seq: number | null;
  chunk: ResponseChunk;
};

export type SubmitPromptResponse = {
  request_id: string;
  session_id: string;
  status: RequestStatus;
};

// ─── MCP server wire shapes ─────────────────────────────────────────────
// Mirrors src/http/routes/mcp.rs and src/mcp/types.rs. Adding a transport
// kind or credential kind requires a paired backend change.

// Wire tag matches Rust `McpTransportInput` (`#[serde(tag = "type")]` in
// src/mcp/types.rs). Don't switch this to `kind` — the BE will reject it.
export type McpTransport = { type: "http"; url: string };

/** Mirrors `src/mcp/types.rs::ConnectionStatus`. */
export type ConnectionStatus =
  | "ok"
  | "auth_pending"
  | "reconnect_required"
  | "error";

/** Per-tool discovery summary surfaced in McpServer.discovered_tools. */
export type DiscoveredTool = {
  prefixed_name: string;
  remote_name: string;
  description: string | null;
};

export type CredentialsKind = "static_headers" | "oauth2";

export const CREDENTIALS_KIND = {
  OAUTH2: "oauth2",
  STATIC_HEADERS: "static_headers",
} as const satisfies Record<string, CredentialsKind>;

export type McpServer = {
  id: string;
  /** Stable id of the `mcp_catalog` entry this server is wired against
   *  ("notion", "linear", …). Drives the tool prefix
   *  (`mcp_<catalog_id>_<remote_name>`) and is what the recruiter uses
   *  in `create_agent.allowed_mcp_tools`. Replaces the pre-launch
   *  operator-chosen `alias`. */
  catalog_id: string;
  enabled: boolean;
  config: McpTransport;
  description: string | null;
  last_seen_at: string | null;
  last_error: string | null;
  discovered_tools: DiscoveredTool[] | null;
  created_by_user_id: string;
  has_credentials: boolean;
  credentials_kind: CredentialsKind | null;
  connection_status: ConnectionStatus;
  /** Email of the user who created the connection (joined from `users`).
   *  Surfaced on every read path. May be `null` if the FK is null. */
  creator_email: string | null;
  /** OAuth access-token expiry (ISO-8601). Surfaced only on the single-
   *  server read path; `null` for non-OAuth credentials, no credentials,
   *  or any list/create/update response. */
  token_expires_at: string | null;
  created_at: string;
  updated_at: string;
};

/** One audit row from `GET /mcp-servers/{id}/tool-calls`. Backed by the
 *  `tool_calls` table; `agent_name` is joined from `agents.name` and
 *  `error_message` is populated only when `is_error === true`. */
export type ToolCall = {
  id: string;
  tool_name: string;
  agent_id: string;
  agent_name: string | null;
  started_at: string;
  duration_ms: number;
  is_error: boolean;
  error_message: string | null;
};

/** Cursor-paginated response. `next_cursor` is the previous page's last
 *  `started_at`; pass it back as `?before=` to fetch the next slice.
 *  `null` when the page is the tail. */
export type ToolCallList = {
  items: ToolCall[];
  next_cursor: string | null;
};

/** One audit row from `GET /agents/{id}/tool-calls`. The per-agent view
 *  spans connections, so the row carries the originating MCP server id +
 *  alias (LEFT JOIN — both fields go `null` if the connection has been
 *  deleted). Other fields mirror `ToolCall`. */
export type AgentToolCall = {
  id: string;
  tool_name: string;
  mcp_server_id: string | null;
  /** Catalog id of the originating server (replaces `mcp_server_alias`). */
  mcp_server_catalog_id: string | null;
  started_at: string;
  duration_ms: number;
  is_error: boolean;
  error_message: string | null;
};

export type AgentToolCallList = {
  items: AgentToolCall[];
  next_cursor: string | null;
};

/** Only `static_headers` is accepted on the create/replace path today;
 *  OAuth tokens are written by the callback handler. */
export type CredentialInput = {
  kind: "static_headers";
  headers: Record<string, string>;
};

/** Two shapes:
 *   * **Short form** — only `catalog_id`. Backend fills `config` from the
 *     catalog default. Drives the click-to-wire button on the connections
 *     page.
 *   * **Full form** — every field present. Lets operators with tenant-
 *     custom transport supply the whole payload. */
export type CreateMcpServerRequest = {
  catalog_id: string;
  config?: McpTransport;
  description?: string | null;
  enabled?: boolean;
  credentials?: CredentialInput;
};

/** `catalog_id` is immutable post-create — to switch a connection's
 *  integration, delete the row and create a new one. */
export type UpdateMcpServerRequest = {
  config?: McpTransport;
  description?: string | null;
  enabled?: boolean;
};

/** Auth mechanism a catalog entry expects when wiring. Wire-stable
 *  labels match the backend `McpAuthKind`. */
export type McpAuthKind = "oauth2" | "static_headers" | "none";

/** Materialised wire-MCP request payload. Used both as the SSE chunk
 *  surface (`ResponseChunk { kind: "wire_mcp_request" }`) shorn of its
 *  `from` field, and as the inline-bubble entry the renderer pulls from
 *  the live store and history fold. Single source of truth so a field
 *  change ripples through both. */
export type McpWireRequest = {
  catalog_id: string;
  display_name: string;
  reason: string;
  auth_kind: McpAuthKind;
  homepage_url?: string;
};

/** One row from `GET /mcp-catalog`. The frontend connections page reads
 *  this list and joins it against `GET /mcp-servers` to render
 *  wired-vs-unwired tiles. */
export type McpCatalogEntry = {
  catalog_id: string;
  display_name: string;
  description: string;
  homepage_url?: string;
  /** Public URL of the tile icon (R2-hosted). Built-ins land here via
   *  migration 33; org-scoped entries via `POST /api/uploads/mcp-catalog/:id`.
   *  Falls back to the FE's Monogram tile when absent. */
  icon_url?: string;
  auth_kind: McpAuthKind;
  /** `true` when the entry was added by the tenant (not a global built-in). */
  is_custom: boolean;
  /** `true` when this org has a wired `mcp_servers` row for this catalog id. */
  wired: boolean;
};

/** Response envelope shared by both upload endpoints. The URL is the
 *  public asset URL the frontend should render. */
export type UploadResponse = { url: string };

export type TestConnectRequest = {
  config: McpTransport;
  credentials?: CredentialInput;
};

export type TestConnectResponse =
  | { outcome: "ok"; discovered_tools: DiscoveredTool[] }
  | { outcome: "failed"; error: string };

export type OAuthStartRequest = {
  redirect_to?: string;
  scope?: string;
  /** Universal auto-continue resume context. Both must be present or
   *  both absent — the BE returns 400 otherwise. When present, the
   *  OAuth callback enqueues a synthetic continuation prompt back into
   *  the session so the agent loop resumes without the user typing. */
  session_id?: string;
  agent_id?: string;
};

export type OAuthStartResponse = { authorize_url: string };

// ─── Agent memory ───────────────────────────────────────────────────
// Wire labels mirror `src/memory/types.rs` exactly (snake-case); the
// frontend never reads the pretty `display_label` form — that's the
// model-facing prompt header, not an API contract.

export type MemoryKind =
  | "self"
  | "other"
  | "collaborator"
  | "procedure"
  | "open";

export type MemoryState = "core" | "validated" | "held" | "tentative";

export type MutationKind = "write" | "update" | "forget";

export type MutationSource = "turn" | "operator" | "librarian";

/** GET /agents/{id}/memory row. Mirrors
 *  `src/http/routes/memory.rs::MemoryRowResponse`. */
export type MemoryRow = {
  id: string;
  agent_id: string;
  kind: MemoryKind;
  content: string;
  state: MemoryState;
  pinned: boolean;
  created_at: string;
  last_validated_at: string;
  last_accessed_at: string;
  access_count: number;
};

/** GET /agents/{id}/memory/events row. Mirrors
 *  `src/http/routes/memory.rs::EventResponse`. `content_before` and
 *  `content_after` are derived from the mutation kind: write → only
 *  after, update → both, forget → only before. */
export type MemoryEvent = {
  id: string;
  agent_id: string;
  mutation: MutationKind;
  target_memory_id: string;
  content_before: string | null;
  content_after: string | null;
  source: MutationSource;
  source_turn_id: string | null;
  created_at: string;
};

/** POST /agents/{id}/memory body. `state` defaults to `"held"` server-
 *  side; we pass it explicitly so an operator endorsing a note at
 *  `validated`/`core` also records a validation event. */
export type CreateMemoryNoteRequest = {
  kind: MemoryKind;
  content: string;
  state?: MemoryState;
  pinned?: boolean;
};

/** Filters for `GET /agents/{id}/memory/events`. Omitted fields don't
 *  add the query param; backend treats absence as "no filter". */
export type MemoryEventsFilter = {
  source?: MutationSource;
  mutation?: MutationKind;
};

// ─── Agent logs & metrics (doc/logs_metrics_tab.md) ────────────────────

/** Turn kind label — mirrors `RequestKind` in src/runtime/types.rs. */
export type TurnKind = "normal" | "reflection" | "resolution";

/** One bucket in the token-spend chart. Aggregated server-side; the FE
 *  never recomputes counts or percentiles. */
export type MetricsBucket = {
  start: string;
  by_kind: { normal: number; reflection: number; resolution: number };
  latency_p50_ms: number;
  latency_p95_ms: number;
  failure_count: number;
  prompt_version_id: string | null;
};

export type MetricsTotals = {
  tokens: number;
  turns: number;
  latency_p50_ms: number;
  latency_p95_ms: number;
  failure_count: number;
};

export type MetricsDeltas = {
  tokens: number | null;
  latency_p95_ms: number | null;
  failure_count: number | null;
};

export type PromptEditMarker = {
  version: number;
  created_at: string;
  edited_by: string | null;
};

export type MetricsTimeseriesResponse = {
  bucket_label: string;
  from: string;
  to: string;
  buckets: MetricsBucket[];
  totals: MetricsTotals;
  deltas_vs_compare: MetricsDeltas;
  prompt_edits: PromptEditMarker[];
};

export type AgentTurnRow = {
  request_id: string;
  started_at: string;
  kind: TurnKind;
  model: string;
  provider: string;
  input_tokens: number;
  output_tokens: number;
  cache_creation_tokens: number | null;
  cache_read_tokens: number | null;
  duration_ms: number;
  stop_reason: string;
  history_count: number;
  status: string;
  failure_reason: string | null;
  prompt_version: number;
};

export type AgentTurnsList = {
  items: AgentTurnRow[];
  next_cursor: string | null;
};

/** UI window choices for the scope strip. */
export type LogsTimeRange = "1h" | "24h" | "7d" | "30d";

/** UI kind filter. `all` collapses to "no filter" at the API. */
export type LogsKindFilter = TurnKind | "all";

/** Compare-window mode. */
export type LogsCompareMode = "prev_window" | "none";

// ─── Logs & Metrics — turn drawer ─────────────────────────────────────
// Wire types for `GET /turns/{request_id}` (slice 2). Mirrors
// `src/http/routes/turns.rs::TurnDetailResponse`; see doc/logs_metrics_tab.md §5.4.

/** `turn_metrics` row + the parent `prompt_requests.failure_reason`,
 *  joined into one payload for the drawer header chips. */
export type TurnMetrics = {
  request_id: string;
  session_id: string;
  agent_id: string;
  prompt_version_id: string;
  /** "normal" | "reflection" | "resolution" — mirrors the
   *  `prompt_requests.kind` enum. */
  kind: string;
  model: string;
  /** "anthropic" | "openai" — mirrors the `turn_metrics.provider`
   *  CHECK constraint. */
  provider: string;
  input_tokens: number;
  output_tokens: number;
  cache_creation_tokens: number | null;
  cache_read_tokens: number | null;
  duration_ms: number;
  /** "end_turn" | "tool_use" | "length" | "timeout" | … — model- /
   *  provider-defined. The drawer just renders the string. */
  stop_reason: string;
  history_count: number;
  started_at: string;
  created_at: string;
  /** Non-null only when the parent request failed; the drawer paints a
   *  rose banner with this text when present. */
  failure_reason: string | null;
};

/** One reasoning block extracted from `session_messages.body`. The
 *  drawer collapses these by default and shows the byte count up-front
 *  so the operator can decide whether to expand. */
export type TurnReasoningBlock = {
  text: string;
  byte_count: number;
};

/** One `tool_calls` row scoped to a single turn. Mirrors `AgentToolCall`
 *  one-for-one but with `mcp_server_alias` instead of catalog id (the
 *  drawer renders the alias inline). */
export type TurnToolCall = {
  id: string;
  tool_name: string;
  mcp_server_id: string | null;
  mcp_server_alias: string | null;
  started_at: string;
  duration_ms: number;
  is_error: boolean;
  error_message: string | null;
};

/** One `memory_events` row attributed to this turn (`source_turn_id =
 *  request_id`). Mirrors the existing `MemoryEvent` shape narrowed to
 *  the fields the drawer renders. */
export type TurnMemoryEvent = {
  id: string;
  /** "write" | "update" | "forget" — mirrors `memory_events.mutation`. */
  mutation: string;
  target_memory_id: string;
  content_before: string | null;
  content_after: string | null;
  created_at: string;
};

/** `agent_prompt_versions` snapshot for the version that was active when
 *  this turn ran. Read-only here; the restore action lives on a separate
 *  endpoint (slice 3). */
export type TurnPromptVersion = {
  id: string;
  version: number;
  system_prompt: string;
  model: string | null;
  edited_by: string | null;
  created_at: string;
};

// ─── Prompt versions (doc/logs_metrics_tab.md §4.1, §4.5) ────────────
/** One row in `agent_prompt_versions`. The diff modal renders these as
 *  the left/right panes. `version` is monotonic per agent, `model` is
 *  `null` when the historical version inherited the workspace default,
 *  and `edited_by` is `null` for the v1 seed row that migration 43
 *  minted for every existing agent. */
export type PromptVersion = {
  id: string;
  version: number;
  system_prompt: string;
  model: string | null;
  edited_by: string | null;
  created_at: string;
};

/** Full payload for `GET /turns/{request_id}`. */
export type TurnDetail = {
  turn: TurnMetrics;
  reasoning_blocks: TurnReasoningBlock[];
  tool_calls: TurnToolCall[];
  memory_writes: TurnMemoryEvent[];
  prompt_version: TurnPromptVersion;
};

export type PromptVersionList = {
  items: PromptVersion[];
};

/** Response from `POST /agents/{id}/prompt-versions/{version}/restore`.
 *  Append-only: the server snapshots the named historical version into a
 *  fresh row whose number is `max+1`, so reverting v7→v6 returns v8. The
 *  frontend invalidates `agents`, the prompt-versions list, the
 *  `metrics/timeseries` query, and the `turns` query. */
export type RestorePromptVersionResponse = {
  version: number;
  id: string;
  created_at: string;
};
