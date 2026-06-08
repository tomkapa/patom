// Drag-to-resize width for an edge-docked panel (e.g. the thread side
// panel). The panel sits on the right, so its grab handle lives on the
// LEFT edge: dragging left widens, dragging right narrows.
//
// During a drag the width is written straight to the panel node's style
// (via `panelRef`) so the host component doesn't re-render on every
// pointer move — only pointer-up commits to React state and persists.
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

// Clamp with the floor winning ties: when the cap drops below the floor
// (a viewport too narrow for `minWidth`), `min` is returned, not `max`.
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
  const getMaxWidth = useCallback(
    () =>
      typeof window === "undefined"
        ? defaultWidth
        : Math.round(window.innerWidth * maxFraction),
    [defaultWidth, maxFraction],
  );

  const [width, setWidth] = useState(() =>
    clamp(readStored(storageKey, defaultWidth), minWidth, getMaxWidth()),
  );
  const [dragging, setDragging] = useState(false);

  // The element to resize, and the latest width during an in-flight drag
  // (kept in a ref so pointer handlers stay stable and re-render-free).
  const panelRef = useRef<HTMLElement | null>(null);
  const liveWidth = useRef(width);
  useEffect(() => {
    liveWidth.current = width;
  }, [width]);

  const dragState = useRef<{ startX: number; startWidth: number } | null>(null);

  // Re-clamp when the viewport shrinks so the panel can't exceed the cap.
  useEffect(() => {
    const onResize = () => setWidth((w) => clamp(w, minWidth, getMaxWidth()));
    window.addEventListener("resize", onResize);
    return () => window.removeEventListener("resize", onResize);
  }, [minWidth, getMaxWidth]);

  const onPointerDown = useCallback((e: React.PointerEvent) => {
    e.preventDefault();
    dragState.current = { startX: e.clientX, startWidth: liveWidth.current };
    setDragging(true);
    e.currentTarget.setPointerCapture(e.pointerId);
  }, []);

  const onPointerMove = useCallback(
    (e: React.PointerEvent) => {
      const start = dragState.current;
      if (!start) return;
      // Handle is on the left edge: leftward drag (clientX down) widens.
      const next = clamp(
        start.startWidth + (start.startX - e.clientX),
        minWidth,
        getMaxWidth(),
      );
      liveWidth.current = next;
      // Imperative write — no React render while dragging.
      const el = panelRef.current;
      if (el) el.style.width = `${next}px`;
    },
    [minWidth, getMaxWidth],
  );

  const endDrag = useCallback(
    (e: React.PointerEvent) => {
      if (!dragState.current) return;
      dragState.current = null;
      setDragging(false);
      e.currentTarget.releasePointerCapture(e.pointerId);
      setWidth(liveWidth.current);
      if (typeof window !== "undefined") {
        window.localStorage.setItem(storageKey, String(liveWidth.current));
      }
    },
    [storageKey],
  );

  return {
    width,
    dragging,
    panelRef,
    handleProps: {
      onPointerDown,
      onPointerMove,
      onPointerUp: endDrag,
      onPointerCancel: endDrag,
    },
  };
}
