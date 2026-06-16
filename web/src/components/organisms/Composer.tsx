import { useEffect, useRef, useState } from "react";
import { AtSign, Code, Hash, Paperclip, Send, Smile } from "lucide-react";
import { Button } from "../atoms/Button";
import { MentionInput, matchMentions } from "../molecules/MentionInput";
import { AttachmentBar } from "../molecules/AttachmentBar";
import { ATTACHMENT_ACCEPT, useAttachments } from "../../hooks/useAttachments";
import { cn, insertAtCaret } from "../../lib/utils";
import type { Attachment, Mentionable } from "../../types/api";

export type ComposerSubmit = {
  content: string;
  /** Everyone @-tagged in the message, in order. Agents among them are
   *  invoked by the backend; humans render as mentions. Empty is a plain
   *  post — that's fine, nobody is required to be tagged. */
  tags: Mentionable[];
  /** Uploaded image/file references to send with the message (issue #187). */
  attachments: Attachment[];
};

export function Composer({
  roster,
  mode,
  dmCounterpart,
  channel,
  pending,
  disabled,
  disabledHint,
  prefill,
  onSubmit,
}: {
  /** Everyone taggable from this composer (channel members + agents). */
  roster: Mentionable[];
  /** "channel" posts to the channel timeline; "dm" posts to the selected
   *  direct-message conversation. Neither requires a tag. */
  mode: "channel" | "dm";
  /** The colleague a DM conversation is with (mode === "dm"). */
  dmCounterpart?: Mentionable;
  channel: string;
  pending?: boolean;
  disabled?: boolean;
  /** When the composer is disabled, an optional hint revealed on hover —
   *  the conversion nudge on the public demo ("Sign up to talk to agents").
   *  Omitted in the live app, where `disabled` only means "roster loading". */
  disabledHint?: string;
  /** Drop text into the input from outside without submitting — the
   *  user reviews and sends. `nonce` distinguishes repeat fills of the
   *  same text (e.g. clicking the welcome CTA twice). Null = no fill. */
  prefill?: { text: string; nonce: number } | null;
  onSubmit: (input: ComposerSubmit) => void;
}) {
  const [value, setValue] = useState("");
  const taRef = useRef<HTMLTextAreaElement | null>(null);
  const fileRef = useRef<HTMLInputElement | null>(null);
  const att = useAttachments();

  // Load externally-supplied text into the box (welcome CTA, retry, …)
  // and place the caret at the end so the user can edit or just hit send.
  // Keyed on `nonce` so the same text can be re-dropped on demand.
  const prefillNonce = prefill?.nonce;
  useEffect(() => {
    if (!prefill) return;
    setValue(prefill.text);
    requestAnimationFrame(() => {
      const el = taRef.current;
      if (!el) return;
      el.focus();
      const end = prefill.text.length;
      el.setSelectionRange(end, end);
    });
    // Intentionally keyed on the nonce only — re-running on `prefill.text`
    // identity would clobber the user's edits on unrelated re-renders.
  }, [prefillNonce]);

  const trimmed = value.trim();
  // Send needs either text or at least one finished upload, and no upload
  // still in flight.
  const canSend =
    (trimmed.length > 0 || att.ready.length > 0) &&
    !pending &&
    !disabled &&
    !att.busy;

  const send = () => {
    if (!canSend) return;
    onSubmit({
      content: trimmed,
      tags: matchMentions(value, roster),
      attachments: att.ready,
    });
    setValue("");
    att.clear();
  };

  const insertAt = () => insertAtCaret(taRef, value, setValue, "@");

  const onPickFiles = (e: React.ChangeEvent<HTMLInputElement>) => {
    if (e.target.files) att.addFiles(e.target.files);
    // Reset so picking the same file again re-fires `change`.
    e.target.value = "";
  };

  const placeholder =
    mode === "dm" && dmCounterpart
      ? `Message ${dmCounterpart.name}`
      : `Message #${channel} — @mention an agent to get a reply`;

  return (
    <div className="group/composer relative border-t border-[var(--color-line)] bg-[var(--color-paper)] px-4 md:px-8 pt-3 pb-4">
      {disabled && disabledHint ? (
        <div
          role="note"
          className="pointer-events-none absolute -top-2 left-1/2 z-10 -translate-x-1/2 -translate-y-full whitespace-nowrap border border-[var(--color-line-strong)] bg-[var(--color-ink)] px-3 py-1.5 font-[var(--font-mono)] text-[11px] text-[var(--color-paper)] opacity-0 shadow-sm transition-opacity duration-150 group-hover/composer:opacity-100"
        >
          {disabledHint}
        </div>
      ) : null}
      <form
        onSubmit={(e) => {
          e.preventDefault();
          send();
        }}
        className={cn(
          "border border-[var(--color-line-strong)] bg-[var(--color-card)] focus-within:ring-2 focus-within:ring-[var(--color-moss)]/15 transition",
          disabled && "opacity-60",
        )}
      >
        <MentionInput
          value={value}
          onChange={setValue}
          roster={roster}
          placeholder={placeholder}
          onSubmit={send}
          disabled={disabled}
          textRef={taRef}
        />
        <AttachmentBar items={att.items} onRemove={att.remove} />
        <input
          ref={fileRef}
          type="file"
          multiple
          accept={ATTACHMENT_ACCEPT}
          onChange={onPickFiles}
          className="hidden"
        />
        <div className="flex items-center gap-1 border-t border-[var(--color-line)] px-2 py-1.5">
          <ToolBtn
            label="Attach"
            onClick={() => fileRef.current?.click()}
          >
            <Paperclip className="h-3.5 w-3.5" />
          </ToolBtn>
          <ToolBtn label="Emoji">
            <Smile className="h-3.5 w-3.5" />
          </ToolBtn>
          <ToolBtn label="Mention" onClick={insertAt}>
            <AtSign className="h-3.5 w-3.5" />
          </ToolBtn>
          <ToolBtn label="Channel">
            <Hash className="h-3.5 w-3.5" />
          </ToolBtn>
          <ToolBtn label="Code block">
            <Code className="h-3.5 w-3.5" />
          </ToolBtn>
          <Button
            type="submit"
            variant="moss"
            size="md"
            loading={pending}
            disabled={!canSend}
            className="ml-auto"
          >
            {pending ? (
              "sending"
            ) : (
              <>
                Send <Send className="h-3 w-3" strokeWidth={2.5} />
              </>
            )}
          </Button>
        </div>
      </form>
    </div>
  );
}

function ToolBtn({
  label,
  onClick,
  children,
}: {
  label: string;
  onClick?: () => void;
  children: React.ReactNode;
}) {
  return (
    <Button
      type="button"
      variant="ghost"
      size="sm"
      iconOnly
      aria-label={label}
      onClick={onClick}
    >
      {children}
    </Button>
  );
}
