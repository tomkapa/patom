use thiserror::Error;

use crate::auth::OrgId;
use crate::hook::{HookDenied, HookError};
use crate::memory::MemoryError;
use crate::provider::ProviderError;
use crate::session::SessionError;
use crate::threads::ThreadError;
use crate::tools::system::todos::TodoStoreError;

#[derive(Debug, Error)]
pub enum AgentError {
    #[error("provider: {0}")]
    Provider(#[from] ProviderError),

    #[error("session: {0}")]
    Session(#[from] SessionError),

    #[error("thread: {0}")]
    Thread(#[from] ThreadError),

    #[error("memory: {0}")]
    Memory(#[from] MemoryError),

    #[error("todos: {0}")]
    Todos(#[from] TodoStoreError),

    #[error("todos pre-turn read timed out")]
    TodosLoadTimeout,

    #[error("hook: {0}")]
    Hook(#[from] HookError),

    #[error(transparent)]
    HookDenied(#[from] HookDenied),

    #[error("provider call timed out")]
    ProviderTimeout,

    #[error("tool `{name}` timed out")]
    ToolTimeout { name: String },

    #[error("model issued an unknown tool: {0}")]
    UnknownTool(String),

    #[error("model issued more than {max} tool calls in a single turn")]
    TooManyToolCalls { max: usize },

    #[error("max turns ({0}) exceeded without final reply")]
    MaxTurnsExceeded(u32),

    #[error("provider returned no usable content")]
    EmptyReply,

    #[error("org {org} has exhausted its monthly spend budget")]
    BudgetExceeded { org: OrgId },

    #[error("agent cancelled")]
    Cancelled,
}
