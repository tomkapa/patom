import { type ReactNode } from "react";
import { NavLink, useNavigate } from "react-router-dom";
import { motion } from "motion/react";
import {
  BarChart3,
  Brain,
  Check,
  ChevronsUpDown,
  Settings2,
  Shield,
} from "lucide-react";
import { MenuRail } from "../organisms/MenuRail";
import { GlobalErrorBanner } from "../organisms/GlobalErrorBanner";
import { Monogram } from "../atoms/Monogram";
import { Dropdown } from "../molecules/Dropdown";
import { useT } from "../../i18n";
import { cn } from "../../lib/utils";
import { indicatorSpring } from "../../lib/motion";
import { useAgents } from "../../hooks/useAgents";
import type { Agent } from "../../types/api";

/** Per-agent settings sub-navigation. `general`, `tools`, and `memory`
 *  are real routes; `logs` is in the design but not yet built and stays
 *  `aria-disabled` so the sidebar still mirrors the design. */
type AgentNavId = "general" | "tools" | "memory" | "logs";

type NavItem = {
  id: AgentNavId;
  label: string;
  icon: typeof Shield;
  /** Sub-path under `/agents/:id/` — undefined for disabled items. */
  to?: string;
  disabled?: boolean;
};

export function AgentLayout({
  agent,
  active,
  children,
}: {
  agent: Agent | null;
  active: AgentNavId;
  children: ReactNode;
}) {
  const { t } = useT();

  const navItems: NavItem[] = [
    {
      id: "general",
      label: t("agent.detail.nav.general"),
      icon: Settings2,
      to: "general",
    },
    {
      id: "tools",
      label: t("agent.detail.nav.tools"),
      icon: Shield,
      to: "tools",
    },
    {
      id: "memory",
      label: t("agent.detail.nav.memory"),
      icon: Brain,
      to: "memory",
    },
    {
      id: "logs",
      label: t("agent.detail.nav.logs"),
      icon: BarChart3,
      disabled: true,
    },
  ];

  return (
    <div className="flex h-screen w-screen overflow-hidden bg-[var(--color-paper)]">
      <MenuRail />
      <aside
        className="flex h-full w-[260px] shrink-0 flex-col border-r border-[var(--color-line)] bg-[var(--color-paper-2)]"
        aria-label={t("agent.detail.nav.aria")}
      >
        <div className="border-b border-[var(--color-line)] px-5 pt-5 pb-4">
          <div className="font-[var(--font-mono)] text-[10px] tracking-[0.15em] text-[var(--color-muted)] uppercase">
            {t("agent.detail.nav.eyebrow")}
          </div>
          <AgentSwitcher current={agent} />
        </div>
        <nav className="flex flex-col gap-0.5 p-2">
          <div className="px-3 pt-2 pb-1 font-[var(--font-mono)] text-[10px] tracking-[0.15em] text-[var(--color-muted)] uppercase">
            {t("agent.detail.nav.section")}
          </div>
          {navItems.map((it) => {
            const Icon = it.icon;
            const isActive = active === it.id;
            const itemClass = cn(
              "group relative flex items-center gap-2.5 pl-3 pr-3 py-2 text-left transition-colors duration-150 ease-out",
              isActive
                ? "bg-[var(--color-moss-tint)] text-[var(--color-moss-deep)]"
                : it.disabled
                  ? "cursor-not-allowed text-[var(--color-muted-2)] opacity-60"
                  : "cursor-pointer text-[var(--color-muted)] hover:text-[var(--color-ink)]",
            );
            const indicator = isActive ? (
              <motion.span
                layoutId="agent-nav-indicator"
                className="absolute top-0 bottom-0 left-0 w-[2px] bg-[var(--color-moss)]"
                transition={indicatorSpring}
                aria-hidden
              />
            ) : null;
            const iconEl = (
              <Icon
                className={cn(
                  "h-4 w-4 shrink-0 transition-colors duration-150 ease-out",
                  isActive
                    ? "text-[var(--color-moss)]"
                    : "text-[var(--color-muted-2)]",
                )}
                strokeWidth={1.75}
              />
            );
            const label = (
              <span
                className={cn(
                  "min-w-0 flex-1 truncate text-[13px]",
                  isActive ? "font-medium" : "font-normal",
                )}
              >
                {it.label}
              </span>
            );
            if (it.disabled || !it.to || !agent) {
              return (
                <button
                  key={it.id}
                  type="button"
                  aria-current={isActive ? "page" : undefined}
                  aria-disabled={it.disabled ? "true" : undefined}
                  disabled={it.disabled || !agent}
                  className={itemClass}
                >
                  {indicator}
                  {iconEl}
                  {label}
                </button>
              );
            }
            return (
              <NavLink
                key={it.id}
                to={`/agents/${agent.id}/${it.to}`}
                aria-current={isActive ? "page" : undefined}
                className={itemClass}
              >
                {indicator}
                {iconEl}
                {label}
              </NavLink>
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

function AgentSwitcher({ current }: { current: Agent | null }) {
  const { t } = useT();
  const nav = useNavigate();
  const list = useAgents();
  const agents = list.data ?? [];

  return (
    <Dropdown
      rootClassName="mt-2"
      menuClassName="max-h-[60vh] overflow-y-auto border border-[var(--color-line)] bg-[var(--color-card)] py-1 shadow-md scroll-thin"
      renderTrigger={({ open, toggle }) => (
        <button
          type="button"
          aria-haspopup="listbox"
          aria-expanded={open}
          aria-label={t("agent.detail.switcher.aria")}
          onClick={toggle}
          className="flex w-full cursor-pointer items-center gap-2.5 text-left outline-none transition-colors duration-150 ease-out focus-visible:ring-1 focus-visible:ring-[var(--color-ink)]"
        >
          <Monogram
            name={current?.name ?? "—"}
            id={current?.id}
            size={32}
            tone="moss"
          />
          <div className="min-w-0 flex-1">
            <div className="truncate font-[var(--font-display)] text-[18px] leading-tight font-bold text-[var(--color-ink)]">
              {current?.name ?? "…"}
            </div>
          </div>
          <ChevronsUpDown
            className="h-4 w-4 shrink-0 text-[var(--color-muted)]"
            strokeWidth={1.75}
          />
        </button>
      )}
    >
      {({ close }) => (
        <ul role="listbox" aria-label={t("agent.detail.switcher.aria")}>
          {agents.length === 0 ? (
            <li className="px-3 py-2 text-[12.5px] text-[var(--color-muted)]">
              {t("agent.detail.switcher.empty")}
            </li>
          ) : (
            agents.map((a) => {
              const isActive = a.id === current?.id;
              return (
                <li key={a.id}>
                  <button
                    type="button"
                    role="option"
                    aria-selected={isActive}
                    onClick={() => {
                      close();
                      if (!isActive) nav(`/agents/${a.id}`);
                    }}
                    className="flex w-full cursor-pointer items-center gap-2.5 px-3 py-2 text-left transition-colors duration-100 ease-out hover:bg-[var(--color-paper-2)]"
                  >
                    <Monogram name={a.name} id={a.id} size={24} tone="moss" />
                    <div className="min-w-0 flex-1 truncate text-[13px] font-semibold text-[var(--color-ink)]">
                      {a.name}
                    </div>
                    {isActive ? (
                      <Check className="h-3.5 w-3.5 shrink-0 text-[var(--color-moss)]" />
                    ) : null}
                  </button>
                </li>
              );
            })
          )}
        </ul>
      )}
    </Dropdown>
  );
}

export function AgentBreadcrumb({
  trail,
}: {
  trail: { label: string; current?: boolean }[];
}) {
  return (
    <div className="flex items-center gap-2 px-8 pt-4 pb-3 font-[var(--font-mono)] text-[11px] text-[var(--color-muted)]">
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
