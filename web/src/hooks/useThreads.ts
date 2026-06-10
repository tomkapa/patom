import { useQuery } from "@tanstack/react-query";
import { api } from "../lib/api";
import type { TagRef } from "../types/api";

/**
 * Thread feed. `channelId` selects a channel's threads; `null` returns the
 * caller's direct messages, optionally narrowed to the conversation with
 * one `counterpart` colleague (both orientations). The key includes both so
 * switching context refetches rather than showing the previous feed.
 */
export function useThreads(
  channelId: string | null = null,
  counterpart: TagRef | null = null,
) {
  return useQuery({
    queryKey: ["threads", { channelId, counterpart }],
    queryFn: () => api.threads(channelId, counterpart),
    refetchInterval: 15_000,
    refetchOnWindowFocus: true,
  });
}

/**
 * G2 thread history.
 *
 * `pollMs` is a fallback for when SSE chunks were dropped (the bun dev
 * proxy occasionally truncates the stream with
 * `ERR_INCOMPLETE_CHUNKED_ENCODING`, and EventSource auto-reconnects do
 * not replay backlog). Pass a small interval while a submission is in
 * flight so the UI eventually catches up to the persisted reply even if
 * the live tap missed every chunk; pass `false` once the conversation
 * has quiesced so the panel isn't polling forever.
 */
export function useThreadHistory(
  rootId: string | null,
  pollMs: number | false = false,
) {
  return useQuery({
    queryKey: ["threads", rootId, "messages"],
    queryFn: () => api.threadMessages(rootId!),
    enabled: !!rootId,
    staleTime: 5_000,
    refetchInterval: pollMs,
  });
}
