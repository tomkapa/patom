// One bubble per `send_message` invocation plus one per follow-up human
// row. Reasoning and non-`send_message` tool calls collapse under the
// next agent delivery as meta. Plain assistant `text` and matching
// tool_results are worker-internals and stay hidden.

import { decodeBody } from "./chatBody";
import type {
  McpWireRequest,
  Mentionable,
  ThreadMessage,
  ToolCallEntry,
  Participant,
} from "../types/api";

const SEND_MESSAGE = "send_message";
const WIRE_MCP_TOOL_NAME = "request_user_wire_mcp";

type ReceiverInput =
  | { kind: "human"; user_id: string }
  | { kind: "agent"; agent_id: string };

type SendMessageInput = {
  content: string;
  receiver?: ReceiverInput;
  context_summary?: string;
};

/** Lifecycle phase of a bubble in the merged view. Persisted bubbles win
 *  over streaming, which win over optimistic — see useThreadView. */
export type BubblePhase = "persisted" | "streaming" | "optimistic";

export type Bubble = {
  kind: "agent" | "human";
  /** React key — unique across the merged view. */
  key: string;
  /** Identity for cross-phase dedup. Persisted, streaming, and optimistic
   *  bubbles that share a request_id refer to the same logical message. */
  request_id: string;
  /** The posting submit's idempotency key, echoed on persisted human rows.
   *  The optimistic bubble for the same submit carries the same key, so the
   *  view hides it the moment the echo lands. `null` for agent rows. */
  client_key: string | null;
  agent_id: string | null;
  /** Resolved at fold time so the renderer doesn't need to look up agents. */
  agent_name: string | null;
  /** Display name + id for human bubbles. */
  human_name: string | null;
  human_id: string | null;
  /** Avatar URL for the human poster; `null` when they haven't set one. */
  human_avatar_url: string | null;
  ts: string;
  text: string;
  reasoning: string;
  tool_calls: ToolCallEntry[];
  /** Inline `WireMcpRequestCard` payloads emitted by
   *  `request_user_wire_mcp` during this turn. Empty for persisted /
   *  human bubbles. */
  wire_requests: McpWireRequest[];
  phase: BubblePhase;
  /** Set on a streaming bubble that ended in a terminal `error` chunk:
   *  `error` is the human reason, `error_code` the stable label (e.g.
   *  `"budget_exceeded"`). Undefined for healthy / persisted / human bubbles. */
  error?: string;
  error_code?: string;
};

export type RootMessage = {
  name: string;
  id: string;
  avatar_url: string | null;
  ts: string;
  text: string;
};

export type FoldedHistory = {
  /** Root post — the first human row in the thread, rendered above the
   *  reply list. `undefined` until history loads. */
  rootMessage: RootMessage | undefined;
  /** Persisted bubbles in fold order (which is also row order). */
  bubbles: Bubble[];
};

type Pending = {
  reasoning: string;
  tool_calls: ToolCallEntry[];
};

const newPending = (): Pending => ({ reasoning: "", tool_calls: [] });

export type Poster = {
  name: string;
  id: string;
  avatar_url: string | null;
};

export function foldHistory(
  history: ThreadMessage[],
  roster: Mentionable[],
  poster: Poster,
): FoldedHistory {
  const agentsById = new Map(
    roster.filter((m) => m.kind === "agent").map((m) => [m.id, m]),
  );
  const humansById = new Map(
    roster.filter((m) => m.kind === "human").map((m) => [m.id, m]),
  );
  const bubbles: Bubble[] = [];
  // Per-(thread, agent) accumulator — reasoning + non-send_message tool
  // calls observed since this agent's last delivery in this thread.
  const pending = new Map<string, Pending>();
  // Most recent agent bubble per (thread, agent). Reasoning rows that land
  // *after* a delivery but before the next one are post-delivery reflection
  // — attach back to the bubble that just shipped.
  const lastBubble = new Map<string, Bubble>();
  // Per-thread tool index for tool_result lookups; system rows carry the
  // results but not the original caller's identity.
  const indexByThread = new Map<string, Map<string, ToolCallEntry>>();
  // send_message tool calls are conversation plumbing; their tool_results
  // are private and never decorate a bubble.
  const sendMessageCallIds = new Set<string>();

  let rootMessage: RootMessage | undefined;

  // A `send_message` whose tool_result errored never reached its recipient
  // (e.g. the model passed `receiver` as a JSON-encoded string, the call
  // failed to parse, and the model retried with the same text). Without
  // this guard the failed attempt still renders — as a second, identical
  // reply bubble. The result lands in a later (system) row than the call,
  // so collect the failed call ids in a pre-pass before the fold.
  const failedCallIds = new Set<string>();
  for (const m of history) {
    for (const tr of decodeBody(m.body).toolResults) {
      if (tr.is_error) failedCallIds.add(tr.call_id);
    }
  }

  // The G2 feed is a single thread, so rows no longer carry a per-row
  // thread id — the dimension that used to be `session_id`. Group the fold
  // by a fixed thread token plus the per-row agent so the accumulator and
  // tool index keep their original (thread, agent) / per-thread shape.
  const THREAD = "thread";
  const threadAgentKey = (agent: string | null) => `${THREAD}|${agent ?? ""}`;
  const getPending = (k: string): Pending => {
    let p = pending.get(k);
    if (!p) {
      p = newPending();
      pending.set(k, p);
    }
    return p;
  };
  const getIndex = (): Map<string, ToolCallEntry> => {
    let i = indexByThread.get(THREAD);
    if (!i) {
      i = new Map();
      indexByThread.set(THREAD, i);
    }
    return i;
  };

  for (const m of history) {
    const decoded = decodeBody(m.body);

    if (m.sender.kind === "agent") {
      const aid = m.sender.agent_id ?? null;
      const k = threadAgentKey(aid);
      const p = getPending(k);
      const idx = getIndex();

      const sendCalls = decoded.toolCalls.filter(
        (tc) => tc.name === SEND_MESSAGE,
      );
      const realCalls = decoded.toolCalls.filter(
        (tc) => tc.name !== SEND_MESSAGE,
      );

      for (const tc of realCalls) {
        const entry: ToolCallEntry = {
          call_id: tc.id,
          name: tc.name,
          input: tc.input,
          status: "running",
        };
        p.tool_calls.push(entry);
        idx.set(tc.id, entry);
      }

      if (sendCalls.length > 0) {
        // send_message results are private plumbing — mark every send id
        // (delivered or failed) so its tool_result never decorates another
        // bubble via attachResults.
        for (const tc of sendCalls) sendMessageCallIds.add(tc.id);

        // Only sends whose tool_result did not error actually reached the
        // recipient; a failed send delivered nothing and must not render.
        const deliveredCalls = sendCalls.filter(
          (tc) => !failedCallIds.has(tc.id),
        );

        if (deliveredCalls.length > 0) {
          // This row delivers the agent's accumulated work. Its own reasoning
          // belongs to the same turn that produced the send_message and joins
          // pending.reasoning in the new bubble.
          const reasoning = joinText(p.reasoning, decoded.reasoning);
          const tools = p.tool_calls;
          for (const tc of deliveredCalls) {
            const input = (tc.input ?? {}) as SendMessageInput;
            const recv = input.receiver ?? null;
            const a = aid ? (agentsById.get(aid) ?? null) : null;
            const bubble: Bubble = {
              kind: "agent",
              key: `h:${m.seq}:${tc.id}`,
              request_id: m.request_id ?? `seq:${m.seq}`,
              client_key: null,
              agent_id: aid,
              agent_name: a?.name ?? null,
              human_name: null,
              human_id: null,
              human_avatar_url: null,
              ts: m.created_at,
              text: prefixWithReceiver(input.content ?? "", recv, agentsById, humansById),
              reasoning,
              tool_calls: tools,
              wire_requests: [],
              phase: "persisted",
            };
            bubbles.push(bubble);
            lastBubble.set(k, bubble);
          }
          pending.set(k, newPending());
        } else if (decoded.reasoning) {
          // Every send in this row failed to deliver. Keep the reasoning
          // accumulating toward the eventual successful delivery rather than
          // flushing pending, so the delivered bubble still carries it.
          p.reasoning = joinText(p.reasoning, decoded.reasoning);
        }
      } else if (decoded.reasoning) {
        // No send in this row. If the agent already shipped a bubble in this
        // session and pending has no in-flight tool calls, the reasoning is
        // post-delivery reflection — attach back to that bubble. Otherwise it
        // leads into the next send_message and stays in pending.
        const lb = lastBubble.get(k);
        if (lb && p.tool_calls.length === 0) {
          lb.reasoning = joinText(lb.reasoning, decoded.reasoning);
        } else {
          p.reasoning = joinText(p.reasoning, decoded.reasoning);
        }
      }

      // Inline tool_results (rare — results normally arrive via a system row).
      attachResults(idx, decoded.toolResults, sendMessageCallIds);
    } else if (m.sender.kind === "system") {
      const idx = indexByThread.get(THREAD);
      if (idx) attachResults(idx, decoded.toolResults, sendMessageCallIds);
    } else if (m.sender.kind === "human") {
      // Resolve the *real* author from the wire — the backend stamps each
      // human row with its sender's name/avatar/id. Falling back to the
      // current-user `poster` only covers legacy rows missing the fields;
      // without this every human bubble showed the viewer (the multi-user
      // identity bug). Avatar is taken verbatim (`null` ⇒ initials) so we
      // never paint one person's photo onto another's message.
      const authorName = m.sender_display_name ?? poster.name;
      const authorId = m.sender.user_id ?? poster.id;
      const authorAvatar = m.sender_avatar_url;

      // First human row is the thread root — rendered separately in the
      // panel header. Subsequent human rows are follow-ups in the thread.
      if (!rootMessage) {
        rootMessage = {
          name: authorName,
          id: authorId,
          avatar_url: authorAvatar,
          ts: m.created_at,
          text: prefixWithReceiver(
            decoded.text,
            receiverFrom(m.receiver),
            agentsById,
            humansById,
          ),
        };
      } else if (decoded.text) {
        const recv = receiverFrom(m.receiver);
        bubbles.push({
          kind: "human",
          key: `h:${m.seq}:user`,
          request_id: m.request_id ?? m.client_key ?? `seq:${m.seq}`,
          client_key: m.client_key,
          agent_id: null,
          agent_name: null,
          human_name: authorName,
          human_id: authorId,
          human_avatar_url: authorAvatar,
          ts: m.created_at,
          text: prefixWithReceiver(decoded.text, recv, agentsById, humansById),
          reasoning: "",
          tool_calls: [],
          wire_requests: [],
          phase: "persisted",
        });
      }
    }
  }

  // The live `WireMcpRequest` chunk doesn't survive into chat_messages —
  // rehydrate the inline card from the persisted tool result, which the
  // backend tool mirrors field-for-field.
  for (const b of bubbles) {
    if (b.kind !== "agent") continue;
    for (const tc of b.tool_calls) {
      if (tc.name !== WIRE_MCP_TOOL_NAME) continue;
      if (tc.status !== "ok" || !tc.output) continue;
      const req = parseWireMcpOutput(tc.output);
      if (!req) continue;
      if (b.wire_requests.some((w) => w.catalog_id === req.catalog_id)) continue;
      b.wire_requests.push(req);
    }
  }

  return { rootMessage, bubbles };
}

function parseWireMcpOutput(output: string): McpWireRequest | null {
  let parsed: unknown;
  try {
    parsed = JSON.parse(output);
  } catch {
    return null;
  }
  if (!parsed || typeof parsed !== "object") return null;
  const o = parsed as Record<string, unknown>;
  if (o.status !== "requested") return null;
  if (typeof o.catalog_id !== "string") return null;
  if (typeof o.display_name !== "string") return null;
  if (typeof o.reason !== "string") return null;
  if (o.auth_kind !== "oauth2" && o.auth_kind !== "static_headers" && o.auth_kind !== "none") {
    return null;
  }
  const homepage_url =
    typeof o.homepage_url === "string" ? o.homepage_url : undefined;
  return {
    catalog_id: o.catalog_id,
    display_name: o.display_name,
    reason: o.reason,
    auth_kind: o.auth_kind,
    homepage_url,
  };
}

function receiverFrom(p: Participant | null): ReceiverInput | null {
  if (!p) return null;
  if (p.kind === "agent") return { kind: "agent", agent_id: p.agent_id };
  if (p.kind === "human") return { kind: "human", user_id: p.user_id };
  return null;
}

function attachResults(
  idx: Map<string, ToolCallEntry>,
  results: { call_id: string; output: string; is_error?: boolean }[],
  drop: Set<string>,
): void {
  for (const tr of results) {
    if (drop.has(tr.call_id)) continue;
    const e = idx.get(tr.call_id);
    if (!e) continue;
    e.output = tr.output;
    e.is_error = tr.is_error;
    e.status = tr.is_error ? "error" : "ok";
  }
}

function prefixWithReceiver(
  content: string,
  receiver: ReceiverInput | null,
  agentsById: ReadonlyMap<string, Mentionable>,
  humansById: ReadonlyMap<string, Mentionable>,
): string {
  if (!receiver) return content;
  const name =
    receiver.kind === "agent"
      ? agentsById.get(receiver.agent_id)?.name
      : humansById.get(receiver.user_id)?.name;
  if (!name || content.startsWith(`@${name}`)) return content;
  return `@${name} ${content}`;
}

function joinText(a: string, b: string): string {
  if (!a) return b;
  if (!b) return a;
  return `${a}\n${b}`;
}
