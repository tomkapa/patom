// Merge rule: persisted (history) > streaming (live) > optimistic (pending).
// When a request_id lands in history, the live/pending entries that share it
// are hidden. No text matching, no clock fallback.

import { useMemo } from "react";

import { foldHistory, type Bubble, type RootMessage } from "../lib/foldHistory";
import { useThreadHistory } from "./useThreads";
import { useThreadStore, type StreamStatus } from "../stores/threadStore";
import type { Agent } from "../types/api";

export type ThreadView = {
  bubbles: Bubble[];
  rootMessage: RootMessage | undefined;
  status: StreamStatus;
  isLoading: boolean;
  /** True iff the most recent visible bubble is a human follow-up that has
   *  not yet been confirmed by a streaming or persisted agent reply. The
   *  panel renders a "thinking" placeholder when this is set. */
  showThinking: boolean;
};

export function useThreadView(
  rootId: string | null,
  agents: Agent[],
  poster: { name: string; id: string },
): ThreadView {
  // History keeps a low-frequency poll only while we have unconfirmed
  // pending submits, so the panel catches up even if SSE drops chunks. Once
  // every pending bubble has resolved (its request_id is in history), the
  // poll stops automatically because the pending map empties.
  const hasUnconfirmed = useThreadStore((s) => {
    if (!rootId) return false;
    const t = s.byThread.get(rootId);
    if (!t) return false;
    for (const _p of t.pending.values()) return true;
    return false;
  });
  const historyQ = useThreadHistory(rootId, hasUnconfirmed ? 2_000 : false);
  const history = historyQ.data ?? [];

  const state = useThreadStore((s) =>
    rootId ? s.byThread.get(rootId) : undefined,
  );

  return useMemo(() => {
    const folded = foldHistory(history, agents, poster);
    const persistedRequestIds = new Set(
      folded.bubbles.map((b) => b.request_id),
    );

    // Live agent bubbles whose request_id has not yet been echoed in
    // history. Once it lands, the persisted version takes over.
    const liveBubbles: Bubble[] = [];
    if (state) {
      for (const lb of state.live.values()) {
        if (persistedRequestIds.has(lb.request_id)) continue;
        liveBubbles.push({
          kind: "agent",
          key: `live:${lb.request_id}`,
          request_id: lb.request_id,
          agent_id: lb.agent_id,
          agent_name:
            agents.find((a) => a.id === lb.agent_id)?.name ?? null,
          human_name: null,
          human_id: null,
          ts: lb.ts,
          text: lb.message,
          reasoning: lb.reasoning,
          tool_calls: Array.from(lb.tool_calls.values()),
          wire_requests: lb.wire_requests,
          phase: "streaming",
        });
      }
    }

    // Optimistic human follow-ups. Hidden once their request_id is
    // persisted; held until then so the user sees their own message echo
    // immediately. The composer's own pending state covers the brief
    // pre-/prompts-response window where request_id is still undefined.
    const optimisticBubbles: Bubble[] = [];
    if (state) {
      for (const p of state.pending.values()) {
        if (p.request_id && persistedRequestIds.has(p.request_id)) continue;
        optimisticBubbles.push({
          kind: "human",
          key: `opt:${p.idempotency_key}`,
          request_id: p.request_id ?? p.idempotency_key,
          agent_id: null,
          agent_name: null,
          human_name: poster.name,
          human_id: poster.id,
          ts: p.ts,
          text: p.text,
          reasoning: "",
          tool_calls: [],
          wire_requests: [],
          phase: "optimistic",
        });
      }
    }

    const bubbles = [
      ...folded.bubbles,
      ...liveBubbles,
      ...optimisticBubbles,
    ].sort(byTs);

    const last = bubbles[bubbles.length - 1];
    const showThinking =
      !!last && last.kind === "human" && last.phase !== "persisted";

    return {
      bubbles,
      rootMessage: folded.rootMessage,
      status: state?.status ?? "idle",
      isLoading: historyQ.isLoading,
      showThinking,
    };
  }, [history, agents, poster, state, historyQ.isLoading]);
}

function byTs(a: Bubble, b: Bubble): number {
  // Stable sort by timestamp; ties broken by phase so persisted rows render
  // before live/optimistic at the same ts (rare — only matters when the
  // first SSE chunk and the persisted echo carry the same created_at).
  const da = Date.parse(a.ts);
  const db = Date.parse(b.ts);
  if (da !== db) return da - db;
  return phaseOrder(a.phase) - phaseOrder(b.phase);
}

function phaseOrder(p: Bubble["phase"]): number {
  switch (p) {
    case "persisted":
      return 0;
    case "streaming":
      return 1;
    case "optimistic":
      return 2;
  }
}
