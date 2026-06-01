import { Navigate } from "react-router-dom";
import { AppFrame } from "../components/templates/AppFrame";
import { Spinner } from "../components/atoms/Spinner";
import { EmptyState } from "../components/molecules/EmptyState";
import { useAgents } from "../hooks/useAgents";
import { useT } from "../i18n";

export function AgentsIndex() {
  const { t } = useT();
  const q = useAgents();

  if (q.isLoading) {
    return (
      <Frame>
        <div className="flex flex-1 items-center justify-center text-[var(--color-muted-foreground)]">
          <Spinner size={16} />
        </div>
      </Frame>
    );
  }

  const first = q.data?.[0];
  if (first) return <Navigate to={`/agents/${first.id}`} replace />;

  return (
    <Frame>
      <div className="flex flex-1 items-center justify-center p-8">
        <EmptyState
          title={t("agent.index.empty.title")}
          description={t("agent.index.empty.body")}
        />
      </div>
    </Frame>
  );
}

function Frame({ children }: { children: React.ReactNode }) {
  const { t } = useT();
  return (
    <AppFrame title={t("menu.agent")}>
      <div className="flex min-h-0 flex-1 flex-col bg-[var(--color-card)]">
        {children}
      </div>
    </AppFrame>
  );
}
