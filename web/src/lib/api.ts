import type {
  Agent,
  AgentToolCallList,
  CreateMcpServerRequest,
  CredentialInput,
  Language,
  Me,
  McpCatalogEntry,
  McpServer,
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
  if (!headers.has("content-type")) {
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
};
