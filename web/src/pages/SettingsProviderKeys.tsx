import { useMemo, useState } from "react";
import { KeyRound, Check, X, Loader2 } from "lucide-react";

import {
  SettingsBreadcrumb,
  SettingsLayout,
  SettingsPageHeader,
} from "../components/templates/SettingsLayout";
import { Button } from "../components/atoms/Button";
import { Spinner } from "../components/atoms/Spinner";
import { SectionCard } from "../components/molecules/SectionCard";
import {
  useDeleteProviderCredentials,
  useProviderCredentials,
  usePutProviderCredentials,
  useValidateProviderCredentials,
} from "../hooks/useProviderCredentials";
import { useOrgCredits } from "../hooks/useOrgCredits";
import { useModels } from "../hooks/useModels";
import { useAuthStore } from "../stores/authStore";
import { useT } from "../i18n";
import type { ProviderCredentialView } from "../types/api";

/** Display labels for the closed provider set. */
const PROVIDER_LABEL: Record<string, string> = {
  anthropic: "Anthropic",
  openai: "OpenAI",
  deepseek: "DeepSeek",
};

export function SettingsProviderKeys() {
  const { t } = useT();
  const role = useAuthStore((s) => s.me?.role ?? null);
  const canEdit = role === "owner" || role === "admin";

  const listQuery = useProviderCredentials();
  const credits = useOrgCredits().data;
  // Out of free credit = credit tracking on (granted > 0) and balance drained.
  const outOfCredit =
    !!credits &&
    credits.granted_total_micro_usd > 0 &&
    credits.balance_micro_usd <= 0;

  if (listQuery.isLoading) {
    return (
      <SettingsLayout active="provider-keys">
        <div className="flex h-full items-center justify-center">
          <Spinner />
        </div>
      </SettingsLayout>
    );
  }

  const rows = listQuery.data ?? [];
  const anyKeySet = rows.some((r) => r.status === "active");

  return (
    <SettingsLayout active="provider-keys">
      <SettingsBreadcrumb
        trail={[
          { label: t("settings.breadcrumb.workspace") },
          { label: t("settings.breadcrumb.settings") },
          { label: t("settings.nav.providerKeys"), current: true },
        ]}
      />
      <SettingsPageHeader
        title={t("settings.nav.providerKeys")}
        subtitle={t("settings.providerKeys.subtitle")}
      />

      <div className="min-h-0 flex-1 overflow-auto p-4 md:p-8">
        <div className="flex flex-col gap-6">
          {outOfCredit ? (
            <div
              className="flex items-start gap-2.5 border border-[var(--color-amber)] bg-[var(--color-amber-soft,transparent)] px-4 py-3 text-[13px] text-[var(--color-ink)]"
              data-testid="provider-keys-out-of-credit"
            >
              <KeyRound
                className="mt-0.5 h-4 w-4 shrink-0 text-[var(--color-amber)]"
                strokeWidth={1.75}
              />
              <span>{t("settings.providerKeys.outOfCredit")}</span>
            </div>
          ) : null}

          {!canEdit ? (
            <div className="font-[var(--font-mono)] text-[11px] tracking-[0.06em] text-[var(--color-muted-foreground)] uppercase">
              {t("settings.providerKeys.memberHint")}
            </div>
          ) : null}

          <SectionCard
            header={
              <div className="flex items-center justify-between border-b border-[var(--color-line)] bg-[var(--color-paper-2)] px-5 py-2.5">
                <span className="font-[var(--font-mono)] text-[11px] font-bold tracking-[0.09em] text-[var(--color-muted-foreground)] uppercase">
                  {t("settings.providerKeys.sectionTitle")}
                </span>
                <span className="text-[12px] text-[var(--color-fg-muted)]">
                  {t("settings.providerKeys.sectionHelper")}
                </span>
              </div>
            }
            bodyClassName="flex flex-col"
          >
            {rows.map((row, i) => (
              <ProviderRow
                key={row.provider}
                row={row}
                canEdit={canEdit}
                isFirstKey={!anyKeySet}
                last={i === rows.length - 1}
              />
            ))}
          </SectionCard>
        </div>
      </div>
    </SettingsLayout>
  );
}

function ProviderRow({
  row,
  canEdit,
  isFirstKey,
  last,
}: {
  row: ProviderCredentialView;
  canEdit: boolean;
  isFirstKey: boolean;
  last: boolean;
}) {
  const { t } = useT();
  const put = usePutProviderCredentials();
  const del = useDeleteProviderCredentials();
  const validate = useValidateProviderCredentials();
  const models = useModels().data ?? [];

  const [editing, setEditing] = useState(false);
  const [apiKey, setApiKey] = useState("");
  const [baseUrl, setBaseUrl] = useState("");
  const [defaultModel, setDefaultModel] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [validateMsg, setValidateMsg] = useState<{
    ok: boolean;
    text: string;
  } | null>(null);

  // Models this provider serves, for the first-key default picker.
  const providerModels = useMemo(
    () => models.filter((m) => m.provider === row.provider),
    [models, row.provider],
  );

  const active = row.status === "active";

  const reset = () => {
    setEditing(false);
    setApiKey("");
    setBaseUrl("");
    setDefaultModel("");
    setError(null);
    setValidateMsg(null);
  };

  const onSave = async () => {
    if (!apiKey.trim()) return;
    setError(null);
    try {
      await put.mutateAsync({
        provider: row.provider,
        body: {
          api_key: apiKey.trim(),
          base_url: baseUrl.trim() || null,
          default_model: isFirstKey && defaultModel ? defaultModel : null,
        },
      });
      reset();
    } catch {
      setError(t("settings.providerKeys.saveError"));
    }
  };

  const onValidate = async () => {
    if (!apiKey.trim()) return;
    setValidateMsg(null);
    const res = await validate.mutateAsync({
      provider: row.provider,
      body: { api_key: apiKey.trim(), base_url: baseUrl.trim() || null },
    });
    if (res.outcome === "ok") {
      setValidateMsg({ ok: true, text: t("settings.providerKeys.validateOk") });
    } else if (res.outcome === "invalid") {
      setValidateMsg({
        ok: false,
        text: t("settings.providerKeys.validateInvalid"),
      });
    } else {
      setValidateMsg({
        ok: false,
        text: t("settings.providerKeys.validateError"),
      });
    }
  };

  return (
    <div
      className={`flex flex-col gap-3 px-5 py-4 ${last ? "" : "border-b border-[var(--color-line)]"}`}
      data-testid={`provider-row-${row.provider}`}
    >
      <div className="flex items-center justify-between gap-3">
        <div className="flex items-center gap-3">
          <span className="text-[14px] font-semibold text-[var(--color-ink)]">
            {PROVIDER_LABEL[row.provider] ?? row.provider}
          </span>
          <span
            className="inline-flex items-center gap-1.5 font-[var(--font-mono)] text-[10px] font-bold tracking-[0.08em] uppercase"
            style={{
              color: active
                ? "var(--color-moss)"
                : "var(--color-muted-foreground)",
            }}
          >
            <span
              aria-hidden
              className="block h-2 w-2 rounded-full"
              style={{
                background: active
                  ? "var(--color-moss)"
                  : "var(--color-line)",
              }}
            />
            {active
              ? t("settings.providerKeys.status.active")
              : t("settings.providerKeys.status.notSet")}
          </span>
        </div>
        {canEdit && !editing ? (
          <div className="flex items-center gap-1.5">
            <Button variant="secondary" onClick={() => setEditing(true)}>
              {active
                ? t("settings.providerKeys.rotate")
                : t("settings.providerKeys.add")}
            </Button>
            {active ? (
              <Button
                variant="danger"
                loading={del.isPending}
                onClick={() => del.mutate(row.provider)}
              >
                {t("settings.providerKeys.remove")}
              </Button>
            ) : null}
          </div>
        ) : null}
      </div>

      {active && !editing ? (
        <div className="flex items-center gap-3 text-[12px] text-[var(--color-muted-foreground)]">
          <span className="font-[var(--font-mono)] text-[var(--color-rail)]">
            {row.masked_key}
          </span>
          {row.base_url ? (
            <span className="font-[var(--font-mono)]">{row.base_url}</span>
          ) : null}
          <span>
            {row.last_validated_at
              ? t("settings.providerKeys.lastValidated", {
                  when: new Date(row.last_validated_at).toLocaleDateString(),
                })
              : t("settings.providerKeys.neverValidated")}
          </span>
        </div>
      ) : null}

      {editing ? (
        <div className="flex flex-col gap-3 border-t border-[var(--color-line)] pt-3">
          <input
            type="password"
            autoComplete="off"
            value={apiKey}
            onChange={(e) => setApiKey(e.target.value)}
            placeholder={t("settings.providerKeys.apiKeyPlaceholder")}
            className="h-9 w-full border border-[var(--color-line)] bg-[var(--color-card)] px-3 text-[13px] text-[var(--color-ink)] outline-none focus:ring-1 focus:ring-[var(--color-moss)]"
            data-testid={`provider-key-input-${row.provider}`}
          />
          <input
            type="text"
            value={baseUrl}
            onChange={(e) => setBaseUrl(e.target.value)}
            placeholder={t("settings.providerKeys.baseUrlPlaceholder")}
            className="h-9 w-full border border-[var(--color-line)] bg-[var(--color-card)] px-3 text-[13px] text-[var(--color-ink)] outline-none focus:ring-1 focus:ring-[var(--color-moss)]"
          />
          {isFirstKey && providerModels.length > 0 ? (
            <label className="flex flex-col gap-1">
              <span className="text-[12px] text-[var(--color-muted-foreground)]">
                {t("settings.providerKeys.defaultModelLabel")} ·{" "}
                {t("settings.providerKeys.defaultModelHelper")}
              </span>
              <select
                value={defaultModel}
                onChange={(e) => setDefaultModel(e.target.value)}
                className="h-9 w-full border border-[var(--color-line)] bg-[var(--color-card)] px-3 text-[13px] text-[var(--color-ink)] outline-none focus:ring-1 focus:ring-[var(--color-moss)]"
              >
                <option value="">—</option>
                {providerModels.map((m) => (
                  <option key={m.id} value={m.id}>
                    {m.id}
                  </option>
                ))}
              </select>
            </label>
          ) : null}

          {validateMsg ? (
            <div
              className="flex items-center gap-1.5 text-[12px]"
              style={{
                color: validateMsg.ok
                  ? "var(--color-moss)"
                  : "var(--color-rose)",
              }}
            >
              {validateMsg.ok ? (
                <Check className="h-3.5 w-3.5" strokeWidth={2} />
              ) : (
                <X className="h-3.5 w-3.5" strokeWidth={2} />
              )}
              {validateMsg.text}
            </div>
          ) : null}
          {error ? (
            <div className="text-[12px] text-[var(--color-rose)]">{error}</div>
          ) : null}

          <div className="flex items-center gap-1.5">
            <Button
              variant="primary"
              disabled={!apiKey.trim()}
              loading={put.isPending}
              onClick={onSave}
              data-testid={`provider-save-${row.provider}`}
            >
              {t("settings.providerKeys.save")}
            </Button>
            <Button
              variant="secondary"
              disabled={!apiKey.trim() || validate.isPending}
              onClick={onValidate}
            >
              {validate.isPending ? (
                <Loader2 className="h-3.5 w-3.5 animate-spin" strokeWidth={2} />
              ) : null}
              {t("settings.providerKeys.validate")}
            </Button>
            <Button variant="ghost" onClick={reset}>
              {t("settings.providerKeys.cancel")}
            </Button>
          </div>
        </div>
      ) : null}
    </div>
  );
}
