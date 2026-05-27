import { forwardRef } from "react";
import type { ButtonHTMLAttributes, ReactNode } from "react";
import { cn } from "../../lib/utils";
import { useT } from "../../i18n";
import { Spinner } from "./Spinner";

type Variant = "primary" | "moss" | "ghost" | "danger";
type Size = "xxs" | "xs" | "sm" | "md";

type BaseProps = {
  variant?: Variant;
  size?: Size;
  loading?: boolean;
  iconOnly?: boolean;
  className?: string;
  children?: ReactNode;
} & Omit<ButtonHTMLAttributes<HTMLButtonElement>, "children" | "className">;

const VARIANT: Record<Variant, string> = {
  primary:
    "bg-[var(--color-ink)] text-[var(--color-paper)] border border-[var(--color-ink)] hover:bg-[var(--color-ink-2)] disabled:opacity-50",
  moss: "bg-[var(--color-moss)] text-white border border-[var(--color-moss)] hover:bg-[var(--color-moss-deep)] disabled:cursor-not-allowed disabled:opacity-40",
  ghost:
    "bg-transparent text-[var(--color-muted-foreground)] border border-transparent hover:text-[var(--color-ink)] hover:bg-[var(--color-paper-2)] disabled:opacity-40",
  danger:
    "bg-transparent text-[var(--color-rose)] border border-transparent hover:bg-[var(--color-rose-soft)] disabled:opacity-50",
};

const SIZE_LABEL: Record<Size, string> = {
  xxs: "h-5 px-1.5 text-[10.5px]",
  xs: "h-6 px-2 text-[11px]",
  sm: "h-7 px-2 text-[11.5px]",
  md: "h-[34px] px-3 text-[12px]",
};

const SIZE_ICON: Record<Size, string> = {
  xxs: "h-5 w-5",
  xs: "h-6 w-6",
  sm: "h-7 w-7",
  md: "h-[34px] w-[34px]",
};

export const Button = forwardRef<HTMLButtonElement, BaseProps>(function Button(
  {
    variant = "ghost",
    size = "md",
    loading = false,
    iconOnly = false,
    disabled,
    className,
    children,
    type = "button",
    ...rest
  },
  ref,
) {
  const { t } = useT();
  const sizing = iconOnly ? SIZE_ICON[size] : SIZE_LABEL[size];
  return (
    <button
      ref={ref}
      type={type}
      disabled={disabled || loading}
      aria-busy={loading || undefined}
      className={cn(
        "inline-flex cursor-pointer items-center justify-center gap-1.5 font-[var(--font-mono)] uppercase tracking-[0.06em] transition-[background-color,color,border-color,box-shadow,transform] duration-150 ease-out outline-none active:scale-[0.97] focus-visible:ring-1 focus-visible:ring-[var(--color-ink)] disabled:cursor-not-allowed disabled:active:scale-100",
        sizing,
        VARIANT[variant],
        className,
      )}
      {...rest}
    >
      {loading ? (
        <>
          <Spinner size={12} />
          <span className="sr-only">{t("button.loading")}</span>
        </>
      ) : (
        children
      )}
    </button>
  );
});
