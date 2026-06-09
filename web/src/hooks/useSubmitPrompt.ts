import { useMutation, useQueryClient } from "@tanstack/react-query";
import { api } from "../lib/api";
import { uuidv7 } from "../lib/utils";
import type { TagRef } from "../types/api";

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
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["threads"] });
    },
  });
}
