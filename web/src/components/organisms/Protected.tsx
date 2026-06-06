import type { ReactNode } from "react";
import { useMe } from "../../hooks/useMe";
import { Spinner } from "../atoms/Spinner";
import { AuthRedirect } from "../../lib/errors";
import { OnboardingGate } from "./OnboardingGate";

export function Protected({ children }: { children: ReactNode }) {
  const { data, isLoading, isError, error } = useMe();

  if (isLoading) {
    return (
      <div
        className="flex h-screen w-screen items-center justify-center"
        aria-label="Loading session"
      >
        <Spinner size={20} />
      </div>
    );
  }

  // 401 already triggered a window.location.href redirect via the api wrapper.
  // Render nothing while the navigation flushes.
  if (isError && error instanceof AuthRedirect) return null;

  if (!data) return null;

  return <OnboardingGate>{children}</OnboardingGate>;
}
