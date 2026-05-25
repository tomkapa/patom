import { Check, ChevronDown } from "lucide-react";
import { useAuthStore } from "../../stores/authStore";
import { useActiveOrg } from "../../hooks/useMe";
import { useSwitchOrg } from "../../hooks/useSwitchOrg";
import { Dropdown } from "../molecules/Dropdown";
import { Monogram } from "../atoms/Monogram";
import { Spinner } from "../atoms/Spinner";

export function OrgSwitcher() {
  const me = useAuthStore((s) => s.me);
  const activeOrg = useActiveOrg();
  const switchOrg = useSwitchOrg();

  if (!me || !activeOrg) return null;

  return (
    <Dropdown
      rootClassName="w-full"
      menuClassName="max-h-[60vh] overflow-y-auto border border-[var(--color-line)] bg-[var(--color-card)] py-1 shadow-md scroll-thin"
      renderTrigger={({ open, toggle }) => (
        <button
          type="button"
          aria-haspopup="listbox"
          aria-expanded={open}
          aria-label="Switch organization"
          onClick={toggle}
          disabled={switchOrg.isPending}
          className="flex w-full cursor-pointer items-center justify-between gap-2 text-left outline-none transition-colors duration-150 ease-out focus-visible:ring-1 focus-visible:ring-[var(--color-ink)] disabled:cursor-not-allowed"
        >
          <div className="min-w-0">
            <div className="font-[var(--font-mono)] text-[10px] uppercase tracking-[0.18em] text-[var(--color-muted)]">
              Relay
            </div>
            <div className="mt-0.5 truncate font-[var(--font-display)] text-[18px] font-bold tracking-tight text-[var(--color-ink)]">
              {activeOrg.name}
            </div>
          </div>
          {switchOrg.isPending ? (
            <Spinner size={14} />
          ) : (
            <ChevronDown className="h-4 w-4 shrink-0 text-[var(--color-muted)]" />
          )}
        </button>
      )}
    >
      {({ close }) => (
        <ul role="listbox" aria-label="Organizations">
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
                  <Monogram name={org.name} id={org.id} size={24} />
                  <div className="min-w-0 flex-1">
                    <div className="truncate text-[13px] font-semibold text-[var(--color-ink)]">
                      {org.name}
                    </div>
                    <div className="font-[var(--font-mono)] text-[10.5px] uppercase tracking-[0.12em] text-[var(--color-muted)]">
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
        </ul>
      )}
    </Dropdown>
  );
}
