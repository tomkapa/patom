import { useMemo } from "react";
import { useLocation } from "react-router-dom";
import { AlertTriangle, Radio } from "lucide-react";
import { Banner } from "../components/molecules/Banner";
import { Button } from "../components/atoms/Button";
import signinArtwork from "../assets/signin.png";
import { useLightModeOnly } from "../lib/theme";
import { useT } from "../i18n";

function GoogleG() {
  return (
    <svg
      aria-hidden="true"
      width="16"
      height="16"
      viewBox="0 0 18 18"
      xmlns="http://www.w3.org/2000/svg"
    >
      <path
        fill="#EA4335"
        d="M9 3.48c1.69 0 2.83.73 3.48 1.34l2.54-2.48C13.46.89 11.43 0 9 0 5.48 0 2.44 2.02.96 4.96l2.91 2.26C4.6 5.05 6.62 3.48 9 3.48z"
      />
      <path
        fill="#4285F4"
        d="M17.64 9.2c0-.74-.06-1.28-.19-1.84H9v3.34h4.96c-.1.83-.64 2.08-1.84 2.92l2.84 2.2c1.7-1.57 2.68-3.88 2.68-6.62z"
      />
      <path
        fill="#FBBC05"
        d="M3.88 10.78A5.54 5.54 0 0 1 3.58 9c0-.62.11-1.22.29-1.78L.96 4.96A9 9 0 0 0 0 9c0 1.45.35 2.82.96 4.04l2.92-2.26z"
      />
      <path
        fill="#34A853"
        d="M9 18c2.43 0 4.47-.8 5.96-2.18l-2.84-2.2c-.76.53-1.78.9-3.12.9-2.38 0-4.4-1.57-5.12-3.74L.97 13.04C2.45 15.98 5.48 18 9 18z"
      />
    </svg>
  );
}

export function SignIn() {
  const location = useLocation();
  const { t } = useT();
  useLightModeOnly();

  const { oauthDown, returnTo } = useMemo(() => {
    const params = new URLSearchParams(location.search);
    const fromQuery = params.get("from");
    const state = location.state as { from?: string } | null;
    const candidate = fromQuery ?? state?.from ?? "/";
    const isSafeInternalPath =
      candidate.startsWith("/") &&
      !candidate.startsWith("//") &&
      candidate !== "/sign-in";
    return {
      oauthDown: params.get("error") === "oauth_unavailable",
      returnTo: isSafeInternalPath ? candidate : "/",
    };
  }, [location.search, location.state]);

  const onSignIn = () => {
    window.location.href = `/auth/google/login?return_to=${encodeURIComponent(returnTo)}`;
  };

  return (
    <div className="flex h-screen w-screen bg-[var(--color-paper)]">
      <aside className="relative hidden flex-col overflow-hidden bg-[var(--color-rail-brand)] px-14 py-14 md:flex md:w-[44%] lg:w-[39%]">
        <header className="flex items-center gap-3">
          <span
            aria-hidden="true"
            className="inline-flex h-7 w-7 items-center justify-center rounded-sm bg-[var(--color-moss)] text-white"
          >
            <Radio className="h-4 w-4" />
          </span>
          <span className="font-[var(--font-mono)] text-[14px] font-semibold uppercase tracking-[0.28em] text-white">
            {t("signin.brand")}
          </span>
          <span
            aria-hidden="true"
            className="ml-2 h-px flex-1 bg-white/20"
          />
          <span className="font-[var(--font-mono)] text-[10.5px] uppercase tracking-[0.24em] text-white/50">
            v1.0
          </span>
        </header>

        <div className="mt-16">
          <p className="font-[var(--font-display)] text-[40px] font-bold leading-[1.1] tracking-tight text-white">
            {t("signin.tagline")}
          </p>
        </div>

        <div className="mt-auto pt-10">
          <img
            src={signinArtwork}
            alt=""
            aria-hidden="true"
            className="w-full select-none rounded-sm opacity-95"
            draggable={false}
          />
        </div>
      </aside>

      <main className="grain-paper relative flex flex-1 items-center justify-center px-6 py-10">
        <div className="w-full max-w-[420px]">
          <h1 className="font-[var(--font-display)] text-[36px] font-bold leading-[1.1] tracking-tight text-[var(--color-ink)]">
            {t("signin.heading")}
          </h1>
          <p className="mt-3 text-[13px] leading-[1.55] text-[var(--color-muted-foreground)]">
            {t("signin.subheading")}
          </p>

          {oauthDown ? (
            <Banner
              variant="amber"
              icon={<AlertTriangle className="h-3.5 w-3.5" />}
              className="mt-5"
            >
              {t("signin.error.oauth_down")}
            </Banner>
          ) : null}

          <Button
            variant="primary"
            size="md"
            onClick={onSignIn}
            className="mt-6 w-full gap-2"
          >
            <GoogleG />
            {t("signin.cta")}
          </Button>

          <p className="mt-8 text-[11px] leading-[1.6] text-[var(--color-fg-muted)]">
            {t("signin.legal")}
          </p>
        </div>
      </main>
    </div>
  );
}
