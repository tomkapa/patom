// English string table.
//
// Keep keys flat and namespaced (`area.subarea.role`). `{name}`-style
// placeholders are supported via the lightweight in-tree interpolator
// in `index.ts::t()` — pass values via `t("key", { name: "…" })`. No
// runtime i18n dep (CLAUDE.md §8); promote only when ICU plurals are
// actually needed.
//
// Every key in this file MUST exist in `vi.ts`. The startup-time check in
// `index.ts` panics if they drift.

const en = {
  "signin.brand": "Relay",
  "signin.tagline": "The operator console for multi-agent sessions.",
  "signin.heading": "Sign in to Relay",
  "signin.subheading": "Continue with your Google account.",
  "signin.cta": "Continue with Google",
  "signin.legal": "By continuing, you agree to the Terms of Service and the Privacy Policy.",
  "signin.error.forbidden": "Access denied.",
  "signin.error.oauth_down": "Sign-in is temporarily unavailable. Please try again.",

  "sidebar.brand": "Relay",
  "sidebar.channels": "Channels",
  "sidebar.dms": "Direct Messages",
  "sidebar.empty_agents": "No agents registered.",

  "usermenu.signout": "Sign out",
  "usermenu.language.label": "Language",
  "usermenu.language.en": "English",
  "usermenu.language.vi": "Tiếng Việt",
  "usermenu.language.error": "Could not change language. Please try again.",

  "button.loading": "Loading",
  "time.justNow": "just now",
  "time.ago": "{value} ago",

  // ─── Connections ─────────────────────────────────────────────────
  "menu.connections": "Connections",
  "connections.workspace.label": "ACME WORKSPACE",
  "connections.nav.title": "Connections",
  "connections.nav.browse": "Browse catalog",
  "connections.nav.my": "My connections",
  "connections.breadcrumb.workspace": "Workspace",
  "connections.breadcrumb.connections": "Connections",
  "connections.breadcrumb.my": "My connections",
  "connections.breadcrumb.add": "Add connection",
  "connections.list.title": "My connections",
  "connections.list.subtitle":
    "Wired-up tools available to your workspace. Admins wire — agent owners allow.",
  "connections.list.add": "Add connection",
  "connections.list.filter": "All statuses",
  "connections.list.empty.title": "No connections yet",
  "connections.list.empty.body":
    "Browse the catalog to wire up Notion, Linear, GitHub, and more.",
  "connections.list.empty.cta": "Browse catalog",
  "connections.list.col.status": "STATUS",
  "connections.list.col.connection": "CONNECTION",
  "connections.list.col.owner": "OWNER",
  "connections.list.col.tools": "TOOLS",
  "connections.list.col.lastSeen": "LAST ACTIVE",
  "connections.list.col.enable": "ENABLE",
  "connections.status.ok": "Healthy",
  "connections.status.reconnect": "Reconnect",
  "connections.status.error": "Error",
  "connections.status.pending": "Pending",
  "connections.row.reconnect": "Reconnect",
  "connections.row.disconnect": "Disconnect",
  "connections.row.remove": "Remove",
  "connections.row.never": "never",
  "connections.row.toolsSuffix": "tools",
  "connections.catalog.title": "Add a connection",
  "connections.catalog.subtitle":
    "Connect Notion, Linear, GitHub, and more so your agents can act on real data.",
  "connections.catalog.search": "Search providers",
  "connections.catalog.added": "Already added",
  "connections.catalog.custom.title": "+ Custom",
  "connections.catalog.custom.blurb": "Paste an MCP server URL.",
  "connections.catalog.empty": "No providers match your search.",
  "connections.modal.cancel": "Cancel",
  "connections.modal.close": "Close",
  "connections.modal.oauth.eyebrow": "Connect",
  "connections.modal.oauth.title": "Connect {name} to your workspace",
  "connections.modal.oauth.bullet1":
    "Available to agents in your workspace once an admin enables it on the agent.",
  "connections.modal.oauth.bullet2": "You can disconnect at any time.",
  "connections.modal.oauth.bullet3":
    "{name} will ask you to choose which pages and databases to share.",
  "connections.modal.oauth.continue": "Continue to {name}",
  "connections.modal.token.tokenLabel": "{name} API token",
  "connections.modal.token.placeholder": "Paste your {name} API token",
  "connections.modal.token.note":
    "Stored encrypted. Never displayed after save.",
  "connections.modal.token.connect": "Connect",
  "connections.modal.token.testing": "Testing connection…",
  "connections.modal.token.error":
    "Couldn't reach {name}. Check the token and try again.",
  "connections.modal.reconnect.eyebrow": "Reconnect",
  "connections.modal.reconnect.title": "Reconnect {name}",
  "connections.modal.reconnect.alertTitle": "{name}'s permission expired",
  "connections.modal.reconnect.alertBody":
    "Token refresh failed. Reconnect to restore access.",
  "connections.modal.reconnect.lastSeen": "Last successful call",
  "connections.modal.reconnect.upstream": "Upstream response",
  "connections.modal.reconnect.notNow": "Not now",
  "connections.modal.reconnect.cta": "Reconnect to {name}",
  "connections.modal.custom.eyebrow": "Admin · Custom server",
  "connections.modal.custom.title": "Add a custom server",
  "connections.modal.custom.nameLabel": "Display name",
  "connections.modal.custom.namePlaceholder": "internal-search",
  "connections.modal.custom.urlLabel": "MCP server URL",
  "connections.modal.custom.urlHint":
    "URL must be reachable from Relay workers.",
  "connections.modal.custom.authLabel": "Authentication",
  "connections.modal.custom.authNone": "None",
  "connections.modal.custom.authToken": "API token",
  "connections.modal.custom.warn":
    "Any agent your admins enable will be able to call this URL with no credentials. Only use this for trusted internal servers.",
  "connections.modal.custom.cta": "Add server",
  "connections.modal.custom.error.alias":
    "Use lowercase letters, digits, _ or -, up to 16 characters.",
  "connections.modal.custom.error.url":
    "Enter a valid http:// or https:// URL.",
  "connections.callback.eyebrow.connecting": "OAuth · Step 2 of 3",
  "connections.callback.eyebrow.authorized": "OAuth · Step 3 of 3",
  "connections.callback.eyebrow.failed": "OAuth · Failed",
  "connections.callback.connecting.title": "Asking {name} to share access…",
  "connections.callback.connecting.body":
    "You should see {name}'s consent screen in a new tab.",
  "connections.callback.authorized.title": "{name} connected.",
  "connections.callback.authorized.discovering": "Discovering tools…",
  "connections.callback.authorized.body":
    "Tools will refresh automatically. {count} tools discovered so far.",
  "connections.callback.authorized.redirect": "Redirecting in {seconds}s.",
  "connections.callback.authorized.goNow": "Go to connection now →",
  "connections.callback.failed.title": "Connection failed",
  "connections.callback.failed.bodyDenied":
    "{name} did not return a token. Nothing was added.",
  "connections.callback.failed.bodyGeneric":
    "We couldn't complete the OAuth handshake. Nothing was added.",
  "connections.callback.failed.options": "Options",
  "connections.callback.failed.response": "Response",
  "connections.callback.failed.reference": "Reference id",
  "connections.callback.failed.back": "Back to catalog",
  "connections.callback.failed.retry": "Try again",
  "connections.callback.steps.redirected": "Redirected",
  "connections.callback.steps.awaiting": "Awaiting consent",
  "connections.callback.steps.discover": "Discover tools",
  "connections.confirm.removeTitle": "Remove this connection?",
  "connections.confirm.removeBody":
    "Agents that depend on it will lose access immediately.",
  "connections.confirm.disconnectTitle": "Disconnect credentials?",
  "connections.confirm.disconnectBody":
    "The server stays in your list but agents can't call it until you reconnect.",
  "connections.confirm.cancel": "Cancel",
  "connections.confirm.confirm": "Confirm",

  // ─── Connection Detail ──────────────────────────────────────────────
  "connections.detail.header.subtitle":
    "{auth} · Workspace connection · {count} tools exposed",
  "connections.detail.header.disable": "Disable",
  "connections.detail.header.enable": "Enable",
  "connections.detail.header.disconnect": "Disconnect",
  "connections.detail.auth.oauth": "OAuth",
  "connections.detail.auth.token": "API token",
  "connections.detail.auth.none": "No credentials",
  "connections.detail.status.eyebrow": "01 · STATUS",
  "connections.detail.status.updated": "Updated {value}",
  "connections.detail.status.lastCall": "LAST SUCCESSFUL CALL",
  "connections.detail.status.lastCallSub": "via MCP dispatch path",
  "connections.detail.status.lastError": "LAST ERROR",
  "connections.detail.status.noError": "No errors recorded",
  "connections.detail.status.never": "No successful calls yet",
  "connections.detail.status.createdBy": "Added by {email}",
  "connections.detail.status.noCreator": "Creator unknown",
  "connections.detail.status.tokenExpires": "Token refreshes automatically · expires {value}",
  "connections.detail.status.tokenStandard": "Credentials stored encrypted",
  "connections.detail.status.manageOn": "Manage on {name}",
  "connections.detail.tools.eyebrow": "02 · TOOLS EXPOSED",
  "connections.detail.tools.filterPlaceholder": "Filter tools",
  "connections.detail.tools.filterAria": "Filter tools by name",
  "connections.detail.tools.showMore": "Show {count} more tools",
  "connections.detail.tools.showLess": "Show fewer tools",
  "connections.detail.tools.empty": "No tools match this filter.",
  "connections.detail.activity.eyebrow": "03 · RECENT ACTIVITY",
  "connections.detail.activity.last50": "Last 50 calls",
  "connections.detail.activity.audit": "View in audit log",
  "connections.detail.activity.col.time": "Time",
  "connections.detail.activity.col.tool": "Tool",
  "connections.detail.activity.col.agent": "Agent",
  "connections.detail.activity.col.latency": "Latency",
  "connections.detail.activity.col.outcome": "Outcome",
  "connections.detail.activity.empty": "No tool calls yet",
  "connections.detail.activity.outcomeOk": "OK",
  "connections.detail.activity.outcomeError": "Error",
  "connections.detail.activity.loadError": "Could not load recent activity",
  "connections.detail.activity.agentUnknown": "Unknown agent",
  "connections.detail.usedBy.eyebrow": "04 · USED BY",
  "connections.detail.usedBy.count": "{count} agents",
  "connections.detail.usedBy.empty":
    "No agents have been granted access to this connection yet.",
  "connections.detail.danger.eyebrow": "05 · DANGER ZONE",
  "connections.detail.danger.disable.title": "Disable connection",
  "connections.detail.danger.disable.body":
    "Agents that depend on it will stop being able to call its tools until you re-enable.",
  "connections.detail.danger.disable.cta": "Disable",
  "connections.detail.danger.enable.title": "Enable connection",
  "connections.detail.danger.enable.body":
    "Re-allow agents to call this connection's tools.",
  "connections.detail.danger.enable.cta": "Enable",
  "connections.detail.danger.disconnect.title": "Disconnect",
  "connections.detail.danger.disconnect.body":
    "Agents lose access to this connection's tools. This also revokes Relay's credentials locally.",
  "connections.detail.danger.disconnect.ack": "I understand the consequences",
  "connections.detail.danger.disconnect.cta": "Disconnect",
  "connections.detail.notFound.title": "Connection not found",
  "connections.detail.notFound.body":
    "The connection has been removed, or you don't have access to it.",
  "connections.detail.notFound.cta": "Back to connections",

  // ─── Agent detail (per-agent allowlist) ──────────────────────────────
  "agent.detail.nav.aria": "Agent settings",
  "agent.detail.nav.eyebrow": "AGENTS",
  "agent.detail.nav.section": "AGENT SETTINGS",
  "agent.detail.nav.general": "General",
  "agent.detail.nav.tools": "Tools & connections",
  "agent.detail.nav.logs": "Logs & metrics",
  "agent.detail.switcher.aria": "Switch agent",
  "agent.detail.switcher.empty": "No agents",
  "agent.index.empty.title": "No agents yet",
  "agent.index.empty.body":
    "This workspace has no agents. Create one from the Agents settings.",
  "agent.detail.breadcrumb.agents": "Agents",
  "agent.detail.breadcrumb.general": "General",
  "agent.detail.breadcrumb.tools": "Tools & connections",
  "agent.detail.general.title": "General",
  "agent.detail.general.subtitle":
    "Configure the agent's name, description, system prompt, and model.",
  "agent.detail.general.save": "Save",
  "agent.detail.general.modelLabel": "Model",
  "agent.detail.general.modelCaption":
    "The language model this agent uses for inference.",
  "agent.detail.general.modelInherit": "Workspace default",
  "agent.detail.general.modelAria": "Pick a model",
  "agent.detail.general.prompt.title": "Job description",
  "agent.detail.general.prompt.lines": "{n} lines",
  "agent.detail.general.prompt.tokens": "~{n} tokens",
  "agent.detail.general.prompt.chars": "{n} chars",
  "agent.detail.general.prompt.fontHint": "IBM Plex Mono · 13px",
  "agent.detail.general.prompt.aria": "System prompt",
  "agent.detail.tools.title": "Tools & connections",
  "agent.detail.tools.subtitle":
    "Pick which workspace connections {name} can use, and optionally narrow down to individual tools.",
  "agent.detail.tools.save": "Save",
  "agent.detail.tools.sectionEyebrow": "01 · THIS AGENT CAN USE",
  "agent.detail.tools.sectionCount": "{enabled} of {total} enabled",
  "agent.detail.tools.empty":
    "No connections in this workspace yet. Add one from Connections first.",
  "agent.detail.tools.row.allOf": "{total} tools · all available",
  "agent.detail.tools.row.someOf": "{enabled} of {total} tools",
  "agent.detail.tools.row.available": "{total} tools",
  "agent.detail.tools.row.empty": "No tools discovered",
  "agent.detail.tools.row.statusReconnect": "RECONNECT",
  "agent.detail.tools.row.statusError": "ERROR",
  "agent.detail.tools.row.toggleAria": "Allow {name}",
  "agent.detail.tools.row.expand": "Show tools",
  "agent.detail.tools.row.collapse": "Hide tools",
  "agent.detail.summary.eyebrow": "03 · CAPABILITY SUMMARY",
  "agent.detail.summary.connections": "Connections enabled",
  "agent.detail.summary.tools": "Tools allowed",
  "agent.detail.activity.eyebrow": "02 · WHAT {name} JUST DID",
  "agent.detail.activity.subtitle": "Last 20 calls across all connections",
  "agent.detail.activity.auditLink": "Full audit log",
  "agent.detail.activity.empty": "No tool calls recorded for this agent yet.",
  "agent.detail.activity.loadError": "Could not load recent activity",
  "agent.detail.activity.unknownConnection": "Removed connection",
  "agent.detail.activity.outcomeError": "Error",
  "agent.detail.activity.col.time": "Time",
  "agent.detail.activity.col.connection": "Connection",
  "agent.detail.activity.col.tool": "Tool",
  "agent.detail.activity.col.outcome": "Outcome",
  "agent.detail.notFound.title": "Agent not found",
  "agent.detail.notFound.body":
    "The agent has been removed, or you don't have access to it.",
  "agent.detail.notFound.cta": "Back to home",
  "agent.detail.loadError.title": "Couldn't load agent",
  "agent.detail.loadError.body":
    "Something went wrong fetching this agent. Try again in a moment.",
  "agent.detail.loadError.cta": "Retry",

  "thread.wireRequest.badge": "Connect requested",
  "thread.wireRequest.badgeConnected": "Connected",
  "thread.wireRequest.learnMore": "Learn more",
  "thread.wireRequest.connect": "Connect {name}",
  "thread.wireRequest.connecting": "Opening…",
  "thread.wireRequest.connected": "{name} connected",
  "thread.wireRequest.created": "Created — finish setup in Connections.",
  "thread.wireRequest.error.generic":
    "Couldn't start the connection. Try again from Connections.",
} as const;

export type TranslationKey = keyof typeof en;
export type TranslationTable = Record<TranslationKey, string>;

export default en satisfies TranslationTable;
