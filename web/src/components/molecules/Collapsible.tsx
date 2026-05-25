import { useState, type ReactNode } from "react";
import { ChevronRight } from "lucide-react";
import { cn } from "../../lib/utils";

export function Collapsible({
  trigger,
  children,
  defaultOpen = false,
  className,
}: {
  trigger: ReactNode;
  children: ReactNode;
  defaultOpen?: boolean;
  className?: string;
}) {
  const [open, setOpen] = useState(defaultOpen);
  return (
    <div className={cn("w-full", className)}>
      <button
        onClick={() => setOpen((v) => !v)}
        className="group flex w-full cursor-pointer items-center gap-1.5 text-left text-[12px] font-medium tracking-tight text-[var(--color-muted)] transition-colors duration-150 ease-out hover:text-[var(--color-ink)]"
        aria-expanded={open}
      >
        <ChevronRight
          className={cn(
            "h-3.5 w-3.5 transition-transform duration-200 ease-out",
            open && "rotate-90",
          )}
        />
        <span className="flex-1">{trigger}</span>
      </button>
      <div
        className={cn(
          "grid transition-[grid-template-rows,margin-top] duration-200 ease-out",
          open ? "mt-1.5 grid-rows-[1fr]" : "mt-0 grid-rows-[0fr]",
        )}
      >
        <div className="overflow-hidden">{children}</div>
      </div>
    </div>
  );
}
