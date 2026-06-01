import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "../lib/api";
import type { OrgBudget } from "../types/api";

export const ORG_BUDGET_KEY = ["orgBudget"] as const;

/** Active workspace's spend budget (cap + warn threshold + period usage).
 *  Refetched lazily; the mutation below invalidates it on save. */
export function useOrgBudget() {
  return useQuery({
    queryKey: ORG_BUDGET_KEY,
    queryFn: api.orgBudget,
    staleTime: 30_000,
  });
}

export function useUpdateOrgBudget() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (body: {
      monthly_cap_micro_usd: number | null;
      warn_threshold_bps: number;
    }) => api.updateOrgBudget(body),
    onSuccess: (data: OrgBudget) => {
      qc.setQueryData(ORG_BUDGET_KEY, data);
      qc.invalidateQueries({ queryKey: ORG_BUDGET_KEY });
    },
  });
}
