import { useMemo } from "react";
import { SectionCard } from "../molecules/SectionCard";
import { SectionHeader } from "../atoms/SectionHeader";
import { cn } from "../../lib/utils";
import { useT } from "../../i18n";
import type { McpServer } from "../../types/api";
import { totalToolsAllowed, type Allowlist } from "./allowlistState";

/** Right-column "Capability summary" panel.
 *
 *  The design also showed a "Write-capable tools" tile but
 *  `DiscoveredTool` carries no side-effect annotation today — see the
 *  follow-up GitHub issue ("Surface write-capable/read-only
 *  classification on DiscoveredTool"). Shipping a name-heuristic count
 *  here would mislead operators making security decisions, so the tile
 *  is intentionally omitted until the underlying field exists. */
export function CapabilitySummary({
  servers,
  list,
}: {
  servers: McpServer[];
  list: Allowlist;
}) {
  const { t } = useT();
  const { toolsByServer, totalToolsKnown } = useMemo(() => {
    const map: Record<string, string[]> = {};
    let total = 0;
    for (const server of servers) {
      const names = (server.discovered_tools ?? []).map(
        (tool) => tool.remote_name,
      );
      // `allowed_mcp_tools` is keyed by catalog_id, so the per-server
      // tool catalog the totaliser consumes must also be keyed by
      // catalog_id for the counts to line up.
      map[server.catalog_id] = names;
      total += names.length;
    }
    return { toolsByServer: map, totalToolsKnown: total };
  }, [servers]);
  // Mirror AllowlistCard: only count keys that point at a currently-loaded
  // server. Stale entries from connections that have been disabled or
  // deleted otherwise inflate the numerator past the denominator.
  const connectionsEnabled = servers.reduce(
    (n, s) => (s.catalog_id in list ? n + 1 : n),
    0,
  );
  const toolsAllowed = totalToolsAllowed(list, toolsByServer);

  return (
    <SectionCard
      header={
        <SectionHeader eyebrow={t("agent.detail.summary.eyebrow")} />
      }
    >
      <SummaryRow
        label={t("agent.detail.summary.connections")}
        value={String(connectionsEnabled)}
        denominator={String(servers.length)}
      />
      <SummaryRow
        label={t("agent.detail.summary.tools")}
        value={String(toolsAllowed)}
        denominator={String(totalToolsKnown)}
        last
      />
    </SectionCard>
  );
}

function SummaryRow({
  label,
  value,
  denominator,
  last,
}: {
  label: string;
  value: string;
  denominator?: string;
  last?: boolean;
}) {
  return (
    <div
      className={cn(
        "flex items-center justify-between gap-3 px-5 py-3",
        !last && "border-b border-[var(--color-line)]",
      )}
    >
      <span className="text-[12px] text-[var(--color-muted)]">{label}</span>
      <span className="flex items-baseline gap-1">
        <span className="font-[var(--font-mono)] text-[16px] font-bold text-[var(--color-ink)]">
          {value}
        </span>
        {denominator ? (
          <span className="font-[var(--font-mono)] text-[12px] text-[var(--color-muted)]">
            / {denominator}
          </span>
        ) : null}
      </span>
    </div>
  );
}
