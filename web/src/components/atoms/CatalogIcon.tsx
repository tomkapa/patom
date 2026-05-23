import { useEffect, useState } from "react";
import { Monogram } from "./Monogram";
import { cn } from "../../lib/utils";

/** Vendor tile icon for an MCP catalog entry. Renders `iconUrl` directly
 *  (R2-hosted SVG) when present; falls back to a neutral monogram of the
 *  display name's first letter when the URL is missing or 404s. */
export function CatalogIcon({
  name,
  iconUrl,
  size = 36,
  className,
}: {
  name: string;
  iconUrl?: string | null;
  size?: number;
  className?: string;
}) {
  const [failed, setFailed] = useState(false);
  // Reset on URL change so a previous 404 doesn't suppress a different URL.
  useEffect(() => setFailed(false), [iconUrl]);
  if (iconUrl && !failed) {
    return (
      <img
        src={iconUrl}
        alt=""
        onError={() => setFailed(true)}
        className={cn(
          "shrink-0 border border-[var(--color-line)] bg-white object-contain p-1",
          className,
        )}
        style={{ width: size, height: size }}
      />
    );
  }
  return (
    <Monogram
      name={name}
      size={size}
      tone="neutral"
      glyph={(name[0] ?? "?").toUpperCase()}
      className={className}
    />
  );
}
