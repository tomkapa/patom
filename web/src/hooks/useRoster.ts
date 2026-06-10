import { useMemo } from "react";
import { useAgents } from "./useAgents";
import { useChannelMembers } from "./useChannels";
import type { Agent, ChannelMember, Mentionable, TagRef } from "../types/api";

/** Project an agent onto the shared mentionable shape. */
function agentMentionable(a: Agent): Mentionable {
  return { kind: "agent", id: a.id, name: a.name, avatar_url: a.avatar_url ?? null };
}

/** Project a channel member (profile-enriched human) onto the shared shape. */
function humanMentionable(m: ChannelMember): Mentionable {
  return {
    kind: "human",
    id: m.user_id,
    name: m.display_name ?? "member",
    avatar_url: m.avatar_url,
  };
}

/** The tags wire shape for one mentionable. */
export function tagRef(m: Mentionable): TagRef {
  return { kind: m.kind, id: m.id };
}

/**
 * Everyone taggable in a context: the channel's human members plus every
 * agent (agents are org-global — reachable from any channel or DM). Humans
 * and agents are the same kind of participant; only the row icon differs.
 *
 * `channelId` scopes the human half. Pass the system `#general` id to get
 * the whole workspace (every org member is auto-enrolled there) — that is
 * the DM-sidebar roster.
 */
export function useRoster(channelId: string | null): {
  roster: Mentionable[];
  isLoading: boolean;
} {
  const agentsQ = useAgents();
  const membersQ = useChannelMembers(channelId);

  return useMemo(() => {
    const agents = (agentsQ.data ?? []).map(agentMentionable);
    const humans = (membersQ.data ?? []).map(humanMentionable);
    return {
      // Humans first, then agents — the caller splits by `kind` when it
      // needs the two halves (and uses demo fixtures in demo mode), so the
      // hook returns just the merged list.
      roster: [...humans, ...agents],
      isLoading: agentsQ.isLoading || membersQ.isLoading,
    };
  }, [agentsQ.data, membersQ.data, agentsQ.isLoading, membersQ.isLoading]);
}
