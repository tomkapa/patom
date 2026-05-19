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
  done: 1,
  error: 1,
  stalled: 1,
} as const satisfies Record<ResponseChunk["kind"], 1>;

/**
 * Open a single SSE connection to G3 for the active thread. Chunks land in
 * `useThreadStore`, deduped by `(request_id, chunk_seq)` so reconnects and
 * G2 backfill never double-render. Terminal events (`done`, `error`,
 * `stalled`) invalidate G2 so the persisted history takes over from the
 * in-memory live bubbles. The view-side selector hides each live bubble the
 * moment its `request_id` lands in history (identity-based dedup), so there
 * is no flash between terminal-event time and the refetch.
 */
export function useThreadStream(rootId: string | null) {
  const setStatus = useThreadStore((s) => s.setStatus);
  const applyEnvelope = useThreadStore((s) => s.applyEnvelope);
  const qc = useQueryClient();

  useEffect(() => {
    if (!rootId) return;
    setStatus(rootId, "connecting");

    const url = `${API_PREFIX}/threads/${rootId}/stream`;
    const es = new EventSource(url, { withCredentials: true });
    let closed = false;

    es.onopen = () => {
      if (!closed) setStatus(rootId, "open");
    };
    es.onerror = () => {
      // Browsers auto-reconnect; reflect the gap in UI status.
      if (!closed) setStatus(rootId, "stalled");
    };

    const handle = (e: MessageEvent) => {
      // Bun's dev proxy occasionally surfaces empty / keepalive frames as
      // `data: undefined`; skip them silently.
      if (!e.data || e.data === "undefined") return;
      try {
        const env = JSON.parse(e.data) as ThreadStreamEnvelope;
        applyEnvelope(rootId, env);
        const k = env.chunk?.kind;
        if (k === "done" || k === "error" || k === "stalled") {
          qc.invalidateQueries({ queryKey: ["threads", rootId, "messages"] });
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
      setStatus(rootId, "closed");
    };
  }, [rootId, setStatus, applyEnvelope, qc]);
}
