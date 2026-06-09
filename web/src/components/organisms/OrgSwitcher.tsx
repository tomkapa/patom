import { Check, Plus } from "lucide-react";
import { useNavigate } from "react-router-dom";
import { useAuthStore } from "../../stores/authStore";
import { useActiveOrg } from "../../hooks/useMe";
import { useSwitchOrg } from "../../hooks/useSwitchOrg";
import { useT } from "../../i18n";
import { Dropdown } from "../molecules/Dropdown";
import { Monogram } from "../atoms/Monogram";
import { Spinner } from "../atoms/Spinner";
// Default tile for workspaces without a custom avatar.
import appLogoUrl from "../../../assets/favicon-192.png";

/** Workspace avatar that doubles as the switcher trigger. Lives at the
 *  top of the menu rail; clicking it opens the workspace list to the
 *  right. When signed-out / no active org it renders a static tile so
 *  the rail still shows the brand mark. */
export function OrgSwitcher() {
  const { t } = useT();
  const me = useAuthStore((s) => s.me);
  const activeOrg = useActiveOrg();
  const switchOrg = useSwitchOrg();
  const navigate = useNavigate();

  const workspaceLogo = activeOrg?.avatar_url ?? appLogoUrl;
  const workspaceName = activeOrg?.name ?? "Patom";

  if (!me || !activeOrg) {
    return (
      <img
        src={workspaceLogo}
        alt={workspaceName}
        aria-label={workspaceName}
        className="h-9 w-9 shrink-0 object-cover select-none"
      />
    );
  }

  return (
    <Dropdown
      placement="right-top"
      rootClassName="w-full"
      menuClassName="w-64 max-h-[60vh] overflow-y-auto border border-[var(--color-line)] bg-[var(--color-card)] py-1 shadow-md scroll-thin"
      renderTrigger={({ open, toggle }) => (
        <button
          type="button"
          aria-haspopup="listbox"
          aria-expanded={open}
          aria-label={t("orgswitcher.aria.switch")}
          onClick={toggle}
          disabled={switchOrg.isPending}
          className="flex h-9 w-full cursor-pointer items-center justify-center outline-none transition-opacity duration-150 ease-out hover:opacity-90 focus-visible:ring-1 focus-visible:ring-white disabled:cursor-not-allowed"
        >
          {switchOrg.isPending ? (
            <Spinner size={16} />
          ) : (
            <img
              src={workspaceLogo}
              alt={workspaceName}
              className="h-9 w-9 object-cover select-none"
            />
          )}
        </button>
      )}
    >
      {({ close }) => (
        <ul role="listbox" aria-label={t("orgswitcher.aria.list")}>
          {me.orgs.map((org) => {
            const isActive = org.id === me.active_org_id;
            const onPick = () => {
              if (switchOrg.isPending) return;
              if (isActive) {
                close();
                return;
              }
              // Hold the menu open during the round-trip so the spinner on
              // the trigger gives users feedback; close once it settles.
              switchOrg.mutate(org.id, { onSettled: close });
            };
            return (
              <li key={org.id}>
                <button
                  type="button"
                  role="option"
                  aria-selected={isActive}
                  onClick={onPick}
                  disabled={switchOrg.isPending}
                  className="flex w-full cursor-pointer items-center gap-2.5 px-3 py-2 text-left transition-colors duration-100 ease-out hover:bg-[var(--color-paper-2)] disabled:cursor-not-allowed disabled:opacity-50"
                >
                  <Monogram
                    name={org.name}
                    id={org.id}
                    size={24}
                    avatarUrl={org.avatar_url ?? appLogoUrl}
                  />
                  <div className="min-w-0 flex-1">
                    <div className="truncate text-[13px] font-semibold text-[var(--color-ink)]">
                      {org.name}
                    </div>
                    <div className="font-[var(--font-mono)] text-[10.5px] uppercase tracking-[0.12em] text-[var(--color-muted-foreground)]">
                      {org.role}
                    </div>
                  </div>
                  {isActive ? (
                    <Check className="h-3.5 w-3.5 shrink-0 text-[var(--color-moss)]" />
                  ) : null}
                </button>
              </li>
            );
          })}
          {/* Create-another-workspace entry. Routes into the onboarding
              wizard with the explicit ?new=1 intent so the gate lets an
              already-onboarded user back in to create a fresh workspace. */}
          <li
            role="separator"
            aria-hidden="true"
            className="my-1 border-t border-[var(--color-line)]"
          />
          <li role="none">
            <button
              type="button"
              role="menuitem"
              disabled={switchOrg.isPending}
              onClick={() => {
                close();
                navigate("/onboarding?new=1");
              }}
              className="flex w-full cursor-pointer items-center gap-2.5 px-3 py-2 text-left transition-colors duration-100 ease-out hover:bg-[var(--color-paper-2)] disabled:cursor-not-allowed disabled:opacity-50"
            >
              <span className="flex h-6 w-6 items-center justify-center text-[var(--color-muted-foreground)]">
                <Plus className="h-4 w-4" />
              </span>
              <span className="text-[13px] font-semibold text-[var(--color-ink)]">
                {t("orgswitcher.create")}
              </span>
            </button>
          </li>
        </ul>
      )}
    </Dropdown>
  );
}
