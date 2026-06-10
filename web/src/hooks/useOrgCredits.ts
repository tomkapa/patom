import { useQuery } from "@tanstack/react-query";
import { api } from "../lib/api";

export const ORG_CREDITS_KEY = ["orgCredits"] as const;

/** Active workspace's free-credit balance + recent ledger (#154). Any member
 *  may read; refetched lazily. */
export function useOrgCredits() {
  return useQuery({
    queryKey: ORG_CREDITS_KEY,
    queryFn: api.orgCredits,
    staleTime: 30_000,
  });
}
