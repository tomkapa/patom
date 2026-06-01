import type { ReactNode } from "react";
import { AppFrame } from "./AppFrame";

/**
 * Chat screen layout — a thin adapter over `AppFrame`. The thread `panel`
 * is an inline fourth column on wide screens and a right overlay below
 * `lg`; `AppFrame` decides which based on the viewport.
 */
export function ChatLayout({
  sidebar,
  main,
  panel,
  panelOpen,
  onPanelClose,
  title,
}: {
  sidebar: ReactNode;
  main: ReactNode;
  panel: ReactNode;
  panelOpen: boolean;
  onPanelClose: () => void;
  title: ReactNode;
}) {
  return (
    <AppFrame
      sidebar={sidebar}
      title={title}
      panel={panel}
      panelOpen={panelOpen}
      onPanelClose={onPanelClose}
    >
      {main}
    </AppFrame>
  );
}
