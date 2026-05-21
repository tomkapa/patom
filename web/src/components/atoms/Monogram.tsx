import { cn, initials, toneById } from "../../lib/utils";
import { BrandIcon } from "./BrandIcon";

type Tone = "moss" | "amber" | "rose" | "ink" | "neutral" | "user";

const toneStyles: Record<Tone, string> = {
  moss: "bg-[var(--color-moss-soft)] text-[var(--color-moss-deep)] border-[var(--color-moss-soft)]",
  amber:
    "bg-[var(--color-amber-soft)] text-[var(--color-amber)] border-[var(--color-amber-soft)]",
  rose: "bg-[var(--color-rose-soft)] text-[var(--color-rose)] border-[var(--color-rose-soft)]",
  ink: "bg-[var(--color-rail)] text-[var(--color-paper)] border-[var(--color-rail)]",
  neutral:
    "bg-[var(--color-paper-2)] text-[var(--color-ink)] border-[var(--color-line)]",
  user: "bg-[var(--color-moss)] text-white border-[var(--color-moss)]",
};

export function Monogram({
  name,
  id,
  size = 28,
  tone,
  bg,
  fg,
  glyph,
  iconSlug,
  avatarUrl,
  className,
}: {
  name: string;
  id?: string;
  size?: number;
  tone?: Tone;
  /** Explicit background. Overrides `tone` palette — used for vendor-branded tiles. */
  bg?: string;
  /** Explicit foreground; pairs with `bg`. */
  fg?: string;
  /** Override the rendered text; defaults to `initials(name)`. */
  glyph?: string;
  /** simple-icons slug. When present the brand SVG icon is shown instead
   *  of a monogram character. Pairs with `bg` for the tile background. */
  iconSlug?: string;
  /** When set, render the image instead of the monogram glyph. Takes
   *  precedence over `iconSlug` / `glyph` / initials. */
  avatarUrl?: string | null;
  className?: string;
}) {
  const useExplicit = bg !== undefined;
  const t = tone ?? toneById(id);
  const iconSize = Math.round(size * 0.55);
  if (avatarUrl) {
    return (
      <img
        src={avatarUrl}
        alt={name}
        aria-label={name}
        className={cn("inline-block shrink-0 object-cover select-none", className)}
        style={{ width: size, height: size }}
      />
    );
  }
  return (
    <span
      className={cn(
        "inline-flex shrink-0 items-center justify-center border select-none",
        !useExplicit && toneStyles[t],
        !iconSlug &&
          "font-[var(--font-mono)] font-semibold uppercase tracking-tight",
        className,
      )}
      style={{
        width: size,
        height: size,
        fontSize: Math.round(size * 0.4),
        ...(useExplicit && { background: bg, color: fg, borderColor: bg }),
      }}
      aria-label={name}
    >
      {iconSlug ? (
        <BrandIcon slug={iconSlug} size={iconSize} />
      ) : (
        (glyph ?? initials(name))
      )}
    </span>
  );
}
