import { FileText, Loader2, X } from "lucide-react";
import { cn } from "../../lib/utils";
import type { PendingAttachment } from "../../hooks/useAttachments";

/** Render the pending/uploaded attachments above a composer as removable
 *  chips: image thumbnails and file pills, each with status (issue #187). */
export function AttachmentBar({
  items,
  onRemove,
}: {
  items: PendingAttachment[];
  onRemove: (id: string) => void;
}) {
  if (items.length === 0) return null;
  return (
    <div className="flex flex-wrap gap-2 border-b border-[var(--color-line)] px-2 py-2">
      {items.map((it) => (
        <div
          key={it.id}
          className={cn(
            "group/att relative flex items-center gap-2 border bg-[var(--color-paper)] pl-2 pr-6 py-1.5 text-[12px]",
            it.status === "error"
              ? "border-[var(--color-rose)] text-[var(--color-rose)]"
              : "border-[var(--color-line-strong)]",
          )}
          title={it.error ? `${it.name} — ${it.error}` : it.name}
        >
          {it.previewUrl ? (
            <img
              src={it.previewUrl}
              alt={it.name}
              className={cn(
                "h-7 w-7 shrink-0 object-cover",
                it.status === "uploading" && "opacity-50",
              )}
            />
          ) : (
            <FileText className="h-4 w-4 shrink-0 opacity-70" />
          )}
          <span className="max-w-[10rem] truncate font-[var(--font-mono)]">
            {it.name}
          </span>
          {it.status === "uploading" ? (
            <Loader2 className="h-3.5 w-3.5 shrink-0 animate-spin opacity-70" />
          ) : null}
          <button
            type="button"
            onClick={() => onRemove(it.id)}
            aria-label={`Remove ${it.name}`}
            className="absolute right-1 top-1/2 -translate-y-1/2 p-0.5 opacity-60 hover:opacity-100"
          >
            <X className="h-3.5 w-3.5" />
          </button>
        </div>
      ))}
    </div>
  );
}
