import { useRef, useState } from "react";
import { Upload, X } from "lucide-react";
import { Button } from "../atoms/Button";
import { Spinner } from "../atoms/Spinner";
import { cn, formatBytes } from "../../lib/utils";

/** Allowed image MIME types for the input picker. Mirrors the per-kind
 *  allow-list enforced server-side in `src/assets/traits.rs`. SVG is
 *  filtered out for the avatar kind by the parent (we mirror that here
 *  via the `accept` prop). */
const ACCEPT_AVATAR = "image/png,image/jpeg,image/webp";
const ACCEPT_CATALOG = "image/png,image/jpeg,image/webp,image/svg+xml";

type Kind = "avatar" | "catalog-icon";

type Props = {
  /** Which uploader: drives the `accept` filter + size cap label. */
  kind: Kind;
  /** Current image URL (null when none). Renders as preview. */
  currentUrl: string | null;
  /** Pixel size of the preview tile. */
  size?: number;
  /** Async upload callback. Resolve to the new URL; reject to surface
   *  an inline error. */
  onUpload: (file: File) => Promise<string>;
  /** Optional fallback content (initials / monogram) shown when no
   *  image is set. */
  fallback?: React.ReactNode;
  /** Override the maximum byte cap shown in the limit hint. Defaults
   *  per `kind`. */
  maxBytes?: number;
  className?: string;
};

const DEFAULT_AVATAR_BYTES = 2 * 1024 * 1024;
const DEFAULT_CATALOG_BYTES = 256 * 1024;

/** Small upload affordance: shows current image (or fallback), a
 *  "Change" button that opens the OS file picker, and an inline error
 *  band when the upload fails. Client-side size check mirrors the
 *  backend cap so the user sees the failure before bytes hit the wire. */
export function ImageUploader({
  kind,
  currentUrl,
  size = 64,
  onUpload,
  fallback,
  maxBytes,
  className,
}: Props) {
  const inputRef = useRef<HTMLInputElement | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const accept = kind === "avatar" ? ACCEPT_AVATAR : ACCEPT_CATALOG;
  const cap =
    maxBytes ?? (kind === "avatar" ? DEFAULT_AVATAR_BYTES : DEFAULT_CATALOG_BYTES);

  async function handleFile(file: File) {
    setError(null);
    if (file.size > cap) {
      setError(`File too large — max ${formatBytes(cap)}`);
      return;
    }
    setBusy(true);
    try {
      await onUpload(file);
    } catch (e) {
      setError(e instanceof Error ? e.message : "Upload failed");
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className={cn("flex items-start gap-3", className)}>
      <div
        className="relative flex shrink-0 items-center justify-center overflow-hidden border border-[var(--color-line)] bg-[var(--color-paper-2)]"
        style={{ width: size, height: size }}
      >
        {currentUrl ? (
          <img
            src={currentUrl}
            alt=""
            className="h-full w-full object-cover"
            onError={() => setError("Image failed to load")}
          />
        ) : (
          (fallback ?? (
            <span className="font-[var(--font-mono)] text-[10px] tracking-[0.1em] text-[var(--color-muted-foreground)] uppercase">
              none
            </span>
          ))
        )}
        {busy ? (
          <div className="absolute inset-0 flex items-center justify-center bg-[var(--color-paper)]/70">
            <Spinner size={16} />
          </div>
        ) : null}
      </div>
      <div className="flex min-w-0 flex-1 flex-col gap-1.5">
        <div className="flex flex-wrap items-center gap-2">
          <input
            ref={inputRef}
            type="file"
            accept={accept}
            className="hidden"
            onChange={(e) => {
              const file = e.target.files?.[0];
              // Reset the input so picking the same file again still
              // fires `change`.
              e.target.value = "";
              if (file) void handleFile(file);
            }}
          />
          <Button
            variant="ghost"
            size="sm"
            disabled={busy}
            onClick={() => inputRef.current?.click()}
          >
            <Upload className="h-3 w-3" />
            {currentUrl ? "Change" : "Upload"}
          </Button>
          <span className="font-[var(--font-mono)] text-[10.5px] tracking-[0.08em] text-[var(--color-muted-foreground)] uppercase">
            {accept.replace(/image\//g, "").replace(/,/g, " ")} · {formatBytes(cap)}
          </span>
        </div>
        {error ? (
          <div className="flex items-center gap-1.5 text-[12px] text-[var(--color-rose)]">
            <X className="h-3 w-3" />
            <span>{error}</span>
          </div>
        ) : null}
      </div>
    </div>
  );
}
