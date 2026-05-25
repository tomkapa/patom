import {
  useMutation,
  useQuery,
  useQueryClient,
} from "@tanstack/react-query";
import { api } from "../lib/api";
import type {
  CreateMemoryNoteRequest,
  MemoryEventsFilter,
} from "../types/api";

const LIST_KEY = (id: string) => ["agents", id, "memory"] as const;
const EVENTS_KEY = (id: string, filter: MemoryEventsFilter) =>
  ["agents", id, "memory", "events", filter] as const;

/** List of materialized memory rows for one agent. Refetches every 15s
 *  so a parallel agent turn (or another operator) becomes visible
 *  without a manual reload — matches the cadence in
 *  `AgentActivityCard`. */
export function useAgentMemory(id: string | null) {
  return useQuery({
    queryKey: id ? LIST_KEY(id) : ["agents", "none", "memory"],
    enabled: Boolean(id),
    queryFn: () => api.agentMemory(id ?? ""),
    refetchInterval: 15_000,
    staleTime: 0,
  });
}

/** Audit-log events for one agent. Filter object is part of the query
 *  key so flipping a chip refetches with the right `?source=`/
 *  `?mutation=` params. */
export function useAgentMemoryEvents(
  id: string | null,
  filter: MemoryEventsFilter,
) {
  return useQuery({
    queryKey: id
      ? EVENTS_KEY(id, filter)
      : ["agents", "none", "memory", "events", filter],
    enabled: Boolean(id),
    queryFn: () => api.agentMemoryEvents(id ?? "", filter),
    refetchInterval: 15_000,
    staleTime: 0,
  });
}

function invalidateMemory(
  qc: ReturnType<typeof useQueryClient>,
  agentId: string,
) {
  qc.invalidateQueries({ queryKey: LIST_KEY(agentId) });
  qc.invalidateQueries({ queryKey: ["agents", agentId, "memory", "events"] });
}

export function useCreateMemoryNote() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({
      agentId,
      input,
    }: {
      agentId: string;
      input: CreateMemoryNoteRequest;
    }) => api.createMemoryNote(agentId, input),
    onSuccess: (_, vars) => invalidateMemory(qc, vars.agentId),
  });
}

export function useSetMemoryPinned() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({
      agentId,
      memoryId,
      pinned,
    }: {
      agentId: string;
      memoryId: string;
      pinned: boolean;
    }) =>
      pinned
        ? api.pinMemory(agentId, memoryId)
        : api.unpinMemory(agentId, memoryId),
    onSuccess: (_, vars) => invalidateMemory(qc, vars.agentId),
  });
}

export function useRevertMemoryEvent() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({
      agentId,
      eventId,
    }: {
      agentId: string;
      eventId: string;
    }) => api.revertMemoryEvent(agentId, eventId),
    onSuccess: (_, vars) => invalidateMemory(qc, vars.agentId),
  });
}
