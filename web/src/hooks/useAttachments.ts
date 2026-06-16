// Manage a composer's pending attachments (issue #187): client-side type/size
// gating, upload to `POST /uploads/attachment`, and the per-item status the UI
// renders as chips. Shared by the channel/DM composer and the thread reply.

import { useCallback, useEffect, useRef, useState } from "react";
import { api } from "../lib/api";
import { formatBytes, uuidv7 } from "../lib/utils";
import type { Attachment } from "../types/api";

const IMAGE_MIMES = ["image/png", "image/jpeg", "image/webp", "image/gif"];

/** Whether a wire mime is one we render inline as an image. Shared with the
 *  read-only `AttachmentList`. */
export function isImageMime(mime: string): boolean {
  return IMAGE_MIMES.includes(mime);
}
const FILE_MIMES = [
  "application/pdf",
  "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
  "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
];

// `application/*` MIME types the server already treats as text. Anything with a
// `text/` prefix is also text; everything else is detected by extension because
// browsers report an empty/unhelpful type for .md/.toml/etc.
const TEXT_APP_MIMES = [
  "application/json",
  "application/toml",
  "application/x-toml",
  "application/xml",
  "application/yaml",
  "application/x-yaml",
  "application/x-ndjson",
];
const TEXT_EXTENSIONS = [
  ".md", ".markdown", ".txt", ".text", ".log", ".json", ".toml", ".yaml",
  ".yml", ".csv", ".tsv", ".xml", ".ini", ".cfg", ".conf", ".env", ".rs",
  ".ts", ".tsx", ".js", ".jsx", ".py", ".go", ".java", ".rb", ".sh", ".sql",
  ".html", ".css",
];

function extensionOf(name: string): string {
  const dot = name.lastIndexOf(".");
  return dot >= 0 ? name.slice(dot).toLowerCase() : "";
}

function isTextFile(file: File): boolean {
  if (file.type.startsWith("text/")) return true;
  if (TEXT_APP_MIMES.includes(file.type)) return true;
  return TEXT_EXTENSIONS.includes(extensionOf(file.name));
}

/** Whether the server's mime allow-list already accepts `file.type`. */
function serverKnowsMime(file: File): boolean {
  return (
    isImageMime(file.type) ||
    FILE_MIMES.includes(file.type) ||
    file.type.startsWith("text/") ||
    TEXT_APP_MIMES.includes(file.type)
  );
}

/** `accept` attribute for the hidden file input. */
export const ATTACHMENT_ACCEPT = [
  ...IMAGE_MIMES,
  ...FILE_MIMES,
  "text/*",
  ...TEXT_APP_MIMES,
  ...TEXT_EXTENSIONS,
].join(",");

// Mirror the backend caps (crate `provider::limits`). The server re-validates,
// so these only give the user fast local feedback.
const MAX_IMAGE_BYTES = 10 * 1024 * 1024;
const MAX_FILE_BYTES = 32 * 1024 * 1024;
const MAX_ATTACHMENTS = 8;

export type AttachmentStatus = "uploading" | "done" | "error";

export type PendingAttachment = {
  /** Client id for list keys + removal (not the storage key). */
  id: string;
  name: string;
  mime: string;
  size: number;
  status: AttachmentStatus;
  /** Set when `status === "error"`. */
  error?: string;
  /** Set when `status === "done"`. */
  attachment?: Attachment;
  /** Object URL for an inline image preview; revoked on removal. */
  previewUrl?: string;
};

function rejectReason(file: File): string | null {
  const allowed =
    isImageMime(file.type) || FILE_MIMES.includes(file.type) || isTextFile(file);
  if (!allowed) return "unsupported file type";
  const cap = isImageMime(file.type) ? MAX_IMAGE_BYTES : MAX_FILE_BYTES;
  if (file.size > cap) {
    return `too large (max ${formatBytes(cap)})`;
  }
  return null;
}

/** Normalize a file for upload: text files whose browser-reported type the
 *  server wouldn't recognize (empty / `application/octet-stream` for .md,
 *  .toml, …) are re-stamped `text/plain` so the upload boundary accepts them. */
function normalizeForUpload(file: File): File {
  if (isTextFile(file) && !serverKnowsMime(file)) {
    return new File([file], file.name, { type: "text/plain" });
  }
  return file;
}

export function useAttachments() {
  const [items, setItems] = useState<PendingAttachment[]>([]);

  // Revoke any outstanding image-preview object URLs when the composer
  // unmounts (e.g. closing the thread panel) so they don't leak. A ref keeps
  // the cleanup current without re-subscribing the effect every render.
  const itemsRef = useRef(items);
  itemsRef.current = items;
  useEffect(
    () => () => {
      for (const it of itemsRef.current) {
        if (it.previewUrl) URL.revokeObjectURL(it.previewUrl);
      }
    },
    [],
  );

  const upload = useCallback(async (id: string, file: File) => {
    try {
      const attachment = await api.uploadAttachment(normalizeForUpload(file));
      setItems((prev) =>
        prev.map((it) =>
          it.id === id ? { ...it, status: "done", attachment } : it,
        ),
      );
    } catch (e) {
      const error = e instanceof Error ? e.message : "upload failed";
      setItems((prev) =>
        prev.map((it) => (it.id === id ? { ...it, status: "error", error } : it)),
      );
    }
  }, []);

  const addFiles = useCallback(
    (files: FileList | File[]) => {
      const incoming = Array.from(files);
      setItems((prev) => {
        const room = MAX_ATTACHMENTS - prev.length;
        const next: PendingAttachment[] = [];
        for (const file of incoming.slice(0, Math.max(0, room))) {
          const id = uuidv7();
          const reason = rejectReason(file);
          if (reason) {
            next.push({
              id,
              name: file.name,
              mime: file.type,
              size: file.size,
              status: "error",
              error: reason,
            });
            continue;
          }
          next.push({
            id,
            name: file.name,
            mime: file.type,
            size: file.size,
            status: "uploading",
            previewUrl: isImageMime(file.type)
              ? URL.createObjectURL(file)
              : undefined,
          });
          // Fire the upload outside the state updater.
          void upload(id, file);
        }
        return [...prev, ...next];
      });
    },
    [upload],
  );

  const remove = useCallback((id: string) => {
    setItems((prev) => {
      const gone = prev.find((it) => it.id === id);
      if (gone?.previewUrl) URL.revokeObjectURL(gone.previewUrl);
      return prev.filter((it) => it.id !== id);
    });
  }, []);

  const clear = useCallback(() => {
    setItems((prev) => {
      for (const it of prev) if (it.previewUrl) URL.revokeObjectURL(it.previewUrl);
      return [];
    });
  }, []);

  /** Successfully-uploaded references, in order — ready to submit. */
  const ready: Attachment[] = items
    .filter((it) => it.status === "done" && it.attachment)
    .map((it) => it.attachment as Attachment);

  /** Whether any upload is still in flight (send should wait / disable). */
  const busy = items.some((it) => it.status === "uploading");

  return { items, addFiles, remove, clear, ready, busy };
}
