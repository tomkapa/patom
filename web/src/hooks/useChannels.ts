import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "../lib/api";

const LIST_KEY = ["channels"] as const;
const MEMBERS_KEY = (id: string) => ["channels", id, "members"] as const;

/** Channels the caller is a member of (active, non-archived). */
export function useChannels() {
  return useQuery({
    queryKey: LIST_KEY,
    queryFn: api.channels,
    staleTime: 30_000,
  });
}

export function useCreateChannel() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (name: string) => api.createChannel(name),
    onSuccess: () => qc.invalidateQueries({ queryKey: LIST_KEY }),
  });
}

/** Rename and/or archive a channel (creator-only on the server). */
export function useUpdateChannel() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({
      id,
      patch,
    }: {
      id: string;
      patch: { name?: string; archived?: boolean };
    }) => api.updateChannel(id, patch),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: LIST_KEY });
      qc.invalidateQueries({ queryKey: ["threads"] });
    },
  });
}

export function useChannelMembers(id: string | null) {
  return useQuery({
    queryKey: id ? MEMBERS_KEY(id) : ["channels", "none", "members"],
    queryFn: () => api.channelMembers(id ?? ""),
    enabled: Boolean(id),
    staleTime: 15_000,
  });
}

/** Invalidate the roster, plus the channel list and thread feed: when the
 *  caller's own membership changes, the sidebar and the visible feed both
 *  depend on it. */
function invalidateMembership(qc: ReturnType<typeof useQueryClient>, id: string) {
  void qc.invalidateQueries({ queryKey: MEMBERS_KEY(id) });
  void qc.invalidateQueries({ queryKey: LIST_KEY });
  void qc.invalidateQueries({ queryKey: ["threads"] });
}

export function useAddChannelMember() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ id, userId }: { id: string; userId: string }) =>
      api.addChannelMember(id, userId),
    onSuccess: (_, vars) => invalidateMembership(qc, vars.id),
  });
}

export function useRemoveChannelMember() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ id, userId }: { id: string; userId: string }) =>
      api.removeChannelMember(id, userId),
    onSuccess: (_, vars) => invalidateMembership(qc, vars.id),
  });
}
