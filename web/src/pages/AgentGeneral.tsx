import { useEffect, useRef, useState } from "react";
import { useNavigate, useParams } from "react-router-dom";
import { FileText, Save } from "lucide-react";
import {
  AgentBreadcrumb,
  AgentLayout,
} from "../components/templates/AgentLayout";
import { Button } from "../components/atoms/Button";
import { Monogram } from "../components/atoms/Monogram";
import { Spinner } from "../components/atoms/Spinner";
import { EmptyState } from "../components/molecules/EmptyState";
import { ImageUploader } from "../components/molecules/ImageUploader";
import { PageTabHeader } from "../components/molecules/PageTabHeader";
import { GutteredEditor } from "../components/organisms/GutteredEditor";
import { ModelPickerRow } from "../components/agentDetail/ModelPickerRow";
import { PromptStatsFooter } from "../components/agentDetail/PromptStatsFooter";
import { useAgent, useUpdateAgent } from "../hooks/useAgents";
import { useModels } from "../hooks/useModels";
import { useT } from "../i18n";
import { api } from "../lib/api";
import { useAuthStore } from "../stores/authStore";
import { ApiError, formatError } from "../lib/errors";
import type { UpdateAgentRequest } from "../types/api";

export function AgentGeneral() {
  const { t } = useT();
  const nav = useNavigate();
  const { id } = useParams<{ id: string }>();
  const agentQuery = useAgent(id ?? null);
  const modelsQuery = useModels();
  const updateAgent = useUpdateAgent();
  const me = useAuthStore((s) => s.me);
  const activeOrg = me?.orgs.find((o) => o.id === me?.active_org_id);
  const workspaceLabel =
    activeOrg?.name ?? t("connections.breadcrumb.workspace");

  const agent = agentQuery.data ?? null;
  const models = modelsQuery.data ?? [];

  const serverPrompt = agent?.system_prompt ?? "";
  const serverModel = agent?.model ?? null;
  const serverAvatar = agent?.avatar_url ?? null;

  // Hydrate once per agent (same pattern as the tools tab) so a
  // background refetch doesn't trample in-flight operator edits.
  const [prompt, setPrompt] = useState<string>(serverPrompt);
  const [model, setModel] = useState<string | null>(serverModel);
  const [avatarUrl, setAvatarUrl] = useState<string | null>(serverAvatar);
  // Track the agent currently shown so an upload that resolves after the
  // operator has switched agents doesn't write the stale URL into the new
  // agent's form.
  const agentIdRef = useRef<string | undefined>(agent?.id);
  useEffect(() => {
    agentIdRef.current = agent?.id;
    if (agent) {
      setPrompt(agent.system_prompt ?? "");
      setModel(agent.model ?? null);
      setAvatarUrl(agent.avatar_url ?? null);
    }
  }, [agent?.id]);

  const dirty =
    prompt !== serverPrompt ||
    model !== serverModel ||
    avatarUrl !== serverAvatar;
  const saving = updateAgent.isPending;

  // Upload the file to object storage and stage the returned URL in form
  // state; it persists to the agent row on Save with the rest of the
  // form (issue #43). The deterministic key means an unsaved upload is
  // harmlessly overwritten next time.
  const handleAvatarUpload = async (file: File): Promise<string> => {
    if (!agent) throw new Error("agent not loaded");
    const initiatingId = agent.id;
    const { url } = await api.uploadAgentAvatar(initiatingId, file);
    // Drop the result if the operator navigated to a different agent
    // while the upload was in flight.
    if (agentIdRef.current === initiatingId) setAvatarUrl(url);
    return url;
  };

  const onSave = () => {
    if (!agent) return;
    const patch: UpdateAgentRequest = {};
    if (prompt !== serverPrompt) patch.system_prompt = prompt;
    if (model !== serverModel) patch.model = model;
    if (avatarUrl !== serverAvatar) patch.avatar_url = avatarUrl;
    updateAgent.mutate({ id: agent.id, patch });
  };

  return (
    <AgentLayout agent={agent} active="general">
      <AgentBreadcrumb
        trail={[
          { label: workspaceLabel },
          { label: t("agent.detail.breadcrumb.agents") },
          { label: agent?.name ?? "…" },
          { label: t("agent.detail.breadcrumb.general"), current: true },
        ]}
      />
      {agentQuery.isLoading && !agentQuery.isError ? (
        <div className="flex flex-1 items-center justify-center text-[var(--color-muted-foreground)]">
          <Spinner size={16} />
        </div>
      ) : !agent ? (
        <div className="flex flex-1 items-center justify-center p-4 md:p-8">
          <AgentLoadFallback
            error={agentQuery.error}
            onRetry={() => agentQuery.refetch()}
            onHome={() => nav("/")}
          />
        </div>
      ) : (
        <>
          <PageTabHeader
            title={t("agent.detail.general.title")}
            subtitle={t("agent.detail.general.subtitle")}
            actions={
              <Button
                variant="primary"
                size="md"
                disabled={!dirty}
                loading={saving}
                onClick={onSave}
              >
                <span className="inline-flex items-center gap-1.5">
                  <Save className="h-3.5 w-3.5" strokeWidth={1.75} />
                  {t("agent.detail.general.save")}
                </span>
              </Button>
            }
          />
          <div className="flex min-h-0 flex-1 flex-col gap-6 p-4 md:p-8">
            <div className="flex flex-col items-start gap-3 md:flex-row md:items-center md:justify-between md:gap-6">
              <div className="flex min-w-0 flex-col gap-1">
                <div className="text-[14px] font-medium text-[var(--color-ink)]">
                  {t("agent.detail.general.avatarLabel")}
                </div>
                <div className="max-w-[42ch] text-[13px] text-[var(--color-muted-foreground)]">
                  {t("agent.detail.general.avatarCaption")}
                </div>
              </div>
              <ImageUploader
                kind="avatar"
                currentUrl={avatarUrl}
                size={64}
                onUpload={handleAvatarUpload}
                fallback={
                  <Monogram
                    name={agent.name}
                    id={agent.id}
                    size={64}
                    tone="moss"
                  />
                }
              />
            </div>
            <div className="h-px shrink-0 bg-[var(--color-line)]" />
            <ModelPickerRow
              models={models}
              value={model}
              onChange={setModel}
              label={t("agent.detail.general.modelLabel")}
              caption={t("agent.detail.general.modelCaption")}
              inheritLabel={t("agent.detail.general.modelInherit")}
              ariaLabel={t("agent.detail.general.modelAria")}
            />
            <div className="h-px shrink-0 bg-[var(--color-line)]" />
            <div className="flex min-h-0 flex-1 flex-col">
              <GutteredEditor
                value={prompt}
                onChange={setPrompt}
                ariaLabel={t("agent.detail.general.prompt.aria")}
                header={{
                  icon: <FileText className="h-3.5 w-3.5" strokeWidth={1.75} />,
                  title: t("agent.detail.general.prompt.title"),
                }}
                footer={<PromptStatsFooter value={prompt} />}
              />
            </div>
          </div>
        </>
      )}
    </AgentLayout>
  );
}

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
      description={
        error ? formatError(error) : t("agent.detail.loadError.body")
      }
      action={
        <Button variant="primary" onClick={onRetry}>
          {t("agent.detail.loadError.cta")}
        </Button>
      }
    />
  );
}
