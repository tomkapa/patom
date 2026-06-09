import { useMutation, useQueryClient } from "@tanstack/react-query";
import { api } from "../lib/api";
import { uuidv7 } from "../lib/utils";

type Vars = {
  thread_id?: string;
  agent_id?: string;
  /** Post a new thread into this channel. Omit for a direct message or a
   *  reply (replies carry `thread_id` and inherit their thread's location). */
  channel_id?: string;
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
