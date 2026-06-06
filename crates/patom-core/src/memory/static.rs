use std::sync::Arc;

use async_trait::async_trait;

use crate::runtime::RequestKindPayload;
use crate::session::SessionId;
use crate::types::Participant;

use super::traits::{Memory, MemoryError};

/// Constant system prompt; identical for every session.
#[derive(Debug, Clone)]
pub struct StaticMemory {
    prompt: Arc<str>,
}

impl StaticMemory {
    #[must_use]
    pub fn new(prompt: impl Into<Arc<str>>) -> Self {
        Self {
            prompt: prompt.into(),
        }
    }
}

#[async_trait]
impl Memory for StaticMemory {
    async fn system_prompt(
        &self,
        _session: SessionId,
        _viewer: Participant,
        _kind_payload: &RequestKindPayload,
    ) -> Result<Arc<str>, MemoryError> {
        Ok(self.prompt.clone())
    }
}
