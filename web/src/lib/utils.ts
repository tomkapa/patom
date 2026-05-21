import { clsx, type ClassValue } from "clsx";
import { twMerge } from "tailwind-merge";

export function cn(...inputs: ClassValue[]) {
  return twMerge(clsx(inputs));
}

export function uuidv7(): string {
  // Minimal UUIDv7. ts(48) | ver(4) | rand_a(12) | var(2) | rand_b(62)
  const ts = BigInt(Date.now());
  const tsHex = ts.toString(16).padStart(12, "0");

  const rand = new Uint8Array(10);
  crypto.getRandomValues(rand);

  // version 7 in high nibble of byte 6 (rand[0])
  rand[0] = (rand[0]! & 0x0f) | 0x70;
  // variant 10 in high bits of byte 8 (rand[2])
  rand[2] = (rand[2]! & 0x3f) | 0x80;

  const hex = Array.from(rand, (b) => b.toString(16).padStart(2, "0")).join("");
  return [
    tsHex.slice(0, 8),
    tsHex.slice(8, 12),
    hex.slice(0, 4),
    hex.slice(4, 8),
    hex.slice(8, 20),
  ].join("-");
}

export function shortId(id: string, n = 6): string {
  return id.replace(/-/g, "").slice(0, n);
}

export function initials(name: string): string {
  const parts = name.split(/[\s_-]+/).filter(Boolean);
  if (parts.length === 0) return "?";
  if (parts.length === 1) return parts[0]!.slice(0, 2).toUpperCase();
  return (parts[0]![0]! + parts[1]![0]!).toUpperCase();
}

/** Insert `s` at the textarea caret, then refocus and re-collapse the selection past it. */
export function insertAtCaret(
  ref: { current: HTMLTextAreaElement | null },
  value: string,
  setValue: (v: string) => void,
  s: string,
) {
  const el = ref.current;
  const pos = el?.selectionStart ?? value.length;
  setValue(value.slice(0, pos) + s + value.slice(pos));
  requestAnimationFrame(() => {
    const e = ref.current;
    if (!e) return;
    e.focus();
    const c = pos + s.length;
    e.setSelectionRange(c, c);
  });
}

export type IdTone = "moss" | "amber" | "ink" | "neutral";

const ID_TONE_PALETTE: IdTone[] = ["moss", "amber", "ink", "neutral"];

export function dedupeById<T extends { id: string }>(items: T[]): T[] {
  const seen = new Set<string>();
  const out: T[] = [];
  for (const it of items) {
    if (seen.has(it.id)) continue;
    seen.add(it.id);
    out.push(it);
  }
  return out;
}

/** Stable hash from `id` to a palette tone — same id always picks the same color. */
export function toneById(id?: string | null): IdTone {
  if (!id) return "neutral";
  let h = 0;
  for (let i = 0; i < id.length; i++) h = (h * 31 + id.charCodeAt(i)) | 0;
  return ID_TONE_PALETTE[Math.abs(h) % ID_TONE_PALETTE.length]!;
}

/** Human-readable byte size: `1500` → `"1 KB"`, `2000000` → `"2 MB"`. */
export function formatBytes(n: number): string {
  if (n >= 1024 * 1024) return `${Math.round(n / (1024 * 1024))} MB`;
  if (n >= 1024) return `${Math.round(n / 1024)} KB`;
  return `${n} B`;
}
