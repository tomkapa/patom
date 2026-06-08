import { Fragment, type ReactNode } from "react";

// Match `@name`, `@name-with-dash`, `@name_with_underscore`. The leading
// boundary is start-of-string or whitespace so an email like `a@b.com` is
// not highlighted.
const MENTION_RE_SRC = String.raw`(^|\s)(@[\w][\w-]*)`;

/** Fresh stateful regex per call — sharing a global `g` regex across modules
 *  is a `lastIndex` footgun. */
function mentionRegex(): RegExp {
  return new RegExp(MENTION_RE_SRC, "g");
}

export function forEachMention(
  text: string,
  cb: (tag: string, tagStart: number) => void,
): void {
  if (!text) return;
  const re = mentionRegex();
  let m: RegExpExecArray | null;
  while ((m = re.exec(text)) !== null) {
    const lead = m[1] ?? "";
    const tag = m[2] ?? "";
    cb(tag, m.index + lead.length);
  }
}

/** Scan `text` for `@AgentName` mentions using the provided agent-name list.
 *  Names are sorted longest-first so multi-word names beat their prefixes.
 *  Returns [{name, start, end}] in order of appearance. */
function findNamedMentions(
  text: string,
  agentNames: string[],
): Array<{ name: string; start: number; end: number }> {
  if (!text || agentNames.length === 0) return [];
  const sorted = [...agentNames].sort((a, b) => b.length - a.length);
  const out: Array<{ name: string; start: number; end: number }> = [];
  let i = 0;
  while (i < text.length) {
    const ch = text[i];
    if (ch === "@" && (i === 0 || /\s/.test(text[i - 1] ?? ""))) {
      let matched: string | null = null;
      for (const name of sorted) {
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

/** Render `@mentions` in `text` as styled spans.
 *  When `agentNames` is provided, only known agent names are highlighted and
 *  multi-word names (e.g. "Sales Lead") are matched in full. Without it,
 *  any `@word` token is highlighted (legacy fallback). */
export function renderMentions(
  text: string,
  agentNames?: string[],
): ReactNode {
  if (!text) return text;
  const out: ReactNode[] = [];

  if (agentNames && agentNames.length > 0) {
    let last = 0;
    for (const { name, start, end } of findNamedMentions(text, agentNames)) {
      if (start > last) out.push(text.slice(last, start));
      out.push(
        <span key={start} className="font-semibold text-[var(--color-moss)]">
          {`@${name}`}
        </span>,
      );
      last = end;
    }
    if (last < text.length) out.push(text.slice(last));
    return <Fragment>{out}</Fragment>;
  }

  let last = 0;
  forEachMention(text, (tag, tagStart) => {
    if (tagStart > last) out.push(text.slice(last, tagStart));
    out.push(
      <span
        key={tagStart}
        className="font-semibold text-[var(--color-moss)]"
      >
        {tag}
      </span>,
    );
    last = tagStart + tag.length;
  });
  if (last < text.length) out.push(text.slice(last));
  return <Fragment>{out}</Fragment>;
}

/** Prepend `@name ` to `text` unless it already starts with that mention. */
export function prefixMention(text: string, name: string | null | undefined): string {
  if (!name) return text;
  if (text.startsWith(`@${name}`)) return text;
  return `@${name} ${text}`;
}
