import { memo } from "react";
import ReactMarkdown, { type Components } from "react-markdown";
import remarkGfm from "remark-gfm";
import { cn } from "../../lib/utils";

const remarkPlugins = [remarkGfm];

const components: Components = {
  p: ({ children }) => <p className="my-1 first:mt-0 last:mb-0">{children}</p>,
  a: ({ children, href }) => (
    <a
      href={href}
      target="_blank"
      rel="noreferrer noopener"
      className="text-[var(--color-moss)] underline decoration-[var(--color-moss)]/40 underline-offset-2 hover:decoration-[var(--color-moss)]"
    >
      {children}
    </a>
  ),
  strong: ({ children }) => (
    <strong className="font-bold text-[var(--color-ink)]">{children}</strong>
  ),
  em: ({ children }) => <em className="italic">{children}</em>,
  ul: ({ children }) => (
    <ul className="my-1 list-disc space-y-0.5 pl-5">{children}</ul>
  ),
  ol: ({ children }) => (
    <ol className="my-1 list-decimal space-y-0.5 pl-5">{children}</ol>
  ),
  li: ({ children }) => <li className="leading-[1.55]">{children}</li>,
  h1: ({ children }) => (
    <h1 className="mt-2 mb-1 font-[var(--font-display)] text-[18px] font-bold tracking-tight">
      {children}
    </h1>
  ),
  h2: ({ children }) => (
    <h2 className="mt-2 mb-1 font-[var(--font-display)] text-[16px] font-bold tracking-tight">
      {children}
    </h2>
  ),
  h3: ({ children }) => (
    <h3 className="mt-2 mb-1 font-[var(--font-display)] text-[14px] font-bold tracking-tight">
      {children}
    </h3>
  ),
  blockquote: ({ children }) => (
    <blockquote className="my-1 border-l-2 border-[var(--color-moss)]/40 pl-3 text-[var(--color-muted-foreground)]">
      {children}
    </blockquote>
  ),
  hr: () => <hr className="my-2 border-t border-[var(--color-line)]" />,
  code: ({ className: cls, children, ...props }) => {
    const inline = !/language-/.test(cls ?? "");
    if (inline) {
      return (
        <code
          className="rounded-sm border border-[var(--color-line)] bg-[var(--color-paper-2)] px-1 py-px font-[var(--font-mono)] text-[12.5px] text-[var(--color-ink-2)]"
          {...props}
        >
          {children}
        </code>
      );
    }
    return (
      <code
        className={cn("font-[var(--font-mono)] text-[12.5px]", cls)}
        {...props}
      >
        {children}
      </code>
    );
  },
  pre: ({ children }) => (
    <pre className="my-1.5 overflow-x-auto border border-[var(--color-line)] bg-[var(--color-paper-2)] p-2 font-[var(--font-mono)] text-[12.5px] leading-[1.5] text-[var(--color-ink-2)]">
      {children}
    </pre>
  ),
  table: ({ children }) => (
    <div className="my-1.5 overflow-x-auto">
      <table className="w-full border-collapse border border-[var(--color-line)] text-[13px]">
        {children}
      </table>
    </div>
  ),
  th: ({ children }) => (
    <th className="border border-[var(--color-line)] bg-[var(--color-paper-2)] px-2 py-1 text-left font-bold">
      {children}
    </th>
  ),
  td: ({ children }) => (
    <td className="border border-[var(--color-line)] px-2 py-1">{children}</td>
  ),
};

export const Markdown = memo(function Markdown({
  text,
  className,
}: {
  text: string;
  className?: string;
}) {
  return (
    <div
      className={cn(
        "markdown font-[var(--font-sans)] text-[14px] leading-[1.55] text-[var(--color-ink)]",
        className,
      )}
    >
      <ReactMarkdown remarkPlugins={remarkPlugins} components={components}>
        {text}
      </ReactMarkdown>
    </div>
  );
});
