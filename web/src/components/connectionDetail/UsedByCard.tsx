import { useMemo } from "react";
import { ChevronRight } from "lucide-react";
import { Link } from "react-router-dom";
import { CountBadge } from "../atoms/CountBadge";
import { Monogram } from "../atoms/Monogram";
import { SectionHeader } from "../atoms/SectionHeader";
import { SectionCard } from "../molecules/SectionCard";
import { useAgents } from "../../hooks/useAgents";
import { useT } from "../../i18n";
import { useMcpServers } from "../../hooks/useMcpServers";
import type { Agent } from "../../types/api";

export function UsedByCard({ serverId }: { serverId: string }) {
  const { t } = useT();
  const agentsQuery = useAgents();
  const serversQuery = useMcpServers();
  const agents = useMemo<Agent[]>(() => {
    // `allowed_mcp_tools` is keyed by catalog_id, not server UUID — look
    // up the wired server's catalog_id, then filter agents whose
    // allowlist names it.
    const server = (serversQuery.data ?? []).find((s) => s.id === serverId);
    if (!server) return [];
    const all = (agentsQuery.data ?? []) as Agent[];
    return all.filter((a) =>
      Object.prototype.hasOwnProperty.call(
        a.allowed_mcp_tools ?? {},
        server.catalog_id,
      ),
    );
  }, [agentsQuery.data, serversQuery.data, serverId]);

  return (
    <SectionCard
      header={
        <SectionHeader
          eyebrow={
            <>
              {t("connections.detail.usedBy.eyebrow")}
              <CountBadge>
                {t("connections.detail.usedBy.count", {
                  count: agents.length,
                })}
              </CountBadge>
            </>
          }
        />
      }
    >
      {agents.length === 0 ? (
        <div className="px-5 py-6 text-center text-[12px] text-[var(--color-muted)]">
          {t("connections.detail.usedBy.empty")}
        </div>
      ) : (
        <ul className="divide-y divide-[var(--color-line)]">
          {agents.map((agent) => (
            <li key={agent.id}>
              <Link
                to={`/`}
                className="flex items-center gap-3 px-5 py-3 transition-colors hover:bg-[var(--color-paper-2)]"
                aria-label={agent.name}
              >
                <Monogram name={agent.name} id={agent.id} size={32} />
                <span className="flex-1 truncate text-[13px] font-semibold text-[var(--color-moss-deep)]">
                  {agent.name}
                </span>
                <ChevronRight
                  className="h-3.5 w-3.5 shrink-0 text-[var(--color-muted)]"
                  strokeWidth={1.75}
                  aria-hidden
                />
              </Link>
            </li>
          ))}
        </ul>
      )}
    </SectionCard>
  );
}
