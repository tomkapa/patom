import { useInfiniteQuery, useQuery } from "@tanstack/react-query";
import { api } from "../lib/api";
import type {
  LogsCompareMode,
  LogsKindFilter,
  LogsTimeRange,
} from "../types/api";

/** Resolve a UI time range label into a `(from, to)` ISO window relative
 *  to `now`. Pure so the hook keys stay stable for the same selection. */
export function rangeToWindow(
  range: LogsTimeRange,
  now: Date = new Date(),
): { from: string; to: string } {
  const to = now.toISOString();
  const offset = (() => {
    switch (range) {
      case "1h":
        return 60 * 60 * 1000;
      case "24h":
        return 24 * 60 * 60 * 1000;
      case "7d":
        return 7 * 24 * 60 * 60 * 1000;
      case "30d":
        return 30 * 24 * 60 * 60 * 1000;
    }
  })();
  return { from: new Date(now.getTime() - offset).toISOString(), to };
}

const TIMESERIES_KEY = (id: string, range: LogsTimeRange, compare: LogsCompareMode) =>
  ["agents", id, "metrics", "timeseries", range, compare] as const;

const TURNS_KEY = (id: string, range: LogsTimeRange, kind: LogsKindFilter) =>
  ["agents", id, "turns", range, kind] as const;

/** Chart payload. Refetches every 15s (matches AgentActivityCard). */
export function useAgentMetricsTimeseries(
  id: string | null,
  range: LogsTimeRange,
  compare: LogsCompareMode,
) {
  return useQuery({
    queryKey: id
      ? TIMESERIES_KEY(id, range, compare)
      : (["agents", "none", "metrics", "timeseries", range, compare] as const),
    enabled: Boolean(id),
    queryFn: () => {
      const { from, to } = rangeToWindow(range);
      return api.agentMetricsTimeseries(id ?? "", { from, to, compare });
    },
    refetchInterval: 15_000,
    staleTime: 0,
  });
}

/** Timeline rows. Paged via cursor — `useInfiniteQuery` so the scroller
 *  can grow without re-fetching the head. */
export function useAgentTurns(
  id: string | null,
  range: LogsTimeRange,
  kind: LogsKindFilter,
) {
  return useInfiniteQuery({
    queryKey: id
      ? TURNS_KEY(id, range, kind)
      : (["agents", "none", "turns", range, kind] as const),
    enabled: Boolean(id),
    initialPageParam: undefined as string | undefined,
    queryFn: ({ pageParam }) => {
      const { from, to } = rangeToWindow(range);
      return api.agentTurns(id ?? "", { from, to, kind, cursor: pageParam });
    },
    getNextPageParam: (last) => last.next_cursor ?? undefined,
    refetchInterval: 15_000,
    staleTime: 0,
  });
}

const TURN_DETAIL_KEY = (requestId: string) =>
  ["turns", requestId, "detail"] as const;

/** Drawer payload for one turn. Enabled only when the caller passes a
 *  non-null request id — the drawer mounts on row expand and unmounts
 *  on collapse, so the hook only fires while the row is open.
 *
 *  Stale-after: 30 s. The data is audit-flavoured (post-turn snapshot),
 *  not realtime — refetching aggressively would burn the BE without
 *  giving the operator any new information mid-turn.
 *
 *  Mirrors `src/http/routes/turns.rs::TurnDetailResponse`. */
export function useTurnDetail(requestId: string | null) {
  return useQuery({
    queryKey: requestId
      ? TURN_DETAIL_KEY(requestId)
      : (["turns", "none", "detail"] as const),
    enabled: Boolean(requestId),
    queryFn: () => api.turnDetail(requestId ?? ""),
    staleTime: 30_000,
  });
}
