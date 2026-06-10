import {
  keepPreviousData,
  useMutation,
  useQuery,
  useQueryClient,
} from "@tanstack/react-query";
import { api } from "../lib/api";
import { track } from "../lib/analytics";
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

/** Create a new workspace and switch into it. The server re-mints the
 *  session cookie, so we invalidate every query to refetch under the new
 *  active org — same posture as `useSwitchOrg`. The mutation resolves to
 *  `{ active_org_id, role }`; callers await it before navigating. */
export function useCreateOrg() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (name: string) => api.createOrg(name),
    onSuccess: () => qc.invalidateQueries(),
  });
}

/** Delete the active workspace. The server re-mints the session into the
 *  next remaining org (or an org-less session). Invalidate everything so
 *  the app re-renders under the new session; the caller routes off the
 *  returned `active_org_id`. */
export function useDeleteOrg() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: () => api.deleteOrg(),
    onSuccess: () => qc.invalidateQueries(),
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
    onSuccess: (data: IssuedInvite[]) => {
      // Viral-loop signal. Onboarding's StepInvite calls api.inviteMembers
      // directly (not this hook), so this fires only for post-onboarding
      // invites from the members settings — no double-count.
      track("invite_sent", { count: data.length });
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
