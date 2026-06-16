//! Gateway connection manager — one connection per registered bot.
//!
//! At startup the manager lists every registered app and spawns one supervised
//! connection task per bot under a `JoinSet`. Each task holds a
//! `pg_try_advisory_lock` on its app id so only **one replica** owns the bot
//! (Discord's 1000-`IDENTIFY`/24h budget punishes duplicate connects), then runs
//! the reconnect driver: fetch `GET /gateway/bot`, dial, run the D3 connection
//! lifecycle, and act on its [`Directive`] (resume / fresh re-identify / stop /
//! fatal). A bot registered after startup connects immediately via
//! [`WsManagerHandle::connect`]; a deleted bot is torn down via `disconnect`.

use std::collections::{HashMap, HashSet};

use sqlx::PgPool;
use tokio::sync::{Mutex as AsyncMutex, mpsc};
use tokio::task::{AbortHandle, JoinSet};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use super::app_store::{DiscordConnectTarget, SharedBotTokenSource, SharedDiscordAppStore};
use super::connection::{self, ConnConfig, Directive, InboundDispatch, RunResult, SessionState};
use super::error::DiscordError;
use super::handshake;
use super::limits::{
    DISCORD_CONNECT_QUEUE, DISCORD_RECONNECT_BASE, DISCORD_RECONNECT_CAP, DISCORD_RECONNECT_MAX,
};
use super::transport::ws_client;
use super::types::{ApplicationId, Intents};

/// Dependencies for the gateway manager.
#[derive(Clone)]
pub struct WsManagerDeps {
    pub apps: SharedDiscordAppStore,
    pub tokens: SharedBotTokenSource,
    pub http: reqwest::Client,
    pub api_base: String,
    pub bridge_tx: mpsc::Sender<InboundDispatch>,
    /// For the single-owner advisory lock per bot.
    pub pool: PgPool,
}

impl std::fmt::Debug for WsManagerDeps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WsManagerDeps").finish_non_exhaustive()
    }
}

/// Handle for the spawned manager.
#[derive(Debug)]
pub struct WsManagerHandle {
    cancel: CancellationToken,
    join: AsyncMutex<Option<tokio::task::JoinHandle<()>>>,
    connect_tx: mpsc::Sender<DiscordConnectTarget>,
    disconnect_tx: mpsc::Sender<ApplicationId>,
}

impl WsManagerHandle {
    pub async fn shutdown(&self) {
        self.cancel.cancel();
        let handle = self.join.lock().await.take();
        if let Some(h) = handle {
            let _ = h.await;
        }
    }

    /// Open a connection for a just-registered bot, without a restart.
    pub async fn connect(&self, target: DiscordConnectTarget) {
        if self.connect_tx.send(target).await.is_err() {
            warn!(event = "discord.ws.connect_after_shutdown");
        }
    }

    /// Tear down a deleted bot's connection so it stops at once.
    pub async fn disconnect(&self, app_id: &ApplicationId) {
        if self.disconnect_tx.send(app_id.clone()).await.is_err() {
            warn!(event = "discord.ws.disconnect_after_shutdown");
        }
    }
}

pub type SharedWsManagerHandle = std::sync::Arc<WsManagerHandle>;

/// Spawn the manager supervisor.
#[must_use]
pub fn spawn(deps: WsManagerDeps, cancel: CancellationToken) -> SharedWsManagerHandle {
    let (connect_tx, connect_rx) = mpsc::channel::<DiscordConnectTarget>(DISCORD_CONNECT_QUEUE);
    let (disconnect_tx, disconnect_rx) = mpsc::channel::<ApplicationId>(DISCORD_CONNECT_QUEUE);
    let supervisor_cancel = cancel.clone();
    let join = tokio::spawn(supervisor(
        deps,
        supervisor_cancel,
        connect_rx,
        disconnect_rx,
    ));
    std::sync::Arc::new(WsManagerHandle {
        cancel,
        join: AsyncMutex::new(Some(join)),
        connect_tx,
        disconnect_tx,
    })
}

async fn supervisor(
    deps: WsManagerDeps,
    cancel: CancellationToken,
    mut connect_rx: mpsc::Receiver<DiscordConnectTarget>,
    mut disconnect_rx: mpsc::Receiver<ApplicationId>,
) {
    let mut set: JoinSet<()> = JoinSet::new();
    let mut spawned: HashSet<ApplicationId> = HashSet::new();
    let mut handles: HashMap<ApplicationId, AbortHandle> = HashMap::new();
    match deps.apps.list_connect_targets().await {
        Ok(targets) => {
            info!(count = targets.len(), event = "discord.ws.manager_start");
            for target in targets {
                spawn_connection(&mut set, &mut spawned, &mut handles, &deps, &cancel, target);
            }
        }
        Err(e) => warn!(error = ?e, event = "discord.ws.list_targets_failed"),
    }
    loop {
        tokio::select! {
            () = cancel.cancelled() => break,
            Some(target) = connect_rx.recv() => {
                spawn_connection(&mut set, &mut spawned, &mut handles, &deps, &cancel, target);
            }
            Some(app_id) = disconnect_rx.recv() => {
                disconnect(&mut spawned, &mut handles, &app_id);
            }
            Some(_) = set.join_next() => {}
        }
    }
    set.abort_all();
    while set.join_next().await.is_some() {}
}

/// Spawn one bot's supervised connection, unless already running.
fn spawn_connection(
    set: &mut JoinSet<()>,
    spawned: &mut HashSet<ApplicationId>,
    handles: &mut HashMap<ApplicationId, AbortHandle>,
    deps: &WsManagerDeps,
    cancel: &CancellationToken,
    target: DiscordConnectTarget,
) {
    if !spawned.insert(target.application_id.clone()) {
        return;
    }
    let app_id = target.application_id.clone();
    let d = deps.clone();
    let c = cancel.clone();
    let handle = set.spawn(async move { supervise_bot(d, target, c).await });
    handles.insert(app_id, handle);
}

/// Abort a bot's connection task and forget it (so a later re-register can
/// reconnect). A no-op for an app the supervisor isn't running.
fn disconnect(
    spawned: &mut HashSet<ApplicationId>,
    handles: &mut HashMap<ApplicationId, AbortHandle>,
    app_id: &ApplicationId,
) {
    spawned.remove(app_id);
    if let Some(handle) = handles.remove(app_id) {
        handle.abort();
        info!(app = %app_id, event = "discord.ws.disconnected");
    }
}

/// Own the bot (advisory lock) and run the reconnect driver for its lifetime.
async fn supervise_bot(
    deps: WsManagerDeps,
    target: DiscordConnectTarget,
    cancel: CancellationToken,
) {
    // Single owner per bot: a detached connection holds the advisory lock and
    // releases it (by closing) when this task ends.
    let _lock = match acquire_owner_lock(&deps.pool, &target.application_id).await {
        Ok(Some(conn)) => conn,
        Ok(None) => {
            info!(app = %target.application_id, event = "discord.ws.not_owner");
            return;
        }
        Err(e) => {
            warn!(error = ?e, app = %target.application_id, event = "discord.ws.lock_failed");
            return;
        }
    };

    let mut session: Option<SessionState> = None;
    let mut reconnects: u32 = 0;
    while !cancel.is_cancelled() && reconnects <= DISCORD_RECONNECT_MAX {
        match connect_and_run(&deps, &target, session.take(), &cancel).await {
            Ok(result) => {
                persist_bot_user_id(&deps, &target.application_id, &result).await;
                match result.directive {
                    Directive::Stop => return,
                    Directive::Resume => {
                        session = result.session;
                        reconnects = 0; // a clean resume is not a failure
                    }
                    Directive::FreshReconnect => {
                        reconnects = reconnects.saturating_add(1);
                        backoff_sleep(reconnects, &cancel).await;
                    }
                    Directive::Fatal(fc) => {
                        error!(app = %target.application_id, reason = %fc, event = "discord.ws.fatal");
                        return;
                    }
                }
            }
            Err(e) => {
                warn!(error = ?e, app = %target.application_id, event = "discord.ws.connection_error");
                // Keep the session for a resume attempt on a transient error.
                reconnects = reconnects.saturating_add(1);
                backoff_sleep(reconnects, &cancel).await;
            }
        }
    }
    warn!(app = %target.application_id, reconnects, event = "discord.ws.reconnect_exhausted");
}

/// Persist the bot's own user id learned at `READY` (opportunistic — the live
/// path already carries it on every dispatch).
async fn persist_bot_user_id(deps: &WsManagerDeps, app_id: &ApplicationId, result: &RunResult) {
    if let Some(session) = &result.session
        && let Err(e) = deps
            .apps
            .set_bot_user_id(app_id, &session.bot_user_id)
            .await
    {
        warn!(error = ?e, event = "discord.ws.set_bot_user_id_failed");
    }
}

/// One connection cycle: token → `GET /gateway/bot` → dial → run the lifecycle.
async fn connect_and_run(
    deps: &WsManagerDeps,
    target: &DiscordConnectTarget,
    prior: Option<SessionState>,
    cancel: &CancellationToken,
) -> Result<RunResult, DiscordError> {
    let token = deps.tokens.token(&target.application_id).await?;
    let info = handshake::get_gateway_bot(&deps.http, &deps.api_base, &token).await?;
    // Resume against the session's resume URL; otherwise the fresh gateway URL.
    let base = prior
        .as_ref()
        .map_or(info.url, |s| s.resume_gateway_url.clone());
    let (sink, mut receiver) = ws_client::connect(&handshake::connect_url(&base)).await?;
    info!(app = %target.application_id, event = "discord.ws.connected");
    let cfg = ConnConfig {
        application_id: target.application_id.clone(),
        token,
        intents: Intents::DEFAULT,
        shard: None,
    };
    connection::run_connection(sink, &mut receiver, &cfg, prior, &deps.bridge_tx, cancel).await
}

/// Exponential backoff (base × 2^n, capped), cancel-aware.
async fn backoff_sleep(reconnects: u32, cancel: &CancellationToken) {
    let factor = 1u32 << reconnects.min(6);
    let wait = (DISCORD_RECONNECT_BASE * factor).min(DISCORD_RECONNECT_CAP);
    tokio::select! {
        () = cancel.cancelled() => {}
        () = tokio::time::sleep(wait) => {}
    }
}

/// Acquire a session-level advisory lock for the bot. Returns the detached
/// connection that holds it (the lock releases when the connection is dropped),
/// or `None` if another replica already owns it.
async fn acquire_owner_lock(
    pool: &PgPool,
    app_id: &ApplicationId,
) -> Result<Option<sqlx::PgConnection>, DiscordError> {
    let key = advisory_key(app_id);
    let mut conn = pool.acquire().await?;
    let (acquired,): (bool,) = sqlx::query_as("SELECT pg_try_advisory_lock($1)")
        .bind(key)
        .fetch_one(&mut *conn)
        .await?;
    Ok(acquired.then_some(conn.detach()))
}

/// Stable `bigint` key for `pg_advisory_lock`, derived from the app id. Stable
/// within a deployment (the same binary on every replica).
fn advisory_key(app_id: &ApplicationId) -> i64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    app_id.as_str().hash(&mut hasher);
    i64::from_ne_bytes(hasher.finish().to_ne_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advisory_key_is_stable_and_distinct() {
        let a = ApplicationId::try_from("111111111111111111").expect("a");
        let b = ApplicationId::try_from("222222222222222222").expect("b");
        assert_eq!(
            advisory_key(&a),
            advisory_key(&a),
            "stable for the same app"
        );
        assert_ne!(advisory_key(&a), advisory_key(&b), "distinct per app");
    }
}
