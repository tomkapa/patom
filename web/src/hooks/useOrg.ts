import {
  keepPreviousData,
  useMutation,
  useQuery,
  useQueryClient,
} from "@tanstack/react-query";
import { api } from "../lib/api";
import type {
  IssuedInvite,
  ListMembersQuery,
  Role,
} from "../types/api";

export const ORG_KEY = ["org"] as const;
export const MEMBERS_KEY = (q: ListMembersQuery = {}) =>
  ["org", "members", q] as const;

/** General-tab payload. Polled lazily — refetch only on tab focus so
 *  routine edits (name, slug) round-trip through the optimistic
 *  update + refetch cycle below. */
export function useOrg() {
  return useQuery({
    queryKey: ORG_KEY,
    queryFn: api.org,
    staleTime: 30_000,
  });
}

/** Paginated Members list. `keepPreviousData` prevents the table from
 *  blanking when the filter / page changes — the new rows fade in. */
export function useMembers(q: ListMembersQuery) {
  return useQuery({
    queryKey: MEMBERS_KEY(q),
    queryFn: () => api.members(q),
    staleTime: 15_000,
    placeholderData: keepPreviousData,
  });
}

export function useUpdateOrg() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (patch: { name?: string; slug?: string }) =>
      api.updateOrg(patch),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ORG_KEY });
      qc.invalidateQueries({ queryKey: ["me"] });
    },
  });
}

export function useChangeMemberRole() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ userId, role }: { userId: string; role: Role }) =>
      api.changeMemberRole(userId, role),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["org", "members"] }),
  });
}

export function useRemoveMember() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (userId: string) => api.removeMember(userId),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["org", "members"] }),
  });
}

export function useLeaveOrg() {
  return useMutation({
    mutationFn: () => api.leaveOrg(),
  });
}

export function useInviteMembers() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ emails, role }: { emails: string[]; role: Role }) =>
      api.inviteMembers(emails, role),
    onSuccess: (_data: IssuedInvite[]) => {
      qc.invalidateQueries({ queryKey: ["org", "members"] });
      qc.invalidateQueries({ queryKey: ORG_KEY });
    },
  });
}

export function useResendInvite() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (inviteId: string) => api.resendInvite(inviteId),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["org", "members"] }),
  });
}

export function useRevokeInvite() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (inviteId: string) => api.revokeInvite(inviteId),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["org", "members"] }),
  });
}
