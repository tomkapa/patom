//! Provider-agnostic LLM interface.
//!
//! The `Agent` talks to providers exclusively through [`LlmProvider`] and the chat types
//! defined in [`chat`]. Adding a new backend (OpenAI, Ollama, local, mock) is a matter of
//! implementing the trait — `Agent::reply` does not change.

pub mod anthropic;
pub mod catalog;
mod chat;
mod embedding;
mod error;
pub mod id;
pub mod openai;
mod registry;
mod traits;

pub use catalog::{CatalogEntry, MODEL_CATALOG, Model, UnknownModel};
pub use chat::{
    AssistantContent, ChatMessage, ChatRequest, ChatResponse, Role, StopReason,
    TOOL_CALL_ID_MAX_BYTES, ToolCall, ToolCallId, ToolResult, ToolSpec, Usage, UserContent,
};
pub use embedding::{EmbeddingProvider, SharedEmbeddingProvider, embed_one};
pub use error::ProviderError;
pub use id::ProviderId;
pub use registry::{ProviderRegistry, ProviderRegistryBuilder, SharedProviderRegistry};
pub use traits::{LlmProvider, SharedProvider};
