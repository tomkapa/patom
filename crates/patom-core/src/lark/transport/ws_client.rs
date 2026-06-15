//! Production WS transport over `tokio-tungstenite` (rustls).
//!
//! Dials the negotiated `wss://…` URL and splits the stream into a [`LarkSink`]
//! (write half, behind a mutex so the ping task and ACK path share it) and a
//! [`WsReceiver`] (read half). Lark frames all ride as binary WS messages;
//! WS-level control frames (ping/pong/close) are handled by tungstenite and
//! skipped here.

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

use super::{LarkSink, LarkSource, SharedLarkSink};
use crate::lark::error::LarkError;
use crate::lark::pbbp2::Frame;

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Dial `url`, returning the write half (shared) and the read half.
pub async fn connect(url: &str) -> Result<(SharedLarkSink, WsReceiver), LarkError> {
    let (stream, _resp) = connect_async(url)
        .await
        .map_err(|e| LarkError::Handshake(format!("ws connect failed: {e}")))?;
    let (write, read) = stream.split();
    let sink: SharedLarkSink = Arc::new(WsSink {
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
impl LarkSink for WsSink {
    async fn send_frame(&self, frame: Frame) -> Result<(), LarkError> {
        let bytes = frame.encode_to_bytes();
        self.write
            .lock()
            .await
            .send(Message::Binary(bytes.into()))
            .await
            .map_err(|e| LarkError::Internal(format!("ws send: {e}")))
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
impl LarkSource for WsReceiver {
    async fn next_frame(&mut self) -> Option<Frame> {
        // Skip non-binary messages; a decode failure drops the one frame and
        // keeps the connection (the bounded loop is the stream itself).
        loop {
            match self.read.next().await {
                Some(Ok(Message::Binary(bytes))) => match Frame::decode_bytes(&bytes) {
                    Ok(frame) => return Some(frame),
                    Err(e) => {
                        warn!(error = %e, event = "lark.ws.frame_decode_failed");
                    }
                },
                Some(Ok(_)) => {}
                Some(Err(e)) => {
                    warn!(error = %e, event = "lark.ws.recv_error");
                    return None;
                }
                None => return None,
            }
        }
    }
}
