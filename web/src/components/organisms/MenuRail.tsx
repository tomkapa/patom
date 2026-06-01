import { Bot, House, Plug, Settings } from "lucide-react";
import { motion } from "motion/react";
import { useLocation, useNavigate } from "react-router-dom";
import { cn } from "../../lib/utils";
import { useT } from "../../i18n";
import { useAuthStore } from "../../stores/authStore";
import { useActiveOrg } from "../../hooks/useMe";
import { indicatorSpring } from "../../lib/motion";
import { UserMenu } from "./UserMenu";
import appLogoUrl from "../../../assets/favicon-192.png";

type MenuItem = {
  id: string;
  label: string;
  icon: typeof House;
  /** Match against the current pathname prefix to highlight as active. */
  match: (pathname: string) => boolean;
  to: string;
};

export function MenuRail() {
  const nav = useNavigate();
  const { pathname } = useLocation();
  const { t } = useT();
  const me = useAuthStore((s) => s.me);
  const activeOrg = useActiveOrg();
  const workspaceLogo = activeOrg?.avatar_url ?? appLogoUrl;
  const workspaceName = activeOrg?.name ?? "Patom";

  const items: MenuItem[] = [
    {
      id: "home",
      label: "Home",
      icon: House,
      match: (p) => p === "/" || p.startsWith("/threads") || p.startsWith("/c/"),
      to: "/",
    },
    {
      id: "agent",
      label: "Agent",
      icon: Bot,
      match: (p) => p.startsWith("/agents"),
      to: "/agents",
    },
    {
      id: "connections",
      label: t("menu.connections"),
      icon: Plug,
      match: (p) => p.startsWith("/connections"),
      to: "/connections",
    },
    {
      id: "settings",
      label: t("menu.settings"),
      icon: Settings,
      match: (p) => p.startsWith("/settings"),
      to: "/settings",
    },
  ];

  return (
    <aside
      className="flex h-full w-[72px] shrink-0 flex-col items-center gap-1.5 bg-[var(--color-rail-brand)] p-2"
      aria-label="Menu rail"
    >
      <img
        src={workspaceLogo}
        alt={workspaceName}
        aria-label={workspaceName}
        className="h-9 w-9 shrink-0 object-cover select-none"
      />

      <div className="my-0.5 h-px w-6 bg-white/20" aria-hidden />

      {items.map((item) => {
        const Icon = item.icon;
        const active = item.match(pathname);
        return (
          <button
            key={item.id}
            type="button"
            onClick={() => nav(item.to)}
            aria-current={active ? "page" : undefined}
            className={cn(
              "relative flex w-full cursor-pointer flex-col items-center gap-1 px-2 py-1 transition-colors duration-150 ease-out",
              active
                ? "text-white"
                : "text-white/80 hover:bg-white/5 hover:text-white",
            )}
          >
            {active ? (
              <motion.span
                layoutId="menu-rail-active"
                className="absolute inset-0 bg-white/10"
                transition={indicatorSpring}
                aria-hidden
              />
            ) : null}
            <Icon className="relative h-5 w-5" strokeWidth={1.75} />
            <span className="relative font-sans text-[11px] font-medium leading-none">
              {item.label}
            </span>
          </button>
        );
      })}

      {me ? (
        <div className="mt-auto flex w-full justify-center pt-2">
          <UserMenu />
        </div>
      ) : null}
    </aside>
  );
}
