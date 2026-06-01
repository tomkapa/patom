import type { ReactNode } from "react";
import { Menu } from "lucide-react";
import { useT } from "../../i18n";

/**
 * Compact-viewport header. Holds the hamburger that opens the contextual
 * sidebar drawer, the current screen title, and an optional right slot.
 * Hidden at `md`+ where the inline sidebar makes a toggle unnecessary.
 */
export function MobileTopBar({
  title,
  onOpenSidebar,
  right,
}: {
  title: ReactNode;
  /** Omit when the screen has no contextual sidebar (e.g. the agents
   *  index) — the hamburger is replaced by a spacer to keep alignment. */
  onOpenSidebar?: () => void;
  right?: ReactNode;
}) {
  const { t } = useT();
  return (
    <header className="flex h-12 shrink-0 items-center gap-2 border-b border-[var(--color-line)] bg-[var(--color-paper)] px-2 md:hidden">
      {onOpenSidebar ? (
        <button
          type="button"
          onClick={onOpenSidebar}
          aria-label={t("nav.aria.openMenu")}
          className="flex h-9 w-9 shrink-0 cursor-pointer items-center justify-center text-[var(--color-fg-secondary)] transition-colors hover:text-[var(--color-ink)]"
        >
          <Menu className="h-5 w-5" strokeWidth={1.75} />
        </button>
      ) : (
        <span className="h-9 w-2 shrink-0" aria-hidden />
      )}
      <div className="min-w-0 flex-1 truncate font-[var(--font-display)] text-[15px] font-bold text-[var(--color-ink)]">
        {title}
      </div>
      {right ? <div className="flex shrink-0 items-center">{right}</div> : null}
    </header>
  );
}
