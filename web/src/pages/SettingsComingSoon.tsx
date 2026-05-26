import { Construction } from "lucide-react";
import {
  SettingsBreadcrumb,
  SettingsLayout,
  type SettingsNavId,
} from "../components/templates/SettingsLayout";
import { useT } from "../i18n";

export function SettingsComingSoon({ kind }: { kind: SettingsNavId }) {
  const { t } = useT();
  const labelKey =
    kind === "billing"
      ? "settings.nav.billing"
      : kind === "webhooks"
        ? "settings.nav.webhooks"
        : "settings.nav.notifications";
  return (
    <SettingsLayout active={kind}>
      <SettingsBreadcrumb
        trail={[
          { label: t("settings.breadcrumb.workspace") },
          { label: t("settings.breadcrumb.settings") },
          { label: t(labelKey), current: true },
        ]}
      />
      <div className="flex flex-1 flex-col items-center justify-center gap-3 px-8 text-center">
        <Construction className="h-8 w-8 text-[var(--color-muted)]" strokeWidth={1.5} />
        <h1 className="font-[var(--font-display)] text-[22px] font-bold text-[var(--color-ink)]">
          {t(labelKey)}
        </h1>
        <p className="max-w-[420px] text-[13px] text-[var(--color-muted)]">
          {t("settings.comingSoon.body")}
        </p>
      </div>
    </SettingsLayout>
  );
}
