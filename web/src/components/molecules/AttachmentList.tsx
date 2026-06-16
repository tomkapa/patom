import { FileText } from "lucide-react";
import { formatBytes } from "../../lib/utils";
import { isImageMime } from "../../hooks/useAttachments";
import type { Attachment } from "../../types/api";

/** Render a message's image/file attachments (issue #187): images as
 *  thumbnails, files as labelled pills. Each opens the stored object in a new
 *  tab. Read-only — the composer's editable variant is `AttachmentBar`. */
export function AttachmentList({ items }: { items: Attachment[] }) {
  if (items.length === 0) return null;
  return (
    <div className="flex flex-wrap gap-2 pt-0.5">
      {items.map((a) =>
        isImageMime(a.mime) ? (
          <a
            key={a.url}
            href={a.url}
            target="_blank"
            rel="noopener noreferrer"
            className="block"
            title={a.filename}
          >
            <img
              src={a.url}
              alt={a.filename}
              className="max-h-48 max-w-[16rem] border border-[var(--color-line-strong)] object-contain"
            />
          </a>
        ) : (
          <a
            key={a.url}
            href={a.url}
            target="_blank"
            rel="noopener noreferrer"
            className="flex items-center gap-2 border border-[var(--color-line-strong)] bg-[var(--color-paper)] px-2 py-1.5 text-[12px] hover:bg-[var(--color-paper-2)]"
            title={a.filename}
          >
            <FileText className="h-4 w-4 shrink-0 opacity-70" />
            <span className="max-w-[12rem] truncate font-[var(--font-mono)]">
              {a.filename}
            </span>
            <span className="shrink-0 text-[var(--color-fg-muted)]">
              {formatBytes(a.size)}
            </span>
          </a>
        ),
      )}
    </div>
  );
}
