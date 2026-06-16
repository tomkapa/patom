import type {
  Agent,
  AgentToolCallList,
  AgentTurnsList,
  Attachment,
  Channel,
  ChannelMember,
  TurnDetail,
  CreateMcpServerRequest,
  CreateMemoryNoteRequest,
  CredentialInput,
  IssuedInvite,
  Language,
  SlackInstallResponse,
  SlackWorkspaceSummary,
  ListMembersQuery,
  ListMembersResponse,
  Me,
  McpCatalogEntry,
  McpServer,
  MemoryEvent,
  MemoryEventsFilter,
  MemoryRow,
  MetricsTimeseriesResponse,
  ModelEntry,
  OrgBilling,
  OrgCredits,
  OrgDetails,
  ProviderCredentialInput,
  ProviderCredentialView,
  ProviderValidateResult,
  PromptVersionList,
  RestorePromptVersionResponse,
  ScheduledTask,
  ScheduledTaskList,
  OAuthStartRequest,
  OAuthStartResponse,
  Role,
  SubmitPromptResponse,
  TestConnectRequest,
  TestConnectResponse,
  ThreadMessage,
  TagRef,
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
const CSRF_COOKIE = "patom_csrf";
const CSRF_HEADER = "X-CSRF-Token";
const SAFE_METHODS = new Set(["GET", "HEAD", "OPTIONS"]);

// JSON endpoints live under this prefix; `/auth/oidc/*` bypasses it.
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
  /** Redeem an invite by its URL token (the `/i/{slug}/{token}` link).
   *  The server joins the inviting org and re-mints the session so it
   *  becomes the active workspace — same response shape as `switchOrg`.
   *  A 401 here means the visitor isn't signed in; `request()` bounces
   *  them through `/sign-in` and back to the invite URL automatically. */
  acceptInvite: (token: string) =>
    request<SwitchOrgResponse>("/me/invites/accept", {
      method: "POST",
      body: JSON.stringify({ token }),
    }),
  /** Create a new workspace (cloud only). The server makes the caller
   *  Owner, seeds a default agent, and re-mints the session into the new
   *  org — same `{ active_org_id, role }` shape as `switchOrg`, so the
   *  caller just invalidates `/me` to land inside it. A 409 means the
   *  per-user workspace cap was hit (`org.limit_reached`); 403 means the
   *  deployment is self-host (creation disabled). */
  createOrg: (name: string) =>
    request<SwitchOrgResponse>("/me/orgs", {
      method: "POST",
      body: JSON.stringify({ name }),
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

  /** Owner/admin only — set or clear the active org's `default_rule`, the
   *  `<organization-rule>` directive injected into every agent's system
   *  prompt. `null` (or an all-whitespace string the server folds to
   *  `null`) clears it. Server returns the stored value; over the 16 KiB
   *  cap it 400s. Members get 403 — the FE hides the editor for them. */
  setOrgRule: (rule: string | null) =>
    request<{ default_rule: string | null }>("/me/org/rule", {
      method: "PATCH",
      body: JSON.stringify({ rule }),
    }),

  // ─── Workspace settings (src/http/routes/org.rs) ────────────────────
  /** Read the General-tab payload for the active workspace. */
  org: () => request<OrgDetails>("/me/org"),
  /** Update the active workspace. Body fields are independent — pass
   *  any subset. `onboarded: true` flips the org's `onboarded_at` from
   *  NULL to NOW() (idempotent via COALESCE; never un-marks); the FE
   *  wizard's final step uses this to release the `OnboardingGate`. */
  updateOrg: (patch: {
    name?: string;
    slug?: string;
    onboarded?: boolean;
  }) =>
    request<OrgDetails>("/me/org", {
      method: "PATCH",
      body: JSON.stringify(patch),
    }),
  /** Permanently delete the active workspace (owner only, server-gated).
   *  Cascades to every org-scoped record. The server re-mints the session
   *  into the caller's first remaining org, or an org-less session when
   *  none remain — returned as `active_org_id` so the caller can route
   *  (another workspace → land there; `null` → onboarding / sign-in). */
  deleteOrg: () =>
    request<{ active_org_id: string | null }>("/me/org", {
      method: "DELETE",
    }),
  /** Read the active workspace's spend budget: cap + warn threshold +
   *  current-period usage. Any member may read. */
  orgBilling: () => request<OrgBilling>("/me/org/billing"),
  /** Read the active workspace's free-credit balance + recent ledger (#154).
   *  Any member may read. */
  orgCredits: () => request<OrgCredits>("/me/org/credits"),
  /** Set/clear the cap + warn threshold. Owner/admin only (server-gated).
   *  `monthly_cap_micro_usd: null` clears the cap (unlimited). */
  updateOrgBilling: (body: {
    monthly_cap_micro_usd: number | null;
    warn_threshold_bps: number;
  }) =>
    request<OrgBilling>("/me/org/billing", {
      method: "PUT",
      body: JSON.stringify(body),
    }),
  /** List BYO provider keys (masked), one per provider (#141). Any member. */
  providerCredentials: () =>
    request<ProviderCredentialView[]>("/me/org/provider-credentials"),
  /** Add or rotate the key for `provider`. Owner/admin only. 204. */
  putProviderCredentials: (provider: string, body: ProviderCredentialInput) =>
    request<void>(
      `/me/org/provider-credentials/${encodeURIComponent(provider)}`,
      { method: "PUT", body: JSON.stringify(body) },
    ),
  /** Remove the key for `provider`. Owner/admin only. 204. */
  deleteProviderCredentials: (provider: string) =>
    request<void>(
      `/me/org/provider-credentials/${encodeURIComponent(provider)}`,
      { method: "DELETE" },
    ),
  /** Test a candidate key against the live provider before/after save. */
  validateProviderCredentials: (
    provider: string,
    body: ProviderCredentialInput,
  ) =>
    request<ProviderValidateResult>(
      `/me/org/provider-credentials/${encodeURIComponent(provider)}/validate`,
      { method: "POST", body: JSON.stringify(body) },
    ),
  members: (q: ListMembersQuery = {}) => {
    const search = new URLSearchParams();
    if (q.q) search.set("q", q.q);
    if (q.status) search.set("status", q.status);
    if (q.role) search.set("role", q.role);
    if (q.page !== undefined) search.set("page", String(q.page));
    if (q.per_page !== undefined)
      search.set("per_page", String(q.per_page));
    const qs = search.toString();
    return request<ListMembersResponse>(
      `/me/org/members${qs ? `?${qs}` : ""}`,
    );
  },
  changeMemberRole: (userId: string, role: Role) =>
    request<void>(`/me/org/members/${userId}/role`, {
      method: "PATCH",
      body: JSON.stringify({ role }),
    }),
  removeMember: (userId: string) =>
    request<void>(`/me/org/members/${userId}`, { method: "DELETE" }),
  leaveOrg: () => request<void>("/me/org/leave", { method: "POST" }),
  inviteMembers: (emails: string[], role: Role) =>
    request<IssuedInvite[]>("/me/org/invites", {
      method: "POST",
      body: JSON.stringify({ emails, role }),
    }),
  resendInvite: (inviteId: string) =>
    request<IssuedInvite>(`/me/org/invites/${inviteId}/resend`, {
      method: "POST",
    }),
  revokeInvite: (inviteId: string) =>
    request<void>(`/me/org/invites/${inviteId}`, { method: "DELETE" }),

  // ─── Integrations — Slack (src/slack/oauth.rs) ──────────────────────
  /** Begin a Slack install. The server returns the Slack consent URL;
   *  the SPA navigates to it so the user lands back on
   *  `/settings/integrations` once they finish the install. */
  slackInstall: () =>
    request<SlackInstallResponse>("/slack/install", { method: "POST" }),
  /** List Slack workspaces installed against the active org. RLS in
   *  Postgres constrains the result to the principal's tenant. */
  slackWorkspaces: () =>
    request<SlackWorkspaceSummary[]>("/slack/workspaces"),
  /** Uninstall a Slack workspace by `team_id`. ON DELETE CASCADE cleans
   *  up identities + thread bindings server-side. */
  slackDisconnect: (teamId: string) =>
    request<void>(`/slack/workspaces/${encodeURIComponent(teamId)}`, {
      method: "DELETE",
    }),

  /** Read-only catalog of catalog model ids the agent picker offers.
   *  Mirrors `src/http/routes/models.rs`. */
  models: () => request<ModelEntry[]>("/models"),

  agents: () => request<Agent[]>("/agents"),
  agent: (id: string) => request<Agent>(`/agents/${id}`),
  /** Create one agent. Mirrors `src/http/routes/agents.rs::CreateAgentRequest`.
   *  Used by the onboarding wizard to hire each preset agent
   *  ({@link teamPresets}) sequentially — the entitlements gate fires
   *  per call so cloud quotas still apply. */
  createAgent: (payload: {
    name: string;
    system_prompt: string;
    description: string;
    allowed_mcp_tools?: Record<string, string[] | null>;
    model?: string | null;
    avatar_url?: string | null;
  }) =>
    request<Agent>("/agents", {
      method: "POST",
      body: JSON.stringify(payload),
    }),
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

  // ─── Scheduled tasks ──────────────────────────────────────────────────
  /** Page through the agent's scheduled tasks. `summary` is a tenant-gated
   *  server rollup, so the stats strip stays correct regardless of which
   *  page is in view. Tenant-gated server-side; a 404 means "not visible
   *  to your principal". */
  scheduledTasks: (
    id: string,
    params?: { page?: number; per_page?: number },
  ) => {
    const search = new URLSearchParams();
    if (params?.page !== undefined) search.set("page", String(params.page));
    if (params?.per_page !== undefined)
      search.set("per_page", String(params.per_page));
    const q = search.toString();
    return request<ScheduledTaskList>(
      `/agents/${id}/scheduled-tasks${q ? `?${q}` : ""}`,
    );
  },
  /** Cancel one scheduled task. Idempotent: cancelling an already-cancelled
   *  task is a no-op 200. Returns the updated row. */
  cancelScheduledTask: (id: string, taskId: string) =>
    request<ScheduledTask>(
      `/agents/${id}/scheduled-tasks/${taskId}/cancel`,
      { method: "POST" },
    ),

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
  turnDetail: (turnId: string) =>
    request<TurnDetail>(`/turns/${turnId}`),

  // ─── Agent prompt versions (doc/logs_metrics_tab.md §4.1, §4.5) ─────
  /** Newest-first list of every (system_prompt, model) snapshot for one
   *  agent. Capped at 100 server-side. The diff modal drives the picker
   *  from this list. */
  agentPromptVersions: (id: string) =>
    request<PromptVersionList>(`/agents/${id}/prompt-versions`),

  /** Append-only restore: server snapshots the named version into a new
   *  row whose `version` is `max+1`, then mirrors onto the live agent.
   *  Reverting v7→v6 returns v8 byte-identical to v6 — history is never
   *  rewritten. */
  restorePromptVersion: (id: string, version: number) =>
    request<RestorePromptVersionResponse>(
      `/agents/${id}/prompt-versions/${version}/restore`,
      { method: "POST" },
    ),

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

  // `channelId` selects the feed: a channel's threads when set, or the
  // caller's direct messages when null/omitted (BE: `channel_id IS NULL`).
  // In DM mode `counterpart` (the same satellite `{kind, id}` the tags wire
  // uses) narrows to one pair's conversation.
  threads: (channelId?: string | null, counterpart?: TagRef | null) => {
    const params = new URLSearchParams();
    if (channelId) params.set("channel_id", channelId);
    else if (counterpart) {
      params.set("counterpart_kind", counterpart.kind);
      params.set("counterpart_id", counterpart.id);
    }
    const q = params.toString();
    return request<ThreadSummary[]>(q ? `/threads?${q}` : "/threads");
  },

  threadMessages: (threadId: string) =>
    request<ThreadMessage[]>(`/threads/${threadId}/messages`),

  submitPrompt: (input: {
    /** Reply into an existing thread. Omit to start a new one. */
    thread_id?: string;
    /** Explicit @tags in message order; empty/omitted = a plain post. */
    tags?: TagRef[];
    /** Post the new thread into this channel. Ignored by the BE when
     *  `thread_id` is set (a reply inherits its thread's location). */
    channel_id?: string;
    /** Who a fresh DM root is with. Required when neither `thread_id` nor
     *  `channel_id` is given. */
    counterpart?: TagRef;
    content: string;
    /** Image/file attachment references from `uploadAttachment` (issue #187). */
    attachments?: Attachment[];
    idempotency_key: string;
  }) =>
    request<SubmitPromptResponse>("/prompts", {
      method: "POST",
      body: JSON.stringify(input),
    }),

  // ─── Channels ────────────────────────────────────────────────────────
  channels: () => request<Channel[]>("/channels"),
  createChannel: (name: string) =>
    request<Channel>("/channels", {
      method: "POST",
      body: JSON.stringify({ name }),
    }),
  updateChannel: (id: string, patch: { name?: string; archived?: boolean }) =>
    request<Channel>(`/channels/${id}`, {
      method: "PATCH",
      body: JSON.stringify(patch),
    }),
  channelMembers: (id: string) =>
    request<ChannelMember[]>(`/channels/${id}/members`),
  addChannelMember: (id: string, userId: string) =>
    request<void>(`/channels/${id}/members`, {
      method: "POST",
      body: JSON.stringify({ user_id: userId }),
    }),
  removeChannelMember: (id: string, userId: string) =>
    request<void>(`/channels/${id}/members/${userId}`, { method: "DELETE" }),

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

  /** Upload one message attachment (image / PDF / Office). Returns the
   *  reference to pass back in `submitPrompt({ attachments })` (issue #187). */
  uploadAttachment: (file: File) => {
    const form = new FormData();
    form.append("file", file);
    return request<Attachment>("/uploads/attachment", {
      method: "POST",
      body: form,
    });
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

  /** Upload a new workspace (organization) avatar. Owner/admin only. */
  uploadWorkspaceAvatar: (file: File) => {
    const form = new FormData();
    form.append("file", file);
    return request<UploadResponse>("/uploads/workspace-avatar", {
      method: "POST",
      body: form,
    });
  },

  /** Upload an avatar for an agent the caller's org owns. The backend
   *  stores the object (keyed by agent id) and returns the URL; it is NOT
   *  persisted to the agent row here — include the returned URL in a
   *  subsequent `updateAgent` patch (issue #43). */
  uploadAgentAvatar: (agentId: string, file: File) => {
    const form = new FormData();
    form.append("file", file);
    return request<UploadResponse>(
      `/uploads/agent-avatar/${encodeURIComponent(agentId)}`,
      { method: "POST", body: form },
    );
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
