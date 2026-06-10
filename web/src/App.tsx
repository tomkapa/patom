import { Navigate, Route, Routes } from "react-router-dom";
import { AcceptInvite } from "./pages/AcceptInvite";
import { AgentGeneral } from "./pages/AgentGeneral";
import { AgentLogs } from "./pages/AgentLogs";
import { AgentMemory } from "./pages/AgentMemory";
import { AgentTools } from "./pages/AgentTools";
import { ScheduledTasks } from "./pages/ScheduledTasks";
import { AgentsIndex } from "./pages/AgentsIndex";
import { ChatView } from "./pages/ChatView";
import { DemoView } from "./pages/DemoView";
import { ConnectionDetail } from "./pages/ConnectionDetail";
import { ConnectionsCatalog } from "./pages/ConnectionsCatalog";
import { ConnectionsList } from "./pages/ConnectionsList";
import { OAuthCallback } from "./pages/OAuthCallback";
import { Onboarding } from "./pages/Onboarding";
import { SettingsBilling } from "./pages/SettingsBilling";
import { SettingsComingSoon } from "./pages/SettingsComingSoon";
import { SettingsGeneral } from "./pages/SettingsGeneral";
import { SettingsIntegrations } from "./pages/SettingsIntegrations";
import { SettingsMembers } from "./pages/SettingsMembers";
import { SignIn } from "./pages/SignIn";
import { Protected } from "./components/organisms/Protected";
import { useLangFromOrg } from "./i18n";

export function App() {
  // Subscribe once at the root so the active org's language is mirrored
  // into the i18n module on initial load and on every org switch.
  // Pre-auth (no `me` yet) the hook is a no-op and `t()` keeps using the
  // browser-detected default.
  useLangFromOrg();
  return (
    <Routes>
      <Route path="/sign-in" element={<SignIn />} />
      {/* Invite landing — the page redeems the token itself and drives
          its own auth (a 401 bounces through /sign-in), so it is not
          wrapped in <Protected>. Must precede the /* catch-all. */}
      <Route path="/i/:slug/:token" element={<AcceptInvite />} />

      {/* Public scripted product demo — synthetic data, no signup. Mounted
          OUTSIDE <Protected> and before the /* catch-all so a logged-out
          visitor reaches it without a /me round-trip or sign-in bounce. */}
      <Route path="/demo" element={<DemoView />} />

      {/* First-time-user wizard. Wrapped in <Protected> so the JWT is
          resolved before any wizard step runs; the OnboardingGate
          (inside Protected) then keeps the user here until they finish
          and redirects past it once `org.onboarded === true`. */}
      <Route
        path="/onboarding"
        element={
          <Protected>
            <Onboarding />
          </Protected>
        }
      />

      <Route
        path="/connections"
        element={
          <Protected>
            <ConnectionsList />
          </Protected>
        }
      />
      <Route
        path="/connections/catalog"
        element={
          <Protected>
            <ConnectionsCatalog />
          </Protected>
        }
      />
      <Route
        path="/connections/oauth-callback"
        element={
          <Protected>
            <OAuthCallback />
          </Protected>
        }
      />
      <Route
        path="/connections/:id"
        element={
          <Protected>
            <ConnectionDetail />
          </Protected>
        }
      />
      <Route
        path="/agents"
        element={
          <Protected>
            <AgentsIndex />
          </Protected>
        }
      />
      <Route
        path="/agents/:id"
        element={<Navigate to="general" replace />}
      />
      <Route
        path="/agents/:id/general"
        element={
          <Protected>
            <AgentGeneral />
          </Protected>
        }
      />
      <Route
        path="/agents/:id/tools"
        element={
          <Protected>
            <AgentTools />
          </Protected>
        }
      />
      <Route
        path="/agents/:id/memory"
        element={
          <Protected>
            <AgentMemory />
          </Protected>
        }
      />
      <Route
        path="/agents/:id/scheduled"
        element={
          <Protected>
            <ScheduledTasks />
          </Protected>
        }
      />
      <Route
        path="/agents/:id/logs"
        element={
          <Protected>
            <AgentLogs />
          </Protected>
        }
      />
      <Route path="/settings" element={<Navigate to="/settings/general" replace />} />
      <Route
        path="/settings/general"
        element={
          <Protected>
            <SettingsGeneral />
          </Protected>
        }
      />
      <Route
        path="/settings/members"
        element={
          <Protected>
            <SettingsMembers />
          </Protected>
        }
      />
      <Route
        path="/settings/billing"
        element={
          <Protected>
            <SettingsBilling />
          </Protected>
        }
      />
      <Route
        path="/settings/integrations"
        element={
          <Protected>
            <SettingsIntegrations />
          </Protected>
        }
      />
      <Route
        path="/settings/notifications"
        element={
          <Protected>
            <SettingsComingSoon kind="notifications" />
          </Protected>
        }
      />
      <Route
        path="/*"
        element={
          <Protected>
            <ChatView />
          </Protected>
        }
      />
    </Routes>
  );
}
