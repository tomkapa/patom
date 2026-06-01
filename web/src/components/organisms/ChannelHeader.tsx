import { Hash } from "lucide-react";
import type { Agent } from "../../types/api";

export function ChannelHeader({
  channel,
  agents,
}: {
  channel: string;
  agents: Agent[];
}) {
  return (
    <header className="border-b border-[var(--color-line)] bg-[var(--color-paper)] px-4 md:px-8 pt-4 pb-3">
      <div className="flex items-baseline gap-2">
        <Hash className="h-[18px] w-[18px] text-[var(--color-ink)]" strokeWidth={2.4} />
        <h1 className="font-[var(--font-display)] text-[20px] font-bold tracking-tight text-[var(--color-ink)]">
          {channel}
        </h1>
      </div>
      <p className="mt-1 font-[var(--font-mono)] text-[11px] text-[var(--color-muted-foreground)]">
        <span className="text-[var(--color-ink)] font-semibold">
          {agents.length}
        </span>{" "}
        {agents.length === 1 ? "agent" : "agents"}
      </p>
    </header>
  );
}
