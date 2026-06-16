//! One Gateway connection's lifecycle — the verified protocol state machine.
//!
//! After the socket opens: read `HELLO` → send `IDENTIFY` (fresh) or `RESUME`
//! (replay a dropped session) → heartbeat every `heartbeat_interval` (only the
//! **first** beat is jittered) while tracking `HEARTBEAT_ACK` → dispatch events
//! to the bridge. The function is transport-agnostic ([`GatewaySink`] +
//! [`GatewaySource`]) so tests drive it with `FakeGateway` under paused time; the
//! multi-bot pool, advisory lock, and reconnect backoff live in `ws_manager`.
//!
//! [`run_connection`] returns a [`Directive`] (Stop / resume / fresh-reconnect /
//! Fatal) plus the latest [`SessionState`], so the manager decides what to do
//! next without this function knowing how sockets are made.

use std::time::Duration;

use rand::Rng;
use tokio::sync::mpsc;
use tokio::time::{Instant, MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use super::codec::{self, Opcode};
use super::error::DiscordError;
use super::limits::DISCORD_HANDSHAKE_TIMEOUT;
use super::transport::{GatewaySource, SharedGatewaySink, WsEvent};
use super::types::{ApplicationId, BotToken, CloseAction, DiscordUserId, FatalClose, Intents};

/// A dispatched Gateway event handed to the bridge (op 0, not `READY`/`RESUMED`).
///
/// Mirrors Lark's `InboundWork`: the raw event plus the connection context the
/// bridge needs (which bot, and the bot's own user id to drop self-messages).
#[derive(Debug, Clone)]
pub struct InboundDispatch {
    pub application_id: ApplicationId,
    pub bot_user_id: DiscordUserId,
    pub event_type: String,
    pub data: serde_json::Value,
}

/// The session identity needed to RESUME a dropped connection. Persisted by the
/// manager across reconnects; `None` means "IDENTIFY fresh".
#[derive(Debug, Clone)]
pub struct SessionState {
    pub session_id: String,
    pub resume_gateway_url: String,
    pub last_seq: u64,
    pub bot_user_id: DiscordUserId,
}

/// What the manager should do after a connection run ends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Directive {
    /// Clean shutdown (cancelled, or a 1000/1001 close) — do not reconnect.
    Stop,
    /// Reconnect to the resume URL and `RESUME` (the session is still valid).
    Resume,
    /// Reconnect and `IDENTIFY` fresh (the session is gone).
    FreshReconnect,
    /// Unrecoverable config/auth error — surface to the admin, stop the loop.
    Fatal(FatalClose),
}

/// The result of one connection run.
#[derive(Debug, Clone)]
pub struct RunResult {
    pub directive: Directive,
    /// The latest session (for a `Resume` directive); `None` if never READY.
    pub session: Option<SessionState>,
}

/// Immutable per-connection config.
#[derive(Debug, Clone)]
pub struct ConnConfig {
    pub application_id: ApplicationId,
    pub token: BotToken,
    pub intents: Intents,
    pub shard: Option<[u32; 2]>,
}

/// Mutable per-run state.
struct ConnState {
    session: Option<SessionState>,
    /// Whether the last heartbeat we sent has been ACKed. A due beat with this
    /// `false` means the connection zombied.
    acked: bool,
}

/// What to do after handling one inbound text frame.
enum FrameAction {
    Continue,
    /// Respond to a server `HEARTBEAT` request with an immediate beat.
    BeatNow,
    End(Directive),
}

/// Run one connection to completion over an already-open socket.
pub async fn run_connection(
    sink: SharedGatewaySink,
    source: &mut (dyn GatewaySource + Send),
    cfg: &ConnConfig,
    prior: Option<SessionState>,
    bridge_tx: &mpsc::Sender<InboundDispatch>,
    cancel: &CancellationToken,
) -> Result<RunResult, DiscordError> {
    let interval = read_hello(source, cancel).await?;
    let opening = match &prior {
        Some(s) => super::handshake::resume(&cfg.token, &s.session_id, s.last_seq)?,
        None => super::handshake::identify(&cfg.token, cfg.intents, cfg.shard)?,
    };
    sink.send_text(opening).await?;

    // Only the first beat is jittered (to avoid a thundering herd across bots).
    let jitter: f64 = rand::thread_rng().gen_range(0.0..1.0);
    let first = interval.mul_f64(jitter);
    let mut hb = tokio::time::interval_at(Instant::now() + first, interval);
    hb.set_missed_tick_behavior(MissedTickBehavior::Delay);

    let mut state = ConnState {
        session: prior,
        acked: true,
    };
    loop {
        tokio::select! {
            () = cancel.cancelled() => {
                return Ok(RunResult { directive: Directive::Stop, session: state.session });
            }
            _ = hb.tick() => {
                if let Some(result) = beat(&sink, &mut state).await? {
                    return Ok(result);
                }
            }
            ev = source.next_event() => {
                let Some(ev) = ev else {
                    // Stream ended with no close frame (transport drop) → resume.
                    return Ok(RunResult { directive: Directive::Resume, session: state.session });
                };
                match handle_event(ev, &mut state, cfg, bridge_tx) {
                    FrameAction::Continue => {}
                    FrameAction::BeatNow => send_beat(&sink, &mut state).await?,
                    FrameAction::End(directive) => {
                        return Ok(RunResult { directive, session: state.session });
                    }
                }
            }
        }
    }
}

/// Send a scheduled heartbeat, first checking the previous one was ACKed.
/// Returns `Some(RunResult)` to end the run (a zombied connection → resume).
async fn beat(
    sink: &SharedGatewaySink,
    state: &mut ConnState,
) -> Result<Option<RunResult>, DiscordError> {
    if !state.acked {
        // No ACK since the last beat → the connection zombied. Close non-1000
        // (keeps the session resumable) and resume.
        warn!(event = "discord.gateway.zombie_detected");
        let _ = sink.close().await;
        return Ok(Some(RunResult {
            directive: Directive::Resume,
            session: state.session.clone(),
        }));
    }
    send_beat(sink, state).await?;
    Ok(None)
}

/// Send a heartbeat carrying the last sequence and mark it un-ACKed.
async fn send_beat(sink: &SharedGatewaySink, state: &mut ConnState) -> Result<(), DiscordError> {
    let last_seq = state.session.as_ref().map(|s| s.last_seq);
    sink.send_text(super::handshake::heartbeat(last_seq)?)
        .await?;
    state.acked = false;
    Ok(())
}

/// Read frames until the opening `HELLO`, returning the heartbeat interval.
async fn read_hello(
    source: &mut (dyn GatewaySource + Send),
    cancel: &CancellationToken,
) -> Result<Duration, DiscordError> {
    let read = async {
        loop {
            match source.next_event().await {
                Some(WsEvent::Text(text)) => {
                    let recv = codec::decode(text.as_bytes())?;
                    if recv.op == Opcode::Hello {
                        let hello = codec::parse_hello(&recv)?;
                        return Ok(Duration::from_millis(hello.heartbeat_interval_ms));
                    }
                    // Anything before HELLO is a protocol violation we ignore.
                }
                Some(WsEvent::Close(code)) => {
                    return Err(DiscordError::Gateway(format!(
                        "closed during handshake: {code:?}"
                    )));
                }
                None => {
                    return Err(DiscordError::Gateway(
                        "stream ended before HELLO".to_owned(),
                    ));
                }
            }
        }
    };
    tokio::select! {
        () = cancel.cancelled() => Err(DiscordError::Gateway("cancelled before HELLO".to_owned())),
        r = tokio::time::timeout(DISCORD_HANDSHAKE_TIMEOUT, read) => {
            r.map_err(|_| DiscordError::Gateway("HELLO timed out".to_owned()))?
        }
    }
}

/// Handle one inbound WS event, mutating session state.
fn handle_event(
    ev: WsEvent,
    state: &mut ConnState,
    cfg: &ConnConfig,
    bridge_tx: &mpsc::Sender<InboundDispatch>,
) -> FrameAction {
    let text = match ev {
        WsEvent::Close(code) => return FrameAction::End(close_directive(code, state)),
        WsEvent::Text(text) => text,
    };
    let recv = match codec::decode(text.as_bytes()) {
        Ok(recv) => recv,
        Err(e) => {
            // Fail-open: a malformed frame drops, the connection stays.
            warn!(error = %e, event = "discord.gateway.frame_decode_failed");
            return FrameAction::Continue;
        }
    };
    match recv.op {
        Opcode::HeartbeatAck => {
            state.acked = true;
            FrameAction::Continue
        }
        Opcode::Heartbeat => FrameAction::BeatNow,
        Opcode::Reconnect => FrameAction::End(Directive::Resume),
        Opcode::InvalidSession => {
            let resumable =
                codec::parse_invalid_session_resumable(&recv) && state.session.is_some();
            FrameAction::End(if resumable {
                Directive::Resume
            } else {
                Directive::FreshReconnect
            })
        }
        Opcode::Dispatch => handle_dispatch(&recv, state, cfg, bridge_tx),
        _ => FrameAction::Continue,
    }
}

/// Map a close code to the directive, downgrading `Resume` to `FreshReconnect`
/// when there is no session to resume.
fn close_directive(code: Option<u16>, state: &ConnState) -> Directive {
    match super::types::classify_close(code) {
        CloseAction::Normal => Directive::Stop,
        CloseAction::Fatal(fc) => Directive::Fatal(fc),
        CloseAction::Reconnect if state.session.is_some() => Directive::Resume,
        CloseAction::Reconnect => Directive::FreshReconnect,
    }
}

/// Handle a dispatch (op 0): track the sequence, learn the session at `READY`,
/// and forward every other event to the bridge.
fn handle_dispatch(
    recv: &codec::GatewayRecv,
    state: &mut ConnState,
    cfg: &ConnConfig,
    bridge_tx: &mpsc::Sender<InboundDispatch>,
) -> FrameAction {
    if let (Some(seq), Some(session)) = (recv.seq, state.session.as_mut()) {
        session.last_seq = seq;
    }
    match recv.event_type.as_deref() {
        Some("READY") => {
            match codec::parse_ready(recv) {
                Ok(ready) => {
                    info!(event = "discord.gateway.ready", patom.discord.application.id = %cfg.application_id);
                    state.session = Some(SessionState {
                        session_id: ready.session_id,
                        resume_gateway_url: ready.resume_gateway_url,
                        last_seq: recv.seq.unwrap_or(0),
                        bot_user_id: ready.bot_user_id,
                    });
                }
                Err(e) => warn!(error = %e, event = "discord.gateway.ready_parse_failed"),
            }
            FrameAction::Continue
        }
        // `RESUMED` (replay finished) and a typeless dispatch are lifecycle-only.
        Some("RESUMED") | None => FrameAction::Continue,
        Some(event_type) => {
            // A dispatch before READY has no bot identity yet — drop it.
            if let Some(session) = state.session.as_ref() {
                forward_dispatch(event_type, recv, cfg, session, bridge_tx);
            }
            FrameAction::Continue
        }
    }
}

/// Forward a non-lifecycle dispatch to the bridge (try_send, fail-open). The
/// `application_id` is connection config, carried so the bridge resolves the
/// app → org without a round-trip.
fn forward_dispatch(
    event_type: &str,
    recv: &codec::GatewayRecv,
    cfg: &ConnConfig,
    session: &SessionState,
    bridge_tx: &mpsc::Sender<InboundDispatch>,
) {
    let Some(data) = recv.data.clone() else {
        return;
    };
    let work = InboundDispatch {
        application_id: cfg.application_id.clone(),
        bot_user_id: session.bot_user_id.clone(),
        event_type: event_type.to_owned(),
        data,
    };
    if bridge_tx.try_send(work).is_err() {
        warn!(event = "discord.gateway.bridge_queue_full");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discord::transport::FakeGateway;

    fn cfg() -> ConnConfig {
        ConnConfig {
            application_id: ApplicationId::try_from("123456789012345678").expect("app"),
            token: BotToken::try_from("MTk4N.example.token".to_owned()).expect("token"),
            intents: Intents::DEFAULT,
            shard: None,
        }
    }

    const HELLO: &str = r#"{"op":10,"d":{"heartbeat_interval":45000}}"#;
    const READY: &str = r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"sess1","resume_gateway_url":"wss://resume.discord.gg","user":{"id":"80351110224678912"}}}"#;

    #[tokio::test(start_paused = true)]
    async fn identifies_then_dispatches_to_bridge() {
        let gw = FakeGateway::new();
        gw.push_text(HELLO);
        gw.push_text(READY);
        gw.push_text(r#"{"op":0,"s":2,"t":"MESSAGE_CREATE","d":{"content":"hi"}}"#);
        gw.push_inbound(WsEvent::Close(Some(1000)));
        let mut src = gw.source();
        let (tx, mut rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();

        let result = run_connection(gw.clone(), &mut src, &cfg(), None, &tx, &cancel)
            .await
            .expect("run");

        assert_eq!(result.directive, Directive::Stop);
        // IDENTIFY (op 2) was sent.
        let sent = gw.sent();
        let identify: serde_json::Value = serde_json::from_str(&sent[0]).expect("json");
        assert_eq!(identify["op"], 2);
        // The MESSAGE_CREATE (not READY) reached the bridge with bot identity.
        let dispatched = rx.try_recv().expect("one dispatch");
        assert_eq!(dispatched.event_type, "MESSAGE_CREATE");
        assert_eq!(dispatched.bot_user_id.as_str(), "80351110224678912");
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn sends_heartbeat_after_interval() {
        let gw = FakeGateway::new();
        gw.push_text(HELLO);
        // No further frames: the source pends (like a real socket) so the
        // heartbeat timer can fire; we cancel to end the run deterministically.
        let mut src = gw.source();
        let (tx, _rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();
        let cancel_task = cancel.clone();
        let gw_task = gw.clone();
        let handle = tokio::spawn(async move {
            run_connection(gw_task, &mut src, &cfg(), None, &tx, &cancel_task).await
        });
        // Advance well past the (jittered ≤ interval) first beat, then stop.
        tokio::time::sleep(Duration::from_mins(1)).await;
        cancel.cancel();
        let _result = handle.await.expect("join").expect("run");
        // IDENTIFY (op 2) then at least one HEARTBEAT (op 1).
        let ops: Vec<u64> = gw
            .sent()
            .iter()
            .map(|s| {
                serde_json::from_str::<serde_json::Value>(s).expect("json")["op"]
                    .as_u64()
                    .expect("op")
            })
            .collect();
        assert!(ops.contains(&1), "expected a heartbeat op 1, got {ops:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn reconnect_opcode_yields_resume_after_ready() {
        let gw = FakeGateway::new();
        gw.push_text(HELLO);
        gw.push_text(READY);
        gw.push_text(r#"{"op":7,"d":null}"#); // server RECONNECT
        let mut src = gw.source();
        let (tx, _rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();
        let result = run_connection(gw.clone(), &mut src, &cfg(), None, &tx, &cancel)
            .await
            .expect("run");
        assert_eq!(result.directive, Directive::Resume);
        let session = result.session.expect("session learned at READY");
        assert_eq!(session.session_id, "sess1");
        assert_eq!(session.resume_gateway_url, "wss://resume.discord.gg");
    }

    #[tokio::test(start_paused = true)]
    async fn fatal_close_4014_surfaces() {
        let gw = FakeGateway::new();
        gw.push_text(HELLO);
        gw.push_inbound(WsEvent::Close(Some(4014)));
        let mut src = gw.source();
        let (tx, _rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();
        let result = run_connection(gw.clone(), &mut src, &cfg(), None, &tx, &cancel)
            .await
            .expect("run");
        assert_eq!(
            result.directive,
            Directive::Fatal(FatalClose::DisallowedIntents)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn invalid_session_non_resumable_is_fresh_reconnect() {
        let gw = FakeGateway::new();
        gw.push_text(HELLO);
        gw.push_text(r#"{"op":9,"d":false}"#);
        let mut src = gw.source();
        let (tx, _rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();
        let result = run_connection(gw.clone(), &mut src, &cfg(), None, &tx, &cancel)
            .await
            .expect("run");
        assert_eq!(result.directive, Directive::FreshReconnect);
    }
}
