import { useState } from "react";
import { useNavigate, useParams } from "react-router-dom";
import {
  useMutation,
  useQuery,
  useQueryClient,
} from "@tanstack/react-query";
import { ShieldCheck } from "lucide-react";
import {
  AgentBreadcrumb,
  AgentLayout,
} from "../components/templates/AgentLayout";
import { Button } from "../components/atoms/Button";
import { Spinner } from "../components/atoms/Spinner";
import { Banner } from "../components/molecules/Banner";
import { EmptyState } from "../components/molecules/EmptyState";
import { Modal, ModalFooter } from "../components/molecules/Modal";
import { PageTabHeader } from "../components/molecules/PageTabHeader";
import {
  PlatformIntegrationCard,
  type PlatformApp,
} from "../components/agentDetail/PlatformIntegrationCard";
import { ConnectPlatformModal } from "../components/agentDetail/ConnectPlatformModal";
import larkLogo from "../assets/lark.svg";
import discordLogo from "../assets/discord.svg";
import { useAgent } from "../hooks/useAgents";
import { useT } from "../i18n";
import { useAuthStore } from "../stores/authStore";
import { api } from "../lib/api";
import { ApiError, formatError } from "../lib/errors";
import type { Agent } from "../types/api";

type PlatformId = "lark" | "discord";

/** Cache keys are scoped per active org AND per agent: the page only ever
 *  shows the apps bound to the agent it opened from, and an org switch must
 *  not surface another tenant's apps. */
const larkKey = (orgId: string | null | undefined, agentId: string) =>
  ["lark-apps", orgId ?? "unknown-org", agentId] as const;
const discordKey = (orgId: string | null | undefined, agentId: string) =>
  ["discord-apps", orgId ?? "unknown-org", agentId] as const;

export function AgentIntegrations() {
  const { t } = useT();
  const nav = useNavigate();
  const { id } = useParams<{ id: string }>();
  const agentQuery = useAgent(id ?? null);
  const me = useAuthStore((s) => s.me);
  const activeOrgId = me?.active_org_id;
  const role = me?.role;
  const isAdmin = role === "owner" || role === "admin";
  const activeOrg = me?.orgs.find((o) => o.id === activeOrgId);
  const workspaceLabel =
    activeOrg?.name ?? t("connections.breadcrumb.workspace");

  const agent = agentQuery.data ?? null;

  return (
    <AgentLayout agent={agent} active="integrations">
      <AgentBreadcrumb
        trail={[
          { label: workspaceLabel },
          { label: t("agent.detail.breadcrumb.agents") },
          { label: agent?.name ?? "…" },
          { label: t("agent.detail.breadcrumb.integrations"), current: true },
        ]}
      />
      {agentQuery.isLoading && !agentQuery.isError ? (
        <div className="flex flex-1 items-center justify-center text-[var(--color-muted-foreground)]">
          <Spinner size={16} />
        </div>
      ) : !agent ? (
        <div className="flex flex-1 items-center justify-center p-8">
          <AgentLoadFallback
            error={agentQuery.error}
            onRetry={() => agentQuery.refetch()}
            onHome={() => nav("/")}
          />
        </div>
      ) : (
        <>
          <PageTabHeader
            title={t("agent.detail.integrations.title")}
            subtitle={t("agent.detail.integrations.subtitle")}
            actions={
              isAdmin ? (
                <span className="inline-flex items-center gap-1.5 border border-[var(--color-moss)] bg-[var(--color-moss-soft)] px-3 py-2">
                  <ShieldCheck
                    className="h-3.5 w-3.5 text-[var(--color-moss)]"
                    strokeWidth={1.75}
                  />
                  <span className="text-[13px] font-medium text-[var(--color-moss)]">
                    {role === "owner"
                      ? t("agent.detail.integrations.role.owner")
                      : t("agent.detail.integrations.role.admin")}
                  </span>
                </span>
              ) : undefined
            }
          />
          <Body
            agent={agent}
            orgId={activeOrgId}
            isAdmin={isAdmin}
          />
        </>
      )}
    </AgentLayout>
  );
}

function Body({
  agent,
  orgId,
  isAdmin,
}: {
  agent: Agent;
  orgId: string | null | undefined;
  isAdmin: boolean;
}) {
  const { t } = useT();
  const qc = useQueryClient();
  const agentId = agent.id;

  // The list endpoints are owner/admin-only (members 403), so members never
  // fetch — they get the read-only platform cards with actions withheld.
  const lark = useQuery({
    queryKey: larkKey(orgId, agentId),
    queryFn: api.larkApps,
    staleTime: 30_000,
    enabled: isAdmin,
  });
  const discord = useQuery({
    queryKey: discordKey(orgId, agentId),
    queryFn: api.discordApps,
    staleTime: 30_000,
    enabled: isAdmin,
  });

  const larkConnect = useMutation({
    mutationFn: api.larkConnect,
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: larkKey(orgId, agentId) });
      setConnecting(null);
    },
  });
  const discordConnect = useMutation({
    mutationFn: api.discordConnect,
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: discordKey(orgId, agentId) });
      setConnecting(null);
    },
  });
  const larkDisconnect = useMutation({
    mutationFn: api.larkDisconnect,
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: larkKey(orgId, agentId) });
      setRemoving(null);
    },
  });
  const discordDisconnect = useMutation({
    mutationFn: api.discordDisconnect,
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: discordKey(orgId, agentId) });
      setRemoving(null);
    },
  });

  const [connecting, setConnecting] = useState<PlatformId | null>(null);
  const [removing, setRemoving] = useState<{
    platform: PlatformId;
    appId: string;
  } | null>(null);

  const larkApp = (lark.data ?? []).find((a) => a.agent_id === agentId) ?? null;
  const discordApp =
    (discord.data ?? []).find((a) => a.agent_id === agentId) ?? null;

  const larkView: PlatformApp | null = larkApp
    ? {
        appId: larkApp.app_id,
        boundValue: t("agent.detail.integrations.card.boundValue", {
          name: agent.name,
        }),
        extraKey: t("agent.detail.integrations.card.tenant"),
        extraValue:
          larkApp.tenant_key ?? t("agent.detail.integrations.card.pending"),
        live: larkApp.tenant_key != null,
      }
    : null;
  const discordView: PlatformApp | null = discordApp
    ? {
        appId: discordApp.application_id,
        boundValue: t("agent.detail.integrations.card.boundValue", {
          name: agent.name,
        }),
        extraKey: t("agent.detail.integrations.card.bot"),
        extraValue:
          discordApp.bot_user_id ??
          t("agent.detail.integrations.card.pending"),
        live: discordApp.bot_user_id != null,
      }
    : null;

  const failed = lark.isError || discord.isError;

  return (
    <div className="min-h-0 flex-1 overflow-auto p-4 md:p-8">
      {/* Intro bar */}
      <div className="mb-5 flex items-center gap-2">
        <span className="font-[var(--font-mono)] text-[11px] font-bold tracking-[0.1em] text-[var(--color-ink-2)] uppercase">
          {t("agent.detail.integrations.intro.label")}
        </span>
        <span aria-hidden className="h-3 w-px bg-[var(--color-line-2)]" />
        <span className="text-[12px] text-[var(--color-muted-foreground)]">
          {t("agent.detail.integrations.intro.desc")}
        </span>
      </div>

      {failed ? (
        <Banner variant="rose">
          {t("agent.detail.integrations.loadError")}
        </Banner>
      ) : lark.isLoading || discord.isLoading ? (
        <div className="flex h-40 items-center justify-center">
          <Spinner size={16} />
        </div>
      ) : (
        <div className="grid grid-cols-1 gap-5 lg:grid-cols-2">
          <PlatformIntegrationCard
            name={t("agent.detail.integrations.lark.name")}
            logo={larkLogo}
            desc={t("agent.detail.integrations.lark.desc")}
            app={larkView}
            canManage={isAdmin}
            onConnect={() => setConnecting("lark")}
            onRemove={() =>
              larkApp &&
              setRemoving({ platform: "lark", appId: larkApp.app_id })
            }
          />
          <PlatformIntegrationCard
            name={t("agent.detail.integrations.discord.name")}
            logo={discordLogo}
            desc={t("agent.detail.integrations.discord.desc")}
            app={discordView}
            canManage={isAdmin}
            onConnect={() => setConnecting("discord")}
            onRemove={() =>
              discordApp &&
              setRemoving({
                platform: "discord",
                appId: discordApp.application_id,
              })
            }
          />
        </div>
      )}

      {/* Connect modal */}
      {connecting ? (
        <ConnectPlatformModal
          open
          onClose={() => setConnecting(null)}
          name={t(`agent.detail.integrations.${connecting}.name`)}
          logo={connecting === "lark" ? larkLogo : discordLogo}
          agent={agent}
          idLabel={t(`agent.detail.integrations.${connecting}.idLabel`)}
          secretLabel={t(`agent.detail.integrations.${connecting}.secretLabel`)}
          idPlaceholder={t(
            `agent.detail.integrations.${connecting}.idPlaceholder`,
          )}
          hint={t(`agent.detail.integrations.${connecting}.hint`)}
          submitting={
            connecting === "lark"
              ? larkConnect.isPending
              : discordConnect.isPending
          }
          error={
            connecting === "lark"
              ? larkConnect.isError
                ? formatError(larkConnect.error)
                : null
              : discordConnect.isError
                ? formatError(discordConnect.error)
                : null
          }
          extraFields={
            connecting === "lark"
              ? [
                  {
                    id: "card_encrypt_key",
                    label: t("agent.detail.integrations.lark.encryptKeyLabel"),
                    helper: t("agent.detail.integrations.lark.cardHelper"),
                  },
                  {
                    id: "card_verification_token",
                    label: t(
                      "agent.detail.integrations.lark.verificationTokenLabel",
                    ),
                  },
                ]
              : undefined
          }
          onSubmit={(idValue, secretValue, extra) => {
            if (connecting === "lark") {
              larkConnect.mutate({
                app_id: idValue,
                app_secret: secretValue,
                agent_id: agentId,
                // Both or neither — the backend rejects a lone value. Blank
                // fields were already dropped by the modal.
                card_encrypt_key: extra.card_encrypt_key,
                card_verification_token: extra.card_verification_token,
              });
            } else {
              discordConnect.mutate({
                application_id: idValue,
                bot_token: secretValue,
                agent_id: agentId,
              });
            }
          }}
        />
      ) : null}

      {/* Remove confirm modal */}
      <Modal
        open={removing !== null}
        onClose={() => setRemoving(null)}
        ariaLabel={t("agent.detail.integrations.remove.title", { name: "" })}
      >
        {removing ? (
          <RemoveConfirm
            name={t(`agent.detail.integrations.${removing.platform}.name`)}
            pending={
              removing.platform === "lark"
                ? larkDisconnect.isPending
                : discordDisconnect.isPending
            }
            onCancel={() => setRemoving(null)}
            onConfirm={() => {
              if (removing.platform === "lark") {
                larkDisconnect.mutate(removing.appId);
              } else {
                discordDisconnect.mutate(removing.appId);
              }
            }}
          />
        ) : null}
      </Modal>
    </div>
  );
}

function RemoveConfirm({
  name,
  pending,
  onCancel,
  onConfirm,
}: {
  name: string;
  pending: boolean;
  onCancel: () => void;
  onConfirm: () => void;
}) {
  const { t } = useT();
  return (
    <>
      <div className="border-b border-[var(--color-line)] px-5 pt-5 pb-4">
        <div className="font-[var(--font-display)] text-[18px] leading-tight font-semibold text-[var(--color-ink-2)]">
          {t("agent.detail.integrations.remove.title", { name })}
        </div>
      </div>
      <div className="px-5 py-4 text-[13px] leading-[1.5] text-[var(--color-muted-foreground)]">
        {t("agent.detail.integrations.remove.body", { name })}
      </div>
      <ModalFooter>
        <Button variant="secondary" onClick={onCancel}>
          {t("agent.detail.integrations.remove.cancel")}
        </Button>
        <Button
          variant="danger"
          loading={pending}
          onClick={onConfirm}
          className="border border-[var(--color-rose)]"
          data-testid="integration-remove-confirm"
        >
          {t("agent.detail.integrations.remove.confirm")}
        </Button>
      </ModalFooter>
    </>
  );
}

// Mirrors AgentTools: 404/403 = "not found / hidden"; anything else is
// transient and gets a retry affordance.
function AgentLoadFallback({
  error,
  onRetry,
  onHome,
}: {
  error: unknown;
  onRetry: () => void;
  onHome: () => void;
}) {
  const { t } = useT();
  const isNotFound =
    error instanceof ApiError && (error.status === 404 || error.status === 403);
  if (isNotFound) {
    return (
      <EmptyState
        title={t("agent.detail.notFound.title")}
        description={t("agent.detail.notFound.body")}
        action={
          <Button variant="primary" onClick={onHome}>
            {t("agent.detail.notFound.cta")}
          </Button>
        }
      />
    );
  }
  return (
    <EmptyState
      title={t("agent.detail.loadError.title")}
      description={error ? formatError(error) : t("agent.detail.loadError.body")}
      action={
        <Button variant="primary" onClick={onRetry}>
          {t("agent.detail.loadError.cta")}
        </Button>
      }
    />
  );
}
