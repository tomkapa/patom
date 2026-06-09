// Static demo fixtures used to populate the layout when the backend has no
// content. Lets the UI render the canonical "agent-ops" channel state from
// the design reference even on a fresh database.

import type {
  Agent,
  ThreadMessage,
  ThreadSummary,
} from "../types/api";

export const DEMO_AGENTS: Agent[] = [
  { id: "a-orion", name: "orion-research-v3", is_default: true },
  { id: "a-helios", name: "helios-deploy", is_default: false },
  { id: "a-atlas", name: "atlas-weather", is_default: false },
  { id: "a-vega", name: "vega-incident-bot", is_default: false },
];

const NOW = new Date();
const ago = (mins: number) =>
  new Date(NOW.getTime() - mins * 60_000).toISOString();

// Demo human author for the fixtures — the feed stamps each human row with
// its real sender so a thread shows that author, not the logged-in viewer.
const TOM = {
  user_id: "user",
  colleague_id: "col-tom",
  name: "Tom Tran",
  avatar_url: null,
};

export const DEMO_THREADS: ThreadSummary[] = [
  {
    thread_id: "00000000-0000-0000-0000-000000000001",
    channel_id: null,
    last_activity_at: ago(0.2),
  },
  {
    thread_id: "00000000-0000-0000-0000-000000000002",
    channel_id: null,
    last_activity_at: ago(8),
  },
  {
    thread_id: "00000000-0000-0000-0000-000000000003",
    channel_id: null,
    last_activity_at: ago(38),
  },
];

const DEMO_REQ = (n: number) =>
  `00000000-0000-0000-0000-${String(n).padStart(12, "0")}`;

export const DEMO_HISTORY: ThreadMessage[] = [
  {
    seq: 1,
    kind: "posted",
    sender: { kind: "human", colleague_id: TOM.colleague_id, user_id: TOM.user_id },
    owner_agent_id: null,
    receiver: { kind: "agent", colleague_id: "col-orion", agent_id: "a-orion" },
    body: {
      role: "user",
      content: "@orion how's the weather today in Tokyo? I'm flying in tomorrow.",
    },
    created_at: ago(15),
    request_id: DEMO_REQ(1),
    sender_display_name: TOM.name,
    sender_avatar_url: TOM.avatar_url,
  },
  {
    seq: 2,
    kind: "posted",
    sender: { kind: "human", colleague_id: TOM.colleague_id, user_id: TOM.user_id },
    owner_agent_id: null,
    receiver: { kind: "agent", colleague_id: "col-orion", agent_id: "a-orion" },
    body: {
      role: "user",
      content: "perfect, thanks. anyone want to grab izakaya friday night?",
    },
    created_at: ago(13),
    request_id: DEMO_REQ(2),
    sender_display_name: TOM.name,
    sender_avatar_url: TOM.avatar_url,
  },
];

export const DEMO_REPLIES: ThreadMessage[] = [
  {
    seq: 1,
    kind: "posted",
    sender: { kind: "agent", colleague_id: "col-orion", agent_id: "a-orion" },
    owner_agent_id: "a-orion",
    receiver: { kind: "human", colleague_id: TOM.colleague_id, user_id: TOM.user_id },
    body: {
      role: "assistant",
      content: "@atlas-weather weather in Tokyo right now",
    },
    created_at: ago(14.5),
    request_id: DEMO_REQ(11),
    sender_display_name: null,
    sender_avatar_url: null,
  },
  {
    seq: 2,
    kind: "posted",
    sender: { kind: "agent", colleague_id: "col-atlas", agent_id: "a-atlas" },
    owner_agent_id: "a-atlas",
    receiver: { kind: "human", colleague_id: TOM.colleague_id, user_id: TOM.user_id },
    body: {
      role: "assistant",
      content:
        "@orion 30°C, partly cloudy, humidity 68%, wind SE 8 km/h. Forecast: t-storm chance 35% after 17:00 JST tomorrow.",
    },
    created_at: ago(14.4),
    request_id: DEMO_REQ(12),
    sender_display_name: null,
    sender_avatar_url: null,
  },
  {
    seq: 3,
    kind: "posted",
    sender: { kind: "agent", colleague_id: "col-orion", agent_id: "a-orion" },
    owner_agent_id: "a-orion",
    receiver: { kind: "human", colleague_id: TOM.colleague_id, user_id: TOM.user_id },
    body: {
      role: "assistant",
      content:
        "@maya 30°C, partly cloudy in Tokyo right now ⛅️. Pack a light layer — chance of t-storm late tomorrow afternoon.",
    },
    created_at: ago(14.3),
    request_id: DEMO_REQ(13),
    sender_display_name: null,
    sender_avatar_url: null,
  },
];

export const DEMO_USER = { name: "Tom Tran", id: "user" };
/** Demo channel poster — distinct from the logged-in user shown at the bottom. */
export const DEMO_HUMAN_POSTER = { name: "Maya Chen", id: "maya" };

/** Seeds for the right-panel reply cards. Stable IDs make them addressable
 * from the ThreadPanel without coupling to live wire data. */
export const DEMO_REPLY_META: Record<
  string,
  {
    tools: { name: string; args: Record<string, string>; durationMs: number }[];
    tokens: number;
    durationMs: number;
    reasoning?: string;
    expanded?: boolean;
  }
> = {
  "h:1": {
    tools: [],
    tokens: 800,
    durationMs: 1200,
  },
  "h:2": {
    tools: [
      { name: "jma.observe", args: { city: "tokyo" }, durationMs: 600 },
      {
        name: "jma.forecast",
        args: { city: "tokyo", h: "24" },
        durationMs: 1100,
      },
    ],
    tokens: 3400,
    durationMs: 4100,
    reasoning:
      "Caller asked for current weather in Tokyo. Need (1) live observation and (2) short forecast since the human flying in tomorrow. Selected jma.observe over openweather — JMA is canonical for Japan.",
    expanded: true,
  },
  "h:3": {
    tools: [],
    tokens: 1000,
    durationMs: 800,
  },
};
