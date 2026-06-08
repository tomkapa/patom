// Drag-to-resize width for an edge-docked panel (e.g. the thread side
// panel). The panel sits on the right, so its grab handle lives on the
// LEFT edge: dragging left widens, dragging right narrows.
//
// Width is clamped to [min, maxFraction × viewport] and persisted to
// localStorage so the choice survives reloads. The max is re-derived on
// viewport resize, and the stored width re-clamped, so a shrinking window
// never leaves the panel wider than the cap.

import { useCallback, useEffect, useRef, useState } from "react";

function readStored(key: string, fallback: number): number {
  if (typeof window === "undefined") return fallback;
  const raw = window.localStorage.getItem(key);
  if (raw === null) return fallback;
  const parsed = Number.parseInt(raw, 10);
  return Number.isFinite(parsed) ? parsed : fallback;
}

function clamp(value: number, min: number, max: number): number {
  return Math.min(Math.max(value, min), Math.max(min, max));
}

export function useResizableWidth({
  storageKey,
  defaultWidth,
  minWidth,
  maxFraction,
}: {
  storageKey: string;
  defaultWidth: number;
  minWidth: number;
  /** Upper bound as a fraction of the viewport width, e.g. 0.5 for ~50%. */
  maxFraction: number;
}) {
  const maxWidth = () =>
    typeof window === "undefined"
      ? defaultWidth
      : Math.round(window.innerWidth * maxFraction);

  const [width, setWidth] = useState(() =>
    clamp(readStored(storageKey, defaultWidth), minWidth, maxWidth()),
  );
  const [dragging, setDragging] = useState(false);

  // Re-clamp when the viewport shrinks so the panel can't exceed the cap.
  useEffect(() => {
    const onResize = () =>
      setWidth((w) => clamp(w, minWidth, maxWidth()));
    window.addEventListener("resize", onResize);
    return () => window.removeEventListener("resize", onResize);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [minWidth, maxFraction, defaultWidth]);

  const dragState = useRef<{ startX: number; startWidth: number } | null>(null);

  const onHandlePointerDown = useCallback(
    (e: React.PointerEvent) => {
      e.preventDefault();
      dragState.current = { startX: e.clientX, startWidth: width };
      setDragging(true);
      e.currentTarget.setPointerCapture(e.pointerId);
    },
    [width],
  );

  const onHandlePointerMove = useCallback(
    (e: React.PointerEvent) => {
      const start = dragState.current;
      if (!start) return;
      // Handle is on the left edge: leftward drag (clientX down) widens.
      const next = start.startWidth + (start.startX - e.clientX);
      setWidth(clamp(next, minWidth, maxWidth()));
    },
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [minWidth, maxFraction],
  );

  const endDrag = useCallback(
    (e: React.PointerEvent) => {
      if (!dragState.current) return;
      dragState.current = null;
      setDragging(false);
      e.currentTarget.releasePointerCapture(e.pointerId);
      if (typeof window !== "undefined") {
        window.localStorage.setItem(storageKey, String(width));
      }
    },
    [storageKey, width],
  );

  return {
    width,
    dragging,
    handleProps: {
      onPointerDown: onHandlePointerDown,
      onPointerMove: onHandlePointerMove,
      onPointerUp: endDrag,
      onPointerCancel: endDrag,
    },
  };
}
