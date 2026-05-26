//! Domain types for `agent_prompt_versions`.
//!
//! Versions ONLY the `system_prompt`. The model is orthogonal — it lives
//! on `agents.model`, mutated in place when the operator picks a
//! different backend. The `turn_metrics` table still snapshots
//! `(prompt_version_id, model)` per call so analytics can pivot on either
//! dimension independently.

use chrono::{DateTime, Utc};

use crate::agents::{AgentId, AgentSystemPrompt};
use crate::auth::{OrgId, UserId};
use crate::types::ParseError;
// `NewPromptVersion` is intentionally absent: the writer (`bump_prompt_version`
// in `PgAgentStore`) builds its INSERT inline now that the store-trait
// indirection is gone. Reintroduce only when a second caller appears.

crate::uuid_newtype! {
    /// Opaque row id in `agent_prompt_versions`. Minted server-side on every
    /// bump; never carried in from the wire.
    pub PromptVersionId
}

/// Monotonic per-agent version counter, starting at 1. Wrapped so the
/// invariant (`> 0`, fits the column's `INTEGER` width) is enforced at
/// every boundary — schema CHECK is defence in depth.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, sqlx::Type,
)]
#[serde(transparent)]
#[sqlx(transparent)]
pub struct PromptVersionNumber(i32);

impl PromptVersionNumber {
    /// First version a newly seeded agent owns.
    pub const FIRST: Self = Self(1);

    #[must_use]
    pub fn get(self) -> i32 {
        self.0
    }

    /// Next version after `self`. Saturates at `i32::MAX` rather than
    /// wrapping (CLAUDE.md §7); a runaway bumper would still fail the
    /// UNIQUE (agent_id, version) before re-using a value, but saturating
    /// keeps the type total.
    #[must_use]
    pub fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

impl TryFrom<i32> for PromptVersionNumber {
    type Error = ParseError;

    fn try_from(raw: i32) -> Result<Self, Self::Error> {
        if raw < 1 {
            return Err(ParseError::OutOfRange {
                field: "prompt_version",
                detail: "version must be >= 1",
            });
        }
        Ok(Self(raw))
    }
}

/// Hydrated row.
#[derive(Debug, Clone)]
pub struct PromptVersionRow {
    pub id: PromptVersionId,
    pub agent_id: AgentId,
    pub org_id: OrgId,
    pub version: PromptVersionNumber,
    pub system_prompt: AgentSystemPrompt,
    /// `None` = system seed (no user principal in hand at insert time).
    pub edited_by: Option<UserId>,
    pub created_at: DateTime<Utc>,
}
