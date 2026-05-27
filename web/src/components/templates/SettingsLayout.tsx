import type { ReactNode } from "react";
import { NavLink } from "react-router-dom";
import { motion } from "motion/react";
import {
  Bell,
  Blocks,
  CreditCard,
  Settings2,
  Users,
} from "lucide-react";
import { MenuRail } from "../organisms/MenuRail";
import { GlobalErrorBanner } from "../organisms/GlobalErrorBanner";
import { useT } from "../../i18n";
import { cn } from "../../lib/utils";
import { indicatorSpring } from "../../lib/motion";
import { useAuthStore } from "../../stores/authStore";

export type SettingsNavId =
  | "general"
  | "members"
  | "billing"
  | "integrations"
  | "notifications";

type NavItem = {
  id: SettingsNavId;
  label: string;
  icon: typeof Settings2;
  to: string;
};

export function SettingsLayout({
  active,
  children,
}: {
  active: SettingsNavId;
  children: ReactNode;
}) {
  const { t } = useT();
  const me = useAuthStore((s) => s.me);
  const activeOrg = me?.orgs.find((o) => o.id === me?.active_org_id);
  const workspaceLabel = activeOrg?.name?.toUpperCase() ?? t("settings.workspace.eyebrow");

  const items: NavItem[] = [
    {
      id: "general",
      label: t("settings.nav.general"),
      icon: Settings2,
      to: "/settings/general",
    },
    {
      id: "members",
      label: t("settings.nav.members"),
      icon: Users,
      to: "/settings/members",
    },
    {
      id: "billing",
      label: t("settings.nav.billing"),
      icon: CreditCard,
      to: "/settings/billing",
    },
    {
      id: "integrations",
      label: t("settings.nav.integrations"),
      icon: Blocks,
      to: "/settings/integrations",
    },
    {
      id: "notifications",
      label: t("settings.nav.notifications"),
      icon: Bell,
      to: "/settings/notifications",
    },
  ];

  return (
    <div className="flex h-screen w-screen overflow-hidden bg-[var(--color-paper)]">
      <MenuRail />
      <aside
        className="flex h-full w-[240px] shrink-0 flex-col border-r border-[var(--color-line)] bg-[var(--color-paper-2)]"
        aria-label="Workspace settings sidebar"
      >
        <div className="border-b border-[var(--color-line)] px-5 pt-5 pb-4">
          <div className="font-[var(--font-mono)] text-[10px] tracking-[0.14em] text-[var(--color-muted)] uppercase">
            {workspaceLabel}
          </div>
          <div className="mt-1.5 font-[var(--font-display)] text-[18px] font-bold text-[var(--color-ink)]">
            {t("settings.nav.title")}
          </div>
        </div>
        <nav className="flex flex-col gap-0.5 p-2">
          {items.map((it) => {
            const Icon = it.icon;
            const isActive = active === it.id;
            return (
              <NavLink
                key={it.id}
                to={it.to}
                aria-current={isActive ? "page" : undefined}
                className={cn(
                  "group relative flex items-center gap-2.5 px-3 py-2 text-left transition-colors duration-150 ease-out",
                  isActive
                    ? "bg-[var(--color-moss-tint)] text-[var(--color-moss-deep)]"
                    : "cursor-pointer text-[var(--color-muted)] hover:text-[var(--color-ink)]",
                )}
              >
                {isActive ? (
                  <motion.span
                    layoutId="settings-nav-indicator"
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
                      : "text-[var(--color-muted-2)]",
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

export function SettingsBreadcrumb({
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

/** Shared title + subtitle + CTA bar used at the top of each settings
 *  page (General, Members). Keeping the markup in one place stops the
 *  two pages from drifting on padding / typography. */
export function SettingsPageHeader({
  title,
  subtitle,
  right,
}: {
  title: string;
  subtitle: string;
  right?: ReactNode;
}) {
  return (
    <header className="flex items-end justify-between gap-4 border-b border-[var(--color-line)] px-8 pt-2 pb-6">
      <div className="min-w-0">
        <h1 className="font-[var(--font-display)] text-[32px] leading-tight font-bold text-[var(--color-ink)]">
          {title}
        </h1>
        <p className="mt-1 max-w-[60ch] text-[14px] text-[var(--color-muted)]">
          {subtitle}
        </p>
      </div>
      {right ? (
        <div className="flex shrink-0 items-center gap-3">{right}</div>
      ) : null}
    </header>
  );
}
