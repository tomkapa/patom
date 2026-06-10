import { Bot, ChevronDown, Hash, Plus, Search, Settings2 } from "lucide-react";
import { Button } from "../atoms/Button";
import { Kbd } from "../atoms/Kbd";
import { Monogram } from "../atoms/Monogram";
import { cn } from "../../lib/utils";
import { useT } from "../../i18n";
import type { Channel, Mentionable } from "../../types/api";

/** Stable key for a DM row (a colleague may share an id space with nothing). */
export function dmKey(m: Mentionable): string {
  return `${m.kind}:${m.id}`;
}

export function Sidebar({
  workspace = "Acme Robotics",
  channels,
  dms,
  selectedChannelId,
  selectedDmKey,
  onSelectChannel,
  onSelectDm,
  onAddChannel,
  onManageChannel,
}: {
  workspace?: string;
  channels: Channel[];
  /** Direct-message roster: every colleague — humans and agents alike. */
  dms: Mentionable[];
  selectedChannelId: string | null;
  selectedDmKey: string | null;
  onSelectChannel: (id: string) => void;
  onSelectDm: (m: Mentionable) => void;
  onAddChannel: () => void;
  onManageChannel: (channel: Channel) => void;
}) {
  const { t } = useT();

  return (
    <aside
      className="flex h-full w-[300px] shrink-0 flex-col border-r border-[var(--color-line)] bg-[var(--color-paper)]"
      aria-label="Channels and threads"
    >
      {/* Static workspace label. Switching workspaces lives on the menu
          rail avatar (OrgSwitcher), not here. */}
      <header className="flex items-center gap-2 border-b border-[var(--color-line)] px-4 py-3">
        <div className="min-w-0">
          <div className="font-[var(--font-mono)] text-[10px] uppercase tracking-[0.18em] text-[var(--color-muted-foreground)]">
            {t("sidebar.brand")}
          </div>
          <div className="mt-0.5 truncate font-[var(--font-display)] text-[18px] font-bold tracking-tight text-[var(--color-ink)]">
            {workspace}
          </div>
        </div>
      </header>

      {/* Search */}
      <div className="border-b border-[var(--color-line)] px-3 py-2.5">
        <div className="flex h-[34px] items-center gap-2 border border-[var(--color-line)] bg-[var(--color-card)] px-2.5">
          <Search className="h-3.5 w-3.5 text-[var(--color-muted-foreground)]" />
          <input
            placeholder="Search workspace"
            className="w-full bg-transparent font-[var(--font-mono)] text-[12px] outline-none placeholder:text-[var(--color-fg-muted)]"
          />
          <Kbd>⌘K</Kbd>
        </div>
      </div>

      <div className="flex-1 overflow-y-auto scroll-thin px-2 py-2">
        {/* CHANNELS */}
        <Section
          title={t("sidebar.channels")}
          expandable
          action={<AddBtn label="Add channel" onClick={onAddChannel} />}
        />
        <div className="mb-2 flex flex-col gap-0.5">
          {channels.map((c) => (
            <SidebarRow
              key={c.id}
              icon={<Hash className="h-3 w-3 text-[var(--color-muted-foreground)]" />}
              label={c.name}
              trailing={
                c.can_manage ? (
                  <button
                    type="button"
                    aria-label={`Manage #${c.name}`}
                    onClick={(e) => {
                      e.stopPropagation();
                      onManageChannel(c);
                    }}
                    className="opacity-0 transition-opacity group-hover:opacity-100 hover:text-[var(--color-ink)]"
                  >
                    <Settings2 className="h-3.5 w-3.5" />
                  </button>
                ) : null
              }
              active={c.id === selectedChannelId}
              onClick={() => onSelectChannel(c.id)}
              mono
            />
          ))}
          {channels.length === 0 && (
            <p className="px-2 py-1 font-[var(--font-mono)] text-[11px] text-[var(--color-fg-muted)]">
              {t("sidebar.empty_channels")}
            </p>
          )}
        </div>

        {/* DIRECT MESSAGES — every colleague, human or agent, opens a
            1:1 conversation on click. */}
        <Section title={t("sidebar.dms")} expandable />
        <div className="mb-2 flex flex-col gap-0.5">
          {dms.map((m) => (
            <SidebarRow
              key={dmKey(m)}
              icon={
                <Monogram
                  name={m.name}
                  id={m.id}
                  size={20}
                  tone={m.kind === "agent" ? "moss" : "user"}
                  avatarUrl={m.avatar_url}
                />
              }
              label={m.name}
              trailing={
                m.kind === "agent" ? (
                  <span className="inline-flex items-center gap-1.5">
                    <Bot className="h-3.5 w-3.5 text-[var(--color-moss)]" />
                  </span>
                ) : null
              }
              active={selectedDmKey === dmKey(m)}
              onClick={() => onSelectDm(m)}
              mono
            />
          ))}
          {dms.length === 0 && (
            <p className="px-2 py-1 font-[var(--font-mono)] text-[11px] text-[var(--color-fg-muted)]">
              {t("sidebar.empty_dms")}
            </p>
          )}
        </div>
      </div>
    </aside>
  );
}

function Section({
  title,
  expandable,
  action,
}: {
  title: string;
  expandable?: boolean;
  action?: React.ReactNode;
}) {
  return (
    <div className="mt-2 mb-1 flex items-center gap-1.5 px-2 h-[24px]">
      {expandable && (
        <ChevronDown className="h-3 w-3 text-[var(--color-fg-muted)]" />
      )}
      <span className="font-[var(--font-mono)] text-[10px] uppercase tracking-[0.16em] text-[var(--color-muted-foreground)]">
        {title}
      </span>
      <span className="ml-auto">{action}</span>
    </div>
  );
}

function AddBtn({ label, onClick }: { label: string; onClick?: () => void }) {
  return (
    <Button
      variant="ghost"
      size="xxs"
      iconOnly
      aria-label={label}
      onClick={onClick}
    >
      <Plus className="h-3.5 w-3.5" />
    </Button>
  );
}

function SidebarRow({
  icon,
  label,
  prefix,
  trailing,
  active,
  muted,
  mono,
  title,
  onClick,
}: {
  icon?: React.ReactNode;
  label: React.ReactNode;
  prefix?: string;
  trailing?: React.ReactNode;
  active?: boolean;
  muted?: boolean;
  mono?: boolean;
  title?: string;
  onClick?: () => void;
}) {
  return (
    <button
      onClick={onClick}
      title={title}
      className={cn(
        "group flex h-[28px] w-full items-center gap-2 px-2 text-left text-[13px] transition-colors",
        mono && "font-[var(--font-mono)] text-[12.5px]",
        active
          ? "bg-[var(--color-moss)] text-white font-medium"
          : muted
            ? "text-[var(--color-muted-foreground)] hover:bg-[var(--color-sidebar-accent)] hover:text-[var(--color-ink)]"
            : "text-[var(--color-ink)] hover:bg-[var(--color-sidebar-accent)]",
      )}
    >
      {icon && <span className="shrink-0">{icon}</span>}
      <span className="flex-1 truncate">
        {prefix && (
          <span
            className={cn(
              "mr-0.5",
              active
                ? "text-white/80"
                : "text-[var(--color-muted-foreground)]",
            )}
          >
            {prefix}
          </span>
        )}
        {label}
      </span>
      {trailing && <span className="shrink-0">{trailing}</span>}
    </button>
  );
}
