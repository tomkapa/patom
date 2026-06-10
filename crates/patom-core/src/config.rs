use std::collections::HashMap;
use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;

use chrono_tz::Tz;
use config::{Config, ConfigError, Environment};
use serde::Deserialize;
use thiserror::Error;

use crate::auth::limits::MAX_CORS_ALLOWED_ORIGINS;
use crate::auth::{CookieDomain, Email, IssuerUrl};
use crate::provider::{Model, ProviderId};
use crate::types::SecretString;

/// Default SPA dist path when `PATOM_WEB_DIST` is unset. Matches
/// `web/build.ts`'s `outdir`; operators running the binary from outside
/// the repo root must override.
const DEFAULT_WEB_DIST: &str = "./web/dist";

#[derive(Debug, Error)]
pub enum SettingsError {
    #[error("config source: {0}")]
    Source(#[from] ConfigError),

    #[error(
        "no provider api key set; set at least one of: ANTHROPIC_API_KEY, OPENAI_API_KEY, \
         DEEPSEEK_API_KEY"
    )]
    NoProviderKey,

    #[error("default model `{model}` resolves to provider `{provider}` which is not configured")]
    DefaultModelProviderNotConfigured {
        model: &'static str,
        provider: ProviderId,
    },

    #[error("embedding configuration missing; set EMBEDDING_API_KEY and EMBEDDING_MODEL")]
    MissingEmbedding,

    #[error("default timezone {raw:?} is not a valid IANA name")]
    InvalidDefaultTimezone { raw: String },

    #[error("auth: jwt secret too short — need at least 32 bytes")]
    AuthSecretTooShort,

    #[error("auth: PATOM_WEB_BASE_URL is not a valid origin: {raw:?} ({reason})")]
    InvalidWebBaseUrl { raw: String, reason: &'static str },

    #[error("auth: PATOM_OIDC_ISSUER is not a valid https issuer: {raw:?} ({reason})")]
    InvalidOidcIssuer { raw: String, reason: &'static str },

    #[error("auth: login redirect URL is not a valid http(s) URL: {raw:?} ({reason})")]
    InvalidRedirectUrl { raw: String, reason: &'static str },

    #[error(
        "auth: login provider not configured; set all of PATOM_OIDC_ISSUER + \
         PATOM_OIDC_CLIENT_ID + PATOM_OIDC_CLIENT_SECRET + PATOM_OIDC_REDIRECT_URL"
    )]
    MissingLoginProvider,

    #[error("auth: PATOM_COOKIE_DOMAIN is not a valid cookie domain: {raw:?} ({reason})")]
    InvalidCookieDomain { raw: String, reason: &'static str },

    #[error("auth: PATOM_CORS_ALLOWED_ORIGINS entry is not a valid origin: {raw:?} ({reason})")]
    InvalidCorsOrigin { raw: String, reason: &'static str },

    #[error("auth: PATOM_CORS_ALLOWED_ORIGINS has too many entries: max {max}, got {got}")]
    TooManyCorsOrigins { max: usize, got: usize },

    #[error("analytics: PATOM_POSTHOG_HOST {raw:?} is not a valid http(s) origin ({reason})")]
    InvalidPosthogHost { raw: String, reason: &'static str },

    #[error(
        "slack: partial configuration; set all of PATOM_SLACK_SIGNING_SECRET, \
         PATOM_SLACK_CLIENT_ID, PATOM_SLACK_CLIENT_SECRET — or none"
    )]
    PartialSlackConfig,

    #[error("slack: PATOM_SLACK_CLIENT_ID must be non-empty after trim")]
    InvalidSlackClientId,

    #[error(
        "s3: partial configuration; set all of PATOM_S3_ENDPOINT, \
         PATOM_S3_BUCKET, PATOM_S3_ACCESS_KEY_ID, PATOM_S3_SECRET_ACCESS_KEY, \
         PATOM_S3_PUBLIC_HOST — or none"
    )]
    PartialS3Config,

    #[error("s3: PATOM_S3_ENDPOINT {raw:?} is not a valid http(s) origin ({reason})")]
    InvalidS3Endpoint { raw: String, reason: &'static str },

    #[error(
        "s3: PATOM_S3_PUBLIC_HOST {raw:?} is not a valid http(s) base URL \
         (optional path prefix allowed) ({reason})"
    )]
    InvalidS3PublicHost { raw: String, reason: &'static str },

    #[error("s3: PATOM_S3_{field} must not be empty")]
    EmptyS3Field { field: &'static str },

    #[error(
        "smtp: partial configuration; set all of PATOM_SMTP_HOST, \
         PATOM_SMTP_USERNAME, PATOM_SMTP_PASSWORD, PATOM_EMAIL_FROM — or none"
    )]
    PartialSmtpConfig,

    #[error("smtp: PATOM_SMTP_{field} must not be empty")]
    EmptySmtpField { field: &'static str },

    #[error("smtp: PATOM_EMAIL_FROM {raw:?} is not a valid email address ({reason})")]
    InvalidEmailFrom { raw: String, reason: &'static str },
}

/// Process-wide configuration loaded once at startup. Secrets are wrapped in
/// [`SecretString`] so a stray `tracing::debug!(?settings)` cannot leak them.
#[derive(Debug, Clone)]
pub struct Settings {
    /// Every LLM backend the operator has wired credentials for. At least one
    /// must be present (validated at construction). The chat router picks one
    /// per agent via [`crate::provider::ProviderRegistry`].
    pub providers: ProviderSettings,
    pub brave_search_api_key: SecretString,
    /// Workspace default model — used when an agent row has no `model` of its
    /// own. Catalog-resolved at parse time, so the provider it routes to is
    /// known and required to be configured (see
    /// [`SettingsError::DefaultModelProviderNotConfigured`]).
    pub model: Model,
    pub http_addr: SocketAddr,
    /// Postgres connection string. Required at startup — there is no in-memory
    /// fallback. Wrapped in [`SecretString`] because the URL embeds a password.
    pub database_url: SecretString,
    /// Embedding provider configuration. Required: the memory subsystem
    /// refuses to start without one. Decoupled from the chat provider so
    /// chat and embeddings can point at different vendors.
    pub embedding: EmbeddingSettings,
    /// Process-wide fallback IANA timezone applied when an agent calls
    /// `schedule_task` without specifying `tz`. Future: per-organisation
    /// override loaded by id; until then the resolver hands every caller
    /// this same value.
    pub default_timezone: Tz,
    /// Auth / tenancy configuration.
    pub auth: AuthSettings,
    /// SPA dist path the `ServeDir` fallback reads from. Sourced from
    /// `PATOM_WEB_DIST` (default `./web/dist`).
    pub web_dist: PathBuf,
    /// PostHog project API key (`PATOM_POSTHOG_KEY`). When `Some`, the server
    /// injects it into `index.html` at startup so the SPA picks it up at
    /// runtime without a network roundtrip. `None` (default) keeps analytics
    /// a hard no-op — suitable for OSS / self-host deployments.
    pub posthog_key: Option<String>,
    /// PostHog ingest host (`PATOM_POSTHOG_HOST`). Defaults to the EU endpoint
    /// when unset; only relevant when `posthog_key` is `Some`.
    pub posthog_host: Option<String>,
    /// Slack adapter — present iff all `PATOM_SLACK_*` env vars are
    /// set. `None` is a first-class deployment (the Slack routes and
    /// background workers stay un-spawned).
    pub slack: Option<SlackSettings>,
    /// Generic S3-compatible object storage (MinIO / AWS / self-hosted /
    /// Cloudflare R2). Present iff all required `PATOM_S3_*` env vars are
    /// set. When `None`, the upload endpoints 503 with "asset storage not
    /// configured" — deployments that don't care about avatar/icon uploads
    /// stay first-class.
    pub object_storage: Option<ObjectStorageSettings>,
    /// Outbound SMTP relay for transactional mail (member invites). Present
    /// iff the required `PATOM_SMTP_*` / `PATOM_EMAIL_FROM` env vars are set.
    /// When `None`, [`crate::orgs::LogMailer`] is wired and invite links are
    /// recoverable from the structured logs — a first-class deployment for
    /// local dev and operators who haven't provisioned a relay yet.
    pub smtp: Option<SmtpSettings>,
}

/// Outbound SMTP relay configuration for transactional mail.
///
/// The four credential/identity fields are required as a group; `port` is
/// optional (defaults to [`DEFAULT_SMTP_PORT`]). The `TryFrom<RawSettings>`
/// impl rejects partial sets via [`SettingsError::PartialSmtpConfig`] and
/// parses `from` through [`Email`] at the boundary (CLAUDE.md §1) so the
/// mailer never re-validates an address.
#[derive(Debug, Clone)]
pub struct SmtpSettings {
    /// Relay hostname (e.g. `smtp.example.com`). Non-empty after trim.
    pub host: String,
    /// Submission port. 587 (STARTTLS) by default; 465 for implicit TLS.
    pub port: u16,
    /// SMTP AUTH username.
    pub username: SecretString,
    /// SMTP AUTH password / API token.
    pub password: SecretString,
    /// Envelope + header `From` address. Parsed through [`Email`] so it is
    /// a structurally valid mailbox by construction.
    pub from: Email,
    /// Optional display name rendered alongside `from` (e.g. `Patom`).
    pub from_name: Option<String>,
}

/// S3-compatible object-storage configuration.
///
/// The endpoint is resolved at the config boundary so the asset layer
/// never composes a URL itself (CLAUDE.md §1). For Cloudflare R2, point
/// `PATOM_S3_ENDPOINT` at `https://<account_id>.r2.cloudflarestorage.com`
/// and set `PATOM_S3_REGION=auto`.
///
/// The five non-region fields are required as a group; the
/// `TryFrom<RawSettings>` impl rejects partial sets via
/// [`SettingsError::PartialS3Config`].
#[derive(Debug, Clone)]
pub struct ObjectStorageSettings {
    /// S3 endpoint URL (e.g. `https://s3.us-east-1.amazonaws.com`,
    /// `http://minio:9000`, or `https://<acct>.r2.cloudflarestorage.com`).
    /// Validated at the boundary: http(s) origin, no path/query/fragment,
    /// no trailing slash.
    pub endpoint: String,
    /// SigV4 region label. `us-east-1` by default; `auto` for R2.
    pub region: String,
    /// Bucket name (e.g. `patom-assets-prod`).
    pub bucket: String,
    /// Access key id.
    pub access_key_id: SecretString,
    /// Secret access key.
    pub secret_access_key: SecretString,
    /// Public-facing base URL the FE renders, e.g. `https://asset.example`
    /// (CDN) or `http://minio:9000/<bucket>` (path-style direct).
    /// Validated at the boundary: `http(s)://` scheme, an optional path
    /// prefix, no query/fragment, no trailing slash.
    pub public_host: String,
}

/// Slack-side configuration. All fields required as a group; the
/// `TryFrom<RawSettings>` impl rejects partial sets via
/// [`SettingsError::PartialSlackConfig`].
#[derive(Debug, Clone)]
pub struct SlackSettings {
    /// HMAC-SHA256 signing secret from the Slack app's Basic Information
    /// page. Validates every inbound webhook in `slack::events`.
    pub signing_secret: SecretString,
    /// OAuth client id for the Slack app (public; no secret material).
    pub client_id: String,
    /// OAuth client secret.
    pub client_secret: SecretString,
    /// Derived: `<auth.oauth_redirect_base>/slack/oauth/callback`. Slack
    /// must whitelist this URL on the app's "OAuth & Permissions" page.
    pub redirect_url: String,
}

/// Patom-supported MCP OAuth client credentials, keyed by the env-var
/// middle (lowercased) of `PATOM_<MID>_CLIENT_ID` / `_CLIENT_SECRET`.
///
/// Sourced from env so the secrets stay in the secret manager, never the
/// database. The catalog-id ↔ env-var-middle mapping (`-` → `_`) is the
/// caller's concern; this type is namespace-agnostic.
#[derive(Clone)]
pub struct PlatformOAuthClient {
    pub client_id: SecretString,
    pub client_secret: SecretString,
}

impl fmt::Debug for PlatformOAuthClient {
    // Both fields are already `SecretString` so Debug is redacted by
    // construction; the explicit impl exists so a future plaintext field
    // can't be added without the Debug review catching it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PlatformOAuthClient")
            .field("client_id", &self.client_id)
            .field("client_secret", &self.client_secret)
            .finish()
    }
}

/// Parse `PATOM_<MID>_CLIENT_ID` / `_CLIENT_SECRET` pairs from env.
///
/// Unpaired halves, empty values, and non-`PATOM_` keys are silently
/// dropped — boot must not fail on stray env vars. The returned map is
/// keyed by the lowercased middle (e.g. `google`, `microsoft_365`).
/// Catalog membership and `-` ↔ `_` normalization are downstream concerns
/// handled by the resolver.
pub fn parse_platform_oauth_clients<I>(vars: I) -> HashMap<String, PlatformOAuthClient>
where
    I: IntoIterator<Item = (String, String)>,
{
    const PREFIX: &str = "PATOM_";
    const ID_SUFFIX: &str = "_CLIENT_ID";
    const SECRET_SUFFIX: &str = "_CLIENT_SECRET";
    // Login-OIDC creds share the `PATOM_<MID>_CLIENT_*` shape but are NOT
    // MCP platform clients (they belong to `AuthSettings.oidc_*`). Exclude
    // the reserved middle so a generic-OIDC deployment doesn't synthesize a
    // bogus `platform_oauth_clients["oidc"]` and so a real future `oidc`
    // catalog entry can't collide with it.
    const RESERVED_MIDDLES: &[&str] = &["oidc"];

    let mut ids: HashMap<String, String> = HashMap::new();
    let mut secrets: HashMap<String, String> = HashMap::new();

    for (key, value) in vars {
        let Some(remainder) = key.strip_prefix(PREFIX) else {
            continue;
        };
        if let Some(middle) = remainder.strip_suffix(SECRET_SUFFIX)
            && !middle.is_empty()
            && !RESERVED_MIDDLES.contains(&middle.to_ascii_lowercase().as_str())
        {
            secrets.insert(middle.to_ascii_lowercase(), value);
        } else if let Some(middle) = remainder.strip_suffix(ID_SUFFIX)
            && !middle.is_empty()
            && !RESERVED_MIDDLES.contains(&middle.to_ascii_lowercase().as_str())
        {
            ids.insert(middle.to_ascii_lowercase(), value);
        }
    }

    let mut out = HashMap::with_capacity(ids.len());
    for (middle, id_value) in ids {
        let Some(secret_value) = secrets.remove(&middle) else {
            continue;
        };
        let Ok(client_id) = SecretString::try_from(id_value) else {
            continue;
        };
        let Ok(client_secret) = SecretString::try_from(secret_value) else {
            continue;
        };
        out.insert(
            middle,
            PlatformOAuthClient {
                client_id,
                client_secret,
            },
        );
    }
    out
}

/// Auth subsystem configuration. All fields are required; the OAuth
/// flow refuses to start without a real Google client.
#[derive(Debug, Clone)]
pub struct AuthSettings {
    /// HS256 signing secret for session JWTs. Must be ≥32 bytes.
    pub jwt_secret: SecretString,
    /// Login-OIDC issuer (ADR-0011), from `PATOM_OIDC_ISSUER`. Endpoints +
    /// JWKS are discovered from this at startup. Point it at any OIDC IdP
    /// (Google, Keycloak, Okta, Entra, …).
    pub oidc_issuer: IssuerUrl,
    /// Login-OIDC client id, from `PATOM_OIDC_CLIENT_ID`. The MCP `google`
    /// catalog entry has its own credentials in [`platform_oauth_clients`]
    /// — this is **login only**.
    pub oidc_client_id: SecretString,
    /// Login-OIDC client secret, from `PATOM_OIDC_CLIENT_SECRET`.
    pub oidc_client_secret: SecretString,
    /// Redirect URL registered with the IdP, e.g.
    /// `http://localhost:8080/auth/oidc/callback`.
    pub oidc_redirect_url: String,
    /// First-admin bootstrap toggle (`PATOM_BOOTSTRAP_ADMIN`, ADR-0011
    /// §3). When true and the org table is empty, the first login creates
    /// the initial org and owns it. Default false.
    pub bootstrap_admin: bool,
    /// GitHub OAuth App client id, used by the MCP shared-client seeder
    /// (`src/mcp/oauth/shared_seed.rs`) to register a platform-owned OAuth
    /// client keyed by issuer `https://github.com`. Required at startup:
    /// GitHub does not support RFC 7591 DCR, so without this the `github`
    /// catalog row cannot complete an OAuth flow and the connector ships
    /// dead. A missing env var surfaces as the `config` crate's own
    /// "missing field" error, same shape as `patom_github_client_secret`.
    pub github_client_id: SecretString,
    /// GitHub OAuth App client secret. Required at startup; same
    /// rationale as `github_client_id`.
    pub github_client_secret: SecretString,
    /// Platform-supported MCP OAuth clients, keyed by env-var middle
    /// (lowercased). Sourced from env so the secrets never touch the DB.
    /// Empty if the operator has wired no platform vendors — entries that
    /// rely on DCR work without these.
    pub platform_oauth_clients: HashMap<String, PlatformOAuthClient>,
    /// Whether to set the `Secure` flag on the session cookie. Off in
    /// local-dev to keep `http://localhost` workable; on everywhere
    /// else.
    pub cookie_secure: bool,
    /// Master KEK used to derive per-org KEKs for the MCP credentials
    /// envelope. Base64-encoded 32 bytes; rejected at the boundary if
    /// missing or wrong size. Sourced from `PATOM_MASTER_KEK`.
    pub master_kek: SecretString,
    /// Base URL Patom tells vendors to redirect back to after consent.
    /// The OAuth callback path is appended to this; e.g.
    /// `http://localhost:8080` → `http://localhost:8080/mcp-oauth/callback`.
    /// Sourced from `PATOM_OAUTH_REDIRECT_BASE`.
    pub oauth_redirect_base: String,
    /// Origin of the SPA (e.g. `http://localhost:5173` in dev). When set,
    /// the BE prepends this to the post-OAuth-callback redirect so the
    /// browser lands on the FE host instead of the BE host. Empty in
    /// same-origin prod deployments where BE and FE share an origin.
    /// Sourced from `PATOM_WEB_BASE_URL`.
    pub web_base_url: Option<String>,
    /// Shared cookie `Domain` for the session + CSRF cookies. When set
    /// (e.g. `.patom.app`) the cookies are visible across the apex
    /// marketing site and the `app.` subdomain so the landing page can
    /// read the logged-in state. `None` (the localhost-dev default)
    /// omits the attribute, keeping cookies host-only. Sourced from
    /// `PATOM_COOKIE_DOMAIN`.
    pub cookie_domain: Option<CookieDomain>,
    /// Cross-origin allowlist for the `/api` subtree, e.g.
    /// `https://patom.app`. Each entry is a canonical origin
    /// (`scheme://host[:port]`, no path). Empty (the default) means no
    /// CORS layer is attached — same-origin app traffic is unaffected.
    /// Sourced from `PATOM_CORS_ALLOWED_ORIGINS` (comma-separated).
    pub cors_allowed_origins: Vec<String>,
    /// Launch-period abuse guardrails master switch (#121), from
    /// `PATOM_LAUNCH_GUARDRAILS`. Default **false** — the OSS / self-host and
    /// any pre-launch build behave exactly like baseline. When true, the
    /// cloud launch promo's anti-farming policy is active: self-service org
    /// creation is capped at [`crate::auth::limits::MAX_ORGS_PER_USER_LAUNCH`]
    /// (one workspace per identity → one signup grant) and the OAuth callback
    /// throttles per-IP signup velocity. A launch-only feature: flipping this
    /// off reverts to baseline without a code change (see
    /// `crate::http::launch_guardrails`).
    pub launch_guardrails: bool,
    /// Number of trusted reverse-proxy hops in front of the app, from
    /// `PATOM_TRUSTED_PROXY_HOPS`. Default **0** — no proxy, so no client IP
    /// is trusted and the signup throttle is inert (correct for local-dev /
    /// self-host). Set to the real ingress hop count in a proxied deployment
    /// (k8s ingress → 1) so [`crate::http::launch_guardrails::ClientIp`] reads
    /// the genuine client address from `X-Forwarded-For` rather than a
    /// spoofable left-most entry.
    pub trusted_proxy_hops: u8,
}

/// Embedding-provider settings — `EMBEDDING_API_KEY` /
/// `EMBEDDING_BASE_URL` / `EMBEDDING_MODEL`. Required as a group:
/// either all three (api_key + model, base_url optional) or none.
#[derive(Debug, Clone)]
pub struct EmbeddingSettings {
    pub api_key: SecretString,
    pub base_url: Option<String>,
    pub model: String,
    /// Vector dimension produced by the model. Must match the
    /// `agent_memories.embedding` column (1536 in migration 9).
    pub dimensions: usize,
}

/// Credentials for every LLM backend the operator wired up.
///
/// Each field is independently optional; the `TryFrom<RawSettings>` impl
/// requires at least one to be present and additionally requires that the
/// default model's provider is configured — so the workspace default is
/// always routable at startup. Per-agent models that point at a provider
/// the operator has since dropped from config degrade to the default at
/// resolve time (see [`crate::agents::StaticAgentModelResolver`]).
#[derive(Debug, Clone, Default)]
pub struct ProviderSettings {
    pub anthropic: Option<ProviderCredentials>,
    pub openai: Option<ProviderCredentials>,
    pub deepseek: Option<ProviderCredentials>,
}

impl ProviderSettings {
    /// Whether `id` has credentials configured.
    #[must_use]
    pub const fn has(&self, id: ProviderId) -> bool {
        match id {
            ProviderId::Anthropic => self.anthropic.is_some(),
            ProviderId::Openai => self.openai.is_some(),
            ProviderId::Deepseek => self.deepseek.is_some(),
        }
    }

    /// Iterator over every configured [`ProviderId`].
    pub fn configured(&self) -> impl Iterator<Item = ProviderId> + '_ {
        ProviderId::ALL.iter().copied().filter(|&id| self.has(id))
    }
}

/// One backend's credentials: API key plus an optional base-URL override
/// (lets DeepSeek reuse the OpenAI SDK against a non-default host, etc.).
#[derive(Debug, Clone)]
pub struct ProviderCredentials {
    pub api_key: SecretString,
    pub base_url: Option<String>,
}

/// Flat env shape — every provider's credentials are optional. At least one
/// provider must be configured; any combination of the three is legal, and
/// the per-agent model picks which one each turn routes to. Kept private
/// because [`Settings`] is the validated type.
#[derive(Debug, Deserialize)]
struct RawSettings {
    #[serde(default)]
    openai_api_key: Option<SecretString>,
    #[serde(default)]
    openai_base_url: Option<String>,

    #[serde(default)]
    anthropic_api_key: Option<SecretString>,
    #[serde(default)]
    anthropic_base_url: Option<String>,

    #[serde(default)]
    deepseek_api_key: Option<SecretString>,
    #[serde(default)]
    deepseek_base_url: Option<String>,

    brave_search_api_key: SecretString,
    #[serde(default = "default_model")]
    model: Model,
    #[serde(default = "default_http_addr")]
    http_addr: SocketAddr,
    database_url: SecretString,

    #[serde(default)]
    embedding_api_key: Option<SecretString>,
    #[serde(default)]
    embedding_base_url: Option<String>,
    #[serde(default)]
    embedding_model: Option<String>,
    #[serde(default)]
    embedding_dimensions: Option<usize>,

    #[serde(default = "default_timezone_raw")]
    default_timezone: String,

    // Auth — required at startup. Missing values surface as the
    // `config` crate's own "missing field" error via `SettingsError::Source`,
    // same as `database_url` / `brave_search_api_key` above.
    patom_jwt_secret: SecretString,
    // Login provider (ADR-0011). The generic OIDC path: all four
    // `patom_oidc_*` values are required, resolved + validated in
    // `TryFrom<RawSettings>`. Optional at the serde layer so a missing
    // one surfaces as the unified `MissingLoginProvider` error rather
    // than the `config` crate's per-field "missing field".
    #[serde(default)]
    patom_oidc_issuer: Option<String>,
    #[serde(default)]
    patom_oidc_client_id: Option<SecretString>,
    #[serde(default)]
    patom_oidc_client_secret: Option<SecretString>,
    #[serde(default)]
    patom_oidc_redirect_url: Option<String>,
    // First-admin bootstrap (ADR-0011 §3). Default false — promotion is
    // a deliberate operator act, never a silent default.
    #[serde(default)]
    patom_bootstrap_admin: bool,
    // GitHub MCP shared OAuth App — required at startup. GitHub does not
    // expose RFC 7591 DCR, so the platform-owned OAuth App is the only
    // way the `github` catalog row can complete a flow. Required (unlike
    // the optional `patom_oidc_*` above); a missing env var surfaces as
    // the `config` crate's own "missing field" error.
    patom_github_client_id: SecretString,
    patom_github_client_secret: SecretString,
    // Secure by default — forgetting to set this in any https-fronted
    // deploy must not silently drop the `Secure` cookie flag. Local-dev
    // (http://localhost) overrides via `PATOM_COOKIE_SECURE=false` in
    // `.env`.
    #[serde(default = "default_cookie_secure")]
    patom_cookie_secure: bool,
    // R2 envelope encryption master key, base64-encoded 32 bytes.
    patom_master_kek: SecretString,
    // R3 upstream-OAuth redirect base URL. The MCP OAuth callback path is
    // appended at runtime; the AS sees `{this}/mcp-oauth/callback`.
    patom_oauth_redirect_base: String,
    // Optional SPA origin. When set, the BE prepends this to the
    // post-OAuth-callback redirect so the browser lands on the FE host
    // instead of the BE host (dev: FE on Vite/Bun, BE on 8080).
    #[serde(default)]
    patom_web_base_url: Option<String>,
    // Optional shared cookie Domain (e.g. `.patom.app`). Validated into
    // a `CookieDomain` in `TryFrom`; `None` omits the attribute.
    #[serde(default)]
    patom_cookie_domain: Option<String>,
    // Optional comma-separated CORS origin allowlist for `/api`
    // (e.g. `https://patom.app`). Each entry validated via `parse_origin`
    // in `TryFrom`; `None`/empty means no CORS layer.
    #[serde(default)]
    patom_cors_allowed_origins: Option<String>,
    // Launch-period abuse guardrails (#121). Both default off so any build
    // that does not opt in behaves like baseline; cloud sets them at rollout.
    #[serde(default)]
    patom_launch_guardrails: bool,
    #[serde(default)]
    patom_trusted_proxy_hops: u8,
    #[serde(default = "default_web_dist")]
    patom_web_dist: PathBuf,
    #[serde(default)]
    patom_posthog_key: Option<String>,
    #[serde(default)]
    patom_posthog_host: Option<String>,

    // Slack adapter — all three are optional individually but accepted
    // only as a complete set (validation in `TryFrom<RawSettings>`).
    #[serde(default)]
    patom_slack_signing_secret: Option<SecretString>,
    #[serde(default)]
    patom_slack_client_id: Option<String>,
    #[serde(default)]
    patom_slack_client_secret: Option<SecretString>,

    // S3-compatible object storage — same all-or-nothing rule as Slack
    // for the five required fields. `region` is optional (defaulted).
    #[serde(default)]
    patom_s3_endpoint: Option<String>,
    #[serde(default)]
    patom_s3_region: Option<String>,
    #[serde(default)]
    patom_s3_bucket: Option<String>,
    #[serde(default)]
    patom_s3_access_key_id: Option<SecretString>,
    #[serde(default)]
    patom_s3_secret_access_key: Option<SecretString>,
    #[serde(default)]
    patom_s3_public_host: Option<String>,

    // SMTP relay — same all-or-nothing rule as Slack/S3 for the four
    // required fields. `port` is optional (defaulted to 587) and
    // `from_name` is genuinely optional.
    #[serde(default)]
    patom_smtp_host: Option<String>,
    #[serde(default)]
    patom_smtp_port: Option<u16>,
    #[serde(default)]
    patom_smtp_username: Option<SecretString>,
    #[serde(default)]
    patom_smtp_password: Option<SecretString>,
    #[serde(default)]
    patom_email_from: Option<String>,
    #[serde(default)]
    patom_email_from_name: Option<String>,
}

fn default_web_dist() -> PathBuf {
    PathBuf::from(DEFAULT_WEB_DIST)
}

const fn default_cookie_secure() -> bool {
    true
}

/// Validate that `raw` is an absolute `scheme://host[:port]` origin —
/// no path, query, fragment, or userinfo — and return its canonical
/// (trailing-slash-stripped) serialization. Returns the failure reason
/// as `Err` so the caller can wrap it into a per-field error variant.
///
/// `allowed_schemes` is the set the URL must match (e.g. `&["https"]`
/// for the OIDC issuer, `&["http", "https"]` for the SPA base url and the
/// S3 endpoint / public host).
fn parse_origin(raw: &str, allowed_schemes: &[&str]) -> Result<String, &'static str> {
    let parsed = parse_http_url(raw, allowed_schemes)?;
    if parsed.path() != "/" && !parsed.path().is_empty() {
        return Err("must be an origin with no path");
    }
    // `Origin::ascii_serialization` yields `scheme://host[:port]` with no
    // trailing slash, regardless of whether `raw` ended with one.
    Ok(parsed.origin().ascii_serialization())
}

/// Parse `raw` and run the validation shared by every config URL: scheme
/// is in `allowed_schemes`, and no query, fragment, or userinfo is
/// present. Hands the parsed [`url::Url`] back so the caller can finish
/// according to its own shape (origin-only via [`parse_origin`], or
/// origin-plus-path via [`parse_base_url`]).
fn parse_http_url(raw: &str, allowed_schemes: &[&str]) -> Result<url::Url, &'static str> {
    let parsed = url::Url::parse(raw).map_err(|_| "not a valid url")?;
    if !allowed_schemes.iter().any(|&s| s == parsed.scheme()) {
        return Err(match allowed_schemes {
            ["https"] => "scheme must be https",
            ["http", "https"] | ["https", "http"] => "scheme must be http or https",
            _ => "scheme not allowed",
        });
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err("must not include query or fragment");
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("userinfo is not allowed");
    }
    Ok(parsed)
}

/// Validate `PATOM_WEB_BASE_URL` — must be an http(s) origin so callers
/// can prepend it directly to a `/`-anchored route without producing
/// malformed redirects.
fn parse_web_base_url(raw: &str) -> Result<String, SettingsError> {
    parse_origin(raw, &["http", "https"]).map_err(|reason| SettingsError::InvalidWebBaseUrl {
        raw: raw.to_owned(),
        reason,
    })
}

/// Collapse a boundary [`crate::types::ParseError`] into a `&'static str`
/// reason for a config-error variant. The newtype's own validation
/// carries the actionable detail; config just needs a stable, static
/// blurb to render next to the offending raw value.
fn parse_error_reason(e: &crate::types::ParseError) -> &'static str {
    use crate::types::ParseError;
    match e {
        ParseError::Empty { .. } => "empty",
        ParseError::TooLong { .. } => "too long",
        ParseError::OutOfRange { detail, .. } | ParseError::Malformed { detail, .. } => detail,
    }
}

/// Pair a resolved login `issuer` with its client credentials, requiring
/// all three to be present. Keeps the "all-or-nothing" rule for the
/// `PATOM_OIDC_*` creds in one place.
fn require_login_creds(
    issuer: IssuerUrl,
    client_id: Option<SecretString>,
    client_secret: Option<SecretString>,
    redirect_url: Option<String>,
) -> Result<(IssuerUrl, SecretString, SecretString, String), SettingsError> {
    let (Some(id), Some(secret), Some(redirect)) = (client_id, client_secret, redirect_url) else {
        return Err(SettingsError::MissingLoginProvider);
    };
    // Parse the callback URL once, here at the config boundary (§1), so an
    // empty / whitespace / malformed value fails fast at startup with a
    // clear error instead of surfacing only when discovery hands it to the
    // OIDC client.
    validate_redirect_url(&redirect)?;
    Ok((issuer, id, secret, redirect))
}

/// Validate a login OIDC `redirect_url`: a syntactically valid `http`/`https`
/// URL (a path is expected, e.g. `/auth/oidc/callback`) within a sane length.
fn validate_redirect_url(raw: &str) -> Result<(), SettingsError> {
    const MAX_BYTES: usize = 2048;
    if raw.len() > MAX_BYTES {
        return Err(SettingsError::InvalidRedirectUrl {
            raw: raw.to_owned(),
            reason: "too long",
        });
    }
    let url = url::Url::parse(raw).map_err(|_| SettingsError::InvalidRedirectUrl {
        raw: raw.to_owned(),
        reason: "not a valid URL",
    })?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(SettingsError::InvalidRedirectUrl {
            raw: raw.to_owned(),
            reason: "scheme must be http or https",
        });
    }
    Ok(())
}

/// Parse the comma-separated `PATOM_CORS_ALLOWED_ORIGINS` list into
/// canonical origins. Blank entries (a trailing comma, double comma) are
/// skipped; each surviving entry must be an http(s) origin with no path.
/// The list is bounded by [`MAX_CORS_ALLOWED_ORIGINS`] (§5).
fn parse_cors_allowed_origins(raw: &str) -> Result<Vec<String>, SettingsError> {
    let entries: Vec<&str> = raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if entries.len() > MAX_CORS_ALLOWED_ORIGINS {
        return Err(SettingsError::TooManyCorsOrigins {
            max: MAX_CORS_ALLOWED_ORIGINS,
            got: entries.len(),
        });
    }
    entries
        .into_iter()
        .map(|entry| {
            parse_origin(entry, &["http", "https"]).map_err(|reason| {
                SettingsError::InvalidCorsOrigin {
                    raw: entry.to_owned(),
                    reason,
                }
            })
        })
        .collect()
}

fn default_timezone_raw() -> String {
    "UTC".to_string()
}

fn default_model() -> Model {
    // Current Sonnet generation per Anthropic's official model list
    // (https://github.com/anthropics/skills/blob/main/skills/claude-api/shared/models.md,
    // May 2026). Operators override via the `MODEL` env var.
    Model::try_from("claude-sonnet-4-6").expect("static default model is in the catalog")
}

fn default_http_addr() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 8080))
}

impl TryFrom<RawSettings> for Settings {
    type Error = SettingsError;

    // Boundary parser: every required field gets one validation step.
    // The function is straight-line guard + assignment, no branching;
    // splitting it into helpers per field would obscure that.
    #[allow(clippy::too_many_lines)]
    fn try_from(raw: RawSettings) -> Result<Self, Self::Error> {
        // Multi-provider: each backend is independently opt-in. At least one
        // must be present, and every provider the catalog references must be
        // configured so the routing invariant holds at runtime (CLAUDE.md §6).
        let providers = ProviderSettings {
            anthropic: raw.anthropic_api_key.map(|api_key| ProviderCredentials {
                api_key,
                base_url: raw.anthropic_base_url,
            }),
            openai: raw.openai_api_key.map(|api_key| ProviderCredentials {
                api_key,
                base_url: raw.openai_base_url,
            }),
            deepseek: raw.deepseek_api_key.map(|api_key| ProviderCredentials {
                api_key,
                base_url: raw.deepseek_base_url,
            }),
        };
        if providers.configured().next().is_none() {
            return Err(SettingsError::NoProviderKey);
        }
        // Only the default model's provider is required at startup; per-agent
        // models are validated at the HTTP/tool write boundary against the
        // built registry, and the resolver falls back to the default with a
        // tracing warn if an agent points at a provider that has since been
        // dropped from config (graceful degradation, not a process crash).
        if !providers.has(raw.model.provider()) {
            return Err(SettingsError::DefaultModelProviderNotConfigured {
                model: raw.model.as_str(),
                provider: raw.model.provider(),
            });
        }
        let embedding = match (raw.embedding_api_key, raw.embedding_model) {
            (Some(api_key), Some(model)) => EmbeddingSettings {
                api_key,
                base_url: raw.embedding_base_url,
                model,
                dimensions: raw
                    .embedding_dimensions
                    .unwrap_or(DEFAULT_EMBEDDING_DIMENSIONS),
            },
            _ => return Err(SettingsError::MissingEmbedding),
        };
        let default_timezone = Tz::from_str(&raw.default_timezone).map_err(|_| {
            SettingsError::InvalidDefaultTimezone {
                raw: raw.default_timezone.clone(),
            }
        })?;
        if raw.patom_jwt_secret.expose().len() < 32 {
            return Err(SettingsError::AuthSecretTooShort);
        }
        let web_base_url = match raw.patom_web_base_url {
            Some(raw_url) => Some(parse_web_base_url(&raw_url)?),
            None => None,
        };
        let cookie_domain = match raw.patom_cookie_domain {
            Some(raw_domain) => Some(CookieDomain::try_from(raw_domain.as_str()).map_err(|e| {
                SettingsError::InvalidCookieDomain {
                    raw: raw_domain.clone(),
                    reason: parse_error_reason(&e),
                }
            })?),
            None => None,
        };
        let cors_allowed_origins = match raw.patom_cors_allowed_origins {
            Some(raw_list) => parse_cors_allowed_origins(&raw_list)?,
            None => Vec::new(),
        };
        // Resolve the login OIDC provider (ADR-0011). One path: operators
        // point `PATOM_OIDC_ISSUER` at their IdP and supply the three
        // `PATOM_OIDC_*` creds. All four are required and validated here.
        let Some(raw_issuer) = raw.patom_oidc_issuer else {
            return Err(SettingsError::MissingLoginProvider);
        };
        let issuer = IssuerUrl::try_from(raw_issuer.as_str()).map_err(|e| {
            SettingsError::InvalidOidcIssuer {
                raw: raw_issuer.clone(),
                reason: parse_error_reason(&e),
            }
        })?;
        let (oidc_issuer, oidc_client_id, oidc_client_secret, oidc_redirect_url) =
            require_login_creds(
                issuer,
                raw.patom_oidc_client_id,
                raw.patom_oidc_client_secret,
                raw.patom_oidc_redirect_url,
            )?;
        let auth = AuthSettings {
            jwt_secret: raw.patom_jwt_secret,
            oidc_issuer,
            oidc_client_id,
            oidc_client_secret,
            oidc_redirect_url,
            bootstrap_admin: raw.patom_bootstrap_admin,
            github_client_id: raw.patom_github_client_id,
            github_client_secret: raw.patom_github_client_secret,
            // Populated by `Settings::load` after `try_from`; left empty
            // here so `try_from` (and tests that go through it) stay pure.
            platform_oauth_clients: HashMap::new(),
            cookie_secure: raw.patom_cookie_secure,
            master_kek: raw.patom_master_kek,
            oauth_redirect_base: raw.patom_oauth_redirect_base,
            web_base_url,
            cookie_domain,
            cors_allowed_origins,
            launch_guardrails: raw.patom_launch_guardrails,
            trusted_proxy_hops: raw.patom_trusted_proxy_hops,
        };
        let slack = match (
            raw.patom_slack_signing_secret,
            raw.patom_slack_client_id,
            raw.patom_slack_client_secret,
        ) {
            (None, None, None) => None,
            (Some(signing_secret), Some(client_id), Some(client_secret)) => {
                // Trim before non-empty check so " " is rejected.
                let client_id = client_id.trim().to_owned();
                if client_id.is_empty() {
                    return Err(SettingsError::InvalidSlackClientId);
                }
                // Normalise a trailing slash on the OAuth redirect base
                // before composing the Slack callback path — otherwise
                // `https://example/` + `/slack/oauth/callback` yields a
                // doubled slash and Slack rejects the install.
                let redirect_base = auth.oauth_redirect_base.trim_end_matches('/');
                let redirect_url = format!("{redirect_base}/slack/oauth/callback");
                Some(SlackSettings {
                    signing_secret,
                    client_id,
                    client_secret,
                    redirect_url,
                })
            }
            _ => return Err(SettingsError::PartialSlackConfig),
        };
        let object_storage = resolve_object_storage(
            raw.patom_s3_endpoint,
            raw.patom_s3_region,
            raw.patom_s3_bucket,
            raw.patom_s3_access_key_id,
            raw.patom_s3_secret_access_key,
            raw.patom_s3_public_host,
        )?;
        let smtp = resolve_smtp(
            raw.patom_smtp_host,
            raw.patom_smtp_port,
            raw.patom_smtp_username,
            raw.patom_smtp_password,
            raw.patom_email_from,
            raw.patom_email_from_name,
        )?;
        let posthog_host = match raw.patom_posthog_host {
            Some(ref h) => Some(parse_origin(h, &["http", "https"]).map_err(|reason| {
                SettingsError::InvalidPosthogHost {
                    raw: h.clone(),
                    reason,
                }
            })?),
            None => None,
        };
        Ok(Self {
            providers,
            brave_search_api_key: raw.brave_search_api_key,
            model: raw.model,
            http_addr: raw.http_addr,
            database_url: raw.database_url,
            embedding,
            default_timezone,
            auth,
            web_dist: raw.patom_web_dist,
            posthog_key: raw.patom_posthog_key,
            posthog_host,
            slack,
            object_storage,
            smtp,
        })
    }
}

/// Resolve the optional SMTP relay settings from the raw env fields. The
/// four credential/identity fields are required as a group: all unset →
/// `None` (transactional mail logged via [`crate::orgs::LogMailer`], a
/// first-class deployment); all set → `Some`; any mixed subset →
/// [`SettingsError::PartialSmtpConfig`]. `port` defaults to
/// [`DEFAULT_SMTP_PORT`]; `from` is parsed through [`Email`] at the
/// boundary (CLAUDE.md §1).
fn resolve_smtp(
    host: Option<String>,
    port: Option<u16>,
    username: Option<SecretString>,
    password: Option<SecretString>,
    from: Option<String>,
    from_name: Option<String>,
) -> Result<Option<SmtpSettings>, SettingsError> {
    match (host, username, password, from) {
        (None, None, None, None) => Ok(None),
        (Some(host_raw), Some(username), Some(password), Some(from_raw)) => {
            let host = require_non_empty_smtp(host_raw, "HOST")?;
            let from = Email::try_from(from_raw.as_str()).map_err(|e| {
                SettingsError::InvalidEmailFrom {
                    raw: from_raw.clone(),
                    reason: parse_error_reason(&e),
                }
            })?;
            // An empty display name is meaningless — treat it as absent so
            // the mailer renders a bare address rather than `<>` noise.
            let from_name = from_name.filter(|n| !n.trim().is_empty());
            Ok(Some(SmtpSettings {
                host,
                port: port.unwrap_or(DEFAULT_SMTP_PORT),
                username,
                password,
                from,
                from_name,
            }))
        }
        _ => Err(SettingsError::PartialSmtpConfig),
    }
}

/// Reject an empty / whitespace-only required SMTP field. `field` is the
/// env var suffix (e.g. `HOST`) used in [`SettingsError::EmptySmtpField`].
/// Returns the value unchanged when non-empty.
fn require_non_empty_smtp(raw: String, field: &'static str) -> Result<String, SettingsError> {
    if raw.trim().is_empty() {
        return Err(SettingsError::EmptySmtpField { field });
    }
    Ok(raw)
}

/// Default SigV4 region when `PATOM_S3_REGION` is unset. MinIO ignores
/// the region; AWS accepts `us-east-1` as the canonical default; R2 wants
/// `auto` (set explicitly by the operator).
const DEFAULT_S3_REGION: &str = "us-east-1";

/// Default SMTP submission port when `PATOM_SMTP_PORT` is unset. 587 is the
/// RFC 6409 message-submission port; the mailer always uses STARTTLS on it
/// (see [`crate::orgs::SmtpMailer`]). Operators override the port only for a
/// relay on a non-standard STARTTLS endpoint — the TLS mode is fixed.
/// Resolved at the config boundary so [`SmtpSettings::port`] is always
/// concrete.
const DEFAULT_SMTP_PORT: u16 = 587;

/// Resolve the optional S3 object-storage settings from the raw env
/// fields. The five non-region fields are required as a group: all unset
/// → `None` (uploads disabled, a first-class deployment); all set →
/// `Some`; any mixed subset → [`SettingsError::PartialS3Config`].
fn resolve_object_storage(
    endpoint: Option<String>,
    region: Option<String>,
    bucket: Option<String>,
    access_key_id: Option<SecretString>,
    secret_access_key: Option<SecretString>,
    public_host: Option<String>,
) -> Result<Option<ObjectStorageSettings>, SettingsError> {
    match (
        endpoint,
        bucket,
        access_key_id,
        secret_access_key,
        public_host,
    ) {
        (None, None, None, None, None) => Ok(None),
        (
            Some(endpoint_raw),
            Some(bucket),
            Some(access_key_id),
            Some(secret_access_key),
            Some(public_host_raw),
        ) => {
            // `bucket` and `region` have no URL shape to parse, but an empty
            // value is still a misconfiguration — reject it here so it fails
            // at startup, not as an opaque S3 error on the first upload.
            let region = match region {
                Some(r) => require_non_empty_s3(r, "REGION")?,
                None => DEFAULT_S3_REGION.to_owned(),
            };
            Ok(Some(ObjectStorageSettings {
                endpoint: parse_s3_endpoint(&endpoint_raw)?,
                region,
                bucket: require_non_empty_s3(bucket, "BUCKET")?,
                access_key_id,
                secret_access_key,
                public_host: parse_s3_public_host(&public_host_raw)?,
            }))
        }
        _ => Err(SettingsError::PartialS3Config),
    }
}

/// Reject an empty / whitespace-only required S3 field. `field` is the env
/// var suffix (e.g. `BUCKET`) used in the [`SettingsError::EmptyS3Field`]
/// message. Returns the value unchanged when non-empty.
fn require_non_empty_s3(raw: String, field: &'static str) -> Result<String, SettingsError> {
    if raw.trim().is_empty() {
        return Err(SettingsError::EmptyS3Field { field });
    }
    Ok(raw)
}

/// Validate `PATOM_S3_ENDPOINT` — must be an http(s) origin (no path) so
/// the SDK receives a clean endpoint URL.
fn parse_s3_endpoint(raw: &str) -> Result<String, SettingsError> {
    parse_origin(raw, &["http", "https"]).map_err(|reason| SettingsError::InvalidS3Endpoint {
        raw: raw.to_owned(),
        reason,
    })
}

/// Validate `PATOM_S3_PUBLIC_HOST` — an http(s) base URL the FE prepends
/// to an object key. Unlike the endpoint, a path is permitted so a
/// path-style MinIO base (`http://minio:9000/<bucket>`) works directly;
/// http is permitted for self-hosted. Any trailing slash is stripped so
/// the asset module joins `{base}/{key}` without producing `//<key>`.
fn parse_s3_public_host(raw: &str) -> Result<String, SettingsError> {
    parse_base_url(raw, &["http", "https"]).map_err(|reason| SettingsError::InvalidS3PublicHost {
        raw: raw.to_owned(),
        reason,
    })
}

/// Validate `raw` as an absolute http(s) base URL — like [`parse_origin`]
/// but a path is allowed (e.g. a path-style bucket prefix). Query,
/// fragment, and userinfo are still rejected; any trailing slash is
/// stripped so callers can join `{base}/{suffix}` cleanly.
fn parse_base_url(raw: &str, allowed_schemes: &[&str]) -> Result<String, &'static str> {
    let parsed = parse_http_url(raw, allowed_schemes)?;
    // `origin()` yields `scheme://host[:port]`; append the path with any
    // trailing slash removed (the root path `/` collapses to empty).
    let origin = parsed.origin().ascii_serialization();
    let path = parsed.path().trim_end_matches('/');
    Ok(format!("{origin}{path}"))
}

/// Default vector dimension. Matches `text-embedding-3-small` / the
/// `agent_memories.embedding` column committed in migration 9. Operators
/// pointing at a model with a different dimension must override
/// `EMBEDDING_DIMENSIONS` *and* run a custom column migration.
const DEFAULT_EMBEDDING_DIMENSIONS: usize = 1536;

impl Settings {
    /// Load settings from environment variables. Missing required values surface as a
    /// `SettingsError` so the caller can decide how to report.
    pub fn load() -> Result<Self, SettingsError> {
        let raw: RawSettings = Config::builder()
            .add_source(Environment::default())
            .build()?
            .try_deserialize()?;
        let mut settings = Self::try_from(raw)?;
        // Populate the platform-OAuth map from env after the rest of the
        // boundary parse. Kept outside `RawSettings` because the keys are
        // dynamic (`PATOM_<X>_CLIENT_ID`) rather than a fixed schema.
        settings.auth.platform_oauth_clients = parse_platform_oauth_clients(std::env::vars());
        Ok(settings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret(s: &str) -> SecretString {
        SecretString::try_from(s.to_string()).expect("non-empty")
    }

    fn empty_raw() -> RawSettings {
        // Embedding settings are required per doc/memory.md §2.9 — fill
        // them in for the cases that expect a successful parse; tests that
        // probe the no-embedding error path overwrite them back to None.
        RawSettings {
            openai_api_key: None,
            openai_base_url: None,
            anthropic_api_key: None,
            anthropic_base_url: None,
            deepseek_api_key: None,
            deepseek_base_url: None,
            brave_search_api_key: secret("brave"),
            model: default_model(),
            http_addr: default_http_addr(),
            database_url: secret("postgres://patom:patom@localhost:5432/patom"),
            embedding_api_key: Some(secret("emb")),
            embedding_base_url: None,
            embedding_model: Some("text-embedding-3-small".to_string()),
            embedding_dimensions: None,
            default_timezone: default_timezone_raw(),
            patom_jwt_secret: secret(&"a".repeat(64)),
            patom_oidc_issuer: Some("https://accounts.google.com".to_string()),
            patom_oidc_client_id: Some(secret("test-client-id")),
            patom_oidc_client_secret: Some(secret("test-client-secret")),
            patom_oidc_redirect_url: Some("http://localhost:8080/auth/oidc/callback".to_string()),
            patom_bootstrap_admin: false,
            patom_github_client_id: secret("test-github-client-id"),
            patom_github_client_secret: secret("test-github-client-secret"),
            patom_cookie_secure: false,
            // base64 of 32 bytes; never used in these tests since they only
            // exercise the Settings boundary, not crypto.
            patom_master_kek: secret("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="),
            patom_oauth_redirect_base: "http://localhost:8080".to_string(),
            patom_web_base_url: None,
            patom_cookie_domain: None,
            patom_cors_allowed_origins: None,
            patom_launch_guardrails: false,
            patom_trusted_proxy_hops: 0,
            patom_web_dist: default_web_dist(),
            patom_posthog_key: None,
            patom_posthog_host: None,
            patom_slack_signing_secret: None,
            patom_slack_client_id: None,
            patom_slack_client_secret: None,
            patom_s3_endpoint: None,
            patom_s3_region: None,
            patom_s3_bucket: None,
            patom_s3_access_key_id: None,
            patom_s3_secret_access_key: None,
            patom_s3_public_host: None,
            patom_smtp_host: None,
            patom_smtp_port: None,
            patom_smtp_username: None,
            patom_smtp_password: None,
            patom_email_from: None,
            patom_email_from_name: None,
        }
    }

    #[test]
    fn no_provider_key_is_rejected() {
        let raw = empty_raw();
        let err = Settings::try_from(raw).expect_err("expected error");
        assert!(matches!(err, SettingsError::NoProviderKey));
    }

    #[test]
    fn multiple_provider_keys_are_allowed() {
        let mut raw = empty_raw();
        raw.anthropic_api_key = Some(secret("sk-ant"));
        raw.openai_api_key = Some(secret("sk-x"));
        raw.deepseek_api_key = Some(secret("sk-ds"));
        let s = Settings::try_from(raw).expect("valid");
        let configured: std::collections::HashSet<_> = s.providers.configured().collect();
        assert!(configured.contains(&ProviderId::Anthropic));
        assert!(configured.contains(&ProviderId::Openai));
        assert!(configured.contains(&ProviderId::Deepseek));
    }

    #[test]
    fn default_model_provider_must_be_configured() {
        let mut raw = empty_raw();
        // Default model is `claude-sonnet-4-6` (Anthropic); configuring only
        // OpenAI leaves Anthropic missing, so the default cannot route.
        raw.openai_api_key = Some(secret("sk-x"));
        let err = Settings::try_from(raw).expect_err("expected error");
        assert!(matches!(
            err,
            SettingsError::DefaultModelProviderNotConfigured {
                provider: ProviderId::Anthropic,
                ..
            }
        ));
    }

    #[test]
    fn anthropic_key_alone_with_anthropic_default() {
        let mut raw = empty_raw();
        raw.anthropic_api_key = Some(secret("sk-ant"));
        let s = Settings::try_from(raw).expect("valid");
        assert!(s.providers.has(ProviderId::Anthropic));
        assert!(!s.providers.has(ProviderId::Openai));
        assert_eq!(s.model.provider(), ProviderId::Anthropic);
    }

    #[test]
    fn openai_default_routes_through_openai_only() {
        let mut raw = empty_raw();
        raw.openai_api_key = Some(secret("sk-x"));
        raw.openai_base_url = Some("https://api.openai.com/v1".to_string());
        raw.model = Model::try_from("gpt-4o-mini").expect("catalog");
        let s = Settings::try_from(raw).expect("valid");
        assert_eq!(s.model.provider(), ProviderId::Openai);
        assert_eq!(
            s.providers
                .openai
                .as_ref()
                .expect("test wired openai credentials")
                .base_url
                .as_deref(),
            Some("https://api.openai.com/v1")
        );
    }

    #[test]
    fn invalid_default_timezone_is_rejected() {
        let mut raw = empty_raw();
        raw.anthropic_api_key = Some(secret("sk-ant"));
        raw.default_timezone = "Mars/Olympus_Mons".to_string();
        let err = Settings::try_from(raw).expect_err("expected error");
        assert!(matches!(err, SettingsError::InvalidDefaultTimezone { .. }));
    }

    #[test]
    fn default_timezone_defaults_to_utc() {
        let mut raw = empty_raw();
        raw.anthropic_api_key = Some(secret("sk-ant"));
        let s = Settings::try_from(raw).expect("valid");
        assert_eq!(s.default_timezone, Tz::UTC);
    }

    #[test]
    fn cookie_domain_unset_is_none() {
        let mut raw = empty_raw();
        raw.anthropic_api_key = Some(secret("sk-ant"));
        let s = Settings::try_from(raw).expect("valid");
        assert!(s.auth.cookie_domain.is_none());
        assert!(s.auth.cors_allowed_origins.is_empty());
    }

    #[test]
    fn launch_guardrails_default_off() {
        // Launch-period abuse guardrails (#121) are opt-in: a build that sets
        // neither env var behaves exactly like baseline.
        let mut raw = empty_raw();
        raw.anthropic_api_key = Some(secret("sk-ant"));
        let s = Settings::try_from(raw).expect("valid");
        assert!(!s.auth.launch_guardrails);
        assert_eq!(s.auth.trusted_proxy_hops, 0);
    }

    #[test]
    fn launch_guardrails_parsed_when_set() {
        let mut raw = empty_raw();
        raw.anthropic_api_key = Some(secret("sk-ant"));
        raw.patom_launch_guardrails = true;
        raw.patom_trusted_proxy_hops = 1;
        let s = Settings::try_from(raw).expect("valid");
        assert!(s.auth.launch_guardrails);
        assert_eq!(s.auth.trusted_proxy_hops, 1);
    }

    #[test]
    fn valid_cookie_domain_is_parsed() {
        let mut raw = empty_raw();
        raw.anthropic_api_key = Some(secret("sk-ant"));
        raw.patom_cookie_domain = Some(".patom.app".to_string());
        let s = Settings::try_from(raw).expect("valid");
        let domain = s.auth.cookie_domain.expect("some");
        assert_eq!(domain.as_str(), ".patom.app");
    }

    #[test]
    fn malformed_cookie_domain_is_rejected() {
        let mut raw = empty_raw();
        raw.anthropic_api_key = Some(secret("sk-ant"));
        raw.patom_cookie_domain = Some("https://patom.app".to_string());
        let err = Settings::try_from(raw).expect_err("expected error");
        assert!(matches!(err, SettingsError::InvalidCookieDomain { .. }));
    }

    #[test]
    fn valid_cors_origins_are_parsed_and_canonicalized() {
        let mut raw = empty_raw();
        raw.anthropic_api_key = Some(secret("sk-ant"));
        raw.patom_cors_allowed_origins =
            Some("https://patom.app/ , https://www.patom.app".to_string());
        let s = Settings::try_from(raw).expect("valid");
        // Trailing slash stripped to a canonical origin; blank entries dropped.
        assert_eq!(
            s.auth.cors_allowed_origins,
            vec![
                "https://patom.app".to_string(),
                "https://www.patom.app".to_string()
            ]
        );
    }

    #[test]
    fn malformed_cors_origin_is_rejected() {
        let mut raw = empty_raw();
        raw.anthropic_api_key = Some(secret("sk-ant"));
        raw.patom_cors_allowed_origins = Some("https://patom.app/login".to_string());
        let err = Settings::try_from(raw).expect_err("expected error");
        assert!(matches!(err, SettingsError::InvalidCorsOrigin { .. }));
    }

    #[test]
    fn too_many_cors_origins_is_rejected() {
        let mut raw = empty_raw();
        raw.anthropic_api_key = Some(secret("sk-ant"));
        let list = (0..=MAX_CORS_ALLOWED_ORIGINS)
            .map(|i| format!("https://h{i}.patom.app"))
            .collect::<Vec<_>>()
            .join(",");
        raw.patom_cors_allowed_origins = Some(list);
        let err = Settings::try_from(raw).expect_err("expected error");
        assert!(matches!(
            err,
            SettingsError::TooManyCorsOrigins {
                max: MAX_CORS_ALLOWED_ORIGINS,
                ..
            }
        ));
    }

    #[test]
    fn default_timezone_parses_iana_name() {
        let mut raw = empty_raw();
        raw.anthropic_api_key = Some(secret("sk-ant"));
        raw.default_timezone = "Asia/Bangkok".to_string();
        let s = Settings::try_from(raw).expect("valid");
        assert_eq!(s.default_timezone, chrono_tz::Asia::Bangkok);
    }

    #[test]
    fn missing_embedding_is_rejected() {
        let mut raw = empty_raw();
        raw.anthropic_api_key = Some(secret("sk-ant"));
        raw.embedding_api_key = None;
        raw.embedding_model = None;
        let err = Settings::try_from(raw).expect_err("expected error");
        assert!(matches!(err, SettingsError::MissingEmbedding));
    }

    #[test]
    fn oidc_issuer_and_creds_configure_login_provider() {
        let mut raw = empty_raw();
        raw.anthropic_api_key = Some(secret("sk-ant"));
        let s = Settings::try_from(raw).expect("valid");
        // `empty_raw` supplies the four `PATOM_OIDC_*` values; the issuer is
        // honored verbatim — there is no Google preset fallback anymore.
        assert_eq!(s.auth.oidc_issuer.as_str(), "https://accounts.google.com");
        assert!(!s.auth.bootstrap_admin);
    }

    #[test]
    fn missing_oidc_issuer_is_rejected() {
        let mut raw = empty_raw();
        raw.anthropic_api_key = Some(secret("sk-ant"));
        raw.patom_oidc_issuer = None;
        let err = Settings::try_from(raw).expect_err("expected error");
        assert!(matches!(err, SettingsError::MissingLoginProvider));
    }

    #[test]
    fn oidc_issuer_selects_generic_provider() {
        let mut raw = empty_raw();
        raw.anthropic_api_key = Some(secret("sk-ant"));
        raw.patom_oidc_issuer = Some("https://idp.example.test".to_string());
        raw.patom_oidc_client_id = Some(secret("oidc-id"));
        raw.patom_oidc_client_secret = Some(secret("oidc-secret"));
        raw.patom_oidc_redirect_url = Some("https://app.example/auth/oidc/callback".to_string());
        let s = Settings::try_from(raw).expect("valid");
        assert_eq!(s.auth.oidc_issuer.as_str(), "https://idp.example.test");
        assert_eq!(s.auth.oidc_client_id.expose(), "oidc-id");
    }

    #[test]
    fn oidc_issuer_set_without_creds_is_rejected() {
        let mut raw = empty_raw();
        raw.anthropic_api_key = Some(secret("sk-ant"));
        raw.patom_oidc_issuer = Some("https://idp.example.test".to_string());
        raw.patom_oidc_client_id = None;
        raw.patom_oidc_client_secret = None;
        raw.patom_oidc_redirect_url = None;
        let err = Settings::try_from(raw).expect_err("expected error");
        assert!(matches!(err, SettingsError::MissingLoginProvider));
    }

    #[test]
    fn malformed_oidc_issuer_is_rejected() {
        let mut raw = empty_raw();
        raw.anthropic_api_key = Some(secret("sk-ant"));
        raw.patom_oidc_issuer = Some("http://idp.example.test".to_string());
        raw.patom_oidc_client_id = Some(secret("oidc-id"));
        raw.patom_oidc_client_secret = Some(secret("oidc-secret"));
        raw.patom_oidc_redirect_url = Some("https://app.example/auth/oidc/callback".to_string());
        let err = Settings::try_from(raw).expect_err("expected error");
        assert!(matches!(err, SettingsError::InvalidOidcIssuer { .. }));
    }

    #[test]
    fn missing_all_login_creds_is_rejected() {
        let mut raw = empty_raw();
        raw.anthropic_api_key = Some(secret("sk-ant"));
        raw.patom_oidc_client_id = None;
        raw.patom_oidc_client_secret = None;
        raw.patom_oidc_redirect_url = None;
        let err = Settings::try_from(raw).expect_err("expected error");
        assert!(matches!(err, SettingsError::MissingLoginProvider));
    }

    #[test]
    fn malformed_login_redirect_url_is_rejected() {
        let mut raw = empty_raw();
        raw.anthropic_api_key = Some(secret("sk-ant"));
        raw.patom_oidc_redirect_url = Some("not a url".to_string());
        let err = Settings::try_from(raw).expect_err("expected error");
        assert!(matches!(err, SettingsError::InvalidRedirectUrl { .. }));
    }

    #[test]
    fn bootstrap_admin_flag_is_honored() {
        let mut raw = empty_raw();
        raw.anthropic_api_key = Some(secret("sk-ant"));
        raw.patom_bootstrap_admin = true;
        let s = Settings::try_from(raw).expect("valid");
        assert!(s.auth.bootstrap_admin);
    }

    /// Fill the five required S3 fields on `raw` with a MinIO-style
    /// configuration so individual tests can mutate one knob at a time.
    fn with_s3(raw: &mut RawSettings) {
        raw.patom_s3_endpoint = Some("http://minio:9000".to_string());
        raw.patom_s3_bucket = Some("patom-assets".to_string());
        raw.patom_s3_access_key_id = Some(secret("AKIA_TEST"));
        raw.patom_s3_secret_access_key = Some(secret("secret_test"));
        raw.patom_s3_public_host = Some("http://minio:9000/patom-assets".to_string());
    }

    #[test]
    fn s3_resolves() {
        let mut raw = empty_raw();
        raw.anthropic_api_key = Some(secret("sk-ant"));
        with_s3(&mut raw);
        let s = Settings::try_from(raw).expect("valid");
        let store = s.object_storage.expect("configured");
        assert_eq!(store.endpoint, "http://minio:9000");
        assert_eq!(store.bucket, "patom-assets");
        assert_eq!(store.access_key_id.expose(), "AKIA_TEST");
        // http public host is accepted for self-hosted MinIO.
        assert_eq!(store.public_host, "http://minio:9000/patom-assets");
    }

    #[test]
    fn s3_region_default() {
        let mut raw = empty_raw();
        raw.anthropic_api_key = Some(secret("sk-ant"));
        with_s3(&mut raw);
        let s = Settings::try_from(raw).expect("valid");
        let store = s.object_storage.expect("configured");
        assert_eq!(store.region, "us-east-1");
    }

    #[test]
    fn s3_explicit_region_flows_through() {
        let mut raw = empty_raw();
        raw.anthropic_api_key = Some(secret("sk-ant"));
        with_s3(&mut raw);
        // R2 deployment: explicit endpoint + region `auto`.
        raw.patom_s3_endpoint = Some("https://acct.r2.cloudflarestorage.com".to_string());
        raw.patom_s3_region = Some("auto".to_string());
        raw.patom_s3_public_host = Some("https://asset.patom.app".to_string());
        let s = Settings::try_from(raw).expect("valid");
        let store = s.object_storage.expect("configured");
        assert_eq!(store.endpoint, "https://acct.r2.cloudflarestorage.com");
        assert_eq!(store.region, "auto");
        assert_eq!(store.public_host, "https://asset.patom.app");
    }

    #[test]
    fn partial_s3_group_is_rejected() {
        let mut raw = empty_raw();
        raw.anthropic_api_key = Some(secret("sk-ant"));
        // Endpoint set, the rest unset — a mixed subset.
        raw.patom_s3_endpoint = Some("http://minio:9000".to_string());
        let err = Settings::try_from(raw).expect_err("expected error");
        assert!(matches!(err, SettingsError::PartialS3Config));
    }

    #[test]
    fn invalid_s3_endpoint_rejected() {
        let mut raw = empty_raw();
        raw.anthropic_api_key = Some(secret("sk-ant"));
        with_s3(&mut raw);
        // A path is not an origin.
        raw.patom_s3_endpoint = Some("http://minio:9000/path".to_string());
        let err = Settings::try_from(raw).expect_err("expected error");
        assert!(matches!(err, SettingsError::InvalidS3Endpoint { .. }));
    }

    #[test]
    fn empty_s3_bucket_rejected() {
        let mut raw = empty_raw();
        raw.anthropic_api_key = Some(secret("sk-ant"));
        with_s3(&mut raw);
        raw.patom_s3_bucket = Some("   ".to_string());
        let err = Settings::try_from(raw).expect_err("expected error");
        assert!(matches!(
            err,
            SettingsError::EmptyS3Field { field: "BUCKET" }
        ));
    }

    #[test]
    fn empty_s3_region_rejected() {
        let mut raw = empty_raw();
        raw.anthropic_api_key = Some(secret("sk-ant"));
        with_s3(&mut raw);
        raw.patom_s3_region = Some(String::new());
        let err = Settings::try_from(raw).expect_err("expected error");
        assert!(matches!(
            err,
            SettingsError::EmptyS3Field { field: "REGION" }
        ));
    }

    #[test]
    fn invalid_s3_public_host_rejected() {
        let mut raw = empty_raw();
        raw.anthropic_api_key = Some(secret("sk-ant"));
        with_s3(&mut raw);
        raw.patom_s3_public_host = Some("ftp://minio:9000".to_string());
        let err = Settings::try_from(raw).expect_err("expected error");
        assert!(matches!(err, SettingsError::InvalidS3PublicHost { .. }));
    }

    #[test]
    fn s3_public_host_trailing_slash_stripped() {
        let mut raw = empty_raw();
        raw.anthropic_api_key = Some(secret("sk-ant"));
        with_s3(&mut raw);
        raw.patom_s3_public_host = Some("https://asset.patom.app/".to_string());
        let s = Settings::try_from(raw).expect("valid");
        let store = s.object_storage.expect("configured");
        assert_eq!(store.public_host, "https://asset.patom.app");
    }

    #[test]
    fn no_object_storage_is_ok() {
        let mut raw = empty_raw();
        raw.anthropic_api_key = Some(secret("sk-ant"));
        let s = Settings::try_from(raw).expect("valid");
        assert!(s.object_storage.is_none());
    }

    fn with_smtp(raw: &mut RawSettings) {
        raw.patom_smtp_host = Some("smtp.example.com".to_string());
        raw.patom_smtp_username = Some(secret("smtp-user"));
        raw.patom_smtp_password = Some(secret("smtp-pass"));
        raw.patom_email_from = Some("invites@patom.app".to_string());
    }

    #[test]
    fn smtp_resolves() {
        let mut raw = empty_raw();
        raw.anthropic_api_key = Some(secret("sk-ant"));
        with_smtp(&mut raw);
        raw.patom_email_from_name = Some("Patom".to_string());
        let s = Settings::try_from(raw).expect("valid");
        let smtp = s.smtp.expect("configured");
        assert_eq!(smtp.host, "smtp.example.com");
        assert_eq!(smtp.username.expose(), "smtp-user");
        assert_eq!(smtp.from.as_str(), "invites@patom.app");
        assert_eq!(smtp.from_name.as_deref(), Some("Patom"));
    }

    #[test]
    fn smtp_port_default_is_587() {
        let mut raw = empty_raw();
        raw.anthropic_api_key = Some(secret("sk-ant"));
        with_smtp(&mut raw);
        let s = Settings::try_from(raw).expect("valid");
        assert_eq!(s.smtp.expect("configured").port, DEFAULT_SMTP_PORT);
    }

    #[test]
    fn smtp_explicit_port_flows_through() {
        let mut raw = empty_raw();
        raw.anthropic_api_key = Some(secret("sk-ant"));
        with_smtp(&mut raw);
        // A non-standard STARTTLS submission port (e.g. SES's 2587).
        raw.patom_smtp_port = Some(2587);
        let s = Settings::try_from(raw).expect("valid");
        assert_eq!(s.smtp.expect("configured").port, 2587);
    }

    #[test]
    fn partial_smtp_group_is_rejected() {
        let mut raw = empty_raw();
        raw.anthropic_api_key = Some(secret("sk-ant"));
        // Host set, the rest unset — a mixed subset.
        raw.patom_smtp_host = Some("smtp.example.com".to_string());
        let err = Settings::try_from(raw).expect_err("expected error");
        assert!(matches!(err, SettingsError::PartialSmtpConfig));
    }

    #[test]
    fn empty_smtp_host_rejected() {
        let mut raw = empty_raw();
        raw.anthropic_api_key = Some(secret("sk-ant"));
        with_smtp(&mut raw);
        raw.patom_smtp_host = Some("   ".to_string());
        let err = Settings::try_from(raw).expect_err("expected error");
        assert!(matches!(
            err,
            SettingsError::EmptySmtpField { field: "HOST" }
        ));
    }

    #[test]
    fn invalid_email_from_rejected() {
        let mut raw = empty_raw();
        raw.anthropic_api_key = Some(secret("sk-ant"));
        with_smtp(&mut raw);
        raw.patom_email_from = Some("not-an-email".to_string());
        let err = Settings::try_from(raw).expect_err("expected error");
        assert!(matches!(err, SettingsError::InvalidEmailFrom { .. }));
    }

    #[test]
    fn blank_from_name_is_treated_as_absent() {
        let mut raw = empty_raw();
        raw.anthropic_api_key = Some(secret("sk-ant"));
        with_smtp(&mut raw);
        raw.patom_email_from_name = Some("   ".to_string());
        let s = Settings::try_from(raw).expect("valid");
        assert!(s.smtp.expect("configured").from_name.is_none());
    }

    #[test]
    fn no_smtp_is_ok() {
        let mut raw = empty_raw();
        raw.anthropic_api_key = Some(secret("sk-ant"));
        let s = Settings::try_from(raw).expect("valid");
        assert!(s.smtp.is_none());
    }
}
