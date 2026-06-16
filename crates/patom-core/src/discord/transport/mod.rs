//! The Gateway WS transport seam — the test boundary for the connection.
//!
//! A connection is two halves: a [`GatewaySink`] (write a JSON text command;
//! cheap-clone, shared by the heartbeat task and the connection loop) and a
//! [`GatewaySource`] (pull the next inbound [`WsEvent`], owned by the loop). The
//! production halves are [`ws_client`]; tests inject [`FakeGateway`] so they
//! never open a socket.
//!
//! Unlike Lark's binary-frame transport, this seam stays **un-decoded**: the
//! source yields raw text (the loop decodes via `codec`) and surfaces the close
//! code, because the close code drives the fatal-vs-reconnect classification
//! (`types::classify_close`) that a decoded-frame seam would hide.

pub mod ws_client;

use std::collections::VecDeque;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use tokio::sync::Notify;

use super::error::DiscordError;

/// An inbound WS event: a text data frame, or the socket closing with an
/// optional code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WsEvent {
    /// A JSON text frame (the loop decodes it via `codec::decode`).
    Text(String),
    /// The server closed the socket; the code (if any) classifies recovery.
    Close(Option<u16>),
}

/// Outbound half: write a JSON command, or close the socket (non-1000 → the
/// session stays resumable).
#[async_trait]
pub trait GatewaySink: fmt::Debug + Send + Sync {
    async fn send_text(&self, text: String) -> Result<(), DiscordError>;
    /// Close the socket without a clean (1000) code so the session remains
    /// resumable — used on zombie detection before a RESUME.
    async fn close(&self) -> Result<(), DiscordError>;
}

/// Shared handle to a [`GatewaySink`] (the heartbeat task and the loop both hold
/// one).
pub type SharedGatewaySink = Arc<dyn GatewaySink>;

/// Inbound half: pull the next event, or `None` once the stream ends with no
/// close frame (a transport drop). Owned exclusively by the connection loop.
#[async_trait]
pub trait GatewaySource: Send {
    async fn next_event(&mut self) -> Option<WsEvent>;
}

/// In-memory transport for tests: feed inbound events, capture sent text.
///
/// Behaves like a real socket: the source **pends** while the inbound queue is
/// empty (rather than ending) and only yields `None` after [`FakeGateway::end`].
/// Not `#[cfg(test)]` so integration tests in `tests/` can drive the connection
/// loop without a network. Use [`FakeGateway::source`] for the receive half.
#[derive(Debug, Default)]
pub struct FakeGateway {
    inbound: Mutex<VecDeque<WsEvent>>,
    sent: Mutex<Vec<String>>,
    closed: Mutex<bool>,
    ended: Mutex<bool>,
    wake: Notify,
}

impl FakeGateway {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Queue an event the loop will later yield.
    pub fn push_inbound(&self, event: WsEvent) {
        self.inbound
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push_back(event);
        self.wake.notify_one();
    }

    /// Convenience: queue a text frame.
    pub fn push_text(&self, text: impl Into<String>) {
        self.push_inbound(WsEvent::Text(text.into()));
    }

    /// Signal the stream has ended (a transport drop): the source yields `None`
    /// once the queue drains.
    pub fn end(&self) {
        *self.ended.lock().unwrap_or_else(PoisonError::into_inner) = true;
        self.wake.notify_one();
    }

    /// Every text command written to the sink so far (heartbeats, IDENTIFY, …).
    #[must_use]
    pub fn sent(&self) -> Vec<String> {
        self.sent
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Whether the sink's `close()` was called.
    #[must_use]
    pub fn was_closed(&self) -> bool {
        *self.closed.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A receive half backed by this transport's inbound queue.
    #[must_use]
    pub fn source(self: &Arc<Self>) -> FakeGatewaySource {
        FakeGatewaySource {
            transport: Arc::clone(self),
        }
    }
}

#[async_trait]
impl GatewaySink for FakeGateway {
    async fn send_text(&self, text: String) -> Result<(), DiscordError> {
        self.sent
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(text);
        Ok(())
    }

    async fn close(&self) -> Result<(), DiscordError> {
        *self.closed.lock().unwrap_or_else(PoisonError::into_inner) = true;
        Ok(())
    }
}

/// The receive half of a [`FakeGateway`]; yields queued events then `None`.
#[derive(Debug)]
pub struct FakeGatewaySource {
    transport: Arc<FakeGateway>,
}

#[async_trait]
impl GatewaySource for FakeGatewaySource {
    async fn next_event(&mut self) -> Option<WsEvent> {
        loop {
            {
                let mut q = self
                    .transport
                    .inbound
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                if let Some(event) = q.pop_front() {
                    return Some(event);
                }
                if *self
                    .transport
                    .ended
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                {
                    return None;
                }
            }
            // Empty and not ended → pend like a real socket until the next push.
            self.transport.wake.notified().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fake_transport_round_trips_events() {
        let t = FakeGateway::new();
        t.push_text(r#"{"op":10,"d":{"heartbeat_interval":1}}"#);
        t.push_inbound(WsEvent::Close(Some(4014)));
        let mut src = t.source();
        assert!(matches!(src.next_event().await, Some(WsEvent::Text(_))));
        assert_eq!(src.next_event().await, Some(WsEvent::Close(Some(4014))));
        // After end(), a drained source yields None (a transport drop).
        t.end();
        assert!(src.next_event().await.is_none());

        t.send_text("hello".to_owned()).await.expect("send");
        assert_eq!(t.sent(), vec!["hello".to_owned()]);
        assert!(!t.was_closed());
        t.close().await.expect("close");
        assert!(t.was_closed());
    }
}
