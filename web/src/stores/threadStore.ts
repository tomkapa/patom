import { create } from "zustand";
import type {
  McpWireRequest,
  ResponseChunk,
  ThreadStreamEnvelope,
  ToolCallEntry,
} from "../types/api";

export type StreamStatus =
  | "idle"
  | "connecting"
  | "open"
  | "stalled"
  | "closed"
  | "error";

export type LiveStatus = "streaming" | "done" | "error" | "stalled";

export type PendingHuman = {
  /** Client-minted; stable across the optimistic → confirmed transition. */
  idempotency_key: string;
  /** Server-assigned by /prompts. Set once the mutation resolves; until
   *  then the bubble is unconfirmed (composer spinner / thinking placeholder). */
  request_id?: string;
  text: string;
  ts: string;
};

export type LiveAgent = {
  request_id: string;
  agent_id: string | null;
  message: string;
  reasoning: string;
  tool_calls: Map<string, ToolCallEntry>;
  /** In-turn wire-MCP requests. Empty in the common case; non-empty when
   *  the recruiter asked the user to wire one or more catalog entries. */
  wire_requests: McpWireRequest[];
  status: LiveStatus;
  error?: string;
  /** Stable, low-cardinality failure label (e.g. `"budget_exceeded"`) from the
   *  error chunk — lets the UI branch on the failure kind without parsing the
   *  human `error` text. */
  errorCode?: string;
  /** Captured on first chunk; stable thereafter. */
  ts: string;
};

export type ThreadState = {
  pending: Map<string, PendingHuman>;
  live: Map<string, LiveAgent>;
  /** dedup key set: `${request_id}:${chunk_seq}`. */
  seen: Set<string>;
  status: StreamStatus;
};

type Store = {
  byThread: Map<string, ThreadState>;

  addPending: (rootId: string, p: PendingHuman) => void;
  attachRequestId: (
    rootId: string,
    idempotencyKey: string,
    requestId: string,
  ) => void;
  removePending: (rootId: string, idempotencyKey: string) => void;
  applyEnvelope: (rootId: string, env: ThreadStreamEnvelope) => void;
  setStatus: (rootId: string, status: StreamStatus) => void;
  reset: (rootId: string) => void;
};

const emptyState = (): ThreadState => ({
  pending: new Map(),
  live: new Map(),
  seen: new Set(),
  status: "idle",
});

function applyChunk(b: LiveAgent, chunk: ResponseChunk): LiveAgent {
  switch (chunk.kind) {
    case "text":
      return b;
    case "reasoning":
      return { ...b, reasoning: b.reasoning + chunk.value };
    case "agent_message":
      return {
        ...b,
        message: b.message + chunk.content,
        agent_id: b.agent_id ?? chunk.from,
      };
    case "wire_mcp_request": {
      // Append; duplicates (same catalog_id) collapse on stream replay.
      // When the chunk is a replay with nothing to add, return `b`
      // unchanged so subscribers don't re-render — the dedup check in
      // `applyEnvelope` below relies on same-reference returns.
      if (b.wire_requests.some((w) => w.catalog_id === chunk.catalog_id)) {
        if (b.agent_id !== null || chunk.from === b.agent_id) return b;
        return { ...b, agent_id: chunk.from };
      }
      return {
        ...b,
        agent_id: b.agent_id ?? chunk.from,
        wire_requests: [
          ...b.wire_requests,
          {
            catalog_id: chunk.catalog_id,
            display_name: chunk.display_name,
            reason: chunk.reason,
            auth_kind: chunk.auth_kind,
            homepage_url: chunk.homepage_url,
          },
        ],
      };
    }
    case "tool_call": {
      const tool_calls = new Map(b.tool_calls);
      tool_calls.set(chunk.id, {
        call_id: chunk.id,
        name: chunk.name,
        input: chunk.input,
        status: "running",
      });
      return { ...b, tool_calls };
    }
    case "tool_result": {
      const existing = b.tool_calls.get(chunk.call_id);
      const tool_calls = new Map(b.tool_calls);
      tool_calls.set(chunk.call_id, {
        call_id: chunk.call_id,
        name: existing?.name ?? "(tool)",
        input: existing?.input,
        output: chunk.output,
        is_error: chunk.is_error,
        status: chunk.is_error ? "error" : "ok",
      });
      return { ...b, tool_calls };
    }
    case "done":
      return { ...b, status: "done" };
    case "error":
      return { ...b, status: "error", error: chunk.reason, errorCode: chunk.code };
    case "stalled":
      return { ...b, status: "stalled" };
  }
}

function update(
  s: { byThread: Map<string, ThreadState> },
  rootId: string,
  fn: (cur: ThreadState) => ThreadState | null,
): { byThread: Map<string, ThreadState> } | null {
  const cur = s.byThread.get(rootId) ?? emptyState();
  const next = fn(cur);
  if (next === null) return null;
  const map = new Map(s.byThread);
  map.set(rootId, next);
  return { byThread: map };
}

export const useThreadStore = create<Store>((set) => ({
  byThread: new Map(),

  addPending(rootId, p) {
    set((s) => {
      const next = update(s, rootId, (cur) => {
        if (cur.pending.has(p.idempotency_key)) return null;
        const pending = new Map(cur.pending);
        pending.set(p.idempotency_key, p);
        return { ...cur, pending };
      });
      return next ?? s;
    });
  },

  attachRequestId(rootId, idempotencyKey, requestId) {
    set((s) => {
      const next = update(s, rootId, (cur) => {
        const entry = cur.pending.get(idempotencyKey);
        if (!entry || entry.request_id === requestId) return null;
        const pending = new Map(cur.pending);
        pending.set(idempotencyKey, { ...entry, request_id: requestId });
        return { ...cur, pending };
      });
      return next ?? s;
    });
  },

  removePending(rootId, idempotencyKey) {
    set((s) => {
      const next = update(s, rootId, (cur) => {
        if (!cur.pending.has(idempotencyKey)) return null;
        const pending = new Map(cur.pending);
        pending.delete(idempotencyKey);
        return { ...cur, pending };
      });
      return next ?? s;
    });
  },

  applyEnvelope(rootId, env) {
    set((s) => {
      const next = update(s, rootId, (cur) => {
        // Synthetic envelopes (Stalled / Error from fan-in) carry a null
        // request_id; they only update connection status.
        if (env.request_id == null || env.chunk_seq == null) {
          const status: StreamStatus =
            env.chunk.kind === "stalled"
              ? "stalled"
              : env.chunk.kind === "error"
                ? "error"
                : cur.status;
          return status === cur.status ? null : { ...cur, status };
        }
        const dedupKey = `${env.request_id}:${env.chunk_seq}`;
        if (cur.seen.has(dedupKey)) return null;

        const existing = cur.live.get(env.request_id);
        const base: LiveAgent = existing ?? {
          request_id: env.request_id,
          agent_id: env.from_agent,
          message: "",
          reasoning: "",
          tool_calls: new Map(),
          wire_requests: [],
          status: "streaming",
          ts: new Date().toISOString(),
        };
        const updated = applyChunk(base, env.chunk);
        // `text` chunks aren't surfaced to bubbles; if nothing changed and
        // we've already accepted a chunk for this request, skip the wrapper
        // churn so subscribers don't re-render on every keepalive token.
        if (updated === base && existing) {
          cur.seen.add(dedupKey);
          return null;
        }
        const live = new Map(cur.live);
        live.set(env.request_id, updated);
        // `seen` is internal to ThreadState and only inspected by this
        // reducer; mutate in place to skip an O(n) Set copy per chunk. The
        // outer wrapper still gets a fresh reference so subscribers update.
        cur.seen.add(dedupKey);
        return { ...cur, live };
      });
      return next ?? s;
    });
  },

  setStatus(rootId, status) {
    set((s) => {
      const next = update(s, rootId, (cur) =>
        cur.status === status ? null : { ...cur, status },
      );
      return next ?? s;
    });
  },

  reset(rootId) {
    set((s) => {
      if (!s.byThread.has(rootId)) return s;
      const map = new Map(s.byThread);
      map.set(rootId, emptyState());
      return { byThread: map };
    });
  },
}));

export const emptyThreadState = emptyState;
