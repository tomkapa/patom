import type {
  Agent,
  AgentToolCallList,
  AgentTurnsList,
  TurnDetail,
  CreateMcpServerRequest,
  CreateMemoryNoteRequest,
  CredentialInput,
  Language,
  Me,
  McpCatalogEntry,
  McpServer,
  MemoryEvent,
  MemoryEventsFilter,
  MemoryRow,
  MetricsTimeseriesResponse,
  ModelEntry,
  OAuthStartRequest,
  OAuthStartResponse,
  Role,
  SubmitPromptResponse,
  TestConnectRequest,
  TestConnectResponse,
  ThreadMessage,
  ThreadSummary,
  ToolCallList,
  UpdateAgentRequest,
  UpdateMcpServerRequest,
  UploadResponse,
} from "../types/api";
import { ApiError, AuthRedirect } from "./errors";
import { readCookie } from "./cookies";
import { useAuthStore } from "../stores/authStore";

// Wire-protocol constants — keep in sync with src/auth/limits.rs
// (`CSRF_COOKIE_NAME`, `CSRF_HEADER_NAME`).
const CSRF_COOKIE = "relay_csrf";
const CSRF_HEADER = "X-CSRF-Token";
const SAFE_METHODS = new Set(["GET", "HEAD", "OPTIONS"]);

// JSON endpoints live under this prefix; `/auth/google/*` bypasses it.
export const API_PREFIX = "/api";

export async function request<T>(
  path: string,
  init?: RequestInit,
): Promise<T> {
  const method = (init?.method ?? "GET").toUpperCase();
  const headers = new Headers(init?.headers);
  // FormData bodies must set their own multipart boundary; if we hand the
  // browser our own Content-Type the boundary is lost and the server
  // can't parse the envelope.
  const isFormData =
    typeof FormData !== "undefined" && init?.body instanceof FormData;
  if (!isFormData && !headers.has("content-type")) {
    headers.set("content-type", "application/json");
  }
  if (!SAFE_METHODS.has(method)) {
    const csrf = readCookie(CSRF_COOKIE);
    if (csrf) headers.set(CSRF_HEADER, csrf);
  }
  const url = path.startsWith("/") ? `${API_PREFIX}${path}` : path;
  const res = await fetch(url, { ...init, credentials: "include", headers });

  if (res.status === 401) {
    // First-touch UX: bounce to the FE /sign-in page so the user sees a
    // "Sign in with Google" affordance before we punt them to Google's
    // consent screen. The page itself picks up `?from=…` to return them
    // to where they were headed once auth completes.
    if (window.location.pathname !== "/sign-in") {
      const back = encodeURIComponent(
        window.location.pathname + window.location.search,
      );
      window.location.href = `/sign-in?from=${back}`;
    }
    throw new AuthRedirect();
  }

  if (res.status === 403) {
    const body = await res.text().catch(() => "");
    useAuthStore
      .getState()
      .setError({ kind: "forbidden", message: body || undefined });
    throw new ApiError(403, body);
  }

  if (!res.ok) {
    const body = await res.text().catch(() => "");
    throw new ApiError(res.status, body);
  }

  if (res.status === 204) return undefined as T;
  return (await res.json()) as T;
}

export type SwitchOrgResponse = { active_org_id: string; role: Role };

export const api = {
  me: () => request<Me>("/me"),
  switchOrg: (orgId: string) =>
    request<SwitchOrgResponse>("/auth/switch-org", {
      method: "POST",
      body: JSON.stringify({ org_id: orgId }),
    }),
  logout: () => request<void>("/auth/logout", { method: "POST" }),

  /** Owner/admin only — mutates the active org's `default_language`.
   *  Server returns `{ default_language: Language }`; the caller is
   *  expected to mirror the value into `useAuthStore` so the UI flips
   *  immediately without waiting for a `/me` re-poll. */
  setOrgLanguage: (language: Language) =>
    request<{ default_language: Language }>("/me/org/language", {
      method: "PATCH",
      body: JSON.stringify({ language }),
    }),

  /** Read-only catalog of catalog model ids the agent picker offers.
   *  Mirrors `src/http/routes/models.rs`. */
  models: () => request<ModelEntry[]>("/models"),

  agents: () => request<Agent[]>("/agents"),
  agent: (id: string) => request<Agent>(`/agents/${id}`),
  updateAgent: (id: string, patch: UpdateAgentRequest) =>
    request<Agent>(`/agents/${id}`, {
      method: "PUT",
      body: JSON.stringify(patch),
    }),
  agentToolCalls: (
    id: string,
    params?: { limit?: number; before?: string },
  ) => {
    const search = new URLSearchParams();
    if (params?.limit !== undefined) search.set("limit", String(params.limit));
    if (params?.before) search.set("before", params.before);
    const q = search.toString();
    return request<AgentToolCallList>(
      `/agents/${id}/tool-calls${q ? `?${q}` : ""}`,
    );
  },

  // ─── Agent logs & metrics ────────────────────────────────────────────
  // Declared in `src/http/routes/agents.rs`. All paths are tenant-gated
  // server-side; a 404 means "this agent isn't visible to your principal"
  // (intentional, no cross-org leak).
  agentMetricsTimeseries: (
    id: string,
    params: {
      from?: string;
      to?: string;
      bucket?: "auto" | "5m" | "1h" | "1d";
      compare?: "prev_window" | "none";
    },
  ) => {
    const search = new URLSearchParams();
    if (params.from) search.set("from", params.from);
    if (params.to) search.set("to", params.to);
    if (params.bucket) search.set("bucket", params.bucket);
    if (params.compare) search.set("compare", params.compare);
    const q = search.toString();
    return request<MetricsTimeseriesResponse>(
      `/agents/${id}/metrics/timeseries${q ? `?${q}` : ""}`,
    );
  },
  agentTurns: (
    id: string,
    params: {
      from?: string;
      to?: string;
      kind?: "normal" | "reflection" | "resolution" | "all";
      cursor?: string;
    },
  ) => {
    const search = new URLSearchParams();
    if (params.from) search.set("from", params.from);
    if (params.to) search.set("to", params.to);
    if (params.kind && params.kind !== "all") search.set("kind", params.kind);
    if (params.cursor) search.set("cursor", params.cursor);
    const q = search.toString();
    return request<AgentTurnsList>(`/agents/${id}/turns${q ? `?${q}` : ""}`);
  },
  /** Drawer payload for one turn (doc/logs_metrics_tab.md §5.4). The BE
   *  caps every join (`MAX_TOOL_CALLS_PER_TURN`,
   *  `MAX_MEMORY_WRITES_PER_TURN`, `MAX_REASONING_BLOCKS_PER_TURN`), so
   *  the response is bounded — no follow-up pagination needed. */
  turnDetail: (requestId: string) =>
    request<TurnDetail>(`/turns/${requestId}`),

  // ─── Agent memory ────────────────────────────────────────────────────
  // All five routes are declared in `src/http/routes/memory.rs`. Every
  // path is tenant-gated by `gate_agent` server-side; a 404 here means
  // "this agent is not visible to your principal" (intentional — we
  // don't leak cross-org existence).
  agentMemory: (id: string) => request<MemoryRow[]>(`/agents/${id}/memory`),
  agentMemoryEvents: (id: string, filter?: MemoryEventsFilter) => {
    const search = new URLSearchParams();
    if (filter?.source) search.set("source", filter.source);
    if (filter?.mutation) search.set("mutation", filter.mutation);
    const q = search.toString();
    return request<MemoryEvent[]>(
      `/agents/${id}/memory/events${q ? `?${q}` : ""}`,
    );
  },
  createMemoryNote: (id: string, input: CreateMemoryNoteRequest) =>
    request<MemoryRow>(`/agents/${id}/memory`, {
      method: "POST",
      body: JSON.stringify(input),
    }),
  pinMemory: (id: string, memoryId: string) =>
    request<MemoryRow>(`/agents/${id}/memory/${memoryId}/pin`, {
      method: "POST",
    }),
  unpinMemory: (id: string, memoryId: string) =>
    request<MemoryRow>(`/agents/${id}/memory/${memoryId}/unpin`, {
      method: "POST",
    }),
  // BE returns one of `MemoryRowResponse` or `{ removed: true }` —
  // collapsed to a discriminated union so callers can branch cleanly.
  revertMemoryEvent: (id: string, eventId: string) =>
    request<MemoryRow | { removed: true }>(
      `/agents/${id}/memory/events/${eventId}/revert`,
      { method: "POST" },
    ),

  threads: () => request<ThreadSummary[]>("/threads"),

  threadMessages: (rootId: string) =>
    request<ThreadMessage[]>(`/threads/${rootId}/messages`),

  submitPrompt: (input: {
    session_id?: string;
    agent_id?: string;
    content: string;
    idempotency_key: string;
  }) =>
    request<SubmitPromptResponse>("/prompts", {
      method: "POST",
      body: JSON.stringify(input),
    }),

  cancelRequest: async (requestId: string) => {
    try {
      await request<void>(`/requests/${requestId}/cancel`, { method: "POST" });
    } catch (e) {
      if (e instanceof ApiError && e.status === 404) return;
      throw e;
    }
  },

  // ─── MCP servers ────────────────────────────────────────────────────
  mcpServers: () => request<McpServer[]>("/mcp-servers"),
  mcpCatalog: () => request<McpCatalogEntry[]>("/mcp-catalog"),
  mcpServer: (id: string) => request<McpServer>(`/mcp-servers/${id}`),
  createMcpServer: (input: CreateMcpServerRequest) =>
    request<McpServer>("/mcp-servers", {
      method: "POST",
      body: JSON.stringify(input),
    }),
  updateMcpServer: (id: string, patch: UpdateMcpServerRequest) =>
    request<McpServer>(`/mcp-servers/${id}`, {
      method: "PUT",
      body: JSON.stringify(patch),
    }),
  deleteMcpServer: (id: string) =>
    request<void>(`/mcp-servers/${id}`, { method: "DELETE" }),
  putMcpCredentials: (id: string, payload: CredentialInput) =>
    request<void>(`/mcp-servers/${id}/credentials`, {
      method: "PUT",
      body: JSON.stringify(payload),
    }),
  deleteMcpCredentials: (id: string) =>
    request<void>(`/mcp-servers/${id}/credentials`, { method: "DELETE" }),
  mcpTestConnect: (input: TestConnectRequest) =>
    request<TestConnectResponse>("/mcp-servers/test-connect", {
      method: "POST",
      body: JSON.stringify(input),
    }),
  mcpOAuthStart: (id: string, input: OAuthStartRequest) =>
    request<OAuthStartResponse>(`/mcp-servers/${id}/oauth/start`, {
      method: "POST",
      body: JSON.stringify(input),
    }),
  mcpOAuthDisconnect: (id: string) =>
    request<{ ok: boolean }>(`/mcp-servers/${id}/oauth/disconnect`, {
      method: "POST",
    }),
  mcpServerToolCalls: (
    serverId: string,
    params?: { limit?: number; before?: string },
  ) => {
    const search = new URLSearchParams();
    if (params?.limit !== undefined) search.set("limit", String(params.limit));
    if (params?.before) search.set("before", params.before);
    const q = search.toString();
    return request<ToolCallList>(
      `/mcp-servers/${serverId}/tool-calls${q ? `?${q}` : ""}`,
    );
  },

  /** Upload a new avatar for the signed-in user. The backend writes the
   *  object to R2 and persists the URL on `users.avatar_url`. */
  uploadAvatar: (file: File) => {
    const form = new FormData();
    form.append("file", file);
    return request<UploadResponse>("/uploads/avatar", {
      method: "POST",
      body: form,
    });
  },

  /** Upload a tile icon for an org-scoped MCP catalog entry. Owner/admin
   *  only; built-in (global) catalog ids return 403. */
  uploadMcpCatalogIcon: (catalogId: string, file: File) => {
    const form = new FormData();
    form.append("file", file);
    return request<UploadResponse>(
      `/uploads/mcp-catalog/${encodeURIComponent(catalogId)}`,
      { method: "POST", body: form },
    );
  },
};
