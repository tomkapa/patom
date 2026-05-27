import type { ReactNode } from "react";
import { LayoutGrid, Link as LinkIcon } from "lucide-react";
import { motion } from "motion/react";
import { useLocation, useNavigate } from "react-router-dom";
import { MenuRail } from "../organisms/MenuRail";
import { GlobalErrorBanner } from "../organisms/GlobalErrorBanner";
import { useMcpServers } from "../../hooks/useMcpServers";
import { useT } from "../../i18n";
import { useAuthStore } from "../../stores/authStore";
import { cn } from "../../lib/utils";
import { indicatorSpring } from "../../lib/motion";

export function ConnectionsLayout({
  active,
  children,
}: {
  active: "list" | "catalog";
  children: ReactNode;
}) {
  const { t } = useT();
  const nav = useNavigate();
  const { pathname } = useLocation();
  const servers = useMcpServers();
  const me = useAuthStore((s) => s.me);
  const activeOrg = me?.orgs.find((o) => o.id === me?.active_org_id);
  const workspaceLabel =
    activeOrg?.name?.toUpperCase() ?? t("connections.workspace.label");

  const items = [
    {
      id: "catalog" as const,
      label: t("connections.nav.browse"),
      icon: LayoutGrid,
      to: "/connections/catalog",
    },
    {
      id: "list" as const,
      label: t("connections.nav.my"),
      icon: LinkIcon,
      to: "/connections",
      badge: servers.data?.length ?? 0,
    },
  ];

  return (
    <div className="flex h-screen w-screen overflow-hidden bg-[var(--color-paper)]">
      <MenuRail />
      <aside
        className="flex h-full w-[240px] shrink-0 flex-col border-r border-[var(--color-line)] bg-[var(--color-paper-2)]"
        aria-label="Connections sidebar"
      >
        <div className="border-b border-[var(--color-line)] px-5 pt-5 pb-4">
          <div className="font-[var(--font-mono)] text-[10px] tracking-[0.14em] text-[var(--color-muted-foreground)] uppercase">
            {workspaceLabel}
          </div>
          <div className="mt-1.5 font-[var(--font-display)] text-[18px] font-bold text-[var(--color-ink)]">
            {t("connections.nav.title")}
          </div>
        </div>
        <nav className="flex flex-col gap-0.5 p-2">
          {items.map((it) => {
            const Icon = it.icon;
            const isActive = active === it.id || pathname === it.to;
            return (
              <button
                key={it.id}
                type="button"
                onClick={() => nav(it.to)}
                aria-current={isActive ? "page" : undefined}
                className={cn(
                  "group relative flex cursor-pointer items-center gap-2.5 px-3 py-2 text-left transition-colors duration-150 ease-out",
                  isActive
                    ? "bg-[var(--color-moss-tint)] text-[var(--color-moss-deep)]"
                    : "text-[var(--color-muted-foreground)] hover:text-[var(--color-ink)]",
                )}
              >
                {isActive ? (
                  <motion.span
                    layoutId="connections-nav-indicator"
                    className="absolute top-0 bottom-0 left-0 w-[2px] bg-[var(--color-moss)]"
                    transition={indicatorSpring}
                    aria-hidden
                  />
                ) : null}
                <Icon
                  className={cn(
                    "h-4 w-4 shrink-0 transition-colors duration-150 ease-out",
                    isActive
                      ? "text-[var(--color-moss)]"
                      : "text-[var(--color-fg-muted)]",
                  )}
                  strokeWidth={1.75}
                />
                <span
                  className={cn(
                    "min-w-0 flex-1 truncate text-[13px]",
                    isActive ? "font-medium" : "font-normal",
                  )}
                >
                  {it.label}
                </span>
                {typeof it.badge === "number" ? (
                  <span
                    className={cn(
                      "shrink-0 px-2 py-0.5 font-[var(--font-mono)] text-[10px]",
                      isActive
                        ? "bg-[var(--color-moss)] text-white"
                        : "bg-[var(--color-line)] text-[var(--color-muted-foreground)]",
                    )}
                  >
                    {it.badge}
                  </span>
                ) : null}
              </button>
            );
          })}
        </nav>
      </aside>
      <main className="flex min-w-0 flex-1 flex-col bg-[var(--color-card)]">
        <GlobalErrorBanner />
        {children}
      </main>
    </div>
  );
}

export function ConnectionsBreadcrumb({
  trail,
}: {
  trail: { label: string; current?: boolean }[];
}) {
  return (
    <div className="flex items-center gap-2 border-b border-transparent px-8 pt-4 pb-3 font-[var(--font-mono)] text-[11px] text-[var(--color-muted-foreground)]">
      {trail.map((step, i) => (
        <span key={`${step.label}-${i}`} className="flex items-center gap-2">
          <span
            className={cn(
              step.current && "font-semibold text-[var(--color-ink)]",
            )}
          >
            {step.label}
          </span>
          {i < trail.length - 1 ? <span aria-hidden>/</span> : null}
        </span>
      ))}
    </div>
  );
}
