//! Provider-agnostic LLM interface.
//!
//! The `Agent` talks to providers exclusively through [`LlmProvider`] and the chat types
//! defined in [`chat`]. Adding a new backend (OpenAI, Ollama, local, mock) is a matter of
//! implementing the trait — `Agent::reply` does not change.

pub mod anthropic;
pub mod catalog;
mod chat;
mod credentials;
mod embedding;
mod error;
pub mod id;
pub mod limits;
pub mod openai;
mod overlay;
mod pg_credentials;
mod refresher;
mod registry;
mod traits;

pub use catalog::{CatalogEntry, ContextWindow, MODEL_CATALOG, Model, UnknownModel};
pub use chat::{
    AssistantContent, ChatMessage, ChatRequest, ChatResponse, Role, StopReason,
    TOOL_CALL_ID_MAX_BYTES, ToolCall, ToolCallId, ToolResult, ToolSpec, Usage, UserContent,
};
pub use credentials::{
    OrgProviderCredentialStore, ProviderApiKey, ProviderBaseUrl, ProviderCredentialError,
    ProviderCredentialRecord, ProviderCredentialWrite, SharedOrgProviderCredentialStore,
};
pub use embedding::{EmbeddingProvider, SharedEmbeddingProvider, embed_one};
pub use error::ProviderError;
pub use id::ProviderId;
pub use overlay::{OrgProviderOverlay, build_byo_client};
pub use pg_credentials::PgOrgProviderCredentialStore;
pub use refresher::{ProviderRefreshTrigger, ProviderRefresher};
pub use registry::{ProviderRegistry, ProviderRegistryBuilder, SharedProviderRegistry};
pub use traits::{LlmProvider, SharedProvider};
