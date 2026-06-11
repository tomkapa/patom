import { Fragment, type ReactNode } from "react";
import type { Element, Root, RootContent, Text } from "hast";

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

/** Scan `text` for `@Name` mentions against a known-name list (humans and
 *  agents alike). Names are sorted longest-first so multi-word names beat
 *  their prefixes. Returns [{name, start, end}] in order of appearance. The
 *  single source of truth for the matching rules — both the read side
 *  (`renderMentions`) and the compose side (`MentionInput`) call it. */
export function findNamedMentions(
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

/** Span boundaries for every mention in `value`, regardless of match mode. */
function mentionSpans(
  value: string,
  agentNames?: string[],
): Array<{ start: number; end: number }> {
  if (agentNames && agentNames.length > 0) {
    return findNamedMentions(value, agentNames).map(({ start, end }) => ({
      start,
      end,
    }));
  }
  const spans: Array<{ start: number; end: number }> = [];
  forEachMention(value, (tag, tagStart) => {
    spans.push({ start: tagStart, end: tagStart + tag.length });
  });
  return spans;
}

function mentionElement(label: string): Element {
  return {
    type: "element",
    tagName: "span",
    properties: { className: ["mention"] },
    children: [{ type: "text", value: label } satisfies Text],
  };
}

/** Split one text run into interleaved text/`<span class="mention">` hast nodes.
 *  The same matching rules as {@link renderMentions} — named-list when
 *  `agentNames` is given, legacy `@word` otherwise — so the markdown read path
 *  highlights mentions identically to the plain-text path. */
export function splitMentionNodes(
  value: string,
  agentNames?: string[],
): RootContent[] {
  const spans = mentionSpans(value, agentNames);
  if (spans.length === 0) return [{ type: "text", value }];
  const out: RootContent[] = [];
  let last = 0;
  for (const { start, end } of spans) {
    if (start > last) out.push({ type: "text", value: value.slice(last, start) });
    out.push(mentionElement(value.slice(start, end)));
    last = end;
  }
  if (last < value.length) out.push({ type: "text", value: value.slice(last) });
  return out;
}

// A node carrying children is the only kind we descend into; bounded below.
const REHYPE_NODE_CAP = 100_000;
const NO_HIGHLIGHT_TAGS = new Set(["code", "pre"]);

/** rehype attacher that highlights `@mentions` in every text node, skipping
 *  `code`/`pre` so literal `@foo` in code stays literal. Walks an explicit
 *  bounded stack — no recursion, no unbounded loop. */
export function rehypeMentions(agentNames?: string[]) {
  return () => (tree: Root) => {
    const stack: Array<Root | Element> = [tree];
    let visited = 0;
    while (stack.length > 0) {
      visited += 1;
      if (visited > REHYPE_NODE_CAP) break;
      const node = stack.pop();
      if (node === undefined) break;
      const next: RootContent[] = [];
      let changed = false;
      for (const child of node.children) {
        if (child.type === "text") {
          const parts = splitMentionNodes(child.value, agentNames);
          if (parts.length > 1) changed = true;
          next.push(...parts);
          continue;
        }
        next.push(child);
        if (child.type === "element" && !NO_HIGHLIGHT_TAGS.has(child.tagName)) {
          stack.push(child);
        }
      }
      if (changed) node.children = next as Element["children"];
    }
  };
}
