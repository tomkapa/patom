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
use crate::auth::{CookieDomain, IssuerUrl};
use crate::provider::{Model, ProviderId};
use crate::types::SecretString;

/// Default SPA dist path when `PATOM_WEB_DIST` is unset. Matches
/// `web/build.ts`'s `outdir`; operators running the binary from outside
/// the repo root must override.
const DEFAULT_WEB_DIST: &str = "./web/dist";

/// Issuer for the Google login preset (ADR-0011). Used when
/// `PATOM_OIDC_ISSUER` is unset so cloud keeps its `GOOGLE_*` config.
const GOOGLE_OIDC_ISSUER: &str = "https://accounts.google.com";

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

    #[error(
        "auth: no login provider configured; set PATOM_OIDC_ISSUER + \
         PATOM_OIDC_CLIENT_ID + PATOM_OIDC_CLIENT_SECRET + PATOM_OIDC_REDIRECT_URL, \
         or GOOGLE_CLIENT_ID + GOOGLE_CLIENT_SECRET + GOOGLE_REDIRECT_URL"
    )]
    MissingLoginProvider,

    #[error("auth: PATOM_COOKIE_DOMAIN is not a valid cookie domain: {raw:?} ({reason})")]
    InvalidCookieDomain { raw: String, reason: &'static str },

    #[error("auth: PATOM_CORS_ALLOWED_ORIGINS entry is not a valid origin: {raw:?} ({reason})")]
    InvalidCorsOrigin { raw: String, reason: &'static str },

    #[error("auth: PATOM_CORS_ALLOWED_ORIGINS has too many entries: max {max}, got {got}")]
    TooManyCorsOrigins { max: usize, got: usize },

    #[error(
        "slack: partial configuration; set all of PATOM_SLACK_SIGNING_SECRET, \
         PATOM_SLACK_CLIENT_ID, PATOM_SLACK_CLIENT_SECRET — or none"
    )]
    PartialSlackConfig,

    #[error("slack: PATOM_SLACK_CLIENT_ID must be non-empty after trim")]
    InvalidSlackClientId,

    #[error(
        "r2: partial configuration; set all of PATOM_R2_ACCOUNT_ID, \
         PATOM_R2_BUCKET, PATOM_R2_ACCESS_KEY_ID, PATOM_R2_SECRET_ACCESS_KEY, \
         PATOM_R2_PUBLIC_HOST — or none"
    )]
    PartialR2Config,

    #[error("r2: PATOM_R2_PUBLIC_HOST {raw:?} is not a valid https origin ({reason})")]
    InvalidR2PublicHost { raw: String, reason: &'static str },
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
    /// Slack adapter — present iff all `PATOM_SLACK_*` env vars are
    /// set. `None` is a first-class deployment (the Slack routes and
    /// background workers stay un-spawned).
    pub slack: Option<SlackSettings>,
    /// Cloudflare R2 (S3-compatible) object storage. Present iff all
    /// `PATOM_R2_*` env vars are set. When `None`, the upload endpoints
    /// 503 with "asset storage not configured" — deployments that don't
    /// care about avatar/icon uploads stay first-class.
    pub r2: Option<R2Settings>,
}

/// R2 object-storage configuration. All five fields are required as a
/// group; the `TryFrom<RawSettings>` impl rejects partial sets via
/// [`SettingsError::PartialR2Config`].
#[derive(Debug, Clone)]
pub struct R2Settings {
    /// Cloudflare account id — used to compose the S3 endpoint URL.
    pub account_id: SecretString,
    /// R2 bucket name (e.g. `patom-assets-prod`).
    pub bucket: String,
    /// Scoped R2 API token: access key id.
    pub access_key_id: SecretString,
    /// Scoped R2 API token: secret access key.
    pub secret_access_key: SecretString,
    /// Public-facing base URL (custom domain) the FE renders. Validated
    /// at the boundary: `https://` scheme, no path/query/fragment, no
    /// trailing slash.
    pub public_host: String,
}

impl R2Settings {
    /// Compose the S3-compatible endpoint URL for the account.
    #[must_use]
    pub fn endpoint(&self) -> String {
        format!(
            "https://{account}.r2.cloudflarestorage.com",
            account = self.account_id.expose(),
        )
    }
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

    let mut ids: HashMap<String, String> = HashMap::new();
    let mut secrets: HashMap<String, String> = HashMap::new();

    for (key, value) in vars {
        let Some(remainder) = key.strip_prefix(PREFIX) else {
            continue;
        };
        if let Some(middle) = remainder.strip_suffix(SECRET_SUFFIX)
            && !middle.is_empty()
        {
            secrets.insert(middle.to_ascii_lowercase(), value);
        } else if let Some(middle) = remainder.strip_suffix(ID_SUFFIX)
            && !middle.is_empty()
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
    /// Resolved login-OIDC issuer (ADR-0011). `PATOM_OIDC_ISSUER` when
    /// set; otherwise the Google preset `https://accounts.google.com`.
    /// Endpoints + JWKS are discovered from this at startup.
    pub oidc_issuer: IssuerUrl,
    /// Login-OIDC client id. `PATOM_OIDC_CLIENT_ID` for a generic issuer;
    /// `GOOGLE_CLIENT_ID` for the Google preset. The MCP `google` catalog
    /// entry has its own credentials in [`platform_oauth_clients`] — this
    /// is **login only**.
    pub oidc_client_id: SecretString,
    /// Login-OIDC client secret. `PATOM_OIDC_CLIENT_SECRET` / `GOOGLE_CLIENT_SECRET`.
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
    /// "missing field" error, same shape as `google_client_id`.
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
    // Login provider (ADR-0011). When `patom_oidc_issuer` is set the
    // generic OIDC path is used (requires the three `patom_oidc_*`
    // creds); otherwise the Google preset is used (requires the three
    // `google_*` creds). Resolved + validated in `TryFrom<RawSettings>`,
    // so all six are optional at the serde layer.
    #[serde(default)]
    google_client_id: Option<SecretString>,
    #[serde(default)]
    google_client_secret: Option<SecretString>,
    #[serde(default)]
    google_redirect_url: Option<String>,
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
    // way the `github` catalog row can complete a flow. Same shape as
    // `google_client_id` above; a missing env var surfaces as the
    // `config` crate's own "missing field" error.
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
    #[serde(default = "default_web_dist")]
    patom_web_dist: PathBuf,

    // Slack adapter — all three are optional individually but accepted
    // only as a complete set (validation in `TryFrom<RawSettings>`).
    #[serde(default)]
    patom_slack_signing_secret: Option<SecretString>,
    #[serde(default)]
    patom_slack_client_id: Option<String>,
    #[serde(default)]
    patom_slack_client_secret: Option<SecretString>,

    // R2 object storage — same all-or-nothing rule as Slack.
    #[serde(default)]
    patom_r2_account_id: Option<SecretString>,
    #[serde(default)]
    patom_r2_bucket: Option<String>,
    #[serde(default)]
    patom_r2_access_key_id: Option<SecretString>,
    #[serde(default)]
    patom_r2_secret_access_key: Option<SecretString>,
    #[serde(default)]
    patom_r2_public_host: Option<String>,
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
/// for R2, `&["http", "https"]` for the SPA base url).
fn parse_origin(raw: &str, allowed_schemes: &[&str]) -> Result<String, &'static str> {
    let parsed = url::Url::parse(raw).map_err(|_| "not a valid url")?;
    if !allowed_schemes.iter().any(|&s| s == parsed.scheme()) {
        return Err(match allowed_schemes {
            ["https"] => "scheme must be https",
            ["http", "https"] | ["https", "http"] => "scheme must be http or https",
            _ => "scheme not allowed",
        });
    }
    if parsed.path() != "/" && !parsed.path().is_empty() {
        return Err("must be an origin with no path");
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err("must not include query or fragment");
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("userinfo is not allowed");
    }
    // `Origin::ascii_serialization` yields `scheme://host[:port]` with no
    // trailing slash, regardless of whether `raw` ended with one.
    Ok(parsed.origin().ascii_serialization())
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
/// all three to be present. Shared by the generic-OIDC and Google-preset
/// arms of `TryFrom<RawSettings>` so the "all-or-nothing" rule lives in
/// one place.
fn require_login_creds(
    issuer: IssuerUrl,
    client_id: Option<SecretString>,
    client_secret: Option<SecretString>,
    redirect_url: Option<String>,
) -> Result<(IssuerUrl, SecretString, SecretString, String), SettingsError> {
    if let (Some(id), Some(secret), Some(redirect)) = (client_id, client_secret, redirect_url) {
        Ok((issuer, id, secret, redirect))
    } else {
        Err(SettingsError::MissingLoginProvider)
    }
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
        // Resolve the login OIDC provider (ADR-0011). `PATOM_OIDC_ISSUER`
        // selects a generic issuer (with the `PATOM_OIDC_*` creds); its
        // absence falls back to the Google preset (with the `GOOGLE_*`
        // creds). Cloud config stays a one-liner; self-host points at its
        // own IdP without touching Google env vars.
        let (oidc_issuer, oidc_client_id, oidc_client_secret, oidc_redirect_url) =
            if let Some(raw_issuer) = raw.patom_oidc_issuer {
                let issuer = IssuerUrl::try_from(raw_issuer.as_str()).map_err(|e| {
                    SettingsError::InvalidOidcIssuer {
                        raw: raw_issuer.clone(),
                        reason: parse_error_reason(&e),
                    }
                })?;
                require_login_creds(
                    issuer,
                    raw.patom_oidc_client_id,
                    raw.patom_oidc_client_secret,
                    raw.patom_oidc_redirect_url,
                )?
            } else {
                // The Google preset's issuer is a compile-time constant;
                // a parse failure here is a programmer error, surfaced via
                // the same variant for symmetry.
                let issuer = IssuerUrl::try_from(GOOGLE_OIDC_ISSUER).map_err(|e| {
                    SettingsError::InvalidOidcIssuer {
                        raw: GOOGLE_OIDC_ISSUER.to_owned(),
                        reason: parse_error_reason(&e),
                    }
                })?;
                require_login_creds(
                    issuer,
                    raw.google_client_id,
                    raw.google_client_secret,
                    raw.google_redirect_url,
                )?
            };
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
        let r2 = match (
            raw.patom_r2_account_id,
            raw.patom_r2_bucket,
            raw.patom_r2_access_key_id,
            raw.patom_r2_secret_access_key,
            raw.patom_r2_public_host,
        ) {
            (None, None, None, None, None) => None,
            (
                Some(account_id),
                Some(bucket),
                Some(access_key_id),
                Some(secret_access_key),
                Some(public_host_raw),
            ) => {
                let public_host = parse_r2_public_host(&public_host_raw)?;
                Some(R2Settings {
                    account_id,
                    bucket,
                    access_key_id,
                    secret_access_key,
                    public_host,
                })
            }
            _ => return Err(SettingsError::PartialR2Config),
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
            slack,
            r2,
        })
    }
}

/// Validate `PATOM_R2_PUBLIC_HOST` — must be an `https://` origin so the
/// asset module can join it to object keys with a single `/` without
/// producing `//<key>` URLs.
fn parse_r2_public_host(raw: &str) -> Result<String, SettingsError> {
    parse_origin(raw, &["https"]).map_err(|reason| SettingsError::InvalidR2PublicHost {
        raw: raw.to_owned(),
        reason,
    })
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
            google_client_id: Some(secret("test-client-id")),
            google_client_secret: Some(secret("test-client-secret")),
            google_redirect_url: Some("http://localhost:8080/auth/google/callback".to_string()),
            patom_oidc_issuer: None,
            patom_oidc_client_id: None,
            patom_oidc_client_secret: None,
            patom_oidc_redirect_url: None,
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
            patom_web_dist: default_web_dist(),
            patom_slack_signing_secret: None,
            patom_slack_client_id: None,
            patom_slack_client_secret: None,
            patom_r2_account_id: None,
            patom_r2_bucket: None,
            patom_r2_access_key_id: None,
            patom_r2_secret_access_key: None,
            patom_r2_public_host: None,
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
    fn google_preset_is_default_login_provider() {
        let mut raw = empty_raw();
        raw.anthropic_api_key = Some(secret("sk-ant"));
        let s = Settings::try_from(raw).expect("valid");
        assert_eq!(s.auth.oidc_issuer.as_str(), "https://accounts.google.com");
        assert!(!s.auth.bootstrap_admin);
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
        raw.google_client_id = None;
        raw.google_client_secret = None;
        raw.google_redirect_url = None;
        let err = Settings::try_from(raw).expect_err("expected error");
        assert!(matches!(err, SettingsError::MissingLoginProvider));
    }

    #[test]
    fn bootstrap_admin_flag_is_honored() {
        let mut raw = empty_raw();
        raw.anthropic_api_key = Some(secret("sk-ant"));
        raw.patom_bootstrap_admin = true;
        let s = Settings::try_from(raw).expect("valid");
        assert!(s.auth.bootstrap_admin);
    }
}
