import { useQuery } from "@tanstack/react-query";
import { api } from "../lib/api";

/**
 * Channel feed. `channelId` selects a channel's threads; `null` returns the
 * caller's direct messages (BE: human-rooted threads with no channel). The
 * key includes `channelId` so switching channels refetches rather than
 * showing the previous feed.
 */
export function useThreads(channelId: string | null = null) {
  return useQuery({
    queryKey: ["threads", { channelId }],
    queryFn: () => api.threads(channelId),
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
