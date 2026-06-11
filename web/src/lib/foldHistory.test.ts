import { describe, expect, test } from "bun:test";

import { foldHistory, type Poster } from "./foldHistory";
import type { Mentionable, ThreadMessage } from "../types/api";

// Roster: the viewer Alice, plus two agents Orion (the author) and Vega (a
// reply target). Mirrors the `Mentionable` keying — `id` is the satellite key
// (agent_id / user_id), never the colleague id.
const ROSTER: Mentionable[] = [
  { kind: "human", id: "user-alice", name: "Alice", avatar_url: null, colleague_id: "col-alice" },
  { kind: "agent", id: "agent-orion", name: "Orion", avatar_url: null, colleague_id: "col-orion" },
  { kind: "agent", id: "agent-vega", name: "Vega", avatar_url: null, colleague_id: "col-vega" },
];

const POSTER: Poster = { name: "Alice", id: "user-alice", avatar_url: null };

const ALICE = { kind: "human", colleague_id: "col-alice", user_id: "user-alice" } as const;
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

// An agent turn that calls send_message once. The bubble is reconstructed from
// this tool_use row, so the recipient tag must come from the call input.
function agentSend(seq: number, receiver: unknown, content: string): ThreadMessage {
  return row({
    seq,
    kind: "tool_use",
    sender: ORION,
    owner_agent_id: "agent-orion",
    body: {
      contents: [
        {
          kind: "tool_call",
          value: { id: `call-${seq}`, name: "send_message", input: { receiver, content } },
        },
      ],
    },
  });
}

const rootHuman = row({
  seq: 1,
  sender: ALICE,
  sender_display_name: "Alice",
  body: { contents: [{ kind: "text", value: "kick off" }] },
});

describe("foldHistory recipient tag", () => {
  test("agent → agent by name is tagged @Name", () => {
    const history = [rootHuman, agentSend(2, { kind: "agent", name: "Vega" }, "on it")];
    const { bubbles } = foldHistory(history, ROSTER, POSTER);
    const agentBubble = bubbles.find((b) => b.kind === "agent");
    expect(agentBubble?.text).toBe("@Vega on it");
  });

  test("agent → colleague-by-id resolves from the roster before the colleague posts", () => {
    // The canonical send_message form is a colleague id. Vega is in the roster
    // but has NOT posted in this thread yet, so resolution must come from the
    // roster seed — this is the first-message case the roster fix targets.
    const history = [rootHuman, agentSend(2, { kind: "colleague", id: "col-vega" }, "ping you")];
    const { bubbles } = foldHistory(history, ROSTER, POSTER);
    const sent = bubbles.find((b) => b.kind === "agent" && b.text.includes("ping you"));
    expect(sent?.text).toBe("@Vega ping you");
  });

  test("agent → off-roster colleague-by-id falls back to a thread participant's name", () => {
    // Carol is not in the roster, but posted earlier with a resolved display
    // name, so the history walk can still harvest col-carol → "Carol".
    const CAROL = { kind: "human", colleague_id: "col-carol", user_id: "user-carol" } as const;
    const carolPost = row({
      seq: 2,
      sender: CAROL,
      sender_display_name: "Carol",
      body: { contents: [{ kind: "text", value: "hi all" }] },
    });
    const history = [
      rootHuman,
      carolPost,
      agentSend(3, { kind: "colleague", id: "col-carol" }, "for you"),
    ];
    const { bubbles } = foldHistory(history, ROSTER, POSTER);
    const sent = bubbles.find((b) => b.kind === "agent" && b.text.includes("for you"));
    expect(sent?.text).toBe("@Carol for you");
  });

  test("agent → human sugar tags the thread-root human", () => {
    const history = [rootHuman, agentSend(2, { kind: "human" }, "done")];
    const { bubbles } = foldHistory(history, ROSTER, POSTER);
    const agentBubble = bubbles.find((b) => b.kind === "agent");
    expect(agentBubble?.text).toBe("@Alice done");
  });

  test("untagged agent post is left as-is", () => {
    const history = [rootHuman, agentSend(2, undefined, "just thinking aloud")];
    const { bubbles } = foldHistory(history, ROSTER, POSTER);
    const agentBubble = bubbles.find((b) => b.kind === "agent");
    expect(agentBubble?.text).toBe("just thinking aloud");
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
