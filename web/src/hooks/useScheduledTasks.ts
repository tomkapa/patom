import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "../lib/api";
import { ApiError } from "../lib/errors";

const KEY = (id: string, page: number, perPage: number) =>
  ["agents", id, "scheduled-tasks", page, perPage] as const;

/** One page of an agent's scheduled tasks plus the server's status
 *  rollup. `keepPreviousData` keeps the table mounted while paging so the
 *  footer/pagination don't flash empty between pages. */
export function useScheduledTasks(
  id: string | null,
  page: number,
  perPage: number,
) {
  return useQuery({
    queryKey: id
      ? KEY(id, page, perPage)
      : ["agents", "none", "scheduled-tasks"],
    queryFn: () => api.scheduledTasks(id ?? "", { page, per_page: perPage }),
    enabled: Boolean(id),
    placeholderData: (prev) => prev,
    staleTime: 30_000,
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

export function useCancelScheduledTask(id: string | null) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (taskId: string) => api.cancelScheduledTask(id ?? "", taskId),
    onSuccess: () => {
      // Invalidate every page + the summary for this agent.
      qc.invalidateQueries({
        queryKey: id
          ? ["agents", id, "scheduled-tasks"]
          : ["agents", "none", "scheduled-tasks"],
      });
    },
  });
}
