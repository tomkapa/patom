use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;

use chrono_tz::Tz;
use config::{Config, ConfigError, Environment};
use serde::Deserialize;
use thiserror::Error;

use crate::types::{ModelId, SecretString};

/// Default SPA dist path when `RELAY_WEB_DIST` is unset. Matches
/// `web/build.ts`'s `outdir`; operators running the binary from outside
/// the repo root must override.
const DEFAULT_WEB_DIST: &str = "./web/dist";

#[derive(Debug, Error)]
pub enum SettingsError {
    #[error("config source: {0}")]
    Source(#[from] ConfigError),

    #[error("no provider api key set; set exactly one of: OPENAI_API_KEY, ANTHROPIC_API_KEY")]
    NoProviderKey,

    #[error(
        "multiple provider api keys set ({set:?}); set only one of: OPENAI_API_KEY, ANTHROPIC_API_KEY"
    )]
    MultipleProviderKeys { set: Vec<&'static str> },

    #[error("embedding configuration missing; set EMBEDDING_API_KEY and EMBEDDING_MODEL")]
    MissingEmbedding,

    #[error("default timezone {raw:?} is not a valid IANA name")]
    InvalidDefaultTimezone { raw: String },

    #[error("auth: jwt secret too short — need at least 32 bytes")]
    AuthSecretTooShort,

    #[error("auth: RELAY_WEB_BASE_URL is not a valid origin: {raw:?} ({reason})")]
    InvalidWebBaseUrl { raw: String, reason: &'static str },

    #[error(
        "slack: partial configuration; set all of RELAY_SLACK_SIGNING_SECRET, \
         RELAY_SLACK_CLIENT_ID, RELAY_SLACK_CLIENT_SECRET — or none"
    )]
    PartialSlackConfig,
}

/// Process-wide configuration loaded once at startup. Secrets are wrapped in
/// [`SecretString`] so a stray `tracing::debug!(?settings)` cannot leak them.
#[derive(Debug, Clone)]
pub struct Settings {
    /// Selected LLM backend plus the credentials it needs. Parsed once at startup so
    /// `build_provider` is an infallible exhaustive match.
    pub provider: ProviderSettings,
    pub brave_search_api_key: SecretString,
    pub model: ModelId,
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
    /// `RELAY_WEB_DIST` (default `./web/dist`).
    pub web_dist: PathBuf,
    /// Slack adapter — present iff all `RELAY_SLACK_*` env vars are
    /// set. `None` is a first-class deployment (the Slack routes and
    /// background workers stay un-spawned).
    pub slack: Option<SlackSettings>,
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

/// Auth subsystem configuration. All fields are required; the OAuth
/// flow refuses to start without a real Google client.
#[derive(Debug, Clone)]
pub struct AuthSettings {
    /// HS256 signing secret for session JWTs. Must be ≥32 bytes.
    pub jwt_secret: SecretString,
    /// Google OAuth client id (no secret material).
    pub google_client_id: SecretString,
    /// Google OAuth client secret.
    pub google_client_secret: SecretString,
    /// Redirect URL registered with Google, e.g.
    /// `http://localhost:8080/auth/google/callback`.
    pub google_redirect_url: String,
    /// Whether to set the `Secure` flag on the session cookie. Off in
    /// local-dev to keep `http://localhost` workable; on everywhere
    /// else.
    pub cookie_secure: bool,
    /// Master KEK used to derive per-org KEKs for the MCP credentials
    /// envelope. Base64-encoded 32 bytes; rejected at the boundary if
    /// missing or wrong size. Sourced from `RELAY_MASTER_KEK`.
    pub master_kek: SecretString,
    /// Base URL Relay tells vendors to redirect back to after consent.
    /// The OAuth callback path is appended to this; e.g.
    /// `http://localhost:8080` → `http://localhost:8080/mcp-oauth/callback`.
    /// Sourced from `RELAY_OAUTH_REDIRECT_BASE`.
    pub oauth_redirect_base: String,
    /// Origin of the SPA (e.g. `http://localhost:5173` in dev). When set,
    /// the BE prepends this to the post-OAuth-callback redirect so the
    /// browser lands on the FE host instead of the BE host. Empty in
    /// same-origin prod deployments where BE and FE share an origin.
    /// Sourced from `RELAY_WEB_BASE_URL`.
    pub web_base_url: Option<String>,
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

/// Provider selection + the credentials that go with it. Exhaustive — adding a backend
/// means a new variant here and a new arm in `app::build_provider`.
#[derive(Debug, Clone)]
pub enum ProviderSettings {
    Openai {
        api_key: SecretString,
        base_url: Option<String>,
    },
    Anthropic {
        api_key: SecretString,
        base_url: Option<String>,
    },
}

impl ProviderSettings {
    /// Low-cardinality identifier for tracing fields (`relay.provider.selected`).
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Openai { .. } => "openai",
            Self::Anthropic { .. } => "anthropic",
        }
    }
}

/// Flat env shape — every provider's credentials are optional. The presence of exactly
/// one provider's `*_API_KEY` selects which provider runs; setting zero or more than one
/// is a misconfiguration and rejected at the boundary. Kept private because `Settings`
/// is the validated type.
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

    brave_search_api_key: SecretString,
    #[serde(default = "default_model")]
    model: ModelId,
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
    relay_jwt_secret: SecretString,
    google_client_id: SecretString,
    google_client_secret: SecretString,
    google_redirect_url: String,
    // Secure by default — forgetting to set this in any https-fronted
    // deploy must not silently drop the `Secure` cookie flag. Local-dev
    // (http://localhost) overrides via `RELAY_COOKIE_SECURE=false` in
    // `.env`.
    #[serde(default = "default_cookie_secure")]
    relay_cookie_secure: bool,
    // R2 envelope encryption master key, base64-encoded 32 bytes.
    relay_master_kek: SecretString,
    // R3 upstream-OAuth redirect base URL. The MCP OAuth callback path is
    // appended at runtime; the AS sees `{this}/mcp-oauth/callback`.
    relay_oauth_redirect_base: String,
    // Optional SPA origin. When set, the BE prepends this to the
    // post-OAuth-callback redirect so the browser lands on the FE host
    // instead of the BE host (dev: FE on Vite/Bun, BE on 8080).
    #[serde(default)]
    relay_web_base_url: Option<String>,
    #[serde(default = "default_web_dist")]
    relay_web_dist: PathBuf,

    // Slack adapter — all three are optional individually but accepted
    // only as a complete set (validation in `TryFrom<RawSettings>`).
    #[serde(default)]
    relay_slack_signing_secret: Option<SecretString>,
    #[serde(default)]
    relay_slack_client_id: Option<String>,
    #[serde(default)]
    relay_slack_client_secret: Option<SecretString>,
}

fn default_web_dist() -> PathBuf {
    PathBuf::from(DEFAULT_WEB_DIST)
}

const fn default_cookie_secure() -> bool {
    true
}

/// Validate and normalize `RELAY_WEB_BASE_URL`: must be an absolute
/// http(s) origin — no path, query, fragment, or userinfo — so callers
/// can prepend it directly to a `/`-anchored route without producing
/// malformed redirects.
fn parse_web_base_url(raw: &str) -> Result<String, SettingsError> {
    let reject = |reason: &'static str| SettingsError::InvalidWebBaseUrl {
        raw: raw.to_owned(),
        reason,
    };
    let parsed = url::Url::parse(raw).map_err(|_| reject("not a valid url"))?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err(reject("scheme must be http or https"));
    }
    if parsed.path() != "/" && !parsed.path().is_empty() {
        return Err(reject("must be an origin with no path"));
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(reject("must not include query or fragment"));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(reject("userinfo is not allowed"));
    }
    // `Origin::ascii_serialization` yields `scheme://host[:port]` with no
    // trailing slash, regardless of whether `raw` ended with one.
    Ok(parsed.origin().ascii_serialization())
}

fn default_timezone_raw() -> String {
    "UTC".to_string()
}

fn default_model() -> ModelId {
    ModelId::try_from("claude-sonnet-4-5").expect("static default model id is valid")
}

fn default_http_addr() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 8080))
}

impl TryFrom<RawSettings> for Settings {
    type Error = SettingsError;

    fn try_from(raw: RawSettings) -> Result<Self, Self::Error> {
        // Provider is inferred from which `*_API_KEY` is set. Refusing the ambiguous
        // "both set" case is intentional: silently picking one would mask a copy-paste
        // bug in the operator's environment for the cost of a clearer error here.
        let provider = match (raw.openai_api_key, raw.anthropic_api_key) {
            (Some(api_key), None) => ProviderSettings::Openai {
                api_key,
                base_url: raw.openai_base_url,
            },
            (None, Some(api_key)) => ProviderSettings::Anthropic {
                api_key,
                base_url: raw.anthropic_base_url,
            },
            (None, None) => return Err(SettingsError::NoProviderKey),
            (Some(_), Some(_)) => {
                return Err(SettingsError::MultipleProviderKeys {
                    set: vec!["OPENAI_API_KEY", "ANTHROPIC_API_KEY"],
                });
            }
        };
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
        if raw.relay_jwt_secret.expose().len() < 32 {
            return Err(SettingsError::AuthSecretTooShort);
        }
        let web_base_url = match raw.relay_web_base_url {
            Some(raw_url) => Some(parse_web_base_url(&raw_url)?),
            None => None,
        };
        let auth = AuthSettings {
            jwt_secret: raw.relay_jwt_secret,
            google_client_id: raw.google_client_id,
            google_client_secret: raw.google_client_secret,
            google_redirect_url: raw.google_redirect_url,
            cookie_secure: raw.relay_cookie_secure,
            master_kek: raw.relay_master_kek,
            oauth_redirect_base: raw.relay_oauth_redirect_base,
            web_base_url,
        };
        let slack = match (
            raw.relay_slack_signing_secret,
            raw.relay_slack_client_id,
            raw.relay_slack_client_secret,
        ) {
            (None, None, None) => None,
            (Some(signing_secret), Some(client_id), Some(client_secret)) => {
                let redirect_url = format!("{}/slack/oauth/callback", auth.oauth_redirect_base);
                Some(SlackSettings {
                    signing_secret,
                    client_id,
                    client_secret,
                    redirect_url,
                })
            }
            _ => return Err(SettingsError::PartialSlackConfig),
        };
        Ok(Self {
            provider,
            brave_search_api_key: raw.brave_search_api_key,
            model: raw.model,
            http_addr: raw.http_addr,
            database_url: raw.database_url,
            embedding,
            default_timezone,
            auth,
            web_dist: raw.relay_web_dist,
            slack,
        })
    }
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
        Self::try_from(raw)
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
            brave_search_api_key: secret("brave"),
            model: default_model(),
            http_addr: default_http_addr(),
            database_url: secret("postgres://relay:relay@localhost:5432/relay"),
            embedding_api_key: Some(secret("emb")),
            embedding_base_url: None,
            embedding_model: Some("text-embedding-3-small".to_string()),
            embedding_dimensions: None,
            default_timezone: default_timezone_raw(),
            relay_jwt_secret: secret(&"a".repeat(64)),
            google_client_id: secret("test-client-id"),
            google_client_secret: secret("test-client-secret"),
            google_redirect_url: "http://localhost:8080/auth/google/callback".to_string(),
            relay_cookie_secure: false,
            // base64 of 32 bytes; never used in these tests since they only
            // exercise the Settings boundary, not crypto.
            relay_master_kek: secret("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="),
            relay_oauth_redirect_base: "http://localhost:8080".to_string(),
            relay_web_base_url: None,
            relay_web_dist: default_web_dist(),
            relay_slack_signing_secret: None,
            relay_slack_client_id: None,
            relay_slack_client_secret: None,
        }
    }

    #[test]
    fn no_provider_key_is_rejected() {
        let raw = empty_raw();
        let err = Settings::try_from(raw).expect_err("expected error");
        assert!(matches!(err, SettingsError::NoProviderKey));
    }

    #[test]
    fn both_provider_keys_set_is_rejected() {
        let mut raw = empty_raw();
        raw.openai_api_key = Some(secret("sk-x"));
        raw.anthropic_api_key = Some(secret("sk-ant"));
        let err = Settings::try_from(raw).expect_err("expected error");
        assert!(matches!(err, SettingsError::MultipleProviderKeys { .. }));
    }

    #[test]
    fn openai_key_alone_selects_openai() {
        let mut raw = empty_raw();
        raw.openai_api_key = Some(secret("sk-x"));
        raw.openai_base_url = Some("https://api.deepseek.com/v1".to_string());
        let s = Settings::try_from(raw).expect("valid");
        let ProviderSettings::Openai { base_url, .. } = &s.provider else {
            panic!("expected openai");
        };
        assert_eq!(base_url.as_deref(), Some("https://api.deepseek.com/v1"));
    }

    #[test]
    fn anthropic_key_alone_selects_anthropic() {
        let mut raw = empty_raw();
        raw.anthropic_api_key = Some(secret("sk-ant"));
        let s = Settings::try_from(raw).expect("valid");
        assert_eq!(s.provider.name(), "anthropic");
    }

    #[test]
    fn invalid_default_timezone_is_rejected() {
        let mut raw = empty_raw();
        raw.openai_api_key = Some(secret("sk-x"));
        raw.default_timezone = "Mars/Olympus_Mons".to_string();
        let err = Settings::try_from(raw).expect_err("expected error");
        assert!(matches!(err, SettingsError::InvalidDefaultTimezone { .. }));
    }

    #[test]
    fn default_timezone_defaults_to_utc() {
        let mut raw = empty_raw();
        raw.openai_api_key = Some(secret("sk-x"));
        let s = Settings::try_from(raw).expect("valid");
        assert_eq!(s.default_timezone, Tz::UTC);
    }

    #[test]
    fn default_timezone_parses_iana_name() {
        let mut raw = empty_raw();
        raw.openai_api_key = Some(secret("sk-x"));
        raw.default_timezone = "Asia/Bangkok".to_string();
        let s = Settings::try_from(raw).expect("valid");
        assert_eq!(s.default_timezone, chrono_tz::Asia::Bangkok);
    }

    #[test]
    fn missing_embedding_is_rejected() {
        let mut raw = empty_raw();
        raw.openai_api_key = Some(secret("sk-x"));
        raw.embedding_api_key = None;
        raw.embedding_model = None;
        let err = Settings::try_from(raw).expect_err("expected error");
        assert!(matches!(err, SettingsError::MissingEmbedding));
    }
}
