import { useEffect, useMemo, useState } from "react";
import { Save, AlertTriangle } from "lucide-react";
import {
  SettingsBreadcrumb,
  SettingsLayout,
  SettingsPageHeader,
} from "../components/templates/SettingsLayout";
import { Button } from "../components/atoms/Button";
import { Spinner } from "../components/atoms/Spinner";
import { Switch } from "../components/atoms/Switch";
import { ProgressBar } from "../components/atoms/ProgressBar";
import { SectionCard } from "../components/molecules/SectionCard";
import { SettingsField } from "../components/molecules/SettingsField";
import { useOrgBilling, useUpdateOrgBilling } from "../hooks/useOrgBilling";
import { formatUSD, microToUsd, usdToMicro } from "../lib/currency";
import { useT } from "../i18n";
import type { TranslationKey } from "../i18n/en";

/** Basis points in 100% — warn threshold is stored in bps, edited in percent. */
const BPS_PER_PERCENT = 100;

export function SettingsBilling() {
  const { t } = useT();
  const budgetQuery = useOrgBilling();
  const updateBudget = useUpdateOrgBilling();
  const budget = budgetQuery.data;

  const canEdit = budget?.role === "owner" || budget?.role === "admin";

  const [unlimited, setUnlimited] = useState(true);
  const [capUsd, setCapUsd] = useState("");
  const [warnPct, setWarnPct] = useState("80");
  const [serverError, setServerError] = useState<string | null>(null);
  const [savedToast, setSavedToast] = useState(false);

  // Hydrate the form once the budget loads (keyed on period_start so a new
  // period doesn't silently re-seed mid-edit).
  useEffect(() => {
    if (!budget) return;
    const cap = budget.monthly_cap_micro_usd;
    setUnlimited(cap === null);
    setCapUsd(cap === null ? "" : String(microToUsd(cap)));
    setWarnPct(String(budget.warn_threshold_bps / BPS_PER_PERCENT));
  }, [budget?.period_start, budget?.monthly_cap_micro_usd, budget?.warn_threshold_bps]);

  const capNumber = Number(capUsd);
  const warnNumber = Number(warnPct);
  const warnValid =
    Number.isFinite(warnNumber) && warnNumber >= 1 && warnNumber <= 100;
  const capValid = unlimited || (Number.isFinite(capNumber) && capNumber > 0);

  const nextCapMicro = unlimited ? null : usdToMicro(capNumber);
  const nextWarnBps = Math.round(warnNumber * BPS_PER_PERCENT);

  const dirty = useMemo(() => {
    if (!budget) return false;
    return (
      nextCapMicro !== budget.monthly_cap_micro_usd ||
      nextWarnBps !== budget.warn_threshold_bps
    );
  }, [budget, nextCapMicro, nextWarnBps]);

  const canSave = canEdit && dirty && warnValid && capValid;

  const onSave = async () => {
    if (!canSave) return;
    setServerError(null);
    try {
      await updateBudget.mutateAsync({
        monthly_cap_micro_usd: nextCapMicro,
        warn_threshold_bps: nextWarnBps,
      });
      setSavedToast(true);
      window.setTimeout(() => setSavedToast(false), 1800);
    } catch {
      setServerError(t("settings.budget.error"));
    }
  };

  if (budgetQuery.isLoading) {
    return (
      <SettingsLayout active="billing">
        <div className="flex h-full items-center justify-center">
          <Spinner />
        </div>
      </SettingsLayout>
    );
  }
  if (!budget) {
    return (
      <SettingsLayout active="billing">
        <div />
      </SettingsLayout>
    );
  }

  const cap = budget.monthly_cap_micro_usd;
  const used = budget.used_micro_usd;

  return (
    <SettingsLayout active="billing">
      <SettingsBreadcrumb
        trail={[
          { label: t("settings.breadcrumb.workspace") },
          { label: t("settings.breadcrumb.settings") },
          { label: t("settings.nav.billing"), current: true },
        ]}
      />
      <SettingsPageHeader
        title={t("settings.nav.billing")}
        subtitle={t("settings.budget.subtitle")}
        right={
          canEdit ? (
            <>
              {savedToast ? (
                <span className="font-[var(--font-mono)] text-[11px] tracking-[0.06em] text-[var(--color-moss-deep)] uppercase">
                  ✓ {t("settings.budget.savedToast")}
                </span>
              ) : null}
              <Button
                variant="primary"
                disabled={!canSave}
                loading={updateBudget.isPending}
                onClick={onSave}
                data-testid="settings-budget-save"
              >
                <Save className="h-3.5 w-3.5" strokeWidth={2} />
                {t("settings.budget.save")}
              </Button>
            </>
          ) : undefined
        }
      />

      <div className="min-h-0 flex-1 overflow-auto p-4 md:p-8">
        {serverError ? (
          <div className="mb-4 border border-[var(--color-rose)] bg-[var(--color-rose-soft)] px-3 py-2 text-[12px] text-[var(--color-rose)]">
            {serverError}
          </div>
        ) : null}

        <div className="flex flex-col gap-6">
          {/* CURRENT PERIOD */}
          <SectionCard
            header={
              <SectionCardHeader
                titleKey="settings.budget.usage.title"
                helperKey="settings.budget.usage.helper"
              />
            }
            bodyClassName="flex flex-col gap-3 px-5 py-5"
          >
            <div className="flex items-baseline justify-between">
              <span className="text-[13px] text-[var(--color-muted-foreground)]">
                {t("settings.budget.used")}
              </span>
              <span className="font-[var(--font-mono)] text-[15px] font-semibold text-[var(--color-ink)]">
                {cap === null
                  ? formatUSD(used)
                  : `${formatUSD(used)} / ${formatUSD(cap)}`}
              </span>
            </div>
            {cap === null ? (
              <p className="text-[12px] text-[var(--color-muted-foreground)]">
                {t("settings.budget.unlimited")}
              </p>
            ) : (
              <>
                <ProgressBar
                  value={used}
                  max={cap}
                  ariaLabel={t("settings.budget.usage.title")}
                />
                <div className="flex items-center justify-between text-[12px] text-[var(--color-muted-foreground)]">
                  <span>
                    {t("settings.budget.remaining", {
                      amount: formatUSD(budget.remaining_micro_usd ?? 0),
                    })}
                  </span>
                  {budget.warned_at ? (
                    <span className="inline-flex items-center gap-1 font-[var(--font-mono)] text-[11px] tracking-[0.04em] text-[var(--color-amber)] uppercase">
                      <AlertTriangle className="h-3.5 w-3.5" strokeWidth={1.75} />
                      {t("settings.budget.warned")}
                    </span>
                  ) : null}
                </div>
              </>
            )}
          </SectionCard>

          {/* BUDGET CONFIG */}
          <SectionCard
            header={
              <SectionCardHeader
                titleKey="settings.budget.config.title"
                helperKey="settings.budget.config.helper"
              />
            }
            bodyClassName="grid grid-cols-1 gap-5 px-5 py-5"
          >
            {!canEdit ? (
              <div className="font-[var(--font-mono)] text-[11px] tracking-[0.06em] text-[var(--color-muted-foreground)] uppercase">
                {t("settings.budget.memberHint")}
              </div>
            ) : null}

            <SettingsField
              label={t("settings.budget.cap")}
              helper={t("settings.budget.cap.helper")}
            >
              <div className="flex flex-col gap-3">
                <label className="flex items-center gap-2.5">
                  <Switch
                    checked={unlimited}
                    onChange={setUnlimited}
                    disabled={!canEdit}
                    ariaLabel={t("settings.budget.cap.unlimitedToggle")}
                  />
                  <span className="text-[13px] text-[var(--color-ink)]">
                    {t("settings.budget.cap.unlimitedToggle")}
                  </span>
                </label>
                {!unlimited ? (
                  <div className="flex items-center gap-2">
                    <span className="font-[var(--font-mono)] text-[13px] text-[var(--color-muted-foreground)]">
                      $
                    </span>
                    <input
                      type="number"
                      min={1}
                      step="1"
                      value={capUsd}
                      onChange={(e) => setCapUsd(e.target.value)}
                      disabled={!canEdit}
                      className="h-9 w-40 border border-[var(--color-line)] bg-[var(--color-card)] px-3 text-[13px] text-[var(--color-ink)] outline-none focus:ring-1 focus:ring-[var(--color-moss)] disabled:opacity-50"
                      data-testid="settings-budget-cap"
                    />
                  </div>
                ) : null}
              </div>
            </SettingsField>

            <SettingsField
              label={t("settings.budget.warnThreshold")}
              helper={t("settings.budget.warnThreshold.helper")}
            >
              <div className="flex items-center gap-2">
                <input
                  type="number"
                  min={1}
                  max={100}
                  step="1"
                  value={warnPct}
                  onChange={(e) => setWarnPct(e.target.value)}
                  disabled={!canEdit || unlimited}
                  className="h-9 w-24 border border-[var(--color-line)] bg-[var(--color-card)] px-3 text-[13px] text-[var(--color-ink)] outline-none focus:ring-1 focus:ring-[var(--color-moss)] disabled:opacity-50"
                  data-testid="settings-budget-warn"
                />
                <span className="font-[var(--font-mono)] text-[13px] text-[var(--color-muted-foreground)]">
                  %
                </span>
              </div>
            </SettingsField>
          </SectionCard>
        </div>
      </div>
    </SettingsLayout>
  );
}

/** Mono-case section title + helper bar shared by the two SectionCards. */
function SectionCardHeader({
  titleKey,
  helperKey,
}: {
  titleKey: TranslationKey;
  helperKey: TranslationKey;
}) {
  const { t } = useT();
  return (
    <div className="flex items-center justify-between border-b border-[var(--color-line)] bg-[var(--color-paper-2)] px-5 py-2.5">
      <span className="font-[var(--font-mono)] text-[11px] font-bold tracking-[0.09em] text-[var(--color-muted-foreground)] uppercase">
        {t(titleKey)}
      </span>
      <span className="text-[12px] text-[var(--color-fg-muted)]">
        {t(helperKey)}
      </span>
    </div>
  );
}
