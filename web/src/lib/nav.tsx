// Single source of truth for the four primary destinations (Home,
// Agent, Connections, Settings). Both the desktop `MenuRail` and the
// compact `BottomTabBar` render from this list, so the two can never
// drift on labels, icons, or active-match logic.

import { Bot, House, Plug, Settings } from "lucide-react";
import { useT } from "../i18n";

export type NavItem = {
  id: string;
  label: string;
  icon: typeof House;
  /** Match against the current pathname to highlight as active. */
  match: (pathname: string) => boolean;
  to: string;
};

/** The primary nav items, labelled in the active language. */
export function useNavItems(): NavItem[] {
  const { t } = useT();
  return [
    {
      id: "home",
      label: t("menu.home"),
      icon: House,
      match: (p) => p === "/" || p.startsWith("/threads") || p.startsWith("/c/"),
      to: "/",
    },
    {
      id: "agent",
      label: t("menu.agent"),
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
}
