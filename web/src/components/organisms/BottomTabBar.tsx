import { motion } from "motion/react";
import { useLocation, useNavigate } from "react-router-dom";
import { cn } from "../../lib/utils";
import { indicatorSpring } from "../../lib/motion";
import { useNavItems } from "../../lib/nav";

/**
 * Compact-viewport replacement for the desktop `MenuRail`: a bottom tab
 * bar over the four primary destinations. Rendered as a flex child at
 * the end of the compact column (not `fixed`), so content above it —
 * e.g. the chat composer — never sits underneath. Hidden at `md`+ where
 * the rail takes over.
 */
export function BottomTabBar() {
  const nav = useNavigate();
  const { pathname } = useLocation();
  const items = useNavItems();

  return (
    <nav
      className="flex shrink-0 items-stretch border-t border-[var(--color-line)] bg-[var(--color-rail-brand)] pb-[env(safe-area-inset-bottom)] md:hidden"
      aria-label="Primary"
    >
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
              "relative flex flex-1 cursor-pointer flex-col items-center gap-1 py-2 transition-colors duration-150 ease-out",
              active ? "text-white" : "text-white/70 hover:text-white",
            )}
          >
            {active ? (
              <motion.span
                layoutId="bottom-tab-active"
                className="absolute inset-x-3 top-0 h-0.5 bg-white"
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
    </nav>
  );
}
