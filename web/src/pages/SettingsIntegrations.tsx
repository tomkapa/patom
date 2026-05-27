import { useState } from "react";
import {
  useMutation,
  useQuery,
  useQueryClient,
} from "@tanstack/react-query";
import {
  BellPlus,
  Calendar,
  Circle,
  MessageCircle,
  Slack,
  Sparkles,
  Unplug,
} from "lucide-react";
import {
  SettingsBreadcrumb,
  SettingsLayout,
  SettingsPageHeader,
} from "../components/templates/SettingsLayout";
import { Button } from "../components/atoms/Button";
import { Spinner } from "../components/atoms/Spinner";
import { Modal, ModalFooter, ModalHeader } from "../components/molecules/Modal";
import { api } from "../lib/api";
import type { SlackWorkspaceSummary } from "../types/api";
import { useT } from "../i18n";

const SLACK_KEY = ["slack-workspaces"] as const;

export function SettingsIntegrations() {
  const { t } = useT();
  const qc = useQueryClient();
  const installs = useQuery({
    queryKey: SLACK_KEY,
    queryFn: api.slackWorkspaces,
    staleTime: 30_000,
  });

  const install = useMutation({
    mutationFn: api.slackInstall,
    onSuccess: ({ authorize_url }) => {
      // Hand the browser off to Slack's consent screen. The OAuth
      // callback redirects back to /settings/integrations so the new
      // install becomes visible on next mount.
      window.location.assign(authorize_url);
    },
  });

  const disconnectMut = useMutation({
    mutationFn: (teamId: string) => api.slackDisconnect(teamId),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: SLACK_KEY });
    },
  });

  const [pendingDisconnect, setPendingDisconnect] =
    useState<SlackWorkspaceSummary | null>(null);
  const [notifyToast, setNotifyToast] = useState<string | null>(null);

  const onNotifyMe = () => {
    setNotifyToast(t("settings.integrations.notifyMe.toast"));
    window.setTimeout(() => setNotifyToast(null), 2400);
  };

  const rows = installs.data ?? [];

  return (
    <SettingsLayout active="integrations">
      <SettingsBreadcrumb
        trail={[
          { label: t("settings.breadcrumb.workspace") },
          { label: t("settings.breadcrumb.settings") },
          { label: t("settings.nav.integrations"), current: true },
        ]}
      />
      <SettingsPageHeader
        title={t("settings.integrations.title")}
        subtitle={t("settings.integrations.subtitle")}
      />

      <div className="min-h-0 flex-1 overflow-auto">
        {/* CONNECTED */}
        <SectionEyebrow label={t("settings.integrations.connected")} />
        <div className="border-b border-[var(--color-line)] px-8 py-6">
          {installs.isLoading ? (
            <div className="flex h-24 items-center justify-center">
              <Spinner />
            </div>
          ) : installs.isError ? (
            <div className="border border-[var(--color-rose)] bg-[var(--color-rose-soft)] px-3 py-2 text-[12px] text-[var(--color-rose)]">
              {t("settings.integrations.errorLoading")}
            </div>
          ) : rows.length === 0 ? (
            <EmptyConnected
              onConnect={() => install.mutate()}
              connecting={install.isPending}
            />
          ) : (
            <div className="flex flex-col border border-[var(--color-line)] bg-[var(--color-card)]">
              {rows.map((row) => (
                <SlackRow
                  key={row.team_id}
                  row={row}
                  onDisconnect={() => setPendingDisconnect(row)}
                />
              ))}
            </div>
          )}
        </div>

        {/* AVAILABLE */}
        <SectionEyebrow
          label={t("settings.integrations.available")}
          helper={t("settings.integrations.available.helper")}
        />
        <div className="grid grid-cols-1 gap-4 px-8 py-6 lg:grid-cols-2">
          <AvailableCard
            icon={<Sparkles className="h-5 w-5" strokeWidth={1.5} />}
            name="Lark"
            eyebrow="BYTEDANCE SUITE"
            description="Native bot for Feishu / Lark groups. Slash commands plus interactive cards backed by Lark Open Platform."
            features={[
              "/relay slash commands",
              "Group chat installation",
              "Open Platform OAuth",
              "Multi-tenant support",
            ]}
            target={t("settings.integrations.target", { date: "Aug 2026" })}
            onNotify={onNotifyMe}
          />
          <AvailableCard
            icon={<MessageCircle className="h-5 w-5" strokeWidth={1.5} />}
            name="Discord"
            eyebrow="COMMUNITY CHAT"
            description="Guild-scoped install. Thread-aware commands for community ops, moderation, and AI helpdesks."
            features={[
              "Guild + channel scopes",
              "Thread spawning",
              "Role-based access",
              "Slash command sync",
            ]}
            target={t("settings.integrations.target", { date: "Nov 2026" })}
            onNotify={onNotifyMe}
          />
        </div>
      </div>

      {notifyToast ? (
        <div
          role="status"
          className="pointer-events-none fixed right-6 bottom-6 z-50 border border-[var(--color-line)] bg-[var(--color-card)] px-4 py-2 text-[12px] text-[var(--color-ink)] shadow-md"
        >
          {notifyToast}
        </div>
      ) : null}

      <Modal
        open={pendingDisconnect !== null}
        onClose={() => setPendingDisconnect(null)}
        ariaLabel={t("settings.integrations.disconnect")}
      >
        {pendingDisconnect ? (
          <>
            <ModalHeader
              eyebrow="SLACK"
              title={t("settings.integrations.disconnect.confirm.title", {
                team: pendingDisconnect.team_name,
              })}
              onClose={() => setPendingDisconnect(null)}
            />
            <div className="px-5 py-4 text-[13px] text-[var(--color-muted)]">
              {t("settings.integrations.disconnect.confirm.body")}
            </div>
            <ModalFooter>
              <Button
                variant="ghost"
                onClick={() => setPendingDisconnect(null)}
              >
                {t("settings.integrations.disconnect.cancel")}
              </Button>
              <Button
                variant="danger"
                loading={disconnectMut.isPending}
                onClick={() => {
                  const teamId = pendingDisconnect.team_id;
                  disconnectMut.mutate(teamId, {
                    onSuccess: () => setPendingDisconnect(null),
                  });
                }}
                data-testid="slack-disconnect-confirm"
              >
                {t("settings.integrations.disconnect.confirmCta")}
              </Button>
            </ModalFooter>
          </>
        ) : null}
      </Modal>
    </SettingsLayout>
  );
}

function SectionEyebrow({
  label,
  helper,
}: {
  label: string;
  helper?: string;
}) {
  return (
    <div className="flex items-center gap-3 border-y border-[var(--color-line)] bg-[var(--color-paper-2)] px-8 py-2.5">
      <span className="font-[var(--font-mono)] text-[11px] font-bold tracking-[0.1em] text-[var(--color-ink)]">
        {label}
      </span>
      {helper ? (
        <>
          <span
            aria-hidden
            className="h-3 w-px bg-[var(--color-line)]"
          />
          <span className="text-[12px] text-[var(--color-muted)]">{helper}</span>
        </>
      ) : null}
    </div>
  );
}

function SlackRow({
  row,
  onDisconnect,
}: {
  row: SlackWorkspaceSummary;
  onDisconnect: () => void;
}) {
  const { t } = useT();
  const scopes = row.scopes
    .split(",")
    .map((s) => s.trim())
    .filter(Boolean);
  return (
    <div
      className="flex flex-col"
      data-testid="slack-workspace-row"
      data-team-id={row.team_id}
    >
      <div className="flex items-center gap-4 border-b border-[var(--color-line)] px-6 py-5">
        <div
          aria-hidden
          className="flex h-14 w-14 shrink-0 items-center justify-center border border-[var(--color-line)] bg-[var(--color-paper-2)]"
        >
          <Slack className="h-7 w-7 text-[var(--color-ink)]" strokeWidth={1.5} />
        </div>
        <div className="flex min-w-0 flex-1 flex-col gap-1.5">
          <span className="font-[var(--font-display)] text-[18px] font-bold text-[var(--color-ink)]">
            {row.team_name || "Slack"}
          </span>
          <div className="flex flex-wrap items-center gap-3 text-[12px] text-[var(--color-muted)]">
            <span className="font-[var(--font-mono)] text-[var(--color-ink)]">
              {row.team_id}
            </span>
            <span aria-hidden className="h-2.5 w-px bg-[var(--color-line)]" />
            <span>
              {t("settings.integrations.installedBy", {
                who: shortenUserId(row.installed_by_user_id),
              })}
            </span>
            <span aria-hidden className="h-2.5 w-px bg-[var(--color-line)]" />
            <span className="font-[var(--font-mono)] text-[var(--color-muted-2)]">
              {formatInstalledAt(row.installed_at)}
            </span>
          </div>
        </div>
        <Button
          variant="danger"
          size="md"
          onClick={onDisconnect}
          className="border border-[var(--color-rose)]"
          data-testid="slack-disconnect"
        >
          <Unplug className="h-3.5 w-3.5" strokeWidth={1.75} />
          {t("settings.integrations.disconnect")}
        </Button>
      </div>
      <div className="grid grid-cols-[140px_minmax(0,1fr)] gap-6 border-b border-[var(--color-line)] px-6 py-3.5">
        <span className="font-[var(--font-mono)] text-[11px] tracking-[0.08em] text-[var(--color-muted)]">
          {t("settings.integrations.command.label")}
        </span>
        <div>
          <span className="inline-flex items-center border border-[var(--color-line)] bg-[var(--color-paper-2)] px-2.5 py-1 font-[var(--font-mono)] text-[13px] font-medium text-[var(--color-ink)]">
            /relay
          </span>
        </div>
      </div>
      <div className="grid grid-cols-[140px_minmax(0,1fr)] gap-6 px-6 py-3.5">
        <span className="font-[var(--font-mono)] text-[11px] tracking-[0.08em] text-[var(--color-muted)]">
          {t("settings.integrations.scopes.label")}
        </span>
        <div className="flex flex-wrap gap-1.5">
          {scopes.map((s) => (
            <span
              key={s}
              className="inline-flex items-center border border-[var(--color-line)] bg-[var(--color-paper-2)] px-2 py-[3px] font-[var(--font-mono)] text-[11px] text-[var(--color-ink)]"
            >
              {s}
            </span>
          ))}
        </div>
      </div>
    </div>
  );
}

function EmptyConnected({
  onConnect,
  connecting,
}: {
  onConnect: () => void;
  connecting: boolean;
}) {
  const { t } = useT();
  return (
    <div className="flex flex-col items-center gap-3 border border-dashed border-[var(--color-line)] bg-[var(--color-card)] px-6 py-10 text-center">
      <Slack className="h-8 w-8 text-[var(--color-muted)]" strokeWidth={1.5} />
      <div className="font-[var(--font-display)] text-[16px] font-semibold text-[var(--color-ink)]">
        {t("settings.integrations.empty.title")}
      </div>
      <p className="max-w-[420px] text-[13px] text-[var(--color-muted)]">
        {t("settings.integrations.empty.body")}
      </p>
      <Button
        variant="primary"
        loading={connecting}
        onClick={onConnect}
        data-testid="slack-connect"
      >
        {t("settings.integrations.connect")}
      </Button>
    </div>
  );
}

function AvailableCard({
  icon,
  name,
  eyebrow,
  description,
  features,
  target,
  onNotify,
}: {
  icon: React.ReactNode;
  name: string;
  eyebrow: string;
  description: string;
  features: string[];
  target: string;
  onNotify: () => void;
}) {
  const { t } = useT();
  return (
    <section className="flex flex-col border border-[var(--color-line)] bg-[var(--color-card)]">
      <header className="flex items-center gap-3 px-5 pt-5 pb-4">
        <div
          aria-hidden
          className="flex h-11 w-11 shrink-0 items-center justify-center border border-[var(--color-line)] bg-[var(--color-paper-2)]"
        >
          {icon}
        </div>
        <div className="flex min-w-0 flex-col">
          <span className="font-[var(--font-display)] text-[16px] font-bold text-[var(--color-ink)]">
            {name}
          </span>
          <span className="font-[var(--font-mono)] text-[10px] tracking-[0.1em] text-[var(--color-muted)]">
            {eyebrow}
          </span>
        </div>
      </header>
      <p className="px-5 pb-4 text-[12px] leading-relaxed text-[var(--color-muted)]">
        {description}
      </p>
      <ul className="flex flex-col gap-1.5 border-y border-[var(--color-line)] bg-[var(--color-paper-2)] px-5 py-3.5">
        {features.map((f) => (
          <li
            key={f}
            className="flex items-center gap-2 text-[12px] text-[var(--color-muted)]"
          >
            <Circle className="h-3 w-3" strokeWidth={1.5} />
            {f}
          </li>
        ))}
      </ul>
      <div className="flex items-center justify-between px-5 py-3.5">
        <span className="flex items-center gap-1.5 font-[var(--font-mono)] text-[11px] text-[var(--color-muted)]">
          <Calendar className="h-3 w-3" strokeWidth={1.5} />
          {target}
        </span>
        <button
          type="button"
          onClick={onNotify}
          className="inline-flex cursor-pointer items-center gap-1.5 border border-[var(--color-line)] bg-[var(--color-card)] px-3 py-1.5 text-[12px] font-medium text-[var(--color-ink)] transition-colors duration-150 ease-out hover:bg-[var(--color-paper-2)]"
        >
          <BellPlus className="h-3 w-3" strokeWidth={1.5} />
          {t("settings.integrations.notifyMe")}
        </button>
      </div>
    </section>
  );
}

/** "Mar 14, 2026". Matches the Pencil design and renders identically
 *  across locales since the date format is fixed (en-US short month). */
function formatInstalledAt(iso: string): string {
  return new Date(iso).toLocaleDateString("en-US", {
    month: "short",
    day: "numeric",
    year: "numeric",
  });
}

/** Until we wire a member lookup for the installer, render the UUID
 *  short-form so the row stays readable. */
function shortenUserId(id: string): string {
  return id.length > 8 ? `${id.slice(0, 8)}…` : id;
}
