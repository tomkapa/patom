//! Production Gateway WS transport over `tokio-tungstenite` (rustls).
//!
//! Dials the `wss://…/?v=10&encoding=json` URL and splits the stream into a
//! [`GatewaySink`] (write half, behind a mutex so the heartbeat task and the
//! loop share it) and a [`WsReceiver`] (read half). Discord frames ride as text
//! WS messages; WS-level ping/pong are answered by tungstenite, and a server
//! close is surfaced as [`WsEvent::Close`] so the loop can classify the code.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use tracing::warn;

use super::{GatewaySink, GatewaySource, SharedGatewaySink, WsEvent};
use crate::discord::error::DiscordError;
use crate::discord::limits::DISCORD_WS_CONNECT_TIMEOUT;

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Dial `url`, returning the write half (shared) and the read half.
pub async fn connect(url: &str) -> Result<(SharedGatewaySink, WsReceiver), DiscordError> {
    // Bound the dial (TCP + TLS + WS upgrade) so an unresponsive gateway fails
    // into the reconnect loop instead of hanging the connection task (§5).
    let (stream, _resp) = tokio::time::timeout(DISCORD_WS_CONNECT_TIMEOUT, connect_async(url))
        .await
        .map_err(|_| DiscordError::Gateway("ws connect timed out".to_owned()))?
        .map_err(|e| DiscordError::Gateway(format!("ws connect failed: {e}")))?;
    let (write, read) = stream.split();
    let sink: SharedGatewaySink = Arc::new(WsSink {
        write: Mutex::new(write),
    });
    Ok((sink, WsReceiver { read }))
}

struct WsSink {
    write: Mutex<SplitSink<WsStream, Message>>,
}

impl fmt::Debug for WsSink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WsSink").finish_non_exhaustive()
    }
}

#[async_trait]
impl GatewaySink for WsSink {
    async fn send_text(&self, text: String) -> Result<(), DiscordError> {
        self.write
            .lock()
            .await
            .send(Message::text(text))
            .await
            .map_err(|e| DiscordError::Gateway(format!("ws send: {e}")))
    }

    async fn close(&self) -> Result<(), DiscordError> {
        // A close with no code is a non-1000 close, so Discord keeps the session
        // resumable (used on zombie detection before a RESUME).
        self.write
            .lock()
            .await
            .send(Message::Close(None))
            .await
            .map_err(|e| DiscordError::Gateway(format!("ws close: {e}")))
    }
}

/// The read half of a live WS connection.
pub struct WsReceiver {
    read: SplitStream<WsStream>,
}

impl fmt::Debug for WsReceiver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WsReceiver").finish_non_exhaustive()
    }
}

#[async_trait]
impl GatewaySource for WsReceiver {
    async fn next_event(&mut self) -> Option<WsEvent> {
        // Skip ping/pong/binary; surface text frames and the close code. A recv
        // error ends the stream (None) → the loop reconnects.
        loop {
            match self.read.next().await {
                Some(Ok(Message::Text(text))) => return Some(WsEvent::Text(text.to_string())),
                Some(Ok(Message::Close(frame))) => {
                    return Some(WsEvent::Close(frame.map(|f| u16::from(f.code))));
                }
                Some(Ok(_)) => {}
                Some(Err(e)) => {
                    warn!(error = %e, event = "discord.ws.recv_error");
                    return None;
                }
                None => return None,
            }
        }
    }
}
