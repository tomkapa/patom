import {
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type KeyboardEvent,
  type MutableRefObject,
} from "react";
import type { Agent } from "../../types/api";
import { Monogram } from "../atoms/Monogram";
import { cn } from "../../lib/utils";

export type MentionMode = "channel" | "dm" | "thread";

type Token =
  | { kind: "text"; text: string }
  | { kind: "mention"; text: string; active: boolean };

/** Find all @-mentions in `text` that match a known agent name.
 *  Sorts names longest-first so "Sales Lead" beats "Sales" when both exist.
 *  A mention is valid when the character immediately after the name is
 *  whitespace, punctuation, or end-of-string. */
function findAgentMentions(
  text: string,
  agentsByName: ReadonlyMap<string, Agent>,
): Array<{ name: string; start: number; end: number }> {
  if (!text || agentsByName.size === 0) return [];
  const names = [...agentsByName.keys()].sort((a, b) => b.length - a.length);
  const out: Array<{ name: string; start: number; end: number }> = [];
  let i = 0;
  while (i < text.length) {
    const ch = text[i];
    if (ch === "@" && (i === 0 || /\s/.test(text[i - 1] ?? ""))) {
      let matched: string | null = null;
      for (const name of names) {
        if (text.slice(i + 1, i + 1 + name.length) === name) {
          const charAfter = text[i + 1 + name.length] ?? "";
          if (charAfter === "" || /[\s.,!?;:]/.test(charAfter)) {
            matched = name;
            break;
          }
        }
      }
      if (matched !== null) {
        out.push({ name: matched, start: i, end: i + 1 + matched.length });
        i += 1 + matched.length;
        continue;
      }
    }
    i++;
  }
  return out;
}

export function tokenizeMentions(
  text: string,
  agentsByName: ReadonlyMap<string, Agent>,
  mode: MentionMode,
): Token[] {
  const out: Token[] = [];
  if (!text) return out;
  const mentions = findAgentMentions(text, agentsByName);
  let last = 0;
  let canBeActive = mode === "channel";
  for (const { name, start, end } of mentions) {
    if (start > last) out.push({ kind: "text", text: text.slice(last, start) });
    let active = false;
    if (canBeActive) {
      active = true;
      canBeActive = false;
    }
    out.push({ kind: "mention", text: `@${name}`, active });
    last = end;
  }
  if (last < text.length) out.push({ kind: "text", text: text.slice(last) });
  return out;
}

export function firstActiveMentionAgent(
  text: string,
  agents: Agent[],
): Agent | undefined {
  if (!text) return undefined;
  const byName = new Map(agents.map((a) => [a.name, a]));
  const mentions = findAgentMentions(text, byName);
  return mentions.length > 0 ? byName.get(mentions[0]!.name) : undefined;
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
  agents,
  mode,
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
  agents: Agent[];
  mode: MentionMode;
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

  const agentsByName = useMemo(
    () => new Map(agents.map((a) => [a.name, a])),
    [agents],
  );
  const tokens = useMemo(
    () => tokenizeMentions(value, agentsByName, mode),
    [value, agentsByName, mode],
  );
  const filtered = useMemo(() => {
    if (!active) return [];
    const q = active.query.toLowerCase();
    return agents.filter((a) => a.name.toLowerCase().includes(q)).slice(0, 8);
  }, [active, agents]);

  const insertMention = (agent: Agent) => {
    if (!active) return;
    const before = value.slice(0, active.start);
    const after = value.slice(caret);
    const insertion = `@${agent.name} `;
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
              <span
                key={i}
                className={
                  t.active
                    ? "text-[var(--color-moss)]"
                    : "text-[var(--color-fg-muted)]"
                }
              >
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
          {filtered.map((a, i) => (
            <button
              key={a.id}
              type="button"
              onMouseDown={(e) => {
                e.preventDefault();
                insertMention(a);
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
                name={a.name}
                id={a.id}
                size={18}
                tone={i === hl ? "user" : "moss"}
              />
              <span className="font-[var(--font-mono)]">{a.name}</span>
              {a.is_default && (
                <span className="ml-auto font-[var(--font-mono)] text-[9.5px] uppercase tracking-[0.14em] text-[var(--color-muted-foreground)]">
                  default
                </span>
              )}
            </button>
          ))}
        </div>
      )}
    </div>
  );
}
