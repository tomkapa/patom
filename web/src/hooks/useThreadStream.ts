import { useEffect } from "react";
import { useQueryClient } from "@tanstack/react-query";
import type { ResponseChunk, ThreadStreamEnvelope } from "../types/api";
import { API_PREFIX } from "../lib/api";
import { useThreadStore } from "../stores/threadStore";

// Keyed by `ResponseChunk["kind"]` so a new chunk variant fails the build
// until it gets a corresponding addEventListener entry below.
const KINDS = {
  text: 1,
  reasoning: 1,
  tool_call: 1,
  tool_result: 1,
  agent_message: 1,
  wire_mcp_request: 1,
  done: 1,
  error: 1,
  stalled: 1,
} as const satisfies Record<ResponseChunk["kind"], 1>;

/**
 * Open a single SSE connection to G3 for the active thread. Chunks land in
 * `useThreadStore`, deduped by `(request_id, chunk_seq)` so reconnects and
 * G2 backfill never double-render.
 *
 * The stream is CONTINUOUS: a `done` / `error` chunk is a per-turn marker,
 * not a stream close. The thread hosts many turns, so we never tear the SSE
 * connection down on a terminal chunk — we only invalidate G2 so the
 * persisted history takes over from the in-memory live bubbles for that
 * turn. The view-side selector hides each live bubble the moment its
 * `request_id` lands in history (identity-based dedup), so there is no flash
 * between terminal-event time and the refetch. The connection closes only on
 * unmount / thread switch (the effect cleanup).
 *
 * `threadId` is the thread feed's stable id (was the root request id).
 */
export function useThreadStream(threadId: string | null) {
  const setStatus = useThreadStore((s) => s.setStatus);
  const applyEnvelope = useThreadStore((s) => s.applyEnvelope);
  const qc = useQueryClient();

  useEffect(() => {
    if (!threadId) return;
    setStatus(threadId, "connecting");

    const url = `${API_PREFIX}/threads/${threadId}/stream`;
    const es = new EventSource(url, { withCredentials: true });
    let closed = false;

    es.onopen = () => {
      if (!closed) setStatus(threadId, "open");
    };
    es.onerror = () => {
      // Browsers auto-reconnect; reflect the gap in UI status.
      if (!closed) setStatus(threadId, "stalled");
    };

    const handle = (e: MessageEvent) => {
      // Bun's dev proxy occasionally surfaces empty / keepalive frames as
      // `data: undefined`; skip them silently.
      if (!e.data || e.data === "undefined") return;
      try {
        const env = JSON.parse(e.data) as ThreadStreamEnvelope;
        applyEnvelope(threadId, env);
        const k = env.chunk?.kind;
        if (k === "done" || k === "error" || k === "stalled") {
          qc.invalidateQueries({ queryKey: ["threads", threadId, "messages"] });
          qc.invalidateQueries({ queryKey: ["threads"] });
        }
      } catch (err) {
        console.warn("thread.stream.parse_error", err);
      }
    };

    const kinds = Object.keys(KINDS);
    for (const k of kinds) es.addEventListener(k, handle);
    es.addEventListener("message", handle);

    return () => {
      closed = true;
      for (const k of kinds) es.removeEventListener(k, handle);
      es.removeEventListener("message", handle);
      es.close();
      setStatus(threadId, "closed");
    };
  }, [threadId, setStatus, applyEnvelope, qc]);
}
