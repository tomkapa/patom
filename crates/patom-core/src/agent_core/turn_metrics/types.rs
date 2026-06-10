//! Domain types for `turn_metrics`.

use chrono::{DateTime, Utc};

use crate::agents::AgentId;
use crate::agents::prompt_versions::PromptVersionId;
use crate::auth::OrgId;
use crate::provider::{Model, ProviderId};
use crate::runtime::{PromptRequestId, RequestKind};
use crate::threads::AgentThreadId;
use crate::types::ParseError;

crate::uuid_newtype! {
    /// Opaque per-row primary key in `turn_metrics`. Generated fresh for each
    /// provider call — `request_id` is *not* unique (it is constant across a
    /// reply's turns, and across retries via `resume`), so it cannot key the
    /// table. One `TurnMetricsId` ↔ one provider call.
    pub TurnMetricsId
}

/// Validated token counter (input, output, or per-cache lane). All four
/// fields share the same shape — `>= 0`, fits a Postgres `INTEGER`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct InputTokens(i32);

impl InputTokens {
    #[must_use]
    pub const fn zero() -> Self {
        Self(0)
    }

    #[must_use]
    pub fn get(self) -> i32 {
        self.0
    }
}

impl TryFrom<u32> for InputTokens {
    type Error = ParseError;
    fn try_from(raw: u32) -> Result<Self, Self::Error> {
        let v = i32::try_from(raw).map_err(|_| ParseError::OutOfRange {
            field: "input_tokens",
            detail: "must fit in i32",
        })?;
        Ok(Self(v))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct OutputTokens(i32);

impl OutputTokens {
    #[must_use]
    pub const fn zero() -> Self {
        Self(0)
    }

    #[must_use]
    pub fn get(self) -> i32 {
        self.0
    }
}

impl TryFrom<u32> for OutputTokens {
    type Error = ParseError;
    fn try_from(raw: u32) -> Result<Self, Self::Error> {
        let v = i32::try_from(raw).map_err(|_| ParseError::OutOfRange {
            field: "output_tokens",
            detail: "must fit in i32",
        })?;
        Ok(Self(v))
    }
}

/// Wall-clock duration of one provider call, capped at the column's
/// `INTEGER` width. Saturates rather than wrapping so a pathological
/// duration cannot land negative.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct DurationMs(i32);

impl DurationMs {
    /// Saturation cap for `duration_ms`. Identical to
    /// [`crate::tools::MAX_TOOL_CALL_DURATION_MS`] in spirit; named here so
    /// the limits file is the only place that pins the upper bound.
    pub const SAT_CAP: i32 = i32::MAX;

    #[must_use]
    pub fn get(self) -> i32 {
        self.0
    }

    /// Lossless conversion from `u128` millis (from `Duration::as_millis`)
    /// with saturation at the column's `INTEGER` width. CLAUDE.md §7: no
    /// `as`-narrowing casts.
    #[must_use]
    pub fn saturating_from_millis(ms: u128) -> Self {
        // §6: i32::MAX always fits in u128; the conversion is infallible.
        let cap = u128::try_from(Self::SAT_CAP).expect("invariant: i32 fits u128");
        if ms >= cap {
            return Self(Self::SAT_CAP);
        }
        // `ms < cap_unsigned` guarantees `ms` fits in i32.
        Self(i32::try_from(ms).expect("invariant: ms < i32::MAX after bound check"))
    }
}

/// Short label for the provider's stop reason. Bounded at the type level so
/// a wide upstream string can't blow past the column's CHECK.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StopReasonLabel(String);

impl StopReasonLabel {
    /// Schema CHECK upper bound — kept in sync with the migration.
    pub const MAX_BYTES: usize = 64;

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Build a label from any free-form string; truncates rather than
    /// rejecting so an exotic provider value still records (CLAUDE.md §6
    /// keeps observability paths best-effort).
    #[must_use]
    pub fn from_truncated(raw: &str) -> Self {
        if raw.is_empty() {
            return Self("unknown".to_owned());
        }
        if raw.len() <= Self::MAX_BYTES {
            return Self(raw.to_owned());
        }
        let mut out = raw.to_owned();
        crate::tools::truncate_to_char_boundary(&mut out, Self::MAX_BYTES);
        Self(out)
    }
}

/// One row to write. Built by `agent_core::turn::record_turn_metrics` after a
/// successful provider call — one row per call.
#[derive(Debug, Clone)]
pub struct TurnMetricsRow {
    /// Per-row primary key, generated at record time (one per provider call).
    pub id: TurnMetricsId,
    /// `prompt_requests.id` for the reply this turn belongs to. Non-unique:
    /// a multi-turn reply (and each retry) shares one `request_id` across
    /// many rows.
    pub request_id: PromptRequestId,
    pub org_id: OrgId,
    /// FK to `agent_thread_state(id)` — the chat turn's participation id.
    /// Sourced from `ToolCallContext.state_id`; background turns skip recording.
    pub state_id: AgentThreadId,
    pub agent_id: AgentId,
    pub prompt_version_id: PromptVersionId,
    pub kind: RequestKind,
    /// Catalog model the worker resolved at turn-start. Recorded as the
    /// catalog handle (not the provider-echoed string) so every row is
    /// guaranteed-resolvable. `Model` already enforces `octet_length`-fit
    /// via the catalog name length cap — no separate newtype needed.
    pub model: Model,
    pub provider: ProviderId,
    pub input_tokens: InputTokens,
    pub output_tokens: OutputTokens,
    pub cache_creation_tokens: Option<InputTokens>,
    pub cache_read_tokens: Option<InputTokens>,
    pub duration_ms: DurationMs,
    pub stop_reason: StopReasonLabel,
    pub started_at: DateTime<Utc>,
}
