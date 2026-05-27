import { Check, Loader2, X } from "lucide-react";
import type { ToolCallEntry } from "../../types/api";
import { formatMs } from "../../lib/time";

export function ToolCallLine({
  call,
  durationMs,
}: {
  call: ToolCallEntry;
  durationMs?: number;
}) {
  const args = formatArgs(call.input);
  const dur = durationMs != null ? formatMs(durationMs) : null;
  const icon =
    call.status === "running" ? (
      <Loader2 className="h-3 w-3 animate-spin text-[var(--color-amber)]" />
    ) : call.status === "ok" ? (
      <Check className="h-3 w-3 text-[var(--color-moss)]" strokeWidth={2.5} />
    ) : (
      <X className="h-3 w-3 text-[var(--color-rose)]" strokeWidth={2.5} />
    );
  return (
    <div className="flex items-center gap-2 font-[var(--font-mono)] text-[11.5px] text-[var(--color-ink)]">
      <span className="shrink-0">{icon}</span>
      <span className="font-semibold whitespace-nowrap">{call.name}</span>
      {args && (
        <span className="text-[var(--color-muted-foreground)] truncate flex-1">{args}</span>
      )}
      {dur && (
        <span className="ml-auto shrink-0 text-[var(--color-fg-muted)]">
          {dur}
        </span>
      )}
    </div>
  );
}

function formatArgs(input: unknown): string {
  if (input == null) return "";
  if (typeof input !== "object") return String(input);
  const obj = input as Record<string, unknown>;
  return Object.entries(obj)
    .slice(0, 4)
    .map(([k, v]) => {
      const val = typeof v === "string" ? v : JSON.stringify(v);
      const trimmed = val.length > 30 ? val.slice(0, 28) + "…" : val;
      return `${k}=${trimmed}`;
    })
    .join(" ");
}
