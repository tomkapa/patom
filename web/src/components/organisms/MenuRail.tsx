import { motion } from "motion/react";
import { useLocation, useNavigate } from "react-router-dom";
import { cn } from "../../lib/utils";
import { useAuthStore } from "../../stores/authStore";
import { indicatorSpring } from "../../lib/motion";
import { useNavItems } from "../../lib/nav";
import { OrgSwitcher } from "./OrgSwitcher";
import { UserMenu } from "./UserMenu";

export function MenuRail() {
  const nav = useNavigate();
  const { pathname } = useLocation();
  const me = useAuthStore((s) => s.me);

  const items = useNavItems();

  return (
    <aside
      className="hidden h-full w-[72px] shrink-0 flex-col items-center gap-1.5 bg-[var(--color-rail-brand)] p-2 md:flex"
      aria-label="Menu rail"
    >
      <OrgSwitcher />

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
