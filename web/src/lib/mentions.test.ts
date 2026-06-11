import { describe, expect, test } from "bun:test";

import { splitMentionNodes, rehypeMentions } from "./mentions";
import type { Element, Root, Text } from "hast";

function text(value: string): Text {
  return { type: "text", value };
}

describe("splitMentionNodes", () => {
  test("returns the original text untouched when there is no mention", () => {
    const out = splitMentionNodes("hello there", ["recruiter"]);
    expect(out).toEqual([text("hello there")]);
  });

  test("wraps a known name in a mention span and keeps surrounding text", () => {
    const out = splitMentionNodes("@Tom all set", ["Tom"]);
    expect(out).toHaveLength(2);
    const span = out[0] as Element;
    expect(span.type).toBe("element");
    expect(span.tagName).toBe("span");
    expect(span.properties?.className).toEqual(["mention"]);
    expect(span.children).toEqual([text("@Tom")]);
    expect(out[1]).toEqual(text(" all set"));
  });

  test("matches multi-word names in full (longest-first)", () => {
    const out = splitMentionNodes("ping @Sales Lead now", ["Sales", "Sales Lead"]);
    const span = out[1] as Element;
    expect(span.children).toEqual([text("@Sales Lead")]);
  });

  test("falls back to any @word when no names are given", () => {
    const out = splitMentionNodes("hi @anyone", undefined);
    const span = out[1] as Element;
    expect(span.tagName).toBe("span");
    expect(span.children).toEqual([text("@anyone")]);
  });

  test("does not match an email address", () => {
    const out = splitMentionNodes("mail a@b.com please", ["b"]);
    expect(out).toEqual([text("mail a@b.com please")]);
  });
});

describe("rehypeMentions", () => {
  test("rewrites text nodes but leaves code spans alone", () => {
    const tree: Root = {
      type: "root",
      children: [
        {
          type: "element",
          tagName: "p",
          properties: {},
          children: [text("hey @Tom")],
        },
        {
          type: "element",
          tagName: "code",
          properties: {},
          children: [text("@Tom")],
        },
      ],
    };
    rehypeMentions(["Tom"])()(tree);

    const para = tree.children[0] as Element;
    expect(para.children).toHaveLength(2);
    expect((para.children[1] as Element).tagName).toBe("span");

    const code = tree.children[1] as Element;
    expect(code.children).toEqual([text("@Tom")]);
  });
});
