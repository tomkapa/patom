// Merge rule: persisted (history) > streaming (live) > optimistic (pending).
// When a request_id lands in history, the live/pending entries that share it
// are hidden. No text matching, no clock fallback.

import { useMemo } from "react";

import {
  foldHistory,
  type Bubble,
  type Poster,
  type RootMessage,
} from "../lib/foldHistory";
import { useThreadHistory } from "./useThreads";
import { useThreadStore, type StreamStatus } from "../stores/threadStore";
import type { Mentionable } from "../types/api";

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
  roster: Mentionable[],
  poster: Poster,
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
    const folded = foldHistory(history, roster, poster);
    const persistedRequestIds = new Set(
      folded.bubbles.map((b) => b.request_id),
    );
    // Human rows echo the submit's idempotency key — the strongest identity
    // for hiding the matching optimistic bubble (request ids don't exist on
    // untagged posts).
    const persistedClientKeys = new Set(
      folded.bubbles.flatMap((b) => (b.client_key ? [b.client_key] : [])),
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
          client_key: null,
          agent_id: lb.agent_id,
          agent_name:
            roster.find((m) => m.kind === "agent" && m.id === lb.agent_id)
              ?.name ?? null,
          human_name: null,
          human_id: null,
          human_avatar_url: null,
          ts: lb.ts,
          text: lb.message,
          reasoning: lb.reasoning,
          tool_calls: Array.from(lb.tool_calls.values()),
          wire_requests: lb.wire_requests,
          phase: "streaming",
          error: lb.status === "error" ? lb.error : undefined,
          error_code: lb.status === "error" ? lb.errorCode : undefined,
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
        if (persistedClientKeys.has(p.idempotency_key)) continue;
        if (p.request_id && persistedRequestIds.has(p.request_id)) continue;
        // Skip a bubble with neither text nor attachments (mirrors the
        // persisted fold's guard) so an attachment-only send doesn't flash an
        // empty bubble before its references resolve.
        const attachments = p.attachments ?? [];
        if (!p.text && attachments.length === 0) continue;
        optimisticBubbles.push({
          kind: "human",
          key: `opt:${p.idempotency_key}`,
          request_id: p.request_id ?? p.idempotency_key,
          client_key: p.idempotency_key,
          agent_id: null,
          agent_name: null,
          human_name: poster.name,
          human_id: poster.id,
          human_avatar_url: poster.avatar_url,
          ts: p.ts,
          text: p.text,
          attachments,
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

    // "Thinking…" only when the trailing human message actually woke an
    // agent (or its submit is still in flight — `triggered` not yet known).
    // An untagged post expects no reply.
    const last = bubbles[bubbles.length - 1];
    const lastPending =
      last && last.client_key ? state?.pending.get(last.client_key) : undefined;
    const showThinking =
      !!last &&
      last.kind === "human" &&
      last.phase !== "persisted" &&
      (lastPending?.triggered ?? true);

    return {
      bubbles,
      rootMessage: folded.rootMessage,
      status: state?.status ?? "idle",
      isLoading: historyQ.isLoading,
      showThinking,
    };
  }, [history, roster, poster, state, historyQ.isLoading]);
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
