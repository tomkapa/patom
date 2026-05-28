//! Codex-style streaming-HTTP MCP transport adapter.
//!
//! Wraps `reqwest::Client` to implement rmcp 1.7's [`StreamableHttpClient`]
//! trait with two behaviours the bare reqwest impl doesn't have:
//!
//!   1. **Refresh-on-acquire.** Every outbound request reads the current
//!      `OAuth2Payload`; if `expires_at` falls inside the skew window
//!      the adapter refreshes the token (taking a per-server mutex so
//!      concurrent requests share one POST to the AS) and persists the
//!      new payload before attaching the Bearer.
//!
//!   2. **Refresh-on-401.** When the upstream server returns
//!      `AuthRequired`, the adapter refreshes (under the same mutex)
//!      and retries the request **exactly once** before propagating the
//!      error. `InsufficientScope` is surfaced verbatim — re-consent is
//!      a user action, not a retry.
//!
//! The refresh path reconstructs [`RefreshCreds`] from the persisted
//! `OAuth2Payload` (DCR entries: `dcr_client_id`/`secret`/`auth_method`;
//! Platform entries: env + catalog) so no separate clients table is
//! consulted. Background refresher gone; the freshness race between a
//! periodic tick and a busy call site closes by construction.

use std::collections::HashMap;
use std::sync::Arc;

use futures::stream::BoxStream;
use reqwest::header::{HeaderName, HeaderValue};
use rmcp::model::ClientJsonRpcMessage;
use rmcp::transport::streamable_http_client::{
    AuthRequiredError, StreamableHttpClient, StreamableHttpError, StreamableHttpPostResponse,
};
use sse_stream::{Error as SseError, Sse};
use tokio::sync::Mutex;
use tokio::time::timeout;

use crate::auth::OrgId;
use crate::clock::SharedClock;
use crate::config::PlatformOAuthClient;
use crate::mcp::catalog::{ClientSource, McpCatalogEntry, platform_env_middle};
use crate::mcp::credentials::{
    CredentialPayload, McpCredentialWrite, OAuth2Payload, SharedMcpCredentialStore,
};
use crate::mcp::types::{McpServerId, TokenAuthMethod};

use super::errors::OAuthError;
use super::flow::{OAuthFlowClient, RefreshCreds, RefreshOutcome, refresh_oauth_token};
use super::store::OAuthClientId;

/// Skew applied at acquire time — refresh when the access token is
/// within this window of expiry. 60 s gives a few RTTs of headroom on
/// vendors that issue 1-hour tokens.
const REFRESH_SKEW: std::time::Duration = std::time::Duration::from_secs(60);

/// Outer bound on the refresh-grant HTTP round-trip.
///
/// `OAuthFlowClient` already configures a 10 s `.timeout` + 5 s
/// `.connect_timeout` on its `reqwest::Client`, but per CLAUDE.md §5
/// every I/O `await` is wrapped explicitly so a future change to the
/// inner client (or a slow body stream on a connection that doesn't
/// otherwise drop) can't hold the per-server refresh mutex
/// indefinitely.
const REFRESH_GRANT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// [`REFRESH_SKEW`] in `chrono::Duration` form. Wrapped in a helper so
/// the `expect` lives in one spot — the conversion is infallible for
/// any sub-i64-ms duration so this is a named assertion (§6).
fn refresh_skew() -> chrono::Duration {
    chrono::Duration::from_std(REFRESH_SKEW)
        .expect("invariant: REFRESH_SKEW fits in chrono::Duration")
}

/// Adapter the rmcp transport consumes. Cheap to clone — every field is
/// behind an `Arc` so the closures rmcp produces internally clone it
/// freely.
#[derive(Clone)]
pub struct PatomMcpHttpClient {
    inner: reqwest::Client,
    state: Arc<AdapterState>,
}

struct AdapterState {
    credentials: SharedMcpCredentialStore,
    flow: OAuthFlowClient,
    clock: SharedClock,
    server_id: McpServerId,
    org_id: OrgId,
    /// Per-(server, org) serialisation point for refreshes. Pulled from
    /// the registry's shared `RefreshLockMap` so concurrent adapters
    /// for the same server share one mutex — a fresh adapter per
    /// connect (the registry's reconnect strategy) doesn't lose
    /// dedup.
    refresh_lock: Arc<Mutex<()>>,
    /// Static custom headers attached to every request (per-deployment
    /// auth shims). The adapter holds them so they don't need to be
    /// passed in to every rmcp transport method.
    custom_headers: HashMap<HeaderName, HeaderValue>,
    /// Catalog row for this server. Drives the refresh path's choice
    /// between env-credentials (Platform) and persisted DCR
    /// (Dcr → `OAuth2Payload.dcr_*`).
    catalog: McpCatalogEntry,
    /// Env-keyed Patom-supported OAuth clients. Empty for deployments
    /// that wire no platform vendors.
    platform_clients: Arc<HashMap<String, PlatformOAuthClient>>,
    /// Public callback URL the OAuth flow was started with. Some ASes
    /// (Microsoft Azure AD with strict redirect validation) echo back
    /// the original redirect on the refresh grant, so the refresh path
    /// must attach it identically.
    redirect_uri: Arc<str>,
}

impl std::fmt::Debug for PatomMcpHttpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PatomMcpHttpClient")
            .field("server_id", &self.state.server_id)
            .field("org_id", &self.state.org_id)
            .field("catalog_id", &self.state.catalog.id)
            .finish_non_exhaustive()
    }
}

/// Inputs to [`PatomMcpHttpClient::new`]. Bundled so the constructor
/// signature stays under clippy's argument-count cap.
pub struct PatomMcpHttpClientConfig {
    pub inner: reqwest::Client,
    pub credentials: SharedMcpCredentialStore,
    pub flow: OAuthFlowClient,
    pub clock: SharedClock,
    pub server_id: McpServerId,
    pub org_id: OrgId,
    pub custom_headers: HashMap<HeaderName, HeaderValue>,
    pub catalog: McpCatalogEntry,
    pub platform_clients: Arc<HashMap<String, PlatformOAuthClient>>,
    /// Per-(server, org) refresh mutex pulled from the registry's
    /// shared `RefreshLockMap`. Cheap-clone Arc; ownership stays on the
    /// registry.
    pub refresh_lock: Arc<Mutex<()>>,
    /// Public OAuth callback URL (e.g. `<oauth_redirect_base>/mcp-oauth/callback`).
    pub redirect_uri: Arc<str>,
}

impl std::fmt::Debug for PatomMcpHttpClientConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PatomMcpHttpClientConfig")
            .field("server_id", &self.server_id)
            .field("org_id", &self.org_id)
            .finish_non_exhaustive()
    }
}

impl PatomMcpHttpClient {
    #[must_use]
    pub fn new(cfg: PatomMcpHttpClientConfig) -> Self {
        Self {
            inner: cfg.inner,
            state: Arc::new(AdapterState {
                credentials: cfg.credentials,
                flow: cfg.flow,
                clock: cfg.clock,
                server_id: cfg.server_id,
                org_id: cfg.org_id,
                refresh_lock: cfg.refresh_lock,
                custom_headers: cfg.custom_headers,
                catalog: cfg.catalog,
                platform_clients: cfg.platform_clients,
                redirect_uri: cfg.redirect_uri,
            }),
        }
    }

    /// Read the current OAuth2 payload and decide whether to refresh
    /// proactively. Returns the Bearer to attach to the next request
    /// (empty `Option` ⇒ no credentials, leave the header off).
    async fn acquire_bearer(&self) -> Result<Option<String>, AdapterError> {
        let Some(payload) = self.read_oauth_payload().await? else {
            return Ok(None);
        };
        let now = self.state.clock.now_utc();
        if payload.expires_at > now + refresh_skew() {
            return Ok(Some(payload.access_token));
        }
        if payload.refresh_token.is_none() {
            return Ok(Some(payload.access_token));
        }
        let refreshed = self.refresh_payload(payload).await?;
        Ok(Some(refreshed.access_token))
    }

    async fn read_oauth_payload(&self) -> Result<Option<OAuth2Payload>, AdapterError> {
        let Some(record) = self
            .state
            .credentials
            .read(self.state.server_id, self.state.org_id)
            .await
            .map_err(AdapterError::Credentials)?
        else {
            return Ok(None);
        };
        match record.payload {
            CredentialPayload::Oauth2(p) => Ok(Some(p)),
            CredentialPayload::StaticHeaders { .. } => Ok(None),
        }
    }

    /// Refresh + persist + return the new payload. Takes the
    /// shared per-(server, org) mutex so N concurrent acquires across
    /// every adapter for this server share exactly one token-endpoint
    /// POST. The mutex is released before the credentials upsert —
    /// only the AS round-trip is the dedup hot path.
    #[tracing::instrument(
        name = "mcp.oauth.transport.refresh_payload",
        skip_all,
        fields(
            patom.mcp.server.id = %self.state.server_id,
            patom.org.id = %self.state.org_id,
            patom.mcp.catalog_id = %self.state.catalog.id,
        ),
    )]
    async fn refresh_payload(
        &self,
        observed: OAuth2Payload,
    ) -> Result<OAuth2Payload, AdapterError> {
        let new_payload = {
            // Lock scope. Holds for the double-check + AS round-trip
            // only; persistence happens after the guard drops so a slow
            // encrypted upsert can't cascade into unrelated requests'
            // tail latency.
            let _g = self.state.refresh_lock.lock().await;
            // Double-checked re-read under the lock. `None` here means
            // a concurrent `POST /oauth/disconnect` deleted the
            // credentials row while we waited for the mutex — return
            // ReconnectRequired so we never re-create the row the user
            // just severed. The previously-observed payload is
            // intentionally dropped.
            let _ = observed;
            let Some(current) = self.read_oauth_payload().await? else {
                return Err(AdapterError::ReconnectRequired(
                    "credentials deleted while refresh was pending".into(),
                ));
            };
            let now = self.state.clock.now_utc();
            if current.expires_at > now + refresh_skew() {
                // Another caller refreshed under the lock; reuse.
                return Ok(current);
            }
            let Some(refresh_token) = current.refresh_token.as_deref() else {
                return Err(AdapterError::ReconnectRequired("no refresh_token".into()));
            };
            let creds = self.build_refresh_creds(&current)?;
            // Belt-and-braces: `OAuthFlowClient`'s reqwest client already
            // configures send + connect timeouts, but per CLAUDE.md §5
            // every I/O await is explicitly bounded so a future change to
            // the inner client can't hold the refresh mutex forever.
            let outcome = timeout(
                REFRESH_GRANT_TIMEOUT,
                refresh_oauth_token(&self.state.flow, &creds, refresh_token, now),
            )
            .await
            .map_err(|_| {
                AdapterError::Refresh(OAuthError::TokenEndpoint(format!(
                    "refresh grant exceeded {}s",
                    REFRESH_GRANT_TIMEOUT.as_secs()
                )))
            })?
            .map_err(AdapterError::Refresh)?;
            match outcome {
                RefreshOutcome::Refreshed(t) => OAuth2Payload {
                    access_token: t.access_token,
                    refresh_token: t.refresh_token.or_else(|| current.refresh_token.clone()),
                    expires_at: t.expires_at,
                    scope: t.scope.or_else(|| current.scope.clone()),
                    issuer: t.issuer,
                    token_endpoint: t.token_endpoint,
                    // Carry DCR material + the chosen auth method
                    // forward — refresh doesn't rotate either.
                    dcr_client_id: current.dcr_client_id.clone(),
                    dcr_client_secret: current.dcr_client_secret.clone(),
                    token_endpoint_auth_method: current.token_endpoint_auth_method,
                },
                RefreshOutcome::Revoked => {
                    return Err(AdapterError::ReconnectRequired(
                        "refresh_token revoked".into(),
                    ));
                }
            }
            // `_g` drops here.
        };
        // Persist outside the mutex — slow DB writes don't serialise
        // concurrent acquires whose tokens are already fresh.
        self.state
            .credentials
            .upsert(McpCredentialWrite {
                server_id: self.state.server_id,
                org_id: self.state.org_id,
                payload: CredentialPayload::Oauth2(new_payload.clone()),
            })
            .await
            .map_err(AdapterError::Credentials)?;
        Ok(new_payload)
    }

    /// Reconstruct the credentials the refresh grant needs from the
    /// persisted payload + catalog. Branches on `catalog.client_source`:
    /// * Platform → env-keyed `(client_id, client_secret)` paired with
    ///   the payload's persisted `token_endpoint_auth_method`.
    /// * Dcr → DCR client material + auth method, all from the payload
    ///   (folded in by the callback handler).
    /// * None → unreachable; the OAuth flow can't have run.
    ///
    /// The persisted `token_endpoint_auth_method` is load-bearing: it's
    /// what the resolver chose from the AS's
    /// `token_endpoint_auth_methods_supported` at start time. Falling
    /// back here would silently misalign the refresh POST with what
    /// the AS expects.
    fn build_refresh_creds(&self, payload: &OAuth2Payload) -> Result<RefreshCreds, AdapterError> {
        // Persisted auth method is the resolver's start-time choice.
        // Old rows that predate this PR decode to `None` via
        // `#[serde(default)]`; the RFC 7591 default is `client_secret_basic`.
        let auth_method = payload
            .token_endpoint_auth_method
            .unwrap_or(TokenAuthMethod::ClientSecretBasic);
        let redirect_uri = self.state.redirect_uri.as_ref().to_owned();
        match self.state.catalog.client_source {
            ClientSource::None => Err(AdapterError::ReconnectRequired(
                "catalog client_source = 'none'; refresh not applicable".into(),
            )),
            ClientSource::Platform => {
                let key_id = self
                    .state
                    .catalog
                    .platform_client_alias
                    .as_ref()
                    .unwrap_or(&self.state.catalog.id);
                let key = platform_env_middle(key_id);
                let creds = self.state.platform_clients.get(&key).ok_or_else(|| {
                    AdapterError::ReconnectRequired(format!(
                        "platform OAuth client for catalog `{id}` not configured",
                        id = self.state.catalog.id
                    ))
                })?;
                let client_id = OAuthClientId::try_from(creds.client_id.expose().to_owned())
                    .map_err(|e| {
                        AdapterError::Refresh(OAuthError::Misconfigured(format!(
                            "platform client_id: {e}"
                        )))
                    })?;
                Ok(RefreshCreds {
                    issuer: payload.issuer.clone(),
                    token_endpoint: payload.token_endpoint.clone(),
                    client_id,
                    client_secret: Some(creds.client_secret.clone()),
                    token_endpoint_auth_method: auth_method,
                    redirect_uri,
                })
            }
            ClientSource::Dcr => {
                let client_id_raw = payload.dcr_client_id.clone().ok_or_else(|| {
                    AdapterError::ReconnectRequired(
                        "OAuth2Payload missing dcr_client_id; re-connect to refresh".into(),
                    )
                })?;
                let client_id = OAuthClientId::try_from(client_id_raw).map_err(|e| {
                    AdapterError::Refresh(OAuthError::Misconfigured(format!(
                        "persisted dcr_client_id rejected: {e}"
                    )))
                })?;
                let client_secret = payload
                    .dcr_client_secret
                    .as_ref()
                    .map(|s| crate::types::SecretString::try_from(s.to_owned()))
                    .transpose()
                    .map_err(|e| {
                        AdapterError::Refresh(OAuthError::Misconfigured(format!(
                            "persisted dcr_client_secret rejected: {e}"
                        )))
                    })?;
                Ok(RefreshCreds {
                    issuer: payload.issuer.clone(),
                    token_endpoint: payload.token_endpoint.clone(),
                    client_id,
                    client_secret,
                    token_endpoint_auth_method: auth_method,
                    redirect_uri,
                })
            }
        }
    }
}

/// Adapter-local error type. Surfaces back to rmcp as `Client(err)` —
/// rmcp's `AuthRequired` / `InsufficientScope` variants come from the
/// inner reqwest impl when the upstream server returns 401/403.
#[derive(Debug, thiserror::Error)]
pub enum AdapterError {
    #[error("credentials store: {0}")]
    Credentials(crate::mcp::error::McpError),
    #[error("refresh: {0}")]
    Refresh(OAuthError),
    #[error("reconnect required: {0}")]
    ReconnectRequired(String),
    #[error("inner http: {0}")]
    Inner(#[from] reqwest::Error),
}

impl StreamableHttpClient for PatomMcpHttpClient {
    type Error = AdapterError;

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        // Caller-supplied auth header is ignored — the adapter computes
        // a fresh one from the credentials store. rmcp's transport
        // machinery doesn't know how to refresh; we do.
        _auth_header: Option<String>,
        _custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        let mut bearer = self
            .acquire_bearer()
            .await
            .map_err(StreamableHttpError::Client)?;
        let mut attempts: u8 = 0;
        loop {
            match self
                .inner
                .post_message(
                    uri.clone(),
                    message.clone(),
                    session_id.clone(),
                    bearer.clone(),
                    self.state.custom_headers.clone(),
                )
                .await
            {
                Ok(resp) => return Ok(resp),
                Err(StreamableHttpError::AuthRequired(ar)) if attempts == 0 => {
                    attempts = 1;
                    bearer = self
                        .refresh_after_401(&ar)
                        .await
                        .map_err(StreamableHttpError::Client)?;
                }
                Err(other) => return Err(map_inner_error(other)),
            }
        }
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        _auth_header: Option<String>,
        _custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), StreamableHttpError<Self::Error>> {
        let bearer = self
            .acquire_bearer()
            .await
            .map_err(StreamableHttpError::Client)?;
        self.inner
            .delete_session(uri, session_id, bearer, self.state.custom_headers.clone())
            .await
            .map_err(map_inner_error)
    }

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        last_event_id: Option<String>,
        _auth_header: Option<String>,
        _custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<BoxStream<'static, Result<Sse, SseError>>, StreamableHttpError<Self::Error>> {
        let bearer = self
            .acquire_bearer()
            .await
            .map_err(StreamableHttpError::Client)?;
        self.inner
            .get_stream(
                uri,
                session_id,
                last_event_id,
                bearer,
                self.state.custom_headers.clone(),
            )
            .await
            .map_err(map_inner_error)
    }
}

impl PatomMcpHttpClient {
    /// Refresh path triggered by a 401 from the upstream server. Logs
    /// the AS's `WWW-Authenticate` challenge for diagnostics, then
    /// reuses the proactive-refresh path under the same mutex.
    async fn refresh_after_401(
        &self,
        ar: &AuthRequiredError,
    ) -> Result<Option<String>, AdapterError> {
        tracing::info!(
            event = "mcp.oauth.transport.refresh_on_401",
            patom.mcp.server.id = %self.state.server_id,
            www_authenticate = %ar.www_authenticate_header,
        );
        let Some(payload) = self.read_oauth_payload().await? else {
            return Err(AdapterError::ReconnectRequired(
                "no OAuth2 credentials present at 401-retry path".into(),
            ));
        };
        let refreshed = self.refresh_payload(payload).await?;
        Ok(Some(refreshed.access_token))
    }
}

/// Adapter's inner client is `reqwest::Client` whose `Error` is
/// `reqwest::Error`. Wrap as `AdapterError::Inner` so the outer
/// `StreamableHttpError<AdapterError>` stays type-aligned.
fn map_inner_error(e: StreamableHttpError<reqwest::Error>) -> StreamableHttpError<AdapterError> {
    match e {
        StreamableHttpError::Client(re) => StreamableHttpError::Client(AdapterError::Inner(re)),
        StreamableHttpError::Sse(s) => StreamableHttpError::Sse(s),
        StreamableHttpError::Io(io) => StreamableHttpError::Io(io),
        StreamableHttpError::UnexpectedEndOfStream => StreamableHttpError::UnexpectedEndOfStream,
        StreamableHttpError::UnexpectedServerResponse(c) => {
            StreamableHttpError::UnexpectedServerResponse(c)
        }
        StreamableHttpError::UnexpectedContentType(c) => {
            StreamableHttpError::UnexpectedContentType(c)
        }
        StreamableHttpError::ServerDoesNotSupportSse => {
            StreamableHttpError::ServerDoesNotSupportSse
        }
        StreamableHttpError::ServerDoesNotSupportDeleteSession => {
            StreamableHttpError::ServerDoesNotSupportDeleteSession
        }
        StreamableHttpError::TokioJoinError(j) => StreamableHttpError::TokioJoinError(j),
        StreamableHttpError::Deserialize(d) => StreamableHttpError::Deserialize(d),
        StreamableHttpError::TransportChannelClosed => StreamableHttpError::TransportChannelClosed,
        StreamableHttpError::MissingSessionIdInResponse => {
            StreamableHttpError::MissingSessionIdInResponse
        }
        StreamableHttpError::AuthRequired(ar) => StreamableHttpError::AuthRequired(ar),
        StreamableHttpError::InsufficientScope(is) => StreamableHttpError::InsufficientScope(is),
        StreamableHttpError::ReservedHeaderConflict(s) => {
            StreamableHttpError::ReservedHeaderConflict(s)
        }
        StreamableHttpError::SessionExpired => StreamableHttpError::SessionExpired,
        other => StreamableHttpError::Client(AdapterError::ReconnectRequired(format!(
            "unexpected transport error: {other:?}"
        ))),
    }
}
