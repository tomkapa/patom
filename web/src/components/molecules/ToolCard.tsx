import type { LucideIcon } from "lucide-react";
import { Wrench } from "lucide-react";
import { iconForTool } from "../../data/toolIcons";
import type { DiscoveredTool } from "../../types/api";

export function ToolCard({ tool }: { tool: DiscoveredTool }) {
  const Icon: LucideIcon = iconForTool(tool.remote_name) ?? Wrench;
  return (
    <div className="flex min-w-0 flex-col gap-1 px-4 py-3">
      <div className="flex items-center gap-2">
        <Icon
          className="h-3.5 w-3.5 shrink-0 text-[var(--color-moss)]"
          strokeWidth={1.75}
        />
        <span className="truncate font-[var(--font-mono)] text-[13px] font-semibold text-[var(--color-ink)]">
          {tool.prefixed_name}
        </span>
      </div>
      {tool.description ? (
        <p className="line-clamp-2 text-[12px] leading-snug text-[var(--color-muted-foreground)]">
          {tool.description}
        </p>
      ) : null}
    </div>
  );
}
