//! Background OAuth token refresher.
//!
//! Single coordinator task, modelled on `src/mcp/refresher.rs`. Every
//! tick:
//!   1. List every `oauth2` credential row across all orgs (privileged).
//!   2. For each row whose `expires_at` is within
//!      [`OAUTH_REFRESH_SKEW`] of `now`, take the per-server lock, call
//!      `refresh_oauth_token`, persist the new sealed payload, release
//!      the lock.
//!   3. On `invalid_grant`, flip `connection_status = 'reconnect_required'`
//!      and clear the cache entry.
//!
//! Concurrent-refresh dedup: the per-server `Arc<Mutex<()>>` cache is
//! shared with the on-call refresh path. Two requests for the same
//! server token line up on the same mutex, so only one outbound POST to
//! the token endpoint happens.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use sqlx::PgPool;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_util::sync::{CancellationToken, DropGuard};
use tracing::{instrument, warn};

use crate::clock::SharedClock;
use crate::mcp::credentials::open_payload;
use crate::mcp::{
    CredentialPayload, McpCredentialWrite, McpServerId, OAuth2Payload, SharedMcpCredentialStore,
};

use super::errors::OAuthError;
use super::flow::{OAuthFlowClient, RefreshOutcome, refresh_oauth_token};
use super::store::SharedMcpOAuthClientStore;

/// Tick cadence. Tokens that expire between ticks are still caught next
/// tick because we use a `now + SKEW` window.
const OAUTH_REFRESH_TICK: Duration = Duration::from_secs(60);

/// Refresh tokens that expire within this window. Wide enough to absorb
/// one missed tick + clock skew between Relay and the AS.
pub const OAUTH_REFRESH_SKEW: Duration = Duration::from_secs(120);

/// Per-tick cap on the number of refreshes; protects against an
/// unbounded token expiry cluster after a long pause.
const MAX_OAUTH_REFRESH_PER_TICK: usize = 200;

/// Per-server lock + cached freshest token timestamp. The mutex is
/// `tokio::sync::Mutex` because the inside region awaits the token
/// endpoint; the cache is read+update under one lock acquisition.
type OAuthTokenCache = RwLock<HashMap<McpServerId, Arc<Mutex<()>>>>;

/// Cheap-clone handle to the cache. Cloning it into `AppState` lets the
/// on-call refresh-and-retry path serialise on the same per-server mutex
/// as the background refresher.
#[derive(Clone, Default)]
pub struct SharedOAuthTokenCache(Arc<OAuthTokenCache>);

impl std::fmt::Debug for SharedOAuthTokenCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedOAuthTokenCache")
            .finish_non_exhaustive()
    }
}

impl SharedOAuthTokenCache {
    #[must_use]
    pub fn new() -> Self {
        Self(Arc::new(RwLock::new(HashMap::new())))
    }

    /// Get-or-insert the per-server mutex. Two callers for the same
    /// server obtain the same `Arc<Mutex<()>>` regardless of which lands
    /// first.
    fn lock_for(&self, id: McpServerId) -> Arc<Mutex<()>> {
        if let Some(m) = self
            .0
            .read()
            .expect("invariant: oauth token cache rwlock poisoned")
            .get(&id)
        {
            return m.clone();
        }
        let mut guard = self
            .0
            .write()
            .expect("invariant: oauth token cache rwlock poisoned");
        guard
            .entry(id)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    fn forget(&self, id: McpServerId) {
        let mut guard = self
            .0
            .write()
            .expect("invariant: oauth token cache rwlock poisoned");
        guard.remove(&id);
    }
}

/// Owned-handle wrapper around the coordinator task. Dropping it
/// cancels the task and joins it.
pub struct OAuthRefresher {
    shutdown: DropGuard,
    handle: JoinHandle<()>,
}

impl std::fmt::Debug for OAuthRefresher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthRefresher").finish_non_exhaustive()
    }
}

impl OAuthRefresher {
    /// Spawn the coordinator task. The returned cache handle is the
    /// shared per-server mutex map that the on-call refresh path
    /// serialises against.
    #[must_use]
    pub fn spawn(deps: RefresherDeps) -> (Self, SharedOAuthTokenCache) {
        let cache = SharedOAuthTokenCache::new();
        let cancel = CancellationToken::new();
        let token = cancel.clone();
        let cache_clone = cache.clone();
        let handle = tokio::spawn(async move {
            let mut tick = tokio::time::interval(OAUTH_REFRESH_TICK);
            // The default `Burst` policy would replay missed ticks back-
            // to-back after a stall; `Skip` collapses them so a paused
            // host can't cause a thundering refresh storm on resume. The
            // first tick fires immediately, which is what we want: any
            // token that expired while we were down gets caught before
            // serving traffic.
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    biased;
                    () = token.cancelled() => return,
                    _ = tick.tick() => {
                        if let Err(e) = run_one_tick(&deps, &cache_clone).await {
                            warn!(error = %e, "mcp.oauth.refresh.tick_failed");
                        }
                    }
                }
            }
        });
        (
            Self {
                shutdown: cancel.drop_guard(),
                handle,
            },
            cache,
        )
    }

    pub async fn shutdown(self) {
        drop(self.shutdown);
        if let Err(e) = self.handle.await {
            warn!(error = %e, "mcp.oauth.refresher.join.error");
        }
    }
}

/// Inputs the refresher needs. Bundled into one struct rather than five
/// `Arc` parameters so the spawn signature stays short.
pub struct RefresherDeps {
    pub pool: PgPool,
    pub clock: SharedClock,
    pub enc: crate::crypto::SharedOrgEncryptor,
    pub credentials: SharedMcpCredentialStore,
    pub oauth_clients: SharedMcpOAuthClientStore,
    pub flow: OAuthFlowClient,
    pub redirect_uri: String,
}

impl std::fmt::Debug for RefresherDeps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RefresherDeps").finish_non_exhaustive()
    }
}

#[instrument(name = "mcp.oauth.refresh.tick", skip_all)]
async fn run_one_tick(
    deps: &RefresherDeps,
    cache: &SharedOAuthTokenCache,
) -> Result<(), OAuthError> {
    let now = deps.clock.now_utc();
    let cutoff = now
        + chrono::Duration::from_std(OAUTH_REFRESH_SKEW)
            .expect("invariant: OAUTH_REFRESH_SKEW fits");
    let rows = crate::auth::run_privileged::<Vec<DueRow>, OAuthError>(&deps.pool, async |tx| {
        Ok(sqlx::query_as::<_, DueRow>(
            "SELECT server_id, org_id, kind, ciphertext, nonce, key_version \
             FROM mcp_server_credentials WHERE kind = 'oauth2' \
             LIMIT $1",
        )
        .bind(i64::try_from(MAX_OAUTH_REFRESH_PER_TICK).unwrap_or(i64::MAX))
        .fetch_all(&mut **tx)
        .await?)
    })
    .await?;

    let mut due = 0usize;
    for row in rows {
        let payload = match decode_oauth2(&deps.enc, &row) {
            Ok(p) => p,
            Err(e) => {
                warn!(
                    relay.mcp.server.id = %row.server_id,
                    error = %e,
                    "mcp.oauth.refresh.decode_failed",
                );
                continue;
            }
        };
        if payload.expires_at > cutoff {
            continue;
        }
        due += 1;
        if let Err(e) = refresh_one(deps, cache, row.server_id, row.org_id, &payload, now).await {
            warn!(
                relay.mcp.server.id = %row.server_id,
                error = %e,
                "mcp.oauth.refresh.failed",
            );
        }
    }
    tracing::debug!(due, "mcp.oauth.refresh.tick.done");
    Ok(())
}

async fn refresh_one(
    deps: &RefresherDeps,
    cache: &SharedOAuthTokenCache,
    server_id: McpServerId,
    org_id: crate::auth::OrgId,
    payload: &OAuth2Payload,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<(), OAuthError> {
    // Take the per-server mutex so a concurrent on-call refresh and the
    // background tick line up.
    let lock = cache.lock_for(server_id);
    let _g = lock.lock().await;

    // Re-read after taking the lock — another caller may have just
    // refreshed the same row while we waited.
    let Some(rec) = deps
        .credentials
        .read(server_id, org_id)
        .await
        .map_err(OAuthError::Mcp)?
    else {
        return Ok(());
    };
    let CredentialPayload::Oauth2(current) = rec.payload else {
        return Ok(());
    };
    let cutoff = now
        + chrono::Duration::from_std(OAUTH_REFRESH_SKEW)
            .expect("invariant: OAUTH_REFRESH_SKEW fits");
    if current.expires_at > cutoff {
        // Another caller refreshed under us; nothing to do.
        return Ok(());
    }
    let Some(refresh_token) = current.refresh_token.as_deref() else {
        // No refresh token — the AS issued an access token only.
        // Treat as `reconnect_required` so the UI prompts the user.
        return mark_reconnect_required(deps, server_id, org_id, "no refresh_token").await;
    };

    let dcr = super::resolve_oauth_client(&deps.oauth_clients, org_id, &payload.issuer)
        .await?
        .ok_or_else(|| {
            OAuthError::Misconfigured(format!(
                "no OAuth client for org {org_id:?} / issuer {}",
                payload.issuer
            ))
        })?;
    let outcome =
        refresh_oauth_token(&deps.flow, &dcr, refresh_token, &deps.redirect_uri, now).await?;
    match outcome {
        RefreshOutcome::Refreshed(t) => {
            // ASes that don't rotate the refresh token leave
            // `refresh_token = None`; carry the prior value forward.
            let new_refresh = t.refresh_token.or_else(|| current.refresh_token.clone());
            let new_payload = CredentialPayload::Oauth2(OAuth2Payload {
                access_token: t.access_token,
                refresh_token: new_refresh,
                expires_at: t.expires_at,
                scope: t.scope.or_else(|| current.scope.clone()),
                issuer: t.issuer,
                token_endpoint: t.token_endpoint,
            });
            deps.credentials
                .upsert(McpCredentialWrite {
                    server_id,
                    org_id,
                    payload: new_payload,
                })
                .await
                .map_err(OAuthError::Mcp)?;
            tracing::info!(
                relay.mcp.server.id = %server_id,
                relay.mcp.oauth.refresh.decision = "refreshed",
                "mcp.oauth.refresh.ok",
            );
            Ok(())
        }
        RefreshOutcome::Revoked => {
            cache.forget(server_id);
            mark_reconnect_required(deps, server_id, org_id, "refresh_token revoked").await
        }
    }
}

async fn mark_reconnect_required(
    deps: &RefresherDeps,
    server_id: McpServerId,
    org_id: crate::auth::OrgId,
    reason: &str,
) -> Result<(), OAuthError> {
    crate::auth::run_privileged::<(), OAuthError>(&deps.pool, async |tx| {
        sqlx::query(
            "UPDATE mcp_servers SET connection_status = 'reconnect_required', \
                                    last_error = $3, updated_at = $4 \
             WHERE id = $1 AND org_id = $2",
        )
        .bind(server_id)
        .bind(org_id)
        .bind(reason)
        .bind(deps.clock.now_utc())
        .execute(&mut **tx)
        .await?;
        Ok(())
    })
    .await?;
    tracing::warn!(
        relay.mcp.server.id = %server_id,
        relay.mcp.oauth.refresh.decision = "revoked",
        reason,
        "mcp.oauth.refresh.reconnect_required",
    );
    Ok(())
}

fn decode_oauth2(
    enc: &crate::crypto::SharedOrgEncryptor,
    row: &DueRow,
) -> Result<OAuth2Payload, OAuthError> {
    let nonce: [u8; crate::crypto::NONCE_BYTES] = row
        .nonce
        .as_slice()
        .try_into()
        .map_err(|_| OAuthError::Misconfigured("credential nonce wrong length".into()))?;
    let blob = crate::crypto::EncryptedBlob {
        key_version: row.key_version,
        nonce,
        ciphertext: row.ciphertext.clone(),
    };
    let payload = open_payload(enc, row.org_id, &row.kind, &blob).map_err(OAuthError::Mcp)?;
    match payload {
        CredentialPayload::Oauth2(p) => Ok(p),
        CredentialPayload::StaticHeaders { .. } => Err(OAuthError::Misconfigured(
            "expected oauth2 payload, got static_headers".into(),
        )),
    }
}

#[derive(sqlx::FromRow)]
struct DueRow {
    server_id: McpServerId,
    org_id: crate::auth::OrgId,
    kind: String,
    ciphertext: Vec<u8>,
    nonce: Vec<u8>,
    key_version: i16,
}
