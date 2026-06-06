import { useEffect, useRef, useState } from "react";
import { useNavigate, useParams } from "react-router-dom";
import { useQueryClient } from "@tanstack/react-query";
import { Spinner } from "../components/atoms/Spinner";
import { Button } from "../components/atoms/Button";
import { api } from "../lib/api";
import { ApiError, AuthRedirect } from "../lib/errors";
import { ME_QUERY_KEY } from "../hooks/useMe";
import { useT } from "../i18n";

type ErrorKind = "expired" | "consumed" | "generic";

/** Landing page for the `/i/{slug}/{token}` invite link. Redeems the
 *  token against `POST /me/invites/accept`, which joins the inviting org
 *  and re-mints the session so it becomes the active workspace, then
 *  drops the user into it. A 401 (visitor not signed in) is handled by
 *  the api wrapper, which bounces them through `/sign-in?from=…` and back
 *  here once Google login completes. */
export function AcceptInvite() {
  const { slug = "", token = "" } = useParams<{ slug: string; token: string }>();
  const nav = useNavigate();
  const queryClient = useQueryClient();
  const { t } = useT();
  const [error, setError] = useState<ErrorKind | null>(null);
  // Guard against React StrictMode's double-invoke: the token is
  // single-use, so a second redeem would see it already consumed.
  const redeemed = useRef(false);

  useEffect(() => {
    if (redeemed.current) return;
    redeemed.current = true;
    if (!token) {
      setError("generic");
      return;
    }
    void (async () => {
      try {
        await api.acceptInvite(token);
        // The session cookie now points at the inviting org. Mark the
        // stale `me` for refetch (don't await — the workspace's own
        // useMe picks it up on mount) and drop into the new active org.
        void queryClient.invalidateQueries({ queryKey: ME_QUERY_KEY });
        nav("/", { replace: true });
      } catch (e) {
        // 401 already triggered a redirect to /sign-in via the api
        // wrapper — render nothing while the navigation flushes.
        if (e instanceof AuthRedirect) return;
        setError(errorKindFor(e));
      }
    })();
  }, [token, nav, queryClient]);

  if (error) {
    return (
      <Shell>
        <h1 className="font-[var(--font-display)] text-[22px] font-semibold text-[var(--color-ink)]">
          {t("invite.accept.error.title")}
        </h1>
        <p className="max-w-[420px] text-center text-[13.5px] text-[var(--color-muted-foreground)]">
          {t(`invite.accept.error.${error}`)}
        </p>
        <Button variant="primary" onClick={() => nav("/", { replace: true })}>
          {t("invite.accept.error.cta")}
        </Button>
      </Shell>
    );
  }

  return (
    <Shell>
      <Spinner size={20} />
      <h1 className="font-[var(--font-display)] text-[22px] font-semibold text-[var(--color-ink)]">
        {t("invite.accept.joining.title", { slug })}
      </h1>
      <p className="text-[13.5px] text-[var(--color-muted-foreground)]">
        {t("invite.accept.joining.body")}
      </p>
    </Shell>
  );
}

/** Map a failed redeem to the message shown. The backend returns 410 for
 *  an expired invite and 409 for an already-consumed one; everything
 *  else (404 unknown token, 5xx) collapses to the generic copy. */
function errorKindFor(e: unknown): ErrorKind {
  if (e instanceof ApiError) {
    if (e.status === 410) return "expired";
    if (e.status === 409) return "consumed";
  }
  return "generic";
}

function Shell({ children }: { children: React.ReactNode }) {
  return (
    <main className="mx-auto flex h-screen w-full max-w-[640px] flex-col items-center justify-center gap-5 bg-[var(--color-paper)] px-6 py-12 text-center">
      {children}
    </main>
  );
}
