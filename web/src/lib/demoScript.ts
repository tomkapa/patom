// The Folio v3-launch storyline for the public `/demo` playback. Three acts —
// Recruit → Collaborate → Proactive — authored as a flat `Beat[]` plus the
// static seed (cast, channel, decorative threads). Pure data; the reducer in
// `demoReducer.ts` folds these into view state and `useDemoPlayback` schedules
// them. Tool *results* are carried by the agent's next message, since the
// tool-call chips render name + args only.

import type { Channel, Mentionable, ThreadSummary } from "../types/api";
import type { Beat, DemoLocation, DemoSeed } from "./demoReducer";

// ── Cast ────────────────────────────────────────────────────────────────────
const TOM: Mentionable = { kind: "human", id: "tom", name: "Tom", avatar_url: null };
const MAYA: Mentionable = { kind: "human", id: "maya", name: "Maya", avatar_url: null };
const DANI: Mentionable = { kind: "human", id: "dani", name: "Dani", avatar_url: null };

const RECRUITER: Mentionable = { kind: "agent", id: "recruiter", name: "recruiter", avatar_url: null };
const MLEAD: Mentionable = { kind: "agent", id: "marketing-lead", name: "marketing-lead", avatar_url: null };
const CONTENT: Mentionable = { kind: "agent", id: "content-writer", name: "content-writer", avatar_url: null };
const SEO: Mentionable = { kind: "agent", id: "seo-analyst", name: "seo-analyst", avatar_url: null };
const SOCIAL: Mentionable = { kind: "agent", id: "social-manager", name: "social-manager", avatar_url: null };
/** The Act-1 hire — joins the roster mid-playback via a `hire` beat. */
const LIFECYCLE: Mentionable = { kind: "agent", id: "lifecycle-marketer", name: "lifecycle-marketer", avatar_url: null };

// ── Locations ───────────────────────────────────────────────────────────────
const CH_LAUNCH = "c-launch";
const LAUNCH: DemoLocation = { kind: "channel", channelId: CH_LAUNCH, name: "launch" };

const T_HIRE = "t-hire";
const T_PRICING = "t-pricing";
const T_BLOG = "t-blog";
const T_PLAN = "t-plan";
const T_SOCIAL = "t-social";

const CHANNELS: Channel[] = [
  {
    id: CH_LAUNCH,
    name: "launch",
    system: true,
    can_manage: false,
    created_at: new Date(Date.UTC(2026, 5, 1)).toISOString(),
    archived_at: null,
  },
];

// Decorative #launch threads so the timeline shows the multi-thread view.
const seededThreads: ThreadSummary[] = [
  {
    thread_id: T_PRICING,
    channel_id: CH_LAUNCH,
    last_activity_at: new Date(Date.UTC(2026, 5, 22, 8, 30)).toISOString(),
    root: {
      snippet: "v3 pricing page — final copy review before Tue? @maya",
      sender: { kind: "human", colleague_id: MAYA.id, user_id: MAYA.id },
      created_at: new Date(Date.UTC(2026, 5, 22, 8, 30)).toISOString(),
      sender_display_name: MAYA.name,
      sender_avatar_url: null,
    },
    reply_count: 4,
  },
  {
    thread_id: T_BLOG,
    channel_id: CH_LAUNCH,
    last_activity_at: new Date(Date.UTC(2026, 5, 22, 8, 50)).toISOString(),
    root: {
      snippet: "Blog calendar for launch week 🗓️ — three slots still open",
      sender: { kind: "human", colleague_id: DANI.id, user_id: DANI.id },
      created_at: new Date(Date.UTC(2026, 5, 22, 8, 50)).toISOString(),
      sender_display_name: DANI.name,
      sender_avatar_url: null,
    },
    reply_count: 2,
  },
];

export const DEMO_SEED: DemoSeed = {
  poster: { name: TOM.name, id: TOM.id, avatar_url: null },
  channels: CHANNELS,
  roster: [TOM, MAYA, DANI, RECRUITER, MLEAD, CONTENT, SEO, SOCIAL],
  threadLocation: {
    [T_HIRE]: { kind: "dm", counterpartId: RECRUITER.id },
    [T_PRICING]: LAUNCH,
    [T_BLOG]: LAUNCH,
    [T_PLAN]: LAUNCH,
    [T_SOCIAL]: LAUNCH,
  },
  seededThreads,
};

// ── Beats ─────────────────────────────────────────────────────────────────
let n = 0;
const id = () => `n${n++}`;

const ACT1: Beat[] = [
  { type: "post", id: id(), thread: T_HIRE, sender: TOM, text: "We're scaling our launch emails and nobody owns lifecycle. Can we hire someone?" },
  {
    type: "post", id: id(), thread: T_HIRE, sender: RECRUITER,
    text: "Happy to. First — is this **lifecycle/retention email** (onboarding, win-back), or broader campaign blasts?",
    meta: { tools: [], tokens: 700, durationMs: 1200, reasoning: "Scope to one concrete sentence before hiring anyone.", expanded: true },
  },
  { type: "post", id: id(), thread: T_HIRE, sender: TOM, text: "Lifecycle — onboarding drips and win-back." },
  {
    type: "post", id: id(), thread: T_HIRE, sender: RECRUITER,
    text: "Checking the bench so we don't duplicate…",
    meta: { tools: [{ name: "search_agents", args: { role: "lifecycle email" }, durationMs: 600 }], tokens: 1400, durationMs: 2200, reasoning: "Closest is content-writer — drafts copy, doesn't own flows. No lifecycle owner today.", expanded: true },
  },
  {
    type: "post", id: id(), thread: T_HIRE, sender: RECRUITER,
    text: "No one owns flows today — worth hiring. Let me see what this role can plug into.",
    meta: { tools: [{ name: "search_tools", args: {}, durationMs: 500 }], tokens: 1100, durationMs: 1600, reasoning: "Wired: Slack, Drive. Notion is available but not yet wired." },
  },
  {
    type: "post", id: id(), thread: T_HIRE, sender: RECRUITER,
    text: "Doing a little homework before I recommend a setup…",
    meta: { tools: [{ name: "web_fetch", args: { url: "notion.so" }, durationMs: 700 }, { name: "web_search", args: { q: "notion api email content" }, durationMs: 1200 }], tokens: 2600, durationMs: 3400 },
  },
  {
    type: "post", id: id(), thread: T_HIRE, sender: RECRUITER,
    text: "Your campaign briefs and the launch doc already live in **Notion**, and a lifecycle marketer needs to read those flows to write them. It complements Slack/Drive without overlap.",
    connect: { catalog_id: "notion", display_name: "Notion", reason: "Connect Notion so the new hire can read the brief.", auth_kind: "oauth2", homepage_url: "https://notion.so" },
  },
  { type: "tile", id: id(), catalogId: "notion", to: "connecting" },
  { type: "tile", id: id(), catalogId: "notion", to: "connected" },
  {
    type: "post", id: id(), thread: T_HIRE, sender: RECRUITER,
    text: "Connected ✅. Re-checking…",
    meta: { tools: [{ name: "search_tools", args: {}, durationMs: 500 }], tokens: 900, durationMs: 1300, reasoning: "Notion now wired:true — safe to recommend a role that reads it." },
  },
  {
    type: "post", id: id(), thread: T_HIRE, sender: RECRUITER,
    text: "Draft: name `lifecycle-marketer`, reads Notion briefs, reports to @marketing-lead, escalates copy questions to @content-writer. Good to hire?",
  },
  { type: "post", id: id(), thread: T_HIRE, sender: TOM, text: "Go." },
  { type: "hire", id: id(), agent: LIFECYCLE },
  {
    type: "post", id: id(), thread: T_HIRE, sender: RECRUITER,
    text: "Hired 🎉 **@lifecycle-marketer** is in the org. Open a session when you want to brief them.",
    meta: { tools: [{ name: "create_agent", args: { name: "lifecycle-marketer", reports_to: "marketing-lead" }, durationMs: 900 }], tokens: 1200, durationMs: 1500 },
  },
];

const ACT2: Beat[] = [
  { type: "jump", id: id(), minutes: 1440 },
  { type: "post", id: id(), thread: T_PLAN, sender: MAYA, text: "v3 ships Tue the 24th. We are not ready on the content side." },
  { type: "post", id: id(), thread: T_PLAN, sender: TOM, text: "Agreed. Landing copy + launch blog + social — who's pulling it together?" },
  { type: "post", id: id(), thread: T_PLAN, sender: DANI, text: "Brief's in Notion ('v3 Launch — messaging'). Don't start from scratch." },
  { type: "post", id: id(), thread: T_PLAN, sender: TOM, text: "@marketing-lead can you own the launch content plan off Dani's brief?" },
  {
    type: "post", id: id(), thread: T_PLAN, sender: MLEAD,
    text: "On it. Reading the brief, then I'll split the work.",
    meta: { tools: [{ name: "notion.search", args: { q: "v3 Launch messaging" }, durationMs: 700 }, { name: "notion.fetch", args: { page: "v3-launch-messaging" }, durationMs: 900 }], tokens: 3100, durationMs: 4200, reasoning: "Read Dani's brief first, then fan the work out to SEO + content so nobody starts cold.", expanded: true },
  },
  { type: "post", id: id(), thread: T_PLAN, sender: MLEAD, text: "**@seo-analyst** — pull the top 5 keywords we should rank for on the v3 landing page. Deltas vs today." },
  { type: "post", id: id(), thread: T_PLAN, sender: MLEAD, text: "**@content-writer** — draft the launch blog from the Notion brief once SEO lands the keywords." },
  {
    type: "post", id: id(), thread: T_PLAN, sender: SEO,
    text: "Researching…",
    meta: { tools: [{ name: "web_search", args: { q: "project management software v3 keywords" }, durationMs: 1300 }, { name: "search_console.query", args: { site: "folio.app" }, durationMs: 800 }], tokens: 2400, durationMs: 3600, reasoning: "'team workspace' sits at pos 8 — an opportunity; 'notion alternative' is rising fast." },
  },
  { type: "post", id: id(), thread: T_PLAN, sender: SEO, text: "**@marketing-lead** Top 5: *team workspace, notion alternative, …* — full deltas in-thread. 'notion alternative' is the wedge." },
  {
    type: "post", id: id(), thread: T_PLAN, sender: CONTENT,
    text: "Drafting around 'notion alternative' + the brief's promise.",
    meta: { tools: [{ name: "notion.fetch", args: { page: "v3-launch-messaging" }, durationMs: 850 }], tokens: 4100, durationMs: 5200, reasoning: "Lead with the wedge keyword, keep the brief's voice, end on a trial CTA." },
  },
  { type: "post", id: id(), thread: T_PLAN, sender: CONTENT, text: "Draft up: **'Folio v3: the workspace that thinks for you'** — 700 words, 3 H2s, CTA to trial. Linked in Notion." },
  { type: "post", id: id(), thread: T_PLAN, sender: MLEAD, text: "**@maya** plan's executing: SEO keywords locked, blog drafted, social next. Ship-ready by Mon." },
];

const ACT3: Beat[] = [
  { type: "jump", id: id(), minutes: 2400 },
  {
    type: "post", id: id(), thread: T_SOCIAL, sender: SOCIAL,
    text: "Morning — launch day 🚀. I've built the social rollout from @content-writer's blog: 5 posts, staggered across peak windows.",
    meta: { tools: [{ name: "notion.fetch", args: { page: "v3-launch-blog" }, durationMs: 800 }, { name: "x.schedule", args: { posts: "5" }, durationMs: 1100 }], tokens: 3300, durationMs: 4400, reasoning: "Scheduled fire. Pull the approved blog, derive a thread, stagger across peak windows: 09:00 / 11:00 / 13:00 / 15:00 / 16:30.", expanded: true },
    badge: { kind: "trigger", label: "launch-day-social", at: "07:00" },
  },
  { type: "post", id: id(), thread: T_SOCIAL, sender: SOCIAL, text: "**@maya** quick review before they go live — first fires in 90 min." },
  { type: "post", id: id(), thread: T_SOCIAL, sender: MAYA, text: "Approved — but hold the 16:30, that clashes with the AMA." },
  {
    type: "post", id: id(), thread: T_SOCIAL, sender: SOCIAL,
    text: "Done — pulled 16:30, rescheduled to **18:00** post-AMA. I'll watch mentions through the day and recap at 6pm.",
    meta: { tools: [{ name: "x.update", args: { from: "16:30", to: "18:00" }, durationMs: 600 }, { name: "schedule_task", args: { name: "recap", at: "18:00" }, durationMs: 500 }], tokens: 1500, durationMs: 2100, reasoning: "Reschedule the clash, then schedule my own 18:00 recap so the day closes itself out." },
  },
  { type: "jump", id: id(), minutes: 260 },
  { type: "mention", id: id(), thread: T_SOCIAL, badge: { kind: "mention", from: "@folio", text: "your billing page double-charged me $48 on the v3 upgrade??" } },
  {
    type: "post", id: id(), thread: T_SOCIAL, sender: SOCIAL,
    text: "**@tom** escalating — public complaint about a **$48 double charge** on v3 upgrade. It touches billing, so I'm holding per policy rather than replying in-voice. Want me to draft a holding reply for your approval?",
    meta: { tools: [], tokens: 1900, durationMs: 2400, reasoning: "Complaint touches money/billing. Policy requires escalation before replying on money/safety/legal — I will not auto-respond.", expanded: true },
    badge: { kind: "guardrail", policy: "money / safety / legal", blocked: "send_message(public)", approver: "@tom" },
  },
  { type: "post", id: id(), thread: T_SOCIAL, sender: TOM, text: "Yes, draft a holding reply and I'll loop in support." },
  { type: "post", id: id(), thread: T_SOCIAL, sender: SOCIAL, text: "Draft (not yet published): *'Really sorry — that's not right. DMing you now to get the double charge fixed today.'* Hold to publish?" },
];

/** The full storyline, in playback order. */
export const BEATS: Beat[] = [...ACT1, ...ACT2, ...ACT3];

/** Act boundaries (beat index where each act starts) — drives the act label
 *  in the playback bar. */
export const ACT_STARTS = [0, ACT1.length, ACT1.length + ACT2.length];
