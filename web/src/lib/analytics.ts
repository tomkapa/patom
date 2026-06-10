// The ONLY module that references `posthog-js`. Everything else calls this
// thin API, so swapping the vendor (or self-hosting later) is a one-file change.
//
// No key ⇒ hard no-op: `posthog-js` is loaded via a *dynamic* import inside
// `init()`, which returns early without a key — so the vendor never ships to
// the client in OSS / self-host / local-dev builds (it's a separate chunk that
// is simply never fetched), and every other call is a guarded no-op.
import type { PostHog } from "posthog-js";
import type { Org, User } from "../types/api";

// `window.__PATOM_CONFIG__` is injected by the Rust server into `index.html` at
// startup (see `crates/patom-core/src/app.rs`). Reading it here is synchronous —
// no fetch roundtrip. An absent or empty key keeps analytics a hard no-op, so
// OSS / self-host deployments without `PATOM_POSTHOG_KEY` are unaffected.
const KEY = window.__PATOM_CONFIG__?.posthogKey ?? "";
// `||` (not `??`): the server injects "" when no host is configured; both ""
// and `undefined` fall through to the EU default, while a real value wins.
const HOST = window.__PATOM_CONFIG__?.posthogHost || "https://eu.i.posthog.com";

/** `false` in OSS / self-host / local-dev builds (no key) — the no-op switch. */
const enabled = KEY.length > 0;

/** Every captured event name. A union, not `string`: a typo is a compile error
 *  and the full surface is greppable from this one declaration. */
export type AnalyticsEvent =
  | "signed_in"
  | "onboarding_step_viewed"
  | "workspace_created"
  | "onboarding_completed"
  | "agent_created"
  | "message_sent"
  | "connection_catalog_opened"
  | "connection_oauth_started"
  | "connection_oauth_completed"
  | "connection_oauth_failed"
  | "thread_opened"
  | "agent_invoked"
  | "invite_sent"
  | "org_switched"
  | "budget_warning_shown";

// The live client, set once the dynamic import in `init()` resolves.
let client: PostHog | null = null;
// Calls made before the async load lands are queued and replayed in order, so
// startup events (`signed_in`, the first `identify`/`pageview`) aren't lost to
// the import race. Bounded — a backstop against an init that never resolves.
const MAX_QUEUE = 64;
const queue: Array<(ph: PostHog) => void> = [];

/** Run an op against the client now, or queue it until the client loads. No-op
 *  (and never queues) when analytics is disabled. */
function withClient(op: (ph: PostHog) => void): void {
  if (!enabled) return;
  if (client) {
    op(client);
    return;
  }
  if (queue.length < MAX_QUEUE) queue.push(op);
}

/** Boot PostHog once, at app startup. No-op (and loads nothing) without a key. */
export function init(): void {
  if (!enabled || client) return;
  void import("posthog-js").then(({ default: posthog }) => {
    posthog.init(KEY, {
      api_host: HOST,
      // Explicit events only — we never auto-capture clicks/inputs on a
      // recruiting product, and pageviews are sent manually per route.
      autocapture: false,
      capture_pageview: false,
      // No anonymous profiles: a person exists only after `identify()`.
      person_profiles: "identified_only",
    });
    client = posthog;
    for (const op of queue.splice(0)) op(posthog);
  });
}

/** Capture a typed product event with optional properties. */
export function track(
  event: AnalyticsEvent,
  props?: Record<string, unknown>,
): void {
  withClient((ph) => ph.capture(event, props));
}

/** Bind the session to a user and their active org (group analytics). Called
 *  on first `me` load and on every org switch. `org` is `null` for an org-less
 *  session (signed in, no workspace yet). */
export function identify(user: User, org: Org | null): void {
  withClient((ph) => {
    ph.identify(user.id, { email: user.email });
    if (org) {
      // Per-workspace activation/retention — the B2B metric that matters more
      // than per-user. `member_count` is intentionally omitted: it is not on
      // the `Org` shape carried by `me` (see `OrgDetails` for that).
      ph.group("org", org.id, { name: org.name, role: org.role });
    }
  });
}

/** Manual SPA pageview — one per route change. */
export function pageview(path: string): void {
  withClient((ph) => ph.capture("$pageview", { $current_url: path }));
}

/** Clear identity on logout so the next user starts a fresh session. */
export function reset(): void {
  withClient((ph) => ph.reset());
}
