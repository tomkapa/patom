//! Anthropic backend for [`LlmProvider`](crate::provider::LlmProvider).
//!
//! All knowledge of `claudius` types is confined to this module — the rest of the codebase
//! deals only with the provider-agnostic chat types. Adding a new provider means writing
//! another module like this one; nothing in `agent` changes.

mod client;
mod convert;

pub use client::AnthropicProvider;
