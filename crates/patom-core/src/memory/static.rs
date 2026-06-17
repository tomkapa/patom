use std::sync::Arc;

use async_trait::async_trait;

use crate::runtime::RequestKindPayload;
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
    async fn system_prompt_for_thread(
        &self,
        _viewer: Participant,
        _overrides: &std::collections::HashMap<
            crate::colleagues::ColleagueId,
            crate::colleagues::ColleagueName,
        >,
        _kind_payload: &RequestKindPayload,
    ) -> Result<Arc<str>, MemoryError> {
        Ok(self.prompt.clone())
    }

    async fn display_overrides(
        &self,
        _thread: Option<crate::threads::ThreadId>,
    ) -> std::collections::HashMap<crate::colleagues::ColleagueId, crate::colleagues::ColleagueName>
    {
        std::collections::HashMap::new()
    }

    async fn participants_block(
        &self,
        _participants: &crate::threads::ThreadParticipants,
        _viewer: crate::types::Participant,
        _overrides: &std::collections::HashMap<
            crate::colleagues::ColleagueId,
            crate::colleagues::ColleagueName,
        >,
    ) -> String {
        // The static prompt carries no participant context.
        String::new()
    }

    async fn agent_persona(&self, _agent: crate::agents::AgentId) -> Option<Arc<str>> {
        // Every session shares one prompt — it is the persona lens for all agents.
        Some(self.prompt.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::AgentId;

    #[tokio::test]
    async fn agent_persona_returns_the_static_prompt() {
        let memory = StaticMemory::new("you are a terse assistant");
        let persona = memory.agent_persona(AgentId::new()).await;
        assert_eq!(persona.as_deref(), Some("you are a terse assistant"));
    }
}
