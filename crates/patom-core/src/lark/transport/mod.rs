//! The WS transport seam — the test boundary for the long-connection.
//!
//! A connection is two halves: a [`LarkSink`] (write a frame; cheap-clone, used
//! by the ping task and the ACK path) and a [`LarkSource`] (pull the next
//! decoded frame, owned by the receive loop). The production halves are
//! [`ws_client`]; tests inject [`FakeLarkTransport`] so they never open a
//! socket.

pub mod ws_client;

use std::collections::VecDeque;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;

use super::error::LarkError;
use super::pbbp2::Frame;

/// Outbound half: write a frame to the connection.
#[async_trait]
pub trait LarkSink: fmt::Debug + Send + Sync {
    async fn send_frame(&self, frame: Frame) -> Result<(), LarkError>;
}

/// Shared handle to a [`LarkSink`] (the ping task and the receive loop both
/// hold one).
pub type SharedLarkSink = Arc<dyn LarkSink>;

/// Inbound half: pull the next decoded frame, or `None` once the connection
/// closes. Owned exclusively by the receive loop.
#[async_trait]
pub trait LarkSource: Send {
    async fn next_frame(&mut self) -> Option<Frame>;
}

/// In-memory transport for tests: feed inbound frames, capture sent frames.
///
/// Not `#[cfg(test)]` so integration tests in `tests/` can drive the manager's
/// frame handling without a network. Use [`FakeLarkTransport::source`] for the
/// receive half.
#[derive(Debug, Default)]
pub struct FakeLarkTransport {
    inbound: Mutex<VecDeque<Frame>>,
    sent: Mutex<Vec<Frame>>,
}

impl FakeLarkTransport {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Queue a frame the receive loop will later yield.
    pub fn push_inbound(&self, frame: Frame) {
        self.inbound
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push_back(frame);
    }

    /// Every frame written to the sink so far (e.g. ACKs, pings).
    #[must_use]
    pub fn sent(&self) -> Vec<Frame> {
        self.sent
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// A receive half backed by this transport's inbound queue.
    #[must_use]
    pub fn source(self: &Arc<Self>) -> FakeLarkSource {
        FakeLarkSource {
            transport: Arc::clone(self),
        }
    }
}

#[async_trait]
impl LarkSink for FakeLarkTransport {
    async fn send_frame(&self, frame: Frame) -> Result<(), LarkError> {
        self.sent
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(frame);
        Ok(())
    }
}

/// The receive half of a [`FakeLarkTransport`]; yields queued frames then
/// `None`.
#[derive(Debug)]
pub struct FakeLarkSource {
    transport: Arc<FakeLarkTransport>,
}

#[async_trait]
impl LarkSource for FakeLarkSource {
    async fn next_frame(&mut self) -> Option<Frame> {
        self.transport
            .inbound
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lark::pbbp2::{ACK_OK, Frame};

    #[tokio::test]
    async fn fake_transport_round_trips_frames() {
        let t = FakeLarkTransport::new();
        t.push_inbound(Frame {
            payload: b"a".to_vec(),
            ..Frame::default()
        });
        let mut src = t.source();
        let got = src.next_frame().await.expect("one frame");
        assert_eq!(got.payload, b"a".to_vec());
        assert!(src.next_frame().await.is_none());

        let ack = Frame::default().into_ack(ACK_OK, 0);
        t.send_frame(ack).await.expect("send");
        assert_eq!(t.sent().len(), 1);
    }
}
