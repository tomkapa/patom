import { useMutation, useQueryClient } from "@tanstack/react-query";
import { api } from "../lib/api";
import { track } from "../lib/analytics";

export function useSwitchOrg() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (orgId: string) => api.switchOrg(orgId),
    onSuccess: () => {
      // Re-identify is handled by AnalyticsBridge once /me refetches under
      // the new org; this just marks the deliberate switch action.
      track("org_switched");
      qc.invalidateQueries();
    },
  });
}
