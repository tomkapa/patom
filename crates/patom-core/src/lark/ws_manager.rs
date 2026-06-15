//! WS connection manager — one long-connection per registered bot.
//!
//! At startup the manager lists every registered app and spawns one connection
//! task per bot under a `JoinSet`. Each task runs the lifecycle: handshake →
//! resolve the bot's own `open_id` → dial → ping loop + receive loop →
//! reconnect (bounded by [`LARK_RECONNECT_MAX`]). On a data frame it reassembles
//! fragments, ACKs immediately (`{"code":200}`) — keeping inside Lark's ~3 s
//! deadline — then hands the parsed event to the bridge off the WS task.
//!
//! A bot registered after startup connects immediately via [`WsManagerHandle::connect`]
//! (the admin route calls it) — no restart. A multi-replica deployment must run a single owner per bot (Lark
//! delivers each event to one random client); a `pg_try_advisory_lock` on the
//! app id is the intended guard and is deferred with the rest of the
//! multi-replica work.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex as AsyncMutex, mpsc};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use super::app_store::{LarkConnectTarget, SharedLarkAppStore};
use super::bridge::InboundWork;
use super::codec::Reassembler;
use super::error::LarkError;
use super::event;
use super::handshake;
use super::limits::{
    LARK_CONNECT_QUEUE, LARK_DEFAULT_PING_INTERVAL, LARK_DEFAULT_RECONNECT_INTERVAL,
    LARK_RECONNECT_MAX,
};
use super::pbbp2::{ACK_OK, Frame, METHOD_DATA, TYPE_EVENT};
use super::roster;
use super::token::{AppSecretSource, SharedTokenProvider};
use super::transport::{LarkSource as _, SharedLarkSink, ws_client};
use super::types::{LarkAppId, LarkOpenId};

/// Dependencies for the WS manager.
#[derive(Clone)]
pub struct WsManagerDeps {
    pub apps: SharedLarkAppStore,
    pub secret_source: Arc<dyn AppSecretSource>,
    pub token_provider: SharedTokenProvider,
    pub http: reqwest::Client,
    pub api_base: String,
    pub bridge_tx: mpsc::Sender<InboundWork>,
}

impl std::fmt::Debug for WsManagerDeps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WsManagerDeps").finish_non_exhaustive()
    }
}

/// Handle for the spawned manager; `shutdown` cancels every connection, and
/// `connect` hot-adds a newly-registered bot without a restart.
#[derive(Debug)]
pub struct WsManagerHandle {
    cancel: CancellationToken,
    join: AsyncMutex<Option<tokio::task::JoinHandle<()>>>,
    connect_tx: mpsc::Sender<LarkConnectTarget>,
}

impl WsManagerHandle {
    pub async fn shutdown(&self) {
        self.cancel.cancel();
        let handle = self.join.lock().await.take();
        if let Some(h) = handle {
            let _ = h.await;
        }
    }

    /// Open a long-connection for a just-registered bot, without a restart.
    /// Idempotent: the supervisor ignores a target it is already running.
    pub async fn connect(&self, target: LarkConnectTarget) {
        if self.connect_tx.send(target).await.is_err() {
            warn!(event = "lark.ws.connect_after_shutdown");
        }
    }
}

/// Shared handle to the WS manager.
pub type SharedWsManagerHandle = Arc<WsManagerHandle>;

/// Spawn the manager supervisor.
#[must_use]
pub fn spawn(deps: WsManagerDeps, cancel: CancellationToken) -> SharedWsManagerHandle {
    let (connect_tx, connect_rx) = mpsc::channel::<LarkConnectTarget>(LARK_CONNECT_QUEUE);
    let supervisor_cancel = cancel.clone();
    let join = tokio::spawn(supervisor(deps, supervisor_cancel, connect_rx));
    Arc::new(WsManagerHandle {
        cancel,
        join: AsyncMutex::new(Some(join)),
        connect_tx,
    })
}

async fn supervisor(
    deps: WsManagerDeps,
    cancel: CancellationToken,
    mut connect_rx: mpsc::Receiver<LarkConnectTarget>,
) {
    let mut set: JoinSet<()> = JoinSet::new();
    // App ids we've already spawned a connection task for — dedup hot-add and
    // the startup sweep so a re-register never opens a second socket.
    let mut spawned: std::collections::HashSet<LarkAppId> = std::collections::HashSet::new();
    match deps.apps.list_connect_targets().await {
        Ok(targets) => {
            info!(count = targets.len(), event = "lark.ws.manager_start");
            for target in targets {
                spawn_connection(&mut set, &mut spawned, &deps, &cancel, target);
            }
        }
        Err(e) => warn!(error = ?e, event = "lark.ws.list_targets_failed"),
    }
    loop {
        tokio::select! {
            () = cancel.cancelled() => break,
            // A newly-registered bot to connect (hot-add). On a closed channel
            // recv() yields None → the `Some` pattern disables this branch.
            Some(target) = connect_rx.recv() => {
                spawn_connection(&mut set, &mut spawned, &deps, &cancel, target);
            }
            // Reap a finished connection task (reconnect exhausted); disabled
            // when the set is empty so the supervisor still parks on `cancel`.
            Some(_) = set.join_next() => {}
        }
    }
    set.abort_all();
    while set.join_next().await.is_some() {}
}

/// Spawn one bot's connection task under `set`, unless it's already running.
fn spawn_connection(
    set: &mut JoinSet<()>,
    spawned: &mut std::collections::HashSet<LarkAppId>,
    deps: &WsManagerDeps,
    cancel: &CancellationToken,
    target: LarkConnectTarget,
) {
    if !spawned.insert(target.app_id.clone()) {
        return;
    }
    let d = deps.clone();
    let c = cancel.clone();
    set.spawn(async move { run_connection(d, target, c).await });
}

/// Bounded reconnect loop for one bot.
async fn run_connection(deps: WsManagerDeps, target: LarkConnectTarget, cancel: CancellationToken) {
    for _ in 0..=LARK_RECONNECT_MAX {
        if cancel.is_cancelled() {
            return;
        }
        if let Err(e) = connect_once(&deps, &target, &cancel).await {
            warn!(error = ?e, app = %target.app_id, event = "lark.ws.connection_error");
        }
        if cancel.is_cancelled() {
            return;
        }
        tokio::time::sleep(LARK_DEFAULT_RECONNECT_INTERVAL).await;
    }
    warn!(app = %target.app_id, event = "lark.ws.reconnect_exhausted");
}

/// One connection lifecycle: handshake → dial → ping + receive until close.
async fn connect_once(
    deps: &WsManagerDeps,
    target: &LarkConnectTarget,
    cancel: &CancellationToken,
) -> Result<(), LarkError> {
    let secret = deps.secret_source.secret(&target.app_id).await?;
    let endpoint =
        handshake::negotiate(&deps.http, &deps.api_base, &target.app_id, &secret).await?;
    // The bot's own open_id, so the bridge can tell a bot-mention from chatter.
    let bot_open_id = match deps.token_provider.token(&target.app_id).await {
        Ok(token) => roster::fetch_bot_open_id(&deps.http, &deps.api_base, &token)
            .await
            .ok(),
        Err(e) => {
            warn!(error = ?e, event = "lark.ws.bot_open_id_token_failed");
            None
        }
    };
    let (sink, mut receiver) = ws_client::connect(&endpoint.url).await?;
    info!(app = %target.app_id, service_id = endpoint.service_id, event = "lark.ws.connected");
    let ping_interval = secs_or_default(endpoint.config.ping_interval, LARK_DEFAULT_PING_INTERVAL);
    let ping_task = tokio::spawn(ping_loop(
        sink.clone(),
        endpoint.service_id,
        ping_interval,
        cancel.clone(),
    ));
    let mut reassembler = Reassembler::new();
    loop {
        tokio::select! {
            () = cancel.cancelled() => break,
            frame = receiver.next_frame() => {
                let Some(frame) = frame else { break; };
                if frame.method == METHOD_DATA
                    && let Err(e) = handle_data_frame(frame, &sink, &mut reassembler, &deps.bridge_tx, bot_open_id.as_ref()).await
                {
                    warn!(error = ?e, event = "lark.ws.handle_frame_failed");
                }
            }
        }
    }
    ping_task.abort();
    Ok(())
}

/// Reassemble a data frame; on completion ACK it and dispatch the event.
async fn handle_data_frame(
    frame: Frame,
    sink: &SharedLarkSink,
    reassembler: &mut Reassembler,
    bridge_tx: &mpsc::Sender<InboundWork>,
    bot_open_id: Option<&LarkOpenId>,
) -> Result<(), LarkError> {
    let msg_type = frame.msg_type().map(str::to_owned);
    let ack = frame.clone();
    let Some(payload) = reassembler.accept(frame)? else {
        return Ok(()); // fragment buffered; ACK once the message is complete
    };
    // ACK first (inside the ~3 s deadline), then dispatch async.
    if let Err(e) = sink.send_frame(ack.into_ack(ACK_OK, 0)).await {
        warn!(error = ?e, event = "lark.ws.ack_failed");
    }
    if msg_type.as_deref() == Some(TYPE_EVENT) {
        dispatch_event(&payload, bridge_tx, bot_open_id);
    }
    Ok(())
}

/// Parse an event payload and hand it to the bridge (non-blocking).
fn dispatch_event(
    payload: &[u8],
    bridge_tx: &mpsc::Sender<InboundWork>,
    bot_open_id: Option<&LarkOpenId>,
) {
    match event::parse_event(payload) {
        Ok(ev) => {
            let work = InboundWork {
                event: ev,
                bot_open_id: bot_open_id.cloned(),
            };
            if bridge_tx.try_send(work).is_err() {
                warn!(event = "lark.ws.bridge_queue_full");
            }
        }
        Err(e) => warn!(error = ?e, event = "lark.ws.event_parse_failed"),
    }
}

async fn ping_loop(
    sink: SharedLarkSink,
    service_id: i32,
    interval: Duration,
    cancel: CancellationToken,
) {
    let mut tick = tokio::time::interval(interval);
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            _ = tick.tick() => {
                if let Err(e) = sink.send_frame(Frame::ping(service_id)).await {
                    warn!(error = ?e, event = "lark.ws.ping_failed");
                    return;
                }
            }
        }
    }
}

/// `Duration::from_secs(secs)` for a positive `secs`, else `default`.
fn secs_or_default(secs: i32, default: Duration) -> Duration {
    u64::try_from(secs)
        .ok()
        .filter(|s| *s > 0)
        .map_or(default, Duration::from_secs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lark::pbbp2::{HEADER_MESSAGE_ID, HEADER_TYPE, Header};
    use crate::lark::transport::FakeLarkTransport;

    fn event_frame() -> Frame {
        let body = br#"{"header":{"event_id":"e","event_type":"im.message.receive_v1","tenant_key":"tk","app_id":"cli"},
            "event":{"sender":{"sender_id":{"open_id":"ou_a","user_id":"u_a"},"sender_type":"user"},
            "message":{"message_id":"om","chat_id":"oc","chat_type":"p2p","message_type":"text","content":"{\"text\":\"hi\"}"}}}"#;
        Frame {
            method: METHOD_DATA,
            headers: vec![
                Header::new(HEADER_TYPE, TYPE_EVENT),
                Header::new(HEADER_MESSAGE_ID, "om"),
            ],
            payload: body.to_vec(),
            ..Frame::default()
        }
    }

    #[tokio::test]
    async fn data_frame_is_acked_and_dispatched() {
        let fake = FakeLarkTransport::new();
        let sink: SharedLarkSink = fake.clone();
        let mut reassembler = Reassembler::new();
        let (tx, mut rx) = mpsc::channel::<InboundWork>(4);
        handle_data_frame(event_frame(), &sink, &mut reassembler, &tx, None)
            .await
            .expect("ok");
        // The frame was ACKed (exactly one frame written to the sink)...
        assert_eq!(fake.sent().len(), 1, "one ACK frame written");
        assert_eq!(
            fake.sent()[0].header_int(super::super::pbbp2::HEADER_BIZ_RT),
            Some(0)
        );
        // ...and the event dispatched to the bridge.
        let work = rx.try_recv().expect("dispatched");
        assert!(matches!(work.event, event::LarkEvent::Message(_)));
    }

    #[test]
    fn secs_or_default_handles_nonpositive() {
        let fallback = Duration::from_secs(7);
        assert_eq!(secs_or_default(125, fallback), Duration::from_secs(125));
        assert_eq!(secs_or_default(0, fallback), fallback);
        assert_eq!(secs_or_default(-5, fallback), fallback);
    }
}
