import { useMemo, useState } from "react";
import {
  Crown,
  Download,
  MoreHorizontal,
  Plus,
  Search,
  Shield,
  User as UserIcon,
} from "lucide-react";
import {
  SettingsBreadcrumb,
  SettingsLayout,
} from "../components/templates/SettingsLayout";
import { Button } from "../components/atoms/Button";
import { Monogram } from "../components/atoms/Monogram";
import { Dropdown } from "../components/molecules/Dropdown";
import {
  DataTable,
  DataTableBody,
  DataTableCell,
  DataTableColumnHeader,
  DataTableEmpty,
  DataTableFooter,
  DataTableHead,
  DataTableHeaderRow,
  DataTableRow,
} from "../components/core/DataTable";
import {
  useChangeMemberRole,
  useMembers,
  useOrg,
  useRemoveMember,
  useResendInvite,
  useRevokeInvite,
} from "../hooks/useOrg";
import { useT } from "../i18n";
import { useTimeAgo } from "../lib/time";
import { cn } from "../lib/utils";
import type { MemberRow, MemberStatus, Role } from "../types/api";
import { InviteModal } from "../components/organisms/InviteModal";

type StatusKey = "all" | "active" | "invited" | "expired";

export function SettingsMembers() {
  const { t } = useT();
  const timeAgo = useTimeAgo();
  const orgQuery = useOrg();
  const org = orgQuery.data;

  const [q, setQ] = useState("");
  const [statusFilter, setStatusFilter] = useState<StatusKey>("all");
  const [page, setPage] = useState(1);
  const [perPage] = useState(10);
  const [inviteOpen, setInviteOpen] = useState(false);

  const queryArgs = useMemo(
    () => ({
      q: q.trim() || undefined,
      status: statusFilter === "all" ? undefined : (statusFilter as MemberStatus),
      page,
      per_page: perPage,
    }),
    [q, statusFilter, page, perPage],
  );

  const members = useMembers(queryArgs);
  const changeRole = useChangeMemberRole();
  const removeMember = useRemoveMember();
  const resendInvite = useResendInvite();
  const revokeInvite = useRevokeInvite();

  const data = members.data;
  const counts = data?.counts ?? { all: 0, active: 0, invited: 0, expired: 0 };
  const rows = data?.rows ?? [];
  const totalPages = Math.max(1, Math.ceil((data?.total ?? 0) / perPage));

  const canManage = org?.role === "owner" || org?.role === "admin";

  return (
    <SettingsLayout active="members">
      <SettingsBreadcrumb
        trail={[
          { label: t("settings.breadcrumb.workspace") },
          { label: t("settings.breadcrumb.settings") },
          { label: t("settings.nav.members"), current: true },
        ]}
      />
      <header className="flex items-end justify-between gap-4 border-b border-[var(--color-line)] px-8 pt-2 pb-6">
        <div className="min-w-0">
          <h1 className="font-[var(--font-display)] text-[32px] leading-tight font-bold text-[var(--color-ink)]">
            {t("settings.members.title")}
          </h1>
          <p className="mt-1 max-w-[60ch] text-[14px] text-[var(--color-muted)]">
            {t("settings.members.subtitle")}
          </p>
        </div>
        {canManage ? (
          <Button
            variant="primary"
            onClick={() => setInviteOpen(true)}
            data-testid="settings-members-invite"
          >
            <Plus className="h-3.5 w-3.5" strokeWidth={2} />
            {t("settings.members.invite")}
          </Button>
        ) : null}
      </header>

      <div className="min-h-0 flex-1 overflow-auto p-8">
        <div className="flex flex-col gap-5">

        {/* Toolbar */}
        <div className="flex flex-wrap items-center gap-3 border-b border-[var(--color-line)] pb-3">
          <div className="relative w-[260px]">
            <Search
              className="absolute top-1/2 left-2.5 h-3.5 w-3.5 -translate-y-1/2 text-[var(--color-muted)]"
              strokeWidth={1.75}
            />
            <input
              value={q}
              onChange={(e) => {
                setQ(e.target.value);
                setPage(1);
              }}
              placeholder={t("settings.members.search.placeholder")}
              className="w-full border border-[var(--color-line)] bg-[var(--color-card)] py-1.5 pr-3 pl-7 font-[var(--font-mono)] text-[12px] outline-none focus:ring-1 focus:ring-[var(--color-moss)]"
              data-testid="settings-members-search"
            />
          </div>
          <div className="flex items-center gap-1.5">
            <FilterTab
              active={statusFilter === "all"}
              onClick={() => {
                setStatusFilter("all");
                setPage(1);
              }}
              count={counts.all}
              label={t("settings.members.filter.all", { count: counts.all })}
              testId="filter-all"
            />
            <FilterTab
              active={statusFilter === "active"}
              onClick={() => {
                setStatusFilter("active");
                setPage(1);
              }}
              count={counts.active}
              label={t("settings.members.filter.active", { count: counts.active })}
              testId="filter-active"
            />
            <FilterTab
              active={statusFilter === "invited"}
              onClick={() => {
                setStatusFilter("invited");
                setPage(1);
              }}
              count={counts.invited}
              label={t("settings.members.filter.invited", { count: counts.invited })}
              testId="filter-invited"
            />
            <FilterTab
              active={statusFilter === "expired"}
              onClick={() => {
                setStatusFilter("expired");
                setPage(1);
              }}
              count={counts.expired}
              label={t("settings.members.filter.expired", { count: counts.expired })}
              testId="filter-expired"
            />
          </div>
          <button
            type="button"
            className="ml-auto inline-flex h-7 cursor-pointer items-center gap-1.5 border border-[var(--color-line)] bg-[var(--color-card)] px-2.5 font-[var(--font-mono)] text-[11px] text-[var(--color-muted)] hover:text-[var(--color-ink)]"
            onClick={() => {
              // Mock-only: trigger download from current cached rows.
              const csv = [
                "email,role,status,joined_at",
                ...rows.map((r) =>
                  [r.email, r.role, r.status, r.joined_at].join(","),
                ),
              ].join("\n");
              const blob = new Blob([csv], { type: "text/csv" });
              const url = URL.createObjectURL(blob);
              const a = document.createElement("a");
              a.href = url;
              a.download = "members.csv";
              a.click();
              URL.revokeObjectURL(url);
            }}
          >
            <Download className="h-3 w-3" strokeWidth={1.75} />
            {t("settings.members.export")}
          </button>
        </div>

        {/* Table */}
        <DataTable caption={t("settings.members.title")}>
          <DataTableHead>
            <DataTableHeaderRow>
              <DataTableColumnHeader>
                {t("settings.members.table.person")}
              </DataTableColumnHeader>
              <DataTableColumnHeader>
                {t("settings.members.table.role")}
              </DataTableColumnHeader>
              <DataTableColumnHeader>
                {t("settings.members.table.status")}
              </DataTableColumnHeader>
              <DataTableColumnHeader>
                {t("settings.members.table.joined")}
              </DataTableColumnHeader>
              <DataTableColumnHeader align="right">
                <span className="sr-only">Actions</span>
              </DataTableColumnHeader>
            </DataTableHeaderRow>
          </DataTableHead>
          <DataTableBody>
            {rows.length === 0 ? (
              <DataTableEmpty cols={5}>
                {t("settings.members.empty")}
              </DataTableEmpty>
            ) : (
              rows.map((row) => (
                <MemberRowView
                  key={
                    row.kind === "member"
                      ? `m-${row.user_id}`
                      : `i-${row.invite_id}`
                  }
                  row={row}
                  canManage={canManage}
                  timeAgo={timeAgo}
                  onChangeRole={(role) =>
                    row.user_id && changeRole.mutate({ userId: row.user_id, role })
                  }
                  onRemove={() =>
                    row.user_id && removeMember.mutate(row.user_id)
                  }
                  onResend={() =>
                    row.invite_id && resendInvite.mutate(row.invite_id)
                  }
                  onRevoke={() =>
                    row.invite_id && revokeInvite.mutate(row.invite_id)
                  }
                />
              ))
            )}
          </DataTableBody>
          <DataTableFooter>
            <div className="flex items-center justify-between gap-3 font-[var(--font-mono)] text-[11px] text-[var(--color-muted)]">
              <span>
                {t("settings.members.pagination.range", {
                  from: rows.length,
                  total: data?.total ?? 0,
                })}
              </span>
              <div className="flex items-center gap-1">
                <button
                  type="button"
                  disabled={page <= 1}
                  onClick={() => setPage((p) => Math.max(1, p - 1))}
                  className="cursor-pointer border border-[var(--color-line)] px-2 py-0.5 hover:text-[var(--color-ink)] disabled:cursor-not-allowed disabled:opacity-40"
                  data-testid="members-prev"
                >
                  ‹ {t("settings.members.pagination.prev")}
                </button>
                {Array.from({ length: totalPages }).map((_, i) => (
                  <button
                    key={i}
                    type="button"
                    onClick={() => setPage(i + 1)}
                    className={cn(
                      "cursor-pointer border px-2 py-0.5",
                      i + 1 === page
                        ? "border-[var(--color-ink)] bg-[var(--color-ink)] text-[var(--color-paper)]"
                        : "border-[var(--color-line)] hover:text-[var(--color-ink)]",
                    )}
                  >
                    {i + 1}
                  </button>
                ))}
                <button
                  type="button"
                  disabled={page >= totalPages}
                  onClick={() => setPage((p) => Math.min(totalPages, p + 1))}
                  className="cursor-pointer border border-[var(--color-line)] px-2 py-0.5 hover:text-[var(--color-ink)] disabled:cursor-not-allowed disabled:opacity-40"
                  data-testid="members-next"
                >
                  {t("settings.members.pagination.next")} ›
                </button>
              </div>
            </div>
          </DataTableFooter>
        </DataTable>
        </div>
      </div>

      {org ? (
        <InviteModal
          open={inviteOpen}
          onClose={() => setInviteOpen(false)}
          orgName={org.name}
          orgSlug={org.slug}
          callerRole={org.role}
        />
      ) : null}
    </SettingsLayout>
  );
}

function FilterTab({
  active,
  onClick,
  count,
  label,
  testId,
}: {
  active: boolean;
  onClick: () => void;
  count: number;
  label: string;
  testId?: string;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      aria-pressed={active}
      data-testid={testId}
      className={cn(
        "inline-flex h-7 cursor-pointer items-center gap-1.5 border px-2.5 font-[var(--font-mono)] text-[11px] tracking-[0.02em] transition-colors",
        active
          ? "border-[var(--color-ink)] bg-[var(--color-ink)] text-[var(--color-paper)]"
          : "border-[var(--color-line)] bg-[var(--color-card)] text-[var(--color-muted)] hover:text-[var(--color-ink)]",
      )}
    >
      {label}
      <span className="sr-only">{count}</span>
    </button>
  );
}

function RoleBadge({ role }: { role: Role }) {
  const Icon = role === "owner" ? Crown : role === "admin" ? Shield : UserIcon;
  const label =
    role === "owner" ? "Owner" : role === "admin" ? "Admin" : "Member";
  return (
    <span
      className={cn(
        "inline-flex items-center gap-1 border px-2 py-0.5 font-[var(--font-mono)] text-[10.5px] tracking-[0.04em] uppercase",
        role === "owner"
          ? "border-[var(--color-amber)] text-[var(--color-amber)]"
          : role === "admin"
            ? "border-[var(--color-moss)] text-[var(--color-moss-deep)]"
            : "border-[var(--color-line-2)] text-[var(--color-muted)]",
      )}
    >
      <Icon className="h-2.5 w-2.5" strokeWidth={2} />
      {label}
    </span>
  );
}

function StatusPill({ status, when }: { status: MemberStatus; when?: string }) {
  const tone =
    status === "active"
      ? "text-[var(--color-moss-deep)]"
      : status === "invited"
        ? "text-[var(--color-amber)]"
        : "text-[var(--color-rose)]";
  const dot =
    status === "active"
      ? "bg-[var(--color-moss)]"
      : status === "invited"
        ? "bg-[var(--color-amber)]"
        : "bg-[var(--color-rose)]";
  return (
    <span className={cn("inline-flex items-center gap-1.5 text-[12px]", tone)}>
      <span className={cn("h-1.5 w-1.5 rounded-full", dot)} aria-hidden />
      <span className="font-medium capitalize">{status}</span>
      {when ? (
        <span className="text-[var(--color-muted)]">· {when}</span>
      ) : null}
    </span>
  );
}

function MemberRowView({
  row,
  canManage,
  timeAgo,
  onChangeRole,
  onRemove,
  onResend,
  onRevoke,
}: {
  row: MemberRow;
  canManage: boolean;
  timeAgo: (iso: string | null) => string;
  onChangeRole: (role: Role) => void;
  onRemove: () => void;
  onResend: () => void;
  onRevoke: () => void;
}) {
  const { t } = useT();
  const name = row.display_name ?? row.email;
  return (
    <DataTableRow>
      <DataTableCell>
        <div className="flex items-center gap-2.5">
          <Monogram
            name={name}
            id={row.user_id ?? row.invite_id ?? row.email}
            size={32}
            tone="moss"
            avatarUrl={row.avatar_url ?? undefined}
          />
          <div className="min-w-0">
            <div className="truncate text-[13px] font-medium text-[var(--color-ink)]">
              {name}
            </div>
            <div className="truncate font-[var(--font-mono)] text-[11px] text-[var(--color-muted)]">
              {row.email}
            </div>
          </div>
        </div>
      </DataTableCell>
      <DataTableCell>
        <RoleBadge role={row.role} />
      </DataTableCell>
      <DataTableCell>
        <StatusPill
          status={row.status}
          when={
            row.status === "active"
              ? undefined
              : row.expires_at
                ? timeAgo(row.expires_at)
                : undefined
          }
        />
      </DataTableCell>
      <DataTableCell>
        <div className="font-[var(--font-mono)] text-[11.5px] text-[var(--color-ink)]">
          {row.kind === "member"
            ? t("settings.members.joined", { when: timeAgo(row.joined_at) })
            : t("settings.members.invited", { when: timeAgo(row.joined_at) })}
        </div>
      </DataTableCell>
      <DataTableCell align="right">
        {canManage ? (
          <Dropdown
            renderTrigger={({ toggle, open }) => (
              <button
                type="button"
                onClick={toggle}
                aria-haspopup="menu"
                aria-expanded={open}
                aria-label="Row actions"
                className="inline-flex h-7 w-7 cursor-pointer items-center justify-center text-[var(--color-muted)] hover:text-[var(--color-ink)]"
                data-testid={`row-actions-${row.email}`}
              >
                <MoreHorizontal className="h-4 w-4" strokeWidth={1.75} />
              </button>
            )}
            menuClassName="min-w-[200px] border border-[var(--color-line)] bg-[var(--color-card)] py-1 shadow-md"
          >
            {({ close }) => (
              <ul role="menu" className="text-[12.5px]">
                {row.kind === "member" ? (
                  <>
                    {(["owner", "admin", "member"] as Role[]).map((r) => (
                      <li key={r}>
                        <button
                          type="button"
                          role="menuitem"
                          onClick={() => {
                            close();
                            onChangeRole(r);
                          }}
                          className="block w-full cursor-pointer px-3 py-1.5 text-left hover:bg-[var(--color-paper-2)]"
                        >
                          {t("settings.members.row.change_role")} → {r}
                        </button>
                      </li>
                    ))}
                    <li>
                      <button
                        type="button"
                        role="menuitem"
                        onClick={() => {
                          close();
                          onRemove();
                        }}
                        className="block w-full cursor-pointer px-3 py-1.5 text-left text-[var(--color-rose)] hover:bg-[var(--color-rose-soft)]"
                      >
                        {t("settings.members.row.remove")}
                      </button>
                    </li>
                  </>
                ) : (
                  <>
                    <li>
                      <button
                        type="button"
                        role="menuitem"
                        onClick={() => {
                          close();
                          onResend();
                        }}
                        className="block w-full cursor-pointer px-3 py-1.5 text-left hover:bg-[var(--color-paper-2)]"
                      >
                        {t("settings.members.row.resend")}
                      </button>
                    </li>
                    <li>
                      <button
                        type="button"
                        role="menuitem"
                        onClick={() => {
                          close();
                          onRevoke();
                        }}
                        className="block w-full cursor-pointer px-3 py-1.5 text-left text-[var(--color-rose)] hover:bg-[var(--color-rose-soft)]"
                      >
                        {t("settings.members.row.revoke")}
                      </button>
                    </li>
                  </>
                )}
              </ul>
            )}
          </Dropdown>
        ) : null}
      </DataTableCell>
    </DataTableRow>
  );
}
