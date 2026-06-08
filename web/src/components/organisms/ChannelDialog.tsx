import { useMemo, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { Hash, Trash2, UserPlus } from "lucide-react";
import { Modal, ModalFooter, ModalHeader } from "../molecules/Modal";
import { Button } from "../atoms/Button";
import { Monogram } from "../atoms/Monogram";
import { api } from "../../lib/api";
import { ApiError } from "../../lib/errors";
import {
  useAddChannelMember,
  useChannelMembers,
  useCreateChannel,
  useRemoveChannelMember,
  useUpdateChannel,
} from "../../hooks/useChannels";
import type { Channel } from "../../types/api";

/** Map a server error body to a friendly line. The BE sends stable codes
 *  (`channel.name_taken`, `channel.limit_reached`) or a parse message. */
function channelError(e: unknown): string {
  if (e instanceof ApiError) {
    if (e.body.includes("name_taken")) return "A channel with that name already exists.";
    if (e.body.includes("limit_reached")) return "This workspace has reached its channel limit.";
    if (e.status === 400) return "Channel names use lowercase letters, numbers and hyphens.";
    if (e.status === 403) return "Only the channel's creator can manage it.";
  }
  return "Something went wrong. Please try again.";
}

export function ChannelDialog({
  open,
  channel,
  onClose,
  onArchived,
}: {
  open: boolean;
  /** Present → manage that channel; absent → create a new one. */
  channel?: Channel;
  onClose: () => void;
  /** Called after a successful archive so the parent can deselect it. */
  onArchived?: (id: string) => void;
}) {
  return (
    <Modal open={open} onClose={onClose} ariaLabel="Channel" width={460}>
      {channel ? (
        <ManageBody channel={channel} onClose={onClose} onArchived={onArchived} />
      ) : (
        <CreateBody onClose={onClose} />
      )}
    </Modal>
  );
}

function CreateBody({ onClose }: { onClose: () => void }) {
  const [name, setName] = useState("");
  const [error, setError] = useState<string | null>(null);
  const create = useCreateChannel();

  const submit = async () => {
    setError(null);
    try {
      await create.mutateAsync(name.trim());
      onClose();
    } catch (e) {
      setError(channelError(e));
    }
  };

  return (
    <form
      onSubmit={(e) => {
        e.preventDefault();
        void submit();
      }}
    >
      <ModalHeader
        eyebrow="Channels"
        title="Create a channel"
        icon={<Hash className="h-5 w-5 text-[var(--color-moss)]" />}
        onClose={onClose}
      />
      <div className="px-5 py-5">
        <label className="font-[var(--font-mono)] text-[11px] uppercase tracking-[0.1em] text-[var(--color-muted-foreground)]">
          Name
        </label>
        <div className="mt-1.5 flex items-center gap-1.5 border border-[var(--color-line-strong)] bg-[var(--color-card)] px-2.5 focus-within:ring-2 focus-within:ring-[var(--color-moss)]/15">
          <span className="text-[var(--color-muted-foreground)]">#</span>
          <input
            autoFocus
            value={name}
            onChange={(e) => setName(e.target.value)}
            placeholder="eng-team"
            className="h-[36px] w-full bg-transparent font-[var(--font-mono)] text-[13px] outline-none placeholder:text-[var(--color-fg-muted)]"
          />
        </div>
        <p className="mt-2 text-[12px] text-[var(--color-muted-foreground)]">
          Lowercase letters, numbers and hyphens. You'll be added automatically;
          add others after it's created.
        </p>
        {error ? (
          <p className="mt-2 text-[12px] text-[var(--color-rose)]">{error}</p>
        ) : null}
      </div>
      <ModalFooter>
        <Button type="button" variant="ghost" size="md" onClick={onClose}>
          Cancel
        </Button>
        <Button
          type="submit"
          variant="moss"
          size="md"
          loading={create.isPending}
          disabled={!name.trim() || create.isPending}
        >
          Create
        </Button>
      </ModalFooter>
    </form>
  );
}

function ManageBody({
  channel,
  onClose,
  onArchived,
}: {
  channel: Channel;
  onClose: () => void;
  onArchived?: (id: string) => void;
}) {
  const [name, setName] = useState(channel.name);
  const [error, setError] = useState<string | null>(null);
  const update = useUpdateChannel();
  const membersQ = useChannelMembers(channel.id);
  const addMember = useAddChannelMember();
  const removeMember = useRemoveChannelMember();
  const rosterQ = useQuery({
    queryKey: ["org-members", "channel-picker"],
    queryFn: () => api.members({}),
    staleTime: 60_000,
  });

  const memberIds = useMemo(
    () => new Set((membersQ.data ?? []).map((m) => m.user_id)),
    [membersQ.data],
  );
  const roster = useMemo(
    () =>
      (rosterQ.data?.rows ?? []).filter(
        (r) => r.kind === "member" && r.user_id && r.status === "active",
      ),
    [rosterQ.data],
  );
  // O(1) id → row lookup for `label()`, which the member list calls several
  // times per row.
  const rosterById = useMemo(
    () => new Map(roster.map((r) => [r.user_id, r])),
    [roster],
  );
  const candidates = roster.filter((r) => r.user_id && !memberIds.has(r.user_id));
  const label = (id: string) => {
    const row = rosterById.get(id);
    return row?.display_name ?? row?.email ?? id;
  };

  const rename = async () => {
    if (name.trim() === channel.name) return;
    setError(null);
    try {
      await update.mutateAsync({ id: channel.id, patch: { name: name.trim() } });
      onClose();
    } catch (e) {
      setError(channelError(e));
    }
  };

  const archive = async () => {
    setError(null);
    try {
      await update.mutateAsync({ id: channel.id, patch: { archived: true } });
      onArchived?.(channel.id);
      onClose();
    } catch (e) {
      setError(channelError(e));
    }
  };

  return (
    <div>
      <ModalHeader
        eyebrow="Channels"
        title={`Manage #${channel.name}`}
        icon={<Hash className="h-5 w-5 text-[var(--color-moss)]" />}
        onClose={onClose}
      />
      <div className="space-y-5 px-5 py-5">
        {/* Rename */}
        <div>
          <label className="font-[var(--font-mono)] text-[11px] uppercase tracking-[0.1em] text-[var(--color-muted-foreground)]">
            Name
          </label>
          <div className="mt-1.5 flex items-center gap-2">
            <div className="flex flex-1 items-center gap-1.5 border border-[var(--color-line-strong)] bg-[var(--color-card)] px-2.5">
              <span className="text-[var(--color-muted-foreground)]">#</span>
              <input
                value={name}
                onChange={(e) => setName(e.target.value)}
                className="h-[34px] w-full bg-transparent font-[var(--font-mono)] text-[13px] outline-none"
              />
            </div>
            <Button
              type="button"
              variant="primary"
              size="md"
              onClick={() => void rename()}
              loading={update.isPending}
              disabled={!name.trim() || name.trim() === channel.name}
            >
              Rename
            </Button>
          </div>
        </div>

        {/* Members */}
        <div>
          <div className="font-[var(--font-mono)] text-[11px] uppercase tracking-[0.1em] text-[var(--color-muted-foreground)]">
            Members
          </div>
          <ul className="mt-2 flex flex-col gap-1">
            {(membersQ.data ?? []).map((m) => (
              <li
                key={m.user_id}
                className="flex items-center gap-2 border border-[var(--color-line)] bg-[var(--color-card)] px-2 py-1.5"
              >
                <Monogram name={label(m.user_id)} id={m.user_id} size={20} tone="moss" />
                <span className="min-w-0 flex-1 truncate text-[13px]">
                  {label(m.user_id)}
                </span>
                <button
                  type="button"
                  aria-label={`Remove ${label(m.user_id)}`}
                  onClick={() =>
                    removeMember.mutate({ id: channel.id, userId: m.user_id })
                  }
                  className="shrink-0 text-[var(--color-muted-foreground)] hover:text-[var(--color-rose)]"
                >
                  <Trash2 className="h-3.5 w-3.5" />
                </button>
              </li>
            ))}
            {(membersQ.data ?? []).length === 0 && (
              <li className="text-[12px] text-[var(--color-muted-foreground)]">
                No members yet.
              </li>
            )}
          </ul>

          {candidates.length > 0 && (
            <div className="mt-3">
              <div className="font-[var(--font-mono)] text-[10px] uppercase tracking-[0.1em] text-[var(--color-muted-foreground)]">
                Add a member
              </div>
              <div className="mt-1.5 flex flex-wrap gap-1.5">
                {candidates.map((r) => (
                  <button
                    key={r.user_id}
                    type="button"
                    onClick={() =>
                      addMember.mutate({ id: channel.id, userId: r.user_id! })
                    }
                    className="inline-flex items-center gap-1 border border-[var(--color-line)] bg-[var(--color-card)] px-2 py-1 text-[12px] hover:border-[var(--color-moss)]"
                  >
                    <UserPlus className="h-3 w-3" />
                    {r.display_name ?? r.email}
                  </button>
                ))}
              </div>
            </div>
          )}
        </div>

        {error ? (
          <p className="text-[12px] text-[var(--color-rose)]">{error}</p>
        ) : null}
      </div>
      <ModalFooter
        left={
          <Button
            type="button"
            variant="ghost"
            size="md"
            onClick={() => void archive()}
            className="text-[var(--color-rose)]"
          >
            <Trash2 className="h-3.5 w-3.5" /> Archive
          </Button>
        }
      >
        <Button type="button" variant="moss" size="md" onClick={onClose}>
          Done
        </Button>
      </ModalFooter>
    </div>
  );
}
