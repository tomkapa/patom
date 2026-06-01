import { useEffect, useState, type ReactNode } from "react";
import { useLocation } from "react-router-dom";
import { MenuRail } from "../organisms/MenuRail";
import { BottomTabBar } from "../organisms/BottomTabBar";
import { MobileTopBar } from "../organisms/MobileTopBar";
import { GlobalErrorBanner } from "../organisms/GlobalErrorBanner";
import { Drawer } from "../molecules/Drawer";
import { useIsCompact, useIsWide } from "../../hooks/useMediaQuery";

/**
 * The one responsive shell every screen renders through. It owns all the
 * breakpoint chrome so the four layout templates don't each reinvent it:
 *
 *   • expanded (≥ md): icon rail + inline `sidebar` + `main`, plus the
 *     chat `panel` as an inline fourth column at ≥ lg.
 *   • compact (< md): a mobile top bar (hamburger → sidebar drawer) +
 *     full-width `main` + bottom tab bar.
 *
 * The chat `panel` is an inline column only at ≥ lg; below that it slides
 * in as a right overlay so the message list keeps full width.
 */
export function AppFrame({
  sidebar,
  title,
  children,
  panel,
  panelOpen,
  onPanelClose,
}: {
  /** Contextual sidebar (channels, agent nav, …). Omit for screens that
   *  have none, e.g. the agents index — the hamburger then hides. */
  sidebar?: ReactNode;
  /** Title shown in the compact top bar. */
  title: ReactNode;
  children: ReactNode;
  /** Chat thread panel (chat only). */
  panel?: ReactNode;
  panelOpen?: boolean;
  onPanelClose?: () => void;
}) {
  const compact = useIsCompact();
  const wide = useIsWide();
  const [navOpen, setNavOpen] = useState(false);
  const { pathname } = useLocation();

  // Cross-section navigation (or any route change) dismisses the nav
  // drawer. Channel/agent picks — which don't change the route — are
  // handled by the drawer's inner-activate close.
  useEffect(() => setNavOpen(false), [pathname]);

  const panelOverlay =
    panel != null ? (
      <Drawer
        open={!!panelOpen}
        onClose={() => onPanelClose?.()}
        side="right"
        ariaLabel="Thread panel"
        panelClassName="w-full sm:w-[420px]"
      >
        {panel}
      </Drawer>
    ) : null;

  if (compact) {
    return (
      <div className="flex h-screen w-screen flex-col overflow-hidden bg-[var(--color-surface)]">
        <MobileTopBar
          title={title}
          onOpenSidebar={sidebar ? () => setNavOpen(true) : undefined}
        />
        <main className="flex min-w-0 flex-1 flex-col overflow-hidden">
          <GlobalErrorBanner />
          {children}
        </main>
        <BottomTabBar />
        {sidebar ? (
          <Drawer
            open={navOpen}
            onClose={() => setNavOpen(false)}
            side="left"
            closeOnInnerActivate
            ariaLabel="Navigation"
          >
            {sidebar}
          </Drawer>
        ) : null}
        {panelOverlay}
      </div>
    );
  }

  return (
    <div className="flex h-screen w-screen overflow-hidden bg-[var(--color-surface)]">
      <MenuRail />
      {sidebar}
      <main className="flex min-w-0 flex-1 flex-col">
        <GlobalErrorBanner />
        {children}
      </main>
      {wide ? (panelOpen ? panel : null) : panelOverlay}
    </div>
  );
}
