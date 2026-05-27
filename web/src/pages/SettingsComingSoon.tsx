import { Construction } from "lucide-react";
import {
  SettingsBreadcrumb,
  SettingsLayout,
  type SettingsNavId,
} from "../components/templates/SettingsLayout";
import { useT } from "../i18n";

/** Narrowed to only the IA slots that genuinely have no real page
 *  yet. Passing "general" or "members" should fail at the call site
 *  rather than render a misleading "Notifications" label. */
type ComingSoonKind = Extract<
  SettingsNavId,
  "billing" | "notifications"
>;

const LABEL_KEYS = {
  billing: "settings.nav.billing",
  notifications: "settings.nav.notifications",
} as const satisfies Record<ComingSoonKind, string>;

export function SettingsComingSoon({ kind }: { kind: ComingSoonKind }) {
  const { t } = useT();
  const labelKey = LABEL_KEYS[kind];
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
