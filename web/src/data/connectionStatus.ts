import type { TranslationKey } from "../i18n/en";
import type { ConnectionStatus, McpServer } from "../types/api";

/** Frontend-side tone derived from the backend `connection_status`.
 *  The backend owns the rule: `auth_pending` is set only when a row
 *  legitimately needs credentials and has none (OAuth catalog mid-flow);
 *  no-auth custom servers and credentialed rows both report `ok` from
 *  the create response onwards. The FE just maps the wire string 1:1. */
export type StatusTone = "ok" | "reconnect" | "error" | "pending";

const FROM_BACKEND: Record<ConnectionStatus, StatusTone> = {
  ok: "ok",
  auth_pending: "pending",
  reconnect_required: "reconnect",
  error: "error",
};

export function statusToneOf(server: McpServer): StatusTone {
  return FROM_BACKEND[server.connection_status];
}

/** CSS `var(--color-*)` reference per tone. Shared by every surface that
 *  needs to color status text/dot/border. */
export const STATUS_COLOR: Record<StatusTone, string> = {
  ok: "var(--color-moss)",
  reconnect: "var(--color-amber)",
  error: "var(--color-rose)",
  pending: "var(--color-muted-2)",
};

/** Background tint per tone (used by the status pill background fill). */
export const STATUS_BG: Record<StatusTone, string> = {
  ok: "var(--color-moss-tint)",
  reconnect: "var(--color-amber-soft)",
  error: "var(--color-rose-soft)",
  pending: "var(--color-paper-2)",
};

/** Mapping onto the `StatusSquare` atom's variant set. */
export const STATUS_SQUARE: Record<StatusTone, "live" | "idle" | "error" | "muted"> = {
  ok: "live",
  reconnect: "idle",
  error: "error",
  pending: "muted",
};

export const STATUS_KEY: Record<StatusTone, TranslationKey> = {
  ok: "connections.status.ok",
  reconnect: "connections.status.reconnect",
  error: "connections.status.error",
  pending: "connections.status.pending",
};
