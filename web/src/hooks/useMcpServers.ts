import {
  useInfiniteQuery,
  useMutation,
  useQuery,
  useQueryClient,
} from "@tanstack/react-query";
import { api } from "../lib/api";
import { ApiError } from "../lib/errors";
import type {
  CreateMcpServerRequest,
  CredentialInput,
  OAuthStartRequest,
  TestConnectRequest,
  UpdateMcpServerRequest,
} from "../types/api";

const LIST_KEY = ["mcp-servers"] as const;
const ONE_KEY = (id: string) => ["mcp-servers", id] as const;
const TOOL_CALLS_KEY = (id: string) =>
  ["mcp-servers", id, "tool-calls"] as const;
const CATALOG_KEY = ["mcp-catalog"] as const;

export function useMcpServers() {
  return useQuery({
    queryKey: LIST_KEY,
    queryFn: api.mcpServers,
    staleTime: 15_000,
  });
}

/** Server-managed list of MCP catalog entries the workspace can wire
 *  (global built-ins + tenant-custom). Backend source of truth — the
 *  hard-coded `MCP_CATALOG` in `data/mcpCatalog.ts` is kept only for
 *  visual metadata (icons, brand colors) keyed by `catalog_id`. */
export function useMcpCatalog() {
  return useQuery({
    queryKey: CATALOG_KEY,
    queryFn: api.mcpCatalog,
    staleTime: 60_000,
  });
}

export function useMcpServer(
  id: string | null,
  options?: { refetchInterval?: number | false; enabled?: boolean },
) {
  return useQuery({
    queryKey: id ? ONE_KEY(id) : ["mcp-servers", "none"],
    queryFn: () => api.mcpServer(id ?? ""),
    enabled: Boolean(id) && options?.enabled !== false,
    refetchInterval: options?.refetchInterval,
    staleTime: 0,
    // 404 / 403 are terminal — render the not-found state immediately
    // instead of spinning through three exponential-backoff retries.
    retry: (count, err) => {
      if (err instanceof ApiError && (err.status === 404 || err.status === 403)) {
        return false;
      }
      return count < 3;
    },
  });
}

function invalidateAll(qc: ReturnType<typeof useQueryClient>) {
  qc.invalidateQueries({ queryKey: LIST_KEY });
  // Wired/unwired state on `useMcpCatalog` is derived from the same
  // backend join — drop the catalog cache too or tiles stay stale.
  qc.invalidateQueries({ queryKey: CATALOG_KEY });
}

export function useCreateMcpServer() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (input: CreateMcpServerRequest) => api.createMcpServer(input),
    onSuccess: () => invalidateAll(qc),
  });
}

export function useUpdateMcpServer() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({
      id,
      patch,
    }: {
      id: string;
      patch: UpdateMcpServerRequest;
    }) => api.updateMcpServer(id, patch),
    onSuccess: (_, vars) => {
      invalidateAll(qc);
      qc.invalidateQueries({ queryKey: ONE_KEY(vars.id) });
    },
  });
}

export function useDeleteMcpServer() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (id: string) => api.deleteMcpServer(id),
    onSuccess: () => invalidateAll(qc),
  });
}

export function usePutMcpCredentials() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ id, payload }: { id: string; payload: CredentialInput }) =>
      api.putMcpCredentials(id, payload),
    onSuccess: (_, vars) => {
      invalidateAll(qc);
      qc.invalidateQueries({ queryKey: ONE_KEY(vars.id) });
    },
  });
}

export function useDeleteMcpCredentials() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (id: string) => api.deleteMcpCredentials(id),
    onSuccess: (_, id) => {
      invalidateAll(qc);
      qc.invalidateQueries({ queryKey: ONE_KEY(id) });
    },
  });
}

export function useTestConnect() {
  return useMutation({
    mutationFn: (input: TestConnectRequest) => api.mcpTestConnect(input),
  });
}

export function useStartOAuth() {
  return useMutation({
    mutationFn: ({ id, input }: { id: string; input: OAuthStartRequest }) =>
      api.mcpOAuthStart(id, input),
  });
}

/**
 * Cursor-paginated audit log for an MCP server's tool calls. Built on
 * `useInfiniteQuery` so the card can render a "Load more" affordance
 * without churning the API later. The first page is what
 * `RecentActivityCard` renders by default; `refetchInterval` matches the
 * page-level server refetch so the table stays close to live.
 */
export function useMcpServerToolCalls(serverId: string | null) {
  return useInfiniteQuery({
    queryKey: serverId
      ? TOOL_CALLS_KEY(serverId)
      : (["mcp-servers", "none", "tool-calls"] as const),
    enabled: Boolean(serverId),
    initialPageParam: undefined as string | undefined,
    queryFn: ({ pageParam }) =>
      api.mcpServerToolCalls(serverId ?? "", { before: pageParam }),
    getNextPageParam: (last) => last.next_cursor ?? undefined,
    refetchInterval: 15_000,
    staleTime: 5_000,
    retry: (count, err) => {
      if (
        err instanceof ApiError &&
        (err.status === 404 || err.status === 403)
      ) {
        return false;
      }
      return count < 3;
    },
  });
}

export function useDisconnectOAuth() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (id: string) => api.mcpOAuthDisconnect(id),
    onSuccess: (_, id) => {
      invalidateAll(qc);
      qc.invalidateQueries({ queryKey: ONE_KEY(id) });
    },
  });
}
