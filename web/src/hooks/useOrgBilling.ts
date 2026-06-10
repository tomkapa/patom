import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "../lib/api";
import type { OrgBilling } from "../types/api";

export const ORG_BILLING_KEY = ["orgBilling"] as const;

/** Active workspace's spend budget (cap + warn threshold + period usage).
 *  Refetched lazily; the mutation below seeds the cache from its response. */
export function useOrgBilling() {
  return useQuery({
    queryKey: ORG_BILLING_KEY,
    queryFn: api.orgBilling,
    staleTime: 30_000,
  });
}

export function useUpdateOrgBilling() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (body: {
      monthly_cap_micro_usd: number | null;
      warn_threshold_bps: number;
    }) => api.updateOrgBilling(body),
    // The PUT returns the fresh view, so seed the cache directly — no refetch.
    onSuccess: (data: OrgBilling) => {
      qc.setQueryData(ORG_BILLING_KEY, data);
    },
  });
}
