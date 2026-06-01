//! Stable, low-cardinality labels recorded on agent.* spans so dashboards
//! can `GROUP BY patom.outcome` instead of grepping log strings.

use crate::types::{AgentReply, Participant};

use super::error::AgentError;

/// Label for `agent.reply` / `agent.resume` outcomes. Recorded as
/// `patom.outcome` on the enclosing span.
pub(super) fn record_reply(result: &Result<AgentReply, AgentError>) {
    let label = match result {
        Ok(_) => "complete",
        Err(e) => error_label(e),
    };
    tracing::Span::current().record("patom.outcome", label);
}

/// Label for one `agent.turn` outcome. Variants stay disjoint from
/// [`record_reply`] because a single turn cannot directly hit `max_turns`
/// (loop-level) but can land in any other error kind.
pub(super) fn record_turn(span: &tracing::Span, outcome: &Result<Option<String>, AgentError>) {
    let label = match outcome {
        Ok(Some(_)) => "text_only",
        Ok(None) => "tool_calls",
        Err(e) => error_label(e),
    };
    span.record("patom.turn.outcome", label);
}

fn error_label(err: &AgentError) -> &'static str {
    match err {
        AgentError::Cancelled => "cancelled",
        AgentError::ProviderTimeout => "provider_timeout",
        AgentError::Provider(_) => "provider_error",
        AgentError::ToolTimeout { .. } => "tool_timeout",
        AgentError::UnknownTool(_) => "tool_unknown",
        AgentError::TooManyToolCalls { .. } => "tool_calls_exceeded",
        AgentError::MaxTurnsExceeded(_) => "max_turns",
        AgentError::EmptyReply => "empty_reply",
        AgentError::BudgetExceeded { .. } => "budget_exceeded",
        AgentError::HookDenied(_) => "hook_denied",
        AgentError::Hook(_) => "hook_error",
        AgentError::Session(_) => "session_error",
        AgentError::Memory(_) => "memory_error",
        AgentError::Todos(_) => "todos_error",
        AgentError::TodosLoadTimeout => "todos_load_timeout",
    }
}

pub(super) fn viewer_kind(viewer: Participant) -> &'static str {
    match viewer {
        Participant::Human => "human",
        Participant::Agent { .. } => "agent",
        Participant::System => "system",
    }
}
