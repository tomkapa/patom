// Pure state machine for the `/demo` scripted playback. `reduce(state, beat)`
// folds one beat onto the view state the existing chat components already
// consume (ThreadSummary[] for the timeline, a per-thread Bubble[] feed,
// roster, connection states). No React, no hooks, no clocks — deterministic by
// construction (timestamps derive from the beat index + accumulated jumps), so
// `BEATS.reduce(reduce, initial)` yields the terminal frame and a future vitest
// can drive it without fake globals.

import type { Bubble, Poster, RootMessage } from "./foldHistory";
import type { DemoReplyMeta } from "./demo";
import type {
  Channel,
  McpWireRequest,
  Mentionable,
  ThreadSummary,
} from "../types/api";

/** Where a thread lives — drives the header title, the timeline filter, and
 *  the composer mode in `DemoView`. */
export type DemoLocation =
  | { kind: "channel"; channelId: string; name: string }
  | { kind: "dm"; counterpartId: string };

export type ConnState = "idle" | "connecting" | "connected";

/** Scripted chrome rendered after a bubble via `ThreadPanel.renderAfterBubble`. */
export type DemoBadge =
  | { kind: "trigger"; label: string; at: string }
  | { kind: "guardrail"; policy: string; blocked: string; approver: string }
  | { kind: "mention"; from: string; text: string }
  | { kind: "connect"; req: McpWireRequest };

type SenderRef = { kind: "human" | "agent"; id: string };

/** One step of the storyline. Most beats are `post`s; meta/badges/connect ride
 *  along on the same beat so an agent turn (message + reasoning + tools +
 *  optional action card) stays a single deterministic unit. */
export type Beat =
  | {
      type: "post";
      id: string;
      thread: string;
      sender: SenderRef;
      text: string;
      /** Reasoning / display-only tool calls / token counters for this turn. */
      meta?: DemoReplyMeta;
      /** Chrome attached after this turn's bubble (trigger / guardrail). */
      badge?: DemoBadge;
      /** A `request_user_wire_mcp` card. Its presence marks this beat as the
       *  one interactive gate — the driver pauses until the viewer clicks. */
      connect?: McpWireRequest;
    }
  | { type: "tile"; id: string; catalogId: string; to: ConnState }
  | { type: "hire"; id: string; agent: Mentionable }
  | { type: "mention"; id: string; thread: string; badge: DemoBadge }
  | { type: "jump"; id: string; minutes: number };

export type DemoFeed = { rootMessage?: RootMessage; bubbles: Bubble[] };

export type DemoState = {
  channels: Channel[];
  roster: Mentionable[];
  threads: ThreadSummary[];
  threadLocation: Record<string, DemoLocation>;
  feeds: Record<string, DemoFeed>;
  metaByKey: Record<string, DemoReplyMeta>;
  badgesByKey: Record<string, DemoBadge[]>;
  connections: Record<string, ConnState>;
  activeThreadId: string | null;
  poster: Poster;
  offsetMs: number;
  beatIndex: number;
};

/** Static seeds: roster, channels, decorative thread roots, and the per-thread
 *  location map. Threads with no `root` here get rooted by their first `post`. */
export type DemoSeed = {
  poster: Poster;
  channels: Channel[];
  roster: Mentionable[];
  threadLocation: Record<string, DemoLocation>;
  /** Already-rooted threads shown in the timeline for context (Act 2's pricing
   *  + blog threads) — never made active, no feed playback. */
  seededThreads: ThreadSummary[];
};

// Fixed origin so timestamps are deterministic (no `Date.now`). The story is
// "launch week", so the clock reads believably across the three acts.
const BASE_MS = Date.UTC(2026, 5, 22, 9, 0, 0);
const STEP_MS = 47_000;

function tsFor(state: DemoState): string {
  return new Date(BASE_MS + state.beatIndex * STEP_MS + state.offsetMs).toISOString();
}

function agentName(roster: Mentionable[], id: string): string | null {
  return roster.find((m) => m.kind === "agent" && m.id === id)?.name ?? null;
}

/** Key of the most recent bubble in a feed — the attach point for badges. */
function lastKey(feed: DemoFeed | undefined): string | undefined {
  if (!feed || feed.bubbles.length === 0) return undefined;
  return feed.bubbles[feed.bubbles.length - 1]!.key;
}

function human(roster: Mentionable[], id: string): Mentionable | undefined {
  return roster.find((m) => m.kind === "human" && m.id === id);
}

export function initialDemoState(seed: DemoSeed): DemoState {
  return {
    channels: seed.channels,
    roster: seed.roster,
    threads: seed.seededThreads,
    threadLocation: seed.threadLocation,
    feeds: {},
    metaByKey: {},
    badgesByKey: {},
    connections: {},
    activeThreadId: null,
    poster: seed.poster,
    offsetMs: 0,
    beatIndex: 0,
  };
}

/** Build a Bubble for a post beat. Demo bubbles leave reasoning/tool_calls
 *  empty — the card reads those from `metaByKey` (the existing demo path). */
function makeBubble(
  state: DemoState,
  beat: { id: string; sender: SenderRef; text: string },
  ts: string,
): Bubble {
  const isAgent = beat.sender.kind === "agent";
  const h = isAgent ? undefined : human(state.roster, beat.sender.id);
  return {
    kind: isAgent ? "agent" : "human",
    key: `b:${beat.id}`,
    request_id: `demo:${beat.id}`,
    client_key: null,
    agent_id: isAgent ? beat.sender.id : null,
    agent_name: isAgent ? agentName(state.roster, beat.sender.id) : null,
    human_name: h?.name ?? null,
    human_id: h?.id ?? null,
    human_avatar_url: h?.avatar_url ?? null,
    ts,
    text: beat.text,
    reasoning: "",
    tool_calls: [],
    wire_requests: [],
    phase: "persisted",
  };
}

function rootSummary(
  state: DemoState,
  beat: { sender: SenderRef; text: string },
  ts: string,
): ThreadSummary["root"] {
  const isAgent = beat.sender.kind === "agent";
  const h = isAgent ? undefined : human(state.roster, beat.sender.id);
  return {
    snippet: beat.text,
    sender: isAgent
      ? { kind: "agent", colleague_id: beat.sender.id, agent_id: beat.sender.id }
      : { kind: "human", colleague_id: beat.sender.id, user_id: beat.sender.id },
    created_at: ts,
    sender_display_name: h?.name ?? null,
    sender_avatar_url: h?.avatar_url ?? null,
  };
}

function applyPost(state: DemoState, beat: Extract<Beat, { type: "post" }>): DemoState {
  const ts = tsFor(state);
  const existing = state.threads.find((t) => t.thread_id === beat.thread);
  const feed: DemoFeed = state.feeds[beat.thread] ?? { bubbles: [] };
  const isFirst = !existing && feed.bubbles.length === 0;
  const humanRoot = isFirst && beat.sender.kind === "human";

  const bubble = makeBubble(state, beat, ts);
  const nextFeed: DemoFeed = humanRoot
    ? {
        rootMessage: {
          name: bubble.human_name ?? "you",
          id: bubble.human_id ?? beat.sender.id,
          avatar_url: bubble.human_avatar_url,
          ts,
          text: beat.text,
        },
        bubbles: feed.bubbles,
      }
    : { ...feed, bubbles: [...feed.bubbles, bubble] };

  // Thread timeline row: create on first post, else bump activity + replies.
  const threads = existing
    ? state.threads.map((t) =>
        t.thread_id === beat.thread
          ? {
              ...t,
              last_activity_at: ts,
              reply_count: nextFeed.bubbles.length,
            }
          : t,
      )
    : [
        ...state.threads,
        {
          thread_id: beat.thread,
          channel_id:
            state.threadLocation[beat.thread]?.kind === "channel"
              ? (state.threadLocation[beat.thread] as { channelId: string }).channelId
              : null,
          last_activity_at: ts,
          root: rootSummary(state, beat, ts),
          reply_count: humanRoot ? 0 : nextFeed.bubbles.length,
        } satisfies ThreadSummary,
      ];

  // Badges attach to the thread's latest bubble — for a human root that's the
  // prior bubble (none on a fresh thread), else the bubble just appended. Both
  // are exactly `lastKey(nextFeed)`.
  const attachKey = lastKey(nextFeed);
  return {
    ...state,
    threads,
    feeds: { ...state.feeds, [beat.thread]: nextFeed },
    metaByKey: beat.meta
      ? { ...state.metaByKey, [bubble.key]: beat.meta }
      : state.metaByKey,
    badgesByKey: addBadges(state.badgesByKey, attachKey, [
      ...(beat.badge ? [beat.badge] : []),
      ...(beat.connect ? [{ kind: "connect", req: beat.connect } as DemoBadge] : []),
    ]),
    connections: beat.connect
      ? { ...state.connections, [beat.connect.catalog_id]: "idle" }
      : state.connections,
    activeThreadId: beat.thread,
    beatIndex: state.beatIndex + 1,
  };
}

function addBadges(
  map: Record<string, DemoBadge[]>,
  key: string | undefined,
  badges: DemoBadge[],
): Record<string, DemoBadge[]> {
  if (!key || badges.length === 0) return map;
  return { ...map, [key]: [...(map[key] ?? []), ...badges] };
}

export function reduce(state: DemoState, beat: Beat): DemoState {
  switch (beat.type) {
    case "post":
      return applyPost(state, beat);
    case "tile":
      return {
        ...state,
        connections: { ...state.connections, [beat.catalogId]: beat.to },
        beatIndex: state.beatIndex + 1,
      };
    case "hire":
      return {
        ...state,
        roster: [...state.roster, beat.agent],
        beatIndex: state.beatIndex + 1,
      };
    case "mention":
      return {
        ...state,
        badgesByKey: addBadges(state.badgesByKey, lastKey(state.feeds[beat.thread]), [
          beat.badge,
        ]),
        activeThreadId: beat.thread,
        beatIndex: state.beatIndex + 1,
      };
    case "jump":
      return {
        ...state,
        offsetMs: state.offsetMs + beat.minutes * 60_000,
        beatIndex: state.beatIndex + 1,
      };
  }
}

/** True when applying this beat should pause for the single viewer click. */
export function isGate(beat: Beat): boolean {
  return beat.type === "post" && !!beat.connect;
}

/** Fold the whole script — the terminal frame, used for reduced-motion. */
export function terminalState(seed: DemoSeed, beats: Beat[]): DemoState {
  return beats.reduce(reduce, initialDemoState(seed));
}
