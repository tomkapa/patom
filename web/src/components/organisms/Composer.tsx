import { useMemo, useRef, useState } from "react";
import { AtSign, Code, Hash, Paperclip, Send, Smile } from "lucide-react";
import { Button } from "../atoms/Button";
import { TipBar } from "../molecules/TipBar";
import {
  MentionInput,
  firstActiveMentionAgent,
  type MentionMode,
} from "../molecules/MentionInput";
import { cn, insertAtCaret } from "../../lib/utils";
import type { Agent } from "../../types/api";

export type ComposerSubmit = {
  content: string;
  /** First @-tagged agent (channel mode only). Empty in dm/thread modes. */
  agent_id?: string;
};

export function Composer({
  agents,
  mode,
  dmAgent,
  channel,
  pending,
  disabled,
  onSubmit,
}: {
  agents: Agent[];
  /** "channel" → must @-tag. "dm" → auto-route to dmAgent. "thread" → reply to existing session. */
  mode: MentionMode;
  /** Required when mode === "dm": the agent the DM targets. */
  dmAgent?: Agent;
  channel: string;
  pending?: boolean;
  disabled?: boolean;
  onSubmit: (input: ComposerSubmit) => void;
}) {
  const [value, setValue] = useState("");
  const taRef = useRef<HTMLTextAreaElement | null>(null);

  const targeted = useMemo(
    () =>
      mode === "channel" ? firstActiveMentionAgent(value, agents) : undefined,
    [mode, value, agents],
  );
  const tipExample =
    agents.find((a) => a.is_default)?.name ?? agents[0]?.name ?? "agent";

  const trimmed = value.trim();
  const channelBlocked = mode === "channel" && !targeted && trimmed.length > 0;

  const send = () => {
    if (!trimmed || pending || disabled) return;
    if (mode === "channel") {
      if (!targeted) return; // must tag a known agent
      onSubmit({ content: value.trim(), agent_id: targeted.id });
    } else if (mode === "dm") {
      onSubmit({ content: value.trim(), agent_id: dmAgent?.id });
    } else {
      // thread: agent_id ignored on existing sessions
      onSubmit({ content: value.trim() });
    }
    setValue("");
  };

  const insertAt = () => insertAtCaret(taRef, value, setValue, "@");

  const placeholder =
    mode === "dm" && dmAgent
      ? `Message ${dmAgent.name} — replies route directly to them`
      : mode === "thread"
        ? "Reply… your message stays in this thread"
        : `Message #${channel} — type @agent-name to route`;

  return (
    <div className="border-t border-[var(--color-line)] bg-[var(--color-paper)] px-4 md:px-8 pt-3 pb-4">
      <div className="space-y-2">
        {mode === "channel" && (
          <TipBar>
            <span>
              <span className="font-semibold">Tip</span> · type{" "}
              <span className="bg-[var(--color-moss)] px-1 py-px text-white">
                @{tipExample}
              </span>{" "}
              to invoke an agent. Only the first <code>@tag</code> is routed —
              additional tags render as plain text.
            </span>
          </TipBar>
        )}
        {mode === "dm" && dmAgent && (
          <TipBar>
            <span>
              <span className="font-semibold">DM</span> · messages here go to{" "}
              <span className="bg-[var(--color-moss)] px-1 py-px text-white">
                @{dmAgent.name}
              </span>{" "}
              automatically. <code>@tags</code> render as plain text.
            </span>
          </TipBar>
        )}

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
            agents={agents}
            mode={mode}
            placeholder={placeholder}
            onSubmit={send}
            disabled={disabled}
            textRef={taRef}
          />
          <div className="flex items-center gap-1 border-t border-[var(--color-line)] px-2 py-1.5">
            <ToolBtn label="Attach">
              <Paperclip className="h-3.5 w-3.5" />
            </ToolBtn>
            <ToolBtn label="Emoji">
              <Smile className="h-3.5 w-3.5" />
            </ToolBtn>
            <ToolBtn label="Mention agent" onClick={insertAt}>
              <AtSign className="h-3.5 w-3.5" />
            </ToolBtn>
            <ToolBtn label="Channel">
              <Hash className="h-3.5 w-3.5" />
            </ToolBtn>
            <ToolBtn label="Code block">
              <Code className="h-3.5 w-3.5" />
            </ToolBtn>
            {channelBlocked && (
              <span className="ml-2 font-[var(--font-mono)] text-[10.5px] text-[var(--color-rose)]">
                tag an agent (e.g. @{tipExample}) to send
              </span>
            )}
            <Button
              type="submit"
              variant="moss"
              size="md"
              loading={pending}
              disabled={
                !trimmed ||
                pending ||
                disabled ||
                channelBlocked ||
                (mode === "dm" && !dmAgent)
              }
              className="ml-auto"
            >
              {pending ? "sending" : (
                <>
                  Send <Send className="h-3 w-3" strokeWidth={2.5} />
                </>
              )}
            </Button>
          </div>
        </form>
      </div>
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
