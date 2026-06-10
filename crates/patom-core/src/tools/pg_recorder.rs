//! Postgres-backed [`ToolCallStore`].
//!
//! The recorder runs on the worker side after every tool dispatch. Inserts
//! use a privileged tx (no caller principal — workers serve many tenants);
//! the `tool_calls_enforce_org` trigger checks every row's `org_id` against
//! the parent session as defence in depth, and the partial indexes on
//! `mcp_server_id` keep the two dashboard reads cheap.

use std::fmt;

use async_trait::async_trait;
use sqlx::PgPool;

use crate::auth::run_privileged;
use crate::clock::SharedClock;

use super::limits::{
    MAX_TOOL_CALL_DURATION_MS, MAX_TOOL_CALL_ERROR_MESSAGE_BYTES, truncate_to_char_boundary,
};
use super::recorder::{ToolCallRow, ToolCallStore, ToolCallStoreError};

/// Postgres-backed [`ToolCallStore`]. Carries the pool + clock by value
/// (both are cheap clones) so the recorder is itself cheap to share via
/// `Arc`.
pub struct PgToolCallStore {
    pool: PgPool,
    // Reserved for future `record`-side timestamps (e.g. `created_at` set
    // server-side rather than threaded through the row). Today every row
    // value comes from the dispatcher's clock — the recorder only carries
    // the handle so call sites can construct one with the canonical
    // (pool, clock) pair (CLAUDE.md §11) and the structure stays uniform
    // with `PgSessionStore`.
    #[allow(dead_code)]
    clock: SharedClock,
}

impl PgToolCallStore {
    #[must_use]
    pub fn new(pool: PgPool, clock: SharedClock) -> Self {
        Self { pool, clock }
    }
}

impl fmt::Debug for PgToolCallStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgToolCallStore").finish_non_exhaustive()
    }
}

#[async_trait]
impl ToolCallStore for PgToolCallStore {
    #[tracing::instrument(
        skip_all,
        name = "tool_calls.record",
        fields(
            patom.state.id = %row.state_id,
            patom.agent.id = %row.agent_id,
            patom.tool = %row.tool_name,
            patom.mcp.server.id = row.mcp_server_id.map(tracing::field::display),
            patom.tool.is_error = row.is_error,
            patom.tool.duration_ms = tracing::field::Empty,
        ),
    )]
    async fn record(&self, row: ToolCallRow) -> Result<(), ToolCallStoreError> {
        // CLAUDE.md §6: assert the invariant the migration-27 CHECK enforces
        // so a caller bug surfaces before the round trip, not after.
        assert!(
            row.is_error || row.error_message.is_none(),
            "tool_calls invariant: error_message set on a successful row"
        );
        let duration_ms = saturating_duration_ms(row.duration.as_millis())?;
        tracing::Span::current().record("patom.tool.duration_ms", duration_ms);

        let created_at = self.clock.now_utc();
        let error_message = row.error_message.as_deref();

        // Privileged tx: the worker serves many tenants and has no single
        // principal; the trigger `tool_calls_enforce_org` still verifies
        // the inserted `org_id` matches the parent session's. Bound
        // parameters per CLAUDE.md §10 — no interpolation.
        run_privileged::<(), ToolCallStoreError>(&self.pool, async |tx| {
            sqlx::query(
                "INSERT INTO tool_calls
                     (id, org_id, state_id, request_id, agent_id,
                      mcp_server_id, tool_name,
                      started_at, duration_ms, is_error, error_message,
                      created_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
            )
            .bind(row.id)
            .bind(row.org_id)
            .bind(row.state_id)
            .bind(row.request_id)
            .bind(row.agent_id)
            .bind(row.mcp_server_id)
            .bind(row.tool_name.as_str())
            .bind(row.started_at)
            .bind(duration_ms)
            .bind(row.is_error)
            .bind(error_message)
            .bind(created_at)
            .execute(&mut **tx.tx_mut())
            .await?;
            Ok(())
        })
        .await
    }
}

/// Clip `s` in place to [`MAX_TOOL_CALL_ERROR_MESSAGE_BYTES`] on a UTF-8
/// boundary, warning on saturation so a recurring offender shows up in
/// the operator log.
#[must_use]
pub fn clip_error_message(mut s: String) -> String {
    if s.len() <= MAX_TOOL_CALL_ERROR_MESSAGE_BYTES {
        return s;
    }
    let original_bytes = s.len();
    truncate_to_char_boundary(&mut s, MAX_TOOL_CALL_ERROR_MESSAGE_BYTES);
    tracing::warn!(
        original_bytes,
        cap_bytes = MAX_TOOL_CALL_ERROR_MESSAGE_BYTES,
        "tool_calls.error_message.truncated",
    );
    s
}

/// Clip a `Duration::as_millis()` (`u128`) down to the schema's `i32`.
///
/// Saturating rather than narrowing-cast (CLAUDE.md §7 bans `as`) so a
/// pathological tool duration cannot wrap to a small or negative value.
/// The agent's `tool_timeout` runs well below `i32::MAX` (~24 days), so
/// saturation is rare-but-possible (clock skew, paused tokio runtime).
fn saturating_duration_ms(ms: u128) -> Result<i32, ToolCallStoreError> {
    let cap_unsigned = u128::try_from(MAX_TOOL_CALL_DURATION_MS).map_err(|_| {
        ToolCallStoreError::DurationOverflow {
            ms,
            cap: MAX_TOOL_CALL_DURATION_MS,
        }
    })?;
    if ms > cap_unsigned {
        tracing::warn!(
            duration_ms = %ms,
            cap_ms = MAX_TOOL_CALL_DURATION_MS,
            "tool_calls.duration.saturated",
        );
        return Ok(MAX_TOOL_CALL_DURATION_MS);
    }
    // `ms <= cap_unsigned <= i32::MAX as u128`, so the conversion is total.
    i32::try_from(ms).map_err(|_| ToolCallStoreError::DurationOverflow {
        ms,
        cap: MAX_TOOL_CALL_DURATION_MS,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saturates_above_cap() {
        let v = saturating_duration_ms(u128::from(u64::MAX)).expect("ok");
        assert_eq!(v, MAX_TOOL_CALL_DURATION_MS);
    }

    #[test]
    fn passes_through_below_cap() {
        assert_eq!(saturating_duration_ms(42).expect("ok"), 42);
    }

    #[test]
    fn zero_is_zero() {
        assert_eq!(saturating_duration_ms(0).expect("ok"), 0);
    }
}
