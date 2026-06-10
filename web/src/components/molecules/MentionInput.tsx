import {
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type KeyboardEvent,
  type MutableRefObject,
} from "react";
import { Bot } from "lucide-react";
import type { Mentionable } from "../../types/api";
import { Monogram } from "../atoms/Monogram";
import { findNamedMentions } from "../../lib/mentions";
import { cn } from "../../lib/utils";

type Token =
  | { kind: "text"; text: string }
  | { kind: "mention"; text: string };

/** Every known-name mention is live — humans and agents alike, first or
 *  fifth. (The old "only the first tag routes" rule is gone; the backend
 *  triggers each tagged agent.) Matching rules live in the shared
 *  `findNamedMentions` so compose-highlight can't drift from feed-render. */
export function tokenizeMentions(text: string, names: string[]): Token[] {
  const out: Token[] = [];
  if (!text) return out;
  let last = 0;
  for (const { name, start, end } of findNamedMentions(text, names)) {
    if (start > last) out.push({ kind: "text", text: text.slice(last, start) });
    out.push({ kind: "mention", text: `@${name}` });
    last = end;
  }
  if (last < text.length) out.push({ kind: "text", text: text.slice(last) });
  return out;
}

/** All distinct roster entries tagged in `text`, in order of first
 *  appearance — the submit's `tags` payload. */
export function matchMentions(
  text: string,
  roster: Mentionable[],
): Mentionable[] {
  if (!text || roster.length === 0) return [];
  const byName = new Map(roster.map((m) => [m.name, m]));
  const seen = new Set<string>();
  const out: Mentionable[] = [];
  for (const { name } of findNamedMentions(text, [...byName.keys()])) {
    const entry = byName.get(name);
    if (!entry || seen.has(`${entry.kind}:${entry.id}`)) continue;
    seen.add(`${entry.kind}:${entry.id}`);
    out.push(entry);
  }
  return out;
}

/** If the caret sits inside an active "@..." token (start-of-input or
 *  after whitespace), return its start index and current query. */
function activeMentionAt(
  text: string,
  caret: number,
): { start: number; query: string } | null {
  let i = caret - 1;
  while (i >= 0) {
    const c = text[i]!;
    if (c === "@") {
      if (i === 0 || /\s/.test(text[i - 1] ?? "")) {
        const q = text.slice(i + 1, caret);
        if (/^[\w-]*$/.test(q)) return { start: i, query: q };
      }
      return null;
    }
    if (!/[\w-]/.test(c)) return null;
    i--;
  }
  return null;
}

export type MentionInputHandle = HTMLTextAreaElement;

export function MentionInput({
  value,
  onChange,
  roster,
  placeholder,
  onSubmit,
  disabled,
  rows = 2,
  maxHeight = 220,
  textRef,
  className,
}: {
  value: string;
  onChange: (v: string) => void;
  /** Everyone taggable here — channel members (humans) plus the org's
   *  agents. Humans and agents render alike; only the row icon differs. */
  roster: Mentionable[];
  placeholder?: string;
  onSubmit?: () => void;
  disabled?: boolean;
  rows?: number;
  maxHeight?: number;
  textRef?: MutableRefObject<HTMLTextAreaElement | null>;
  className?: string;
}) {
  const localRef = useRef<HTMLTextAreaElement | null>(null);
  const setRef = (el: HTMLTextAreaElement | null) => {
    localRef.current = el;
    if (textRef) textRef.current = el;
  };
  const overlayRef = useRef<HTMLDivElement>(null);
  const [caret, setCaret] = useState(0);
  const [active, setActive] = useState<{ start: number; query: string } | null>(
    null,
  );
  const [hl, setHl] = useState(0);

  // Auto-grow
  useLayoutEffect(() => {
    const el = localRef.current;
    if (!el) return;
    el.style.height = "auto";
    const next = Math.min(maxHeight, Math.max(40, el.scrollHeight));
    el.style.height = next + "px";
    if (overlayRef.current) {
      overlayRef.current.style.height = next + "px";
    }
  }, [value, maxHeight]);

  useEffect(() => {
    setActive(activeMentionAt(value, caret));
  }, [value, caret]);

  useEffect(() => {
    setHl(0);
  }, [active?.query]);

  const names = useMemo(() => roster.map((m) => m.name), [roster]);
  const tokens = useMemo(
    () => tokenizeMentions(value, names),
    [value, names],
  );
  const filtered = useMemo(() => {
    if (!active) return [];
    const q = active.query.toLowerCase();
    return roster.filter((m) => m.name.toLowerCase().includes(q)).slice(0, 8);
  }, [active, roster]);

  const insertMention = (entry: Mentionable) => {
    if (!active) return;
    const before = value.slice(0, active.start);
    const after = value.slice(caret);
    const insertion = `@${entry.name} `;
    const next = before + insertion + after;
    onChange(next);
    const newCaret = (before + insertion).length;
    setActive(null);
    requestAnimationFrame(() => {
      const el = localRef.current;
      if (!el) return;
      el.focus();
      el.setSelectionRange(newCaret, newCaret);
      setCaret(newCaret);
    });
  };

  const onKeyDown = (e: KeyboardEvent<HTMLTextAreaElement>) => {
    if (active && filtered.length > 0) {
      if (e.key === "ArrowDown") {
        e.preventDefault();
        setHl((i) => (i + 1) % filtered.length);
        return;
      }
      if (e.key === "ArrowUp") {
        e.preventDefault();
        setHl((i) => (i - 1 + filtered.length) % filtered.length);
        return;
      }
      if (e.key === "Enter" || e.key === "Tab") {
        e.preventDefault();
        insertMention(filtered[hl] ?? filtered[0]!);
        return;
      }
      if (e.key === "Escape") {
        e.preventDefault();
        setActive(null);
        return;
      }
    }
    if (e.key === "Enter" && !e.shiftKey && !e.metaKey && !e.ctrlKey) {
      e.preventDefault();
      onSubmit?.();
    }
  };

  const syncScroll = () => {
    const el = localRef.current;
    const ov = overlayRef.current;
    if (!el || !ov) return;
    ov.scrollTop = el.scrollTop;
  };

  // Identical typography for textarea and overlay so wrapping matches.
  const typography =
    "font-[var(--font-sans)] text-[14px] leading-[1.55] px-3.5 pt-3 pb-2";
  // Kerning/ligatures apply within a single text node but break at span
  // boundaries; disabling them keeps the overlay's split-span text aligned
  // with the textarea's continuous text so the caret stays under the glyph.
  const metricLock: React.CSSProperties = {
    fontKerning: "none",
    fontVariantLigatures: "none",
    fontFeatureSettings: "normal",
  };

  return (
    <div className={cn("relative", className)}>
      <div
        ref={overlayRef}
        aria-hidden
        style={metricLock}
        className={cn(
          "pointer-events-none absolute inset-0 overflow-hidden whitespace-pre-wrap break-words text-[var(--color-ink)]",
          typography,
        )}
      >
        {tokens.length === 0 ? (
          <span className="text-[var(--color-fg-muted)]">{placeholder}</span>
        ) : (
          tokens.map((t, i) =>
            t.kind === "text" ? (
              <span key={i}>{t.text}</span>
            ) : (
              <span key={i} className="text-[var(--color-moss)]">
                {t.text}
              </span>
            ),
          )
        )}
        {/* keep height when value ends with newline */}
        {value.endsWith("\n") && <span> </span>}
      </div>
      <textarea
        ref={setRef}
        style={metricLock}
        value={value}
        onChange={(e) => {
          onChange(e.target.value);
          setCaret(e.target.selectionStart ?? e.target.value.length);
        }}
        onKeyUp={(e) => {
          const el = e.currentTarget;
          setCaret(el.selectionStart ?? 0);
        }}
        onClick={(e) => {
          const el = e.currentTarget;
          setCaret(el.selectionStart ?? 0);
        }}
        onSelect={(e) => {
          const el = e.currentTarget;
          setCaret(el.selectionStart ?? 0);
        }}
        onScroll={syncScroll}
        onKeyDown={onKeyDown}
        onBlur={() => setTimeout(() => setActive(null), 120)}
        placeholder=""
        disabled={disabled}
        rows={rows}
        className={cn(
          "relative block w-full resize-none bg-transparent outline-none",
          "text-transparent caret-[var(--color-ink)] selection:bg-[var(--color-moss-soft)]",
          typography,
        )}
      />
      {active && filtered.length > 0 && (
        <div className="absolute bottom-full left-2 z-30 mb-1 max-h-56 w-64 overflow-y-auto border border-[var(--color-line-strong)] bg-[var(--color-card)] shadow-lg">
          {filtered.map((m, i) => (
            <button
              key={`${m.kind}:${m.id}`}
              type="button"
              onMouseDown={(e) => {
                e.preventDefault();
                insertMention(m);
              }}
              onMouseEnter={() => setHl(i)}
              className={cn(
                "flex w-full items-center gap-2 px-2 py-1.5 text-left text-[12px]",
                i === hl
                  ? "bg-[var(--color-moss)] text-white"
                  : "text-[var(--color-ink)] hover:bg-[var(--color-paper-2)]",
              )}
            >
              <Monogram
                name={m.name}
                id={m.id}
                size={18}
                tone={m.kind === "agent" ? "moss" : "user"}
                avatarUrl={m.avatar_url}
              />
              <span className="font-[var(--font-mono)]">{m.name}</span>
              {m.kind === "agent" && (
                <Bot className="ml-auto h-3 w-3 text-[var(--color-moss)]" />
              )}
            </button>
          ))}
        </div>
      )}
    </div>
  );
}
