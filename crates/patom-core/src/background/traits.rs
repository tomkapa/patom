//! Background-turn store trait + opaque id.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;

use crate::agents::AgentId;
use crate::auth::Caller;
use crate::colleagues::ColleagueId;
use crate::provider::ChatMessage;
use crate::runtime::PromptRequestId;

use super::error::BackgroundError;

crate::uuid_newtype! {
    /// Opaque background-turn id (`background_turns.id`).
    ///
    /// Doubles as the queue's `claim_key` for the cognition path — the
    /// `Background` arm of the claim key.
    pub BackgroundTurnId
}

/// A row to append to a background turn's private message log.
///
/// `sender = None` encodes the synthetic System sender (the injected reflection
/// prompt, tool results). The body is stored verbatim and replayed as the
/// turn's LLM context on a follow-up cognition step.
#[derive(Debug, Clone)]
pub struct NewBackgroundMessage {
    pub sender: Option<ColleagueId>,
    pub body: ChatMessage,
    pub request_id: Option<PromptRequestId>,
}

/// Storage for background-cognition turns (reflection / resolution). Kept apart
/// from [`crate::threads::ThreadStore`] so a cognition turn can never write a
/// chat-feed row.
#[async_trait]
pub trait BackgroundStore: fmt::Debug + Send + Sync {
    /// Open a fresh background turn for `agent`. Returns its id — the cognition
    /// `claim_key`.
    async fn create_turn(
        &self,
        caller: &Caller,
        agent: AgentId,
    ) -> Result<BackgroundTurnId, BackgroundError>;

    /// Append one message to the turn's private log, allocating the next
    /// per-turn `seq` atomically. Returns the assigned `seq`.
    async fn append(
        &self,
        caller: &Caller,
        turn: BackgroundTurnId,
        message: NewBackgroundMessage,
    ) -> Result<i64, BackgroundError>;

    /// The turn's message log in `seq` order — the agent's private cognition
    /// context. Already in the agent's own perspective (no viewer mapping: a
    /// background turn is single-agent).
    async fn context(
        &self,
        caller: &Caller,
        turn: BackgroundTurnId,
    ) -> Result<Vec<ChatMessage>, BackgroundError>;
}

/// Cheap-clone handle so consumers hold the store without a generic parameter.
pub type SharedBackgroundStore = Arc<dyn BackgroundStore>;
