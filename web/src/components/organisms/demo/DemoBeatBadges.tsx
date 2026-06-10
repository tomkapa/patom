// Scripted chrome the live chat has no equivalent for — rendered after a
// bubble via `ThreadPanel.renderAfterBubble` during the `/demo` playback. All
// pure/prop-driven; the Connect card reuses the hook-free `WireMcpRequestCardView`
// so nothing here touches the network.

import { AtSign, Clock, ShieldAlert } from "lucide-react";
import { WireMcpRequestCardView } from "../WireMcpRequestCard";
import { cn } from "../../../lib/utils";
import type { ConnState, DemoBadge } from "../../../lib/demoReducer";

export function DemoBeatBadges({
  badges,
  connections,
  onConnect,
  reduced,
}: {
  badges: DemoBadge[] | undefined;
  connections: Record<string, ConnState>;
  /** Resolves the one interactive gate (the Connect-Notion click). */
  onConnect: () => void;
  reduced: boolean;
}) {
  if (!badges || badges.length === 0) return null;
  const enter = reduced ? "" : "bubble-in";
  return (
    <div className="px-5 pb-3">
      {badges.map((b, i) => {
        switch (b.kind) {
          case "trigger":
            return (
              <div
                key={i}
                className={cn(
                  "flex items-center gap-2 border border-[var(--color-moss-soft)] bg-[var(--color-moss-tint)] px-3 py-1.5 font-[var(--font-mono)] text-[11px] text-[var(--color-moss-deep)]",
                  enter,
                )}
              >
                <Clock className="h-3.5 w-3.5" strokeWidth={2} aria-hidden />
                <span>
                  Started by scheduled task{" "}
                  <span className="font-bold">{b.label}</span> · {b.at}
                </span>
              </div>
            );
          case "mention":
            return (
              <div
                key={i}
                className={cn(
                  "flex items-start gap-2 border border-[var(--color-line-strong)] bg-[var(--color-paper-2)] px-3 py-2 text-[12px] text-[var(--color-ink)]",
                  enter,
                )}
              >
                <AtSign className="mt-0.5 h-3.5 w-3.5 shrink-0 text-[var(--color-muted-foreground)]" aria-hidden />
                <span>
                  <span className="font-[var(--font-mono)] font-bold">
                    New mention {b.from}
                  </span>{" "}
                  — “{b.text}”
                </span>
              </div>
            );
          case "guardrail":
            return (
              <div
                key={i}
                role="note"
                className={cn(
                  "border border-[var(--color-rose)] bg-[var(--color-rose-soft)] px-3 py-2 text-[12px] text-[var(--color-rose)]",
                  enter,
                )}
              >
                <div className="flex items-center gap-2 font-[var(--font-mono)] text-[11px] font-bold uppercase tracking-[0.1em]">
                  <ShieldAlert className="h-3.5 w-3.5 shrink-0" strokeWidth={2} aria-hidden />
                  <span className="line-through decoration-2">{b.blocked}</span>
                  <span className="no-underline">blocked by policy</span>
                </div>
                <p className="mt-1 leading-[1.5]">
                  Policy <span className="font-semibold">{b.policy}</span> requires
                  human approval — escalated to{" "}
                  <span className="font-semibold">{b.approver}</span> before any
                  outward reply.
                </p>
              </div>
            );
          case "connect": {
            const conn = connections[b.req.catalog_id] ?? "idle";
            return (
              <div key={i} className={enter}>
                <WireMcpRequestCardView
                  entry={b.req}
                  wired={conn === "connected"}
                  done={false}
                  submitting={conn === "connecting"}
                  onConnect={onConnect}
                />
              </div>
            );
          }
        }
      })}
    </div>
  );
}
