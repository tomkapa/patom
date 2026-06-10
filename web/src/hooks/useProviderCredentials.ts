import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";

import { api } from "../lib/api";
import type { ProviderCredentialInput } from "../types/api";

export const PROVIDER_CREDENTIALS_KEY = ["providerCredentials"] as const;

/** Masked per-org BYO provider keys (#141). Any member may read. */
export function useProviderCredentials() {
  return useQuery({
    queryKey: PROVIDER_CREDENTIALS_KEY,
    queryFn: api.providerCredentials,
    staleTime: 30_000,
  });
}

/** Add or rotate a provider key. Invalidates the list on success so the
 *  status flips to active and the masked suffix updates. */
export function usePutProviderCredentials() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (vars: { provider: string; body: ProviderCredentialInput }) =>
      api.putProviderCredentials(vars.provider, vars.body),
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: PROVIDER_CREDENTIALS_KEY });
    },
  });
}

export function useDeleteProviderCredentials() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (provider: string) => api.deleteProviderCredentials(provider),
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: PROVIDER_CREDENTIALS_KEY });
    },
  });
}

/** Test a candidate key before saving. Does not mutate the list (the server
 *  may stamp `last_validated_at` on an existing row, so callers refetch). */
export function useValidateProviderCredentials() {
  return useMutation({
    mutationFn: (vars: { provider: string; body: ProviderCredentialInput }) =>
      api.validateProviderCredentials(vars.provider, vars.body),
  });
}
