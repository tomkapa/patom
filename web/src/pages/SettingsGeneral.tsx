import { useEffect, useMemo, useState } from "react";
import { useNavigate } from "react-router-dom";
import { useQueryClient } from "@tanstack/react-query";
import { Save, LogOut, Trash2 } from "lucide-react";
import {
  SettingsBreadcrumb,
  SettingsLayout,
  SettingsPageHeader,
} from "../components/templates/SettingsLayout";
import { Button } from "../components/atoms/Button";
import { Spinner } from "../components/atoms/Spinner";
import { SectionCard } from "../components/molecules/SectionCard";
import { Modal, ModalFooter, ModalHeader } from "../components/molecules/Modal";
import { Select } from "../components/molecules/Select";
import { PrefixInput } from "../components/core/PrefixInput";
import {
  ORG_KEY,
  useLeaveOrg,
  useOrg,
  useUpdateOrg,
} from "../hooks/useOrg";
import { api } from "../lib/api";
import { ApiError } from "../lib/errors";
import { useT } from "../i18n";
import { useAuthStore } from "../stores/authStore";
import type { Language } from "../types/api";

const SLUG_RE = /^[a-z0-9][a-z0-9-]{0,62}$/;

export function SettingsGeneral() {
  const { t } = useT();
  const nav = useNavigate();
  const qc = useQueryClient();
  const orgQuery = useOrg();
  const updateOrg = useUpdateOrg();
  const leaveOrg = useLeaveOrg();
  const org = orgQuery.data;

  const [name, setName] = useState("");
  const [slug, setSlug] = useState("");
  const [language, setLanguage] = useState<Language>("en");
  const [slugError, setSlugError] = useState<string | null>(null);
  const [serverError, setServerError] = useState<string | null>(null);
  const [savedToast, setSavedToast] = useState<boolean>(false);
  const [leaveOpen, setLeaveOpen] = useState(false);
  const [deleteOpen, setDeleteOpen] = useState(false);
  const [deleteInput, setDeleteInput] = useState("");

  useEffect(() => {
    if (org) {
      setName(org.name);
      setSlug(org.slug);
      setLanguage(org.default_language);
    }
  }, [org?.id]);

  const dirty = useMemo(() => {
    if (!org) return false;
    return (
      name !== org.name ||
      slug !== org.slug ||
      language !== org.default_language
    );
  }, [org, name, slug, language]);

  const canSave = dirty && !slugError;
  const [langSaving, setLangSaving] = useState(false);

  const onSave = async () => {
    if (!org) return;
    setServerError(null);
    const patch: { name?: string; slug?: string } = {};
    if (name !== org.name) patch.name = name.trim();
    if (slug !== org.slug) patch.slug = slug.trim();
    const langChanged = language !== org.default_language;
    try {
      if (patch.name !== undefined || patch.slug !== undefined) {
        await updateOrg.mutateAsync(patch);
      }
      if (langChanged) {
        setLangSaving(true);
        const { default_language } = await api.setOrgLanguage(language);
        // Mirror into auth store so i18n (driven by the active org's
        // default_language) flips immediately without a /me re-poll.
        // Read the latest snapshot to avoid clobbering a concurrent
        // /me refresh.
        const latest = useAuthStore.getState().me;
        if (latest) {
          useAuthStore.getState().setMe({
            ...latest,
            orgs: latest.orgs.map((o) =>
              o.id === latest.active_org_id ? { ...o, default_language } : o,
            ),
          });
        }
        qc.invalidateQueries({ queryKey: ORG_KEY });
      }
      setSavedToast(true);
      window.setTimeout(() => setSavedToast(false), 1800);
    } catch (e) {
      if (e instanceof ApiError) {
        const body = e.body ?? "";
        if (e.status === 409 && body.includes("org_slug.taken")) {
          setSlugError(t("settings.general.identity.slug.taken"));
        } else {
          setServerError(body || e.message);
        }
      }
    } finally {
      setLangSaving(false);
    }
  };

  if (orgQuery.isLoading) {
    return (
      <SettingsLayout active="general">
        <div className="flex h-full items-center justify-center">
          <Spinner />
        </div>
      </SettingsLayout>
    );
  }
  if (!org) {
    return <SettingsLayout active="general"><div /></SettingsLayout>;
  }

  return (
    <SettingsLayout active="general">
      <SettingsBreadcrumb
        trail={[
          { label: t("settings.breadcrumb.workspace") },
          { label: t("settings.breadcrumb.settings") },
          { label: t("settings.nav.general"), current: true },
        ]}
      />
      <SettingsPageHeader
        title={t("settings.general.title")}
        subtitle={t("settings.general.subtitle")}
        right={
          <>
            {savedToast ? (
              <span className="font-[var(--font-mono)] text-[11px] tracking-[0.06em] text-[var(--color-moss-deep)] uppercase">
                ✓ {t("settings.general.savedToast")}
              </span>
            ) : null}
            <Button
              variant="primary"
              disabled={!canSave}
              loading={updateOrg.isPending || langSaving}
              onClick={onSave}
              data-testid="settings-general-save"
            >
              <Save className="h-3.5 w-3.5" strokeWidth={2} />
              {t("settings.general.save")}
            </Button>
          </>
        }
      />

      <div className="min-h-0 flex-1 overflow-auto p-8">
        {serverError ? (
          <div className="mb-4 border border-[var(--color-rose)] bg-[var(--color-rose-soft)] px-3 py-2 text-[12px] text-[var(--color-rose)]">
            {serverError}
          </div>
        ) : null}

        <div className="flex flex-col gap-6">

        {/* IDENTITY */}
        <SectionCard
          header={
            <div className="flex items-center justify-between border-b border-[var(--color-line)] bg-[var(--color-paper-2)] px-5 py-2.5">
              <span className="font-[var(--font-mono)] text-[11px] font-bold tracking-[0.09em] text-[var(--color-muted-foreground)] uppercase">
                {t("settings.general.identity.title")}
              </span>
              <span className="text-[12px] text-[var(--color-fg-muted)]">
                {t("settings.general.identity.helper")}
              </span>
            </div>
          }
          bodyClassName="grid grid-cols-1 gap-5 px-5 py-5"
        >
          <Field
            label={t("settings.general.identity.name")}
            helper={t("settings.general.identity.name.helper")}
          >
            <input
              value={name}
              onChange={(e) => setName(e.target.value)}
              maxLength={200}
              className="h-9 w-full border border-[var(--color-line)] bg-[var(--color-card)] px-3 text-[13px] text-[var(--color-ink)] outline-none focus:ring-1 focus:ring-[var(--color-moss)]"
              data-testid="settings-general-name"
            />
          </Field>
          <Field
            label={t("settings.general.identity.slug")}
            helper={t("settings.general.identity.slug.helper")}
          >
            <PrefixInput
              prefix="relay.app/"
              value={slug}
              onChange={(e) => {
                const v = e.target.value;
                setSlug(v);
                if (v && !SLUG_RE.test(v)) {
                  setSlugError(t("settings.general.identity.slug.invalid"));
                } else {
                  setSlugError(null);
                }
              }}
              invalid={Boolean(slugError)}
              maxLength={63}
              data-testid="settings-general-slug"
            />
            {slugError ? (
              <div className="mt-1 font-[var(--font-mono)] text-[11px] text-[var(--color-rose)]">
                {slugError}
              </div>
            ) : null}
          </Field>
        </SectionCard>

        {/* DEFAULTS */}
        <SectionCard
          header={
            <div className="flex items-center justify-between border-b border-[var(--color-line)] bg-[var(--color-paper-2)] px-5 py-2.5">
              <span className="font-[var(--font-mono)] text-[11px] font-bold tracking-[0.09em] text-[var(--color-muted-foreground)] uppercase">
                {t("settings.general.defaults.title")}
              </span>
              <span className="text-[12px] text-[var(--color-fg-muted)]">
                {t("settings.general.defaults.helper")}
              </span>
            </div>
          }
          bodyClassName="grid grid-cols-1 gap-5 px-5 py-5"
        >
          <Field
            label={t("settings.general.defaults.language")}
            helper={t("settings.general.defaults.language.helper")}
          >
            <Select<Language>
              value={language}
              onChange={setLanguage}
              options={[
                { value: "en", label: "🇺🇸 English (United States)" },
                { value: "vi", label: "🇻🇳 Tiếng Việt" },
              ]}
              variant="default"
              ariaLabel={t("settings.general.defaults.language")}
            />
          </Field>
        </SectionCard>

        {/* DANGER ZONE */}
        <SectionCard
          tone="danger"
          header={
            <div className="flex items-center justify-between border-b border-[var(--color-rose)] bg-[var(--color-rose-soft)]/40 px-5 py-2.5">
              <span className="font-[var(--font-mono)] text-[11px] font-bold tracking-[0.09em] text-[var(--color-rose)] uppercase">
                {t("settings.general.danger.title")}
              </span>
              <span className="text-[12px] text-[var(--color-muted-foreground)]">
                {t("settings.general.danger.helper")}
              </span>
            </div>
          }
          bodyClassName="flex flex-col"
        >
          <DangerRow
            icon={<LogOut className="h-4 w-4 text-[var(--color-rose)]" strokeWidth={1.75} />}
            title={t("settings.general.danger.leave.title")}
            body={t("settings.general.danger.leave.body", { name: org.name })}
            cta={t("settings.general.danger.leave.cta")}
            onClick={() => setLeaveOpen(true)}
            data-testid="settings-general-leave"
          />
          <DangerRow
            icon={<Trash2 className="h-4 w-4 text-[var(--color-rose)]" strokeWidth={1.75} />}
            title={t("settings.general.danger.delete.title")}
            body={t("settings.general.danger.delete.body", { name: org.name })}
            cta={t("settings.general.danger.delete.cta")}
            onClick={() => setDeleteOpen(true)}
            data-testid="settings-general-delete"
            disabled={org.role !== "owner"}
          />
        </SectionCard>
        </div>
      </div>

      {/* Leave-workspace confirm */}
      <Modal
        open={leaveOpen}
        onClose={() => setLeaveOpen(false)}
        ariaLabel={t("settings.general.danger.leave.title")}
        width={420}
      >
        <ModalHeader
          eyebrow={t("settings.general.danger.title")}
          title={t("settings.general.danger.leave.title")}
          onClose={() => setLeaveOpen(false)}
        />
        <div className="px-5 py-4 text-[13px] text-[var(--color-ink)]">
          {t("settings.general.danger.leave.confirm", { name: org.name })}
        </div>
        <ModalFooter>
          <Button variant="ghost" onClick={() => setLeaveOpen(false)}>
            {t("settings.general.cancel")}
          </Button>
          <button
            type="button"
            onClick={async () => {
              try {
                await leaveOrg.mutateAsync();
                nav("/sign-in", { replace: true });
              } catch (e) {
                if (e instanceof ApiError && e.body?.includes("last_owner")) {
                  setServerError(t("settings.invite.error.lastOwner"));
                  setLeaveOpen(false);
                }
              }
            }}
            className="inline-flex h-[34px] cursor-pointer items-center justify-center gap-1.5 border border-[var(--color-rose)] bg-[var(--color-rose)] px-3 font-[var(--font-mono)] text-[12px] uppercase tracking-[0.06em] text-white hover:opacity-90"
          >
            {t("settings.general.danger.leave.cta")}
          </button>
        </ModalFooter>
      </Modal>

      {/* Delete-workspace confirm */}
      <Modal
        open={deleteOpen}
        onClose={() => {
          setDeleteOpen(false);
          setDeleteInput("");
        }}
        ariaLabel={t("settings.general.danger.delete.title")}
        width={460}
      >
        <ModalHeader
          eyebrow={t("settings.general.danger.title")}
          title={t("settings.general.danger.delete.confirm.title", {
            name: org.name,
          })}
          onClose={() => {
            setDeleteOpen(false);
            setDeleteInput("");
          }}
        />
        <div className="px-5 py-4 text-[13px] text-[var(--color-ink)]">
          <p className="mb-3">
            {t("settings.general.danger.delete.confirm.body", {
              slug: org.slug,
            })}
          </p>
          <input
            value={deleteInput}
            onChange={(e) => setDeleteInput(e.target.value)}
            placeholder={t(
              "settings.general.danger.delete.confirm.placeholder",
            )}
            className="h-9 w-full border border-[var(--color-line)] bg-[var(--color-card)] px-3 font-[var(--font-mono)] text-[13px] text-[var(--color-ink)] outline-none focus:ring-1 focus:ring-[var(--color-moss)]"
          />
        </div>
        <ModalFooter>
          <Button
            variant="ghost"
            onClick={() => {
              setDeleteOpen(false);
              setDeleteInput("");
            }}
          >
            {t("settings.general.cancel")}
          </Button>
          <button
            type="button"
            disabled={deleteInput !== org.slug}
            className="inline-flex h-[34px] cursor-pointer items-center justify-center gap-1.5 border border-[var(--color-rose)] bg-[var(--color-rose)] px-3 font-[var(--font-mono)] text-[12px] uppercase tracking-[0.06em] text-white hover:opacity-90 disabled:cursor-not-allowed disabled:opacity-50"
          >
            {t("settings.general.danger.delete.cta")}
          </button>
        </ModalFooter>
      </Modal>
    </SettingsLayout>
  );
}

function Field({
  label,
  helper,
  children,
}: {
  label: string;
  helper?: string;
  children: React.ReactNode;
}) {
  return (
    <div className="grid grid-cols-[280px_1fr] gap-8">
      <div>
        <div className="text-[13px] font-semibold text-[var(--color-ink)]">
          {label}
        </div>
        {helper ? (
          <div className="mt-1 text-[12px] leading-snug text-[var(--color-muted-foreground)]">
            {helper}
          </div>
        ) : null}
      </div>
      <div className="min-w-0">{children}</div>
    </div>
  );
}

function DangerRow({
  icon,
  title,
  body,
  cta,
  onClick,
  disabled,
  "data-testid": testId,
}: {
  icon: React.ReactNode;
  title: string;
  body: string;
  cta: string;
  onClick: () => void;
  disabled?: boolean;
  "data-testid"?: string;
}) {
  return (
    <div className="flex items-start gap-4 border-b border-[var(--color-line)] px-5 py-4 last:border-b-0">
      <div className="mt-0.5 shrink-0">{icon}</div>
      <div className="min-w-0 flex-1">
        <div className="text-[13px] font-semibold text-[var(--color-ink)]">
          {title}
        </div>
        <div className="mt-0.5 text-[12px] leading-snug text-[var(--color-muted-foreground)]">
          {body}
        </div>
      </div>
      <button
        type="button"
        onClick={onClick}
        disabled={disabled}
        data-testid={testId}
        className="inline-flex h-8 shrink-0 cursor-pointer items-center gap-1.5 border border-[var(--color-rose)] bg-transparent px-3.5 text-[12px] font-medium text-[var(--color-rose)] hover:bg-[var(--color-rose-soft)] disabled:cursor-not-allowed disabled:opacity-40"
      >
        {cta}
      </button>
    </div>
  );
}
