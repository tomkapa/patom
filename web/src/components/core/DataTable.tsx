import type { ReactNode } from "react";
import { cn } from "../../lib/utils";

/**
 * Thin semantic-HTML wrapper around `<table>` that matches the
 * Pencil `shadcnDataTable` reusable: sticky monospace header, body
 * rows with hover, a divider per row, and an optional footer slot
 * for pagination. Width is 100% of the parent — the layout pads.
 */
export function DataTable({
  children,
  className,
  caption,
}: {
  children: ReactNode;
  className?: string;
  caption?: ReactNode;
}) {
  return (
    <div className={cn("w-full border border-[var(--color-line)] bg-[var(--color-card)]", className)}>
      <table className="w-full border-collapse text-left text-[13px]">
        {caption ? <caption className="sr-only">{caption}</caption> : null}
        {children}
      </table>
    </div>
  );
}

export function DataTableHead({ children }: { children: ReactNode }) {
  return (
    <thead className="border-b border-[var(--color-line)] bg-[var(--color-paper-2)]">
      {children}
    </thead>
  );
}

export function DataTableHeaderRow({ children }: { children: ReactNode }) {
  return <tr>{children}</tr>;
}

export function DataTableColumnHeader({
  children,
  align = "left",
  className,
}: {
  children: ReactNode;
  align?: "left" | "right";
  className?: string;
}) {
  return (
    <th
      scope="col"
      className={cn(
        "px-4 py-2.5 font-[var(--font-mono)] text-[10.5px] font-semibold tracking-[0.14em] text-[var(--color-muted)] uppercase",
        align === "right" ? "text-right" : "text-left",
        className,
      )}
    >
      {children}
    </th>
  );
}

export function DataTableBody({ children }: { children: ReactNode }) {
  return <tbody>{children}</tbody>;
}

export function DataTableRow({
  children,
  className,
}: {
  children: ReactNode;
  className?: string;
}) {
  return (
    <tr
      className={cn(
        "border-b border-[var(--color-line)] last:border-b-0 transition-colors hover:bg-[var(--color-paper-2)]",
        className,
      )}
    >
      {children}
    </tr>
  );
}

export function DataTableCell({
  children,
  align = "left",
  className,
}: {
  children: ReactNode;
  align?: "left" | "right";
  className?: string;
}) {
  return (
    <td
      className={cn(
        "px-4 py-3 align-middle text-[var(--color-ink)]",
        align === "right" ? "text-right" : "text-left",
        className,
      )}
    >
      {children}
    </td>
  );
}

export function DataTableFooter({ children }: { children: ReactNode }) {
  return (
    <tfoot className="border-t border-[var(--color-line)] bg-[var(--color-paper-2)]">
      <tr>
        <td colSpan={99} className="px-4 py-2.5">
          {children}
        </td>
      </tr>
    </tfoot>
  );
}

export function DataTableEmpty({
  children,
  cols = 99,
}: {
  children: ReactNode;
  cols?: number;
}) {
  return (
    <tr>
      <td
        colSpan={cols}
        className="px-4 py-10 text-center text-[13px] text-[var(--color-muted)]"
      >
        {children}
      </td>
    </tr>
  );
}
