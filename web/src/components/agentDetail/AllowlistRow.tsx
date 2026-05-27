import { ChevronDown, ChevronUp } from "lucide-react";
import { useMemo, useState } from "react";
import { CatalogIcon } from "../atoms/CatalogIcon";
import { Checkbox } from "../molecules/Checkbox";
import { useT } from "../../i18n";
import { useCatalogLookup } from "../../hooks/useMcpServers";
import { cn } from "../../lib/utils";
import type { TranslationKey } from "../../i18n/en";
import type { McpServer } from "../../types/api";
import {
  isToolAllowed,
  shapeOf,
  toggleServer,
  toggleTool,
  type Allowlist,
  type ServerCheckState,
} from "./allowlistState";

const SUMMARY_KEY: Record<ServerCheckState, TranslationKey> = {
  all: "agent.detail.tools.row.allOf",
  mixed: "agent.detail.tools.row.someOf",
  unchecked: "agent.detail.tools.row.available",
};

/** One MCP server row inside the per-agent allowlist editor. */
export function AllowlistRow({
  server,
  list,
  onChange,
}: {
  server: McpServer;
  list: Allowlist;
  onChange: (next: Allowlist) => void;
}) {
  const { t } = useT();
  const tools = server.discovered_tools ?? [];
  const toolNames = useMemo(
    () => tools.map((tool) => tool.remote_name),
    [tools],
  );
  const catalogLookup = useCatalogLookup();
  const catalog = useMemo(
    () => catalogLookup(server.catalog_id),
    [catalogLookup, server.catalog_id],
  );
  const displayName = catalog?.display_name ?? server.catalog_id;
  const shape = shapeOf(list, server.catalog_id, toolNames);
  const isOn = shape !== "unchecked";
  const [open, setOpen] = useState<boolean>(false);
  const canExpand = isOn && tools.length > 0;

  const enabled =
    shape === "all"
      ? tools.length
      : shape === "mixed"
        ? (list[server.catalog_id] as string[]).length
        : 0;
  const summary = t(SUMMARY_KEY[shape], {
    total: String(tools.length),
    enabled: String(enabled),
  });

  return (
    <div
      className={cn(
        "border-b border-[var(--color-line)] last:border-b-0",
        isOn ? "bg-[var(--color-moss-tint)]" : "bg-[var(--color-card)]",
        !server.enabled && "opacity-60",
      )}
    >
      <div className="flex items-center gap-3.5 px-5 py-3.5">
        <Checkbox
          checked={isOn}
          onChange={(next) => {
            onChange(toggleServer(list, server.catalog_id, next));
            if (!next) setOpen(false);
          }}
          aria-label={t("agent.detail.tools.row.toggleAria", {
            name: server.catalog_id,
          })}
        />
        <CatalogIcon
          name={displayName}
          iconUrl={catalog?.icon_url}
          size={32}
        />
        <div className="min-w-0 flex-1">
          <div className="flex items-center gap-2">
            <span className="truncate font-[var(--font-display)] text-[14px] font-semibold text-[var(--color-ink)]">
              {displayName}
            </span>
            {server.connection_status !== "ok" ? (
              <span className="font-[var(--font-mono)] text-[10px] tracking-[0.1em] text-[var(--color-amber)] uppercase">
                {server.connection_status === "reconnect_required"
                  ? t("agent.detail.tools.row.statusReconnect")
                  : t("agent.detail.tools.row.statusError")}
              </span>
            ) : null}
          </div>
          <div className="mt-0.5 font-[var(--font-mono)] text-[11px] text-[var(--color-muted-foreground)]">
            {tools.length === 0
              ? t("agent.detail.tools.row.empty")
              : summary}
          </div>
        </div>
        {canExpand ? (
          <button
            type="button"
            onClick={() => setOpen((v) => !v)}
            aria-expanded={open}
            aria-label={t(
              open
                ? "agent.detail.tools.row.collapse"
                : "agent.detail.tools.row.expand",
            )}
            className="rounded p-1 text-[var(--color-muted-foreground)] hover:text-[var(--color-ink)]"
          >
            {open ? (
              <ChevronUp className="h-4 w-4" strokeWidth={1.75} />
            ) : (
              <ChevronDown className="h-4 w-4" strokeWidth={1.75} />
            )}
          </button>
        ) : (
          <ChevronDown
            className="h-4 w-4 text-[var(--color-fg-muted)] opacity-40"
            strokeWidth={1.75}
            aria-hidden
          />
        )}
      </div>
      {open ? (
        <div className="flex flex-col gap-1 px-5 pt-1 pb-4 pl-[68px]">
          {tools.map((tool) => {
            const checked = isToolAllowed(list, server.catalog_id, tool.remote_name);
            return (
              <label
                key={tool.remote_name}
                className="flex cursor-pointer items-start gap-3.5 px-3 py-2"
              >
                <span className="mt-0.5">
                  <Checkbox
                    checked={checked}
                    onChange={(next) =>
                      onChange(
                        toggleTool(
                          list,
                          server.catalog_id,
                          tool.remote_name,
                          toolNames,
                          next,
                        ),
                      )
                    }
                    aria-label={tool.remote_name}
                  />
                </span>
                <span
                  className={cn(
                    "min-w-0 flex-1 font-[var(--font-mono)] text-[12px] font-semibold",
                    checked
                      ? "text-[var(--color-ink)]"
                      : "text-[var(--color-muted-foreground)]",
                  )}
                >
                  {tool.remote_name}
                </span>
              </label>
            );
          })}
        </div>
      ) : null}
    </div>
  );
}
