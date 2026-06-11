import { describe, expect, test } from "bun:test";

import { foldHistory, type Poster } from "./foldHistory";
import type { Mentionable, ThreadMessage } from "../types/api";

// Roster: the viewer Alice, plus two agents Orion (the author) and Vega (a
// reply target). Mirrors the `Mentionable` keying — `id` is the satellite key
// (agent_id / user_id), never the colleague id.
const ROSTER: Mentionable[] = [
  { kind: "human", id: "user-alice", name: "Alice", avatar_url: null, colleague_id: "col-alice" },
  { kind: "human", id: "user-bob", name: "Bob", avatar_url: null, colleague_id: "col-bob" },
  { kind: "agent", id: "agent-orion", name: "Orion", avatar_url: null, colleague_id: "col-orion" },
  { kind: "agent", id: "agent-vega", name: "Vega", avatar_url: null, colleague_id: "col-vega" },
];

const POSTER: Poster = { name: "Alice", id: "user-alice", avatar_url: null };

const ALICE = { kind: "human", colleague_id: "col-alice", user_id: "user-alice" } as const;
const BOB = { kind: "human", colleague_id: "col-bob", user_id: "user-bob" } as const;
const ORION = { kind: "agent", colleague_id: "col-orion", agent_id: "agent-orion" } as const;
const VEGA = { kind: "agent", colleague_id: "col-vega", agent_id: "agent-vega" } as const;

function row(over: Partial<ThreadMessage> & Pick<ThreadMessage, "seq" | "sender">): ThreadMessage {
  return {
    kind: "posted",
    owner_agent_id: null,
    receiver: null,
    body: { contents: [] },
    created_at: `t${over.seq}`,
    request_id: `r${over.seq}`,
    client_key: null,
    sender_display_name: null,
    sender_avatar_url: null,
    ...over,
  };
}

// The tool_use row of an agent's send_message turn — the bubble is built from
// this. `receiver` is no longer read for the tag (it's resolved from the
// materialized posted row), so the input carries only `content`.
function agentToolUse(seq: number, content: string): ThreadMessage {
  return row({
    seq,
    kind: "tool_use",
    sender: ORION,
    owner_agent_id: "agent-orion",
    request_id: `req-${seq}`,
    body: {
      contents: [
        {
          kind: "tool_call",
          value: { id: `call-${seq}`, name: "send_message", input: { content } },
        },
      ],
    },
  });
}

// A full agent delivery: the tool_use row plus the materialized `posted` row
// the backend wrote, sharing a request_id. The recipient tag is sourced from
// the posted row's resolved `receiver` (ground truth) — `null` ⇒ untagged.
function agentDelivers(
  seq: number,
  receiver: ThreadMessage["receiver"],
  content: string,
): ThreadMessage[] {
  const posted = row({
    seq: seq + 1,
    kind: "posted",
    sender: ORION,
    request_id: `req-${seq}`,
    receiver,
    body: { contents: [{ kind: "text", value: content }] },
  });
  return [agentToolUse(seq, content), posted];
}

const rootHuman = row({
  seq: 1,
  sender: ALICE,
  sender_display_name: "Alice",
  body: { contents: [{ kind: "text", value: "kick off" }] },
});

describe("foldHistory recipient tag", () => {
  test("agent reply is tagged from the delivered receiver", () => {
    const history = [rootHuman, ...agentDelivers(2, VEGA, "on it")];
    const { bubbles } = foldHistory(history, ROSTER, POSTER);
    const agentBubble = bubbles.find((b) => b.kind === "agent");
    expect(agentBubble?.text).toBe("@Vega on it");
  });

  test("agent reply tags the addressed person, NOT the thread root", () => {
    // The bug this change fixes: Alice opened the thread, but the agent replies
    // to Bob. The tag must follow the materialized receiver (Bob), never the
    // root prompter (Alice) — the old `{kind:"human"}` sugar guessed the root.
    const history = [rootHuman, ...agentDelivers(2, BOB, "got it")];
    const { bubbles } = foldHistory(history, ROSTER, POSTER);
    const agentBubble = bubbles.find((b) => b.kind === "agent");
    expect(agentBubble?.text).toBe("@Bob got it");
  });

  test("agent reply to an off-roster colleague resolves via a thread participant", () => {
    // Carol is not in the roster, but posted earlier with a resolved display
    // name, so the history walk harvests col-carol → "Carol" for the tag.
    const CAROL = { kind: "human", colleague_id: "col-carol", user_id: "user-carol" } as const;
    const carolPost = row({
      seq: 2,
      sender: CAROL,
      sender_display_name: "Carol",
      body: { contents: [{ kind: "text", value: "hi all" }] },
    });
    const history = [rootHuman, carolPost, ...agentDelivers(3, CAROL, "for you")];
    const { bubbles } = foldHistory(history, ROSTER, POSTER);
    const sent = bubbles.find((b) => b.kind === "agent" && b.text.includes("for you"));
    expect(sent?.text).toBe("@Carol for you");
  });

  test("agent reply with no receiver is untagged", () => {
    const history = [rootHuman, ...agentDelivers(2, null, "just thinking aloud")];
    const { bubbles } = foldHistory(history, ROSTER, POSTER);
    const agentBubble = bubbles.find((b) => b.kind === "agent");
    expect(agentBubble?.text).toBe("just thinking aloud");
  });

  test("agent reply with no posted row yet (streaming) is untagged", () => {
    // Before the posted row persists there is no ground-truth receiver, so the
    // tag is omitted rather than guessed.
    const history = [rootHuman, agentToolUse(2, "in flight")];
    const { bubbles } = foldHistory(history, ROSTER, POSTER);
    const agentBubble = bubbles.find((b) => b.kind === "agent");
    expect(agentBubble?.text).toBe("in flight");
  });

  test("human → agent follow-up still tags @Agent (wire receiver path)", () => {
    const followUp = row({
      seq: 2,
      sender: ALICE,
      sender_display_name: "Alice",
      receiver: ORION,
      body: { contents: [{ kind: "text", value: "any update?" }] },
    });
    const { bubbles } = foldHistory([rootHuman, followUp], ROSTER, POSTER);
    const humanBubble = bubbles.find((b) => b.kind === "human");
    expect(humanBubble?.text).toBe("@Orion any update?");
  });
});
