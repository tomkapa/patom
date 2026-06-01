import type { ReactNode } from "react";
import { cn } from "../../lib/utils";

/**
 * Horizontal-scroll wrapper for wide tables. Our tables — both `<table>`
 * and hand-rolled CSS grids — use fixed/`minmax` column tracks that don't
 * fit a phone; without this they crush and overlap their columns. The
 * wrapper scrolls instead: `minWidth` is the floor below which the inner
 * content stops shrinking and the viewport scrolls horizontally.
 */
export function TableScroll({
  minWidth,
  className,
  children,
}: {
  /** Minimum content width in px before horizontal scroll kicks in. */
  minWidth: number;
  /** Applied to the scroll viewport (e.g. the table's border/background). */
  className?: string;
  children: ReactNode;
}) {
  return (
    <div className={cn("w-full overflow-x-auto scroll-thin", className)}>
      <div style={{ minWidth }}>{children}</div>
    </div>
  );
}
