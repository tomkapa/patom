import { useMutation, useQueryClient } from "@tanstack/react-query";
import { api } from "../lib/api";
import { uuidv7 } from "../lib/utils";
import { track } from "../lib/analytics";
import type { Attachment, TagRef } from "../types/api";

type Vars = {
  thread_id?: string;
  /** Everyone @-tagged, in message order. Agents among them are invoked
   *  by the backend; empty/omitted is a plain post. */
  tags?: TagRef[];
  /** Post a new thread into this channel. Omit for a direct message or a
   *  reply (replies carry `thread_id` and inherit their thread's location). */
  channel_id?: string;
  /** Who a fresh DM root is with (no `thread_id`, no `channel_id`). */
  counterpart?: TagRef;
  content: string;
  /** Image/file attachment references (issue #187). */
  attachments?: Attachment[];
  /** Caller-supplied so the optimistic bubble can be tagged with the same
   *  key the server sees. Auto-generated when omitted (channel-level
   *  submits that don't need an optimistic echo). */
  idempotency_key?: string;
};

export function useSubmitPrompt() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (v: Vars) =>
      api.submitPrompt({
        ...v,
        idempotency_key: v.idempotency_key ?? uuidv7(),
      }),
    onSuccess: (data, v) => {
      qc.invalidateQueries({ queryKey: ["threads"] });
      // North-star action. Properties are non-PII shape only — never the
      // message content, just its length and routing. `has_agent` reflects
      // intent (an agent was @-tagged).
      const agentTagged = v.tags?.some((tag) => tag.kind === "agent") ?? false;
      track("message_sent", {
        is_reply: Boolean(v.thread_id),
        has_agent: agentTagged,
        has_channel: Boolean(v.channel_id),
        content_len: v.content.length,
        attachment_count: v.attachments?.length ?? 0,
      });
      // `agent_invoked` is the actual outcome — the agents the backend woke,
      // not merely those tagged (a tag may not trigger).
      for (const agentId of data.triggered_agent_ids) {
        track("agent_invoked", { agent_id: agentId });
      }
    },
  });
}
