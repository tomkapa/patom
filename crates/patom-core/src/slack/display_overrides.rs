//! Slack implementation of [`ThreadDisplayNames`] (issue #41).
//!
//! When a thread is mirrored from a Slack channel, the agent should refer
//! to people by the Slack handle their teammates know — not the canonical
//! Patom name. This resolves, for a thread, each linked human's
//! `colleague_id → slack display name` in one query
//! (`threads → slack_channels → slack_identities → colleagues`). It
//! returns empty for any thread that isn't Slack-backed, so the renderer
//! falls back to canonical names.

use std::collections::HashMap;

use async_trait::async_trait;
use sqlx::PgPool;

use crate::auth::run_privileged;
use crate::colleagues::{ColleagueId, ColleagueName, ThreadDisplayNames};
use crate::threads::ThreadId;

use super::error::SlackError;

#[derive(Debug)]
pub struct PgSlackThreadDisplayNames {
    pool: PgPool,
}

impl PgSlackThreadDisplayNames {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl ThreadDisplayNames for PgSlackThreadDisplayNames {
    async fn overrides_for_thread(&self, thread: ThreadId) -> HashMap<ColleagueId, ColleagueName> {
        // Infallible by contract: a query failure logs and yields no
        // overrides (the renderer falls back to canonical names).
        let rows =
            run_privileged::<Vec<(ColleagueId, String)>, SlackError>(&self.pool, async move |tx| {
                Ok(sqlx::query_as(
                    "SELECT c.id, si.display_name \
                       FROM threads t \
                       JOIN slack_channels sc \
                         ON sc.channel_id = t.channel_id AND sc.org_id = t.org_id \
                       JOIN slack_identities si \
                         ON si.org_id = t.org_id AND si.team_id = sc.team_id \
                            AND si.display_name IS NOT NULL \
                       JOIN colleagues c \
                         ON c.user_id = si.user_id AND c.org_id = t.org_id \
                      WHERE t.id = $1",
                )
                .bind(thread)
                .fetch_all(&mut **tx)
                .await?)
            })
            .await
            .unwrap_or_else(|e| {
                tracing::error!(
                    error = ?e,
                    patom.thread.id = %thread.as_uuid(),
                    event = "slack.display_overrides.query_failed",
                );
                Vec::new()
            });
        // A stored name that no longer satisfies the colleague newtype is
        // dropped (renderer falls back) rather than failing the turn.
        rows.into_iter()
            .filter_map(|(id, name)| ColleagueName::try_from(name.as_str()).ok().map(|n| (id, n)))
            .collect()
    }
}
