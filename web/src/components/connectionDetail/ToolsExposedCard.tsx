import { useMemo, useState } from "react";
import { ChevronDown, ChevronUp, Search } from "lucide-react";
import { CountBadge } from "../atoms/CountBadge";
import { SectionHeader } from "../atoms/SectionHeader";
import { SectionCard } from "../molecules/SectionCard";
import { ToolCard } from "../molecules/ToolCard";
import { useT } from "../../i18n";
import type { DiscoveredTool } from "../../types/api";

const COLLAPSED_LIMIT = 6;

export function ToolsExposedCard({
  tools,
}: {
  tools: readonly DiscoveredTool[];
}) {
  const { t } = useT();
  const [filter, setFilter] = useState("");
  const [expanded, setExpanded] = useState(false);

  const filtered = useMemo(() => {
    const q = filter.trim().toLowerCase();
    if (!q) return tools;
    return tools.filter((tool) => {
      return (
        tool.prefixed_name.toLowerCase().includes(q) ||
        tool.remote_name.toLowerCase().includes(q) ||
        (tool.description?.toLowerCase().includes(q) ?? false)
      );
    });
  }, [tools, filter]);

  const visible = expanded ? filtered : filtered.slice(0, COLLAPSED_LIMIT);
  const hidden = Math.max(0, filtered.length - visible.length);

  return (
    <SectionCard
      header={
        <SectionHeader
          eyebrow={
            <>
              {t("connections.detail.tools.eyebrow")}
              <CountBadge>{tools.length}</CountBadge>
            </>
          }
          right={
            <label className="flex w-[220px] items-center gap-2 border border-[var(--color-line)] bg-[var(--color-card)] px-3 py-1.5">
              <Search
                className="h-3 w-3 shrink-0 text-[var(--color-muted-foreground)]"
                strokeWidth={1.75}
                aria-hidden
              />
              <input
                type="search"
                value={filter}
                onChange={(e) => setFilter(e.target.value)}
                placeholder={t("connections.detail.tools.filterPlaceholder")}
                className="w-full bg-transparent text-[12px] text-[var(--color-ink)] outline-none placeholder:text-[var(--color-muted-foreground)]"
                aria-label={t("connections.detail.tools.filterAria")}
              />
            </label>
          }
        />
      }
      footer={
        filtered.length > COLLAPSED_LIMIT ? (
          <button
            type="button"
            onClick={() => setExpanded((v) => !v)}
            className="flex w-full items-center justify-center gap-1.5 border-t border-[var(--color-line)] bg-[var(--color-paper-2)] px-5 py-2.5 text-[12px] font-semibold text-[var(--color-moss-deep)] hover:bg-[var(--color-paper-3)]"
          >
            {expanded
              ? t("connections.detail.tools.showLess")
              : t("connections.detail.tools.showMore", { count: hidden })}
            {expanded ? (
              <ChevronUp className="h-3 w-3" strokeWidth={1.75} />
            ) : (
              <ChevronDown className="h-3 w-3" strokeWidth={1.75} />
            )}
          </button>
        ) : null
      }
    >
      {visible.length === 0 ? (
        <div className="px-5 py-8 text-center text-[12px] text-[var(--color-muted-foreground)]">
          {t("connections.detail.tools.empty")}
        </div>
      ) : (
        <div className="grid grid-cols-2 [&>*]:border-b [&>*]:border-r [&>*]:border-[var(--color-line)] [&>*:nth-child(2n)]:border-r-0 [&>*:nth-last-child(-n+2)]:border-b-0">
          {visible.map((tool) => (
            <ToolCard key={tool.prefixed_name} tool={tool} />
          ))}
        </div>
      )}
    </SectionCard>
  );
}
