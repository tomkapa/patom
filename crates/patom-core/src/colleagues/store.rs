//! The colleagues directory store interface.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;

use crate::agents::AgentId;
use crate::auth::{OrgId, UserId};

use super::error::ColleagueError;
use super::types::{Colleague, ColleagueId, ColleagueRef};

/// Read access to the per-org colleagues directory.
///
/// Minting is handled by database triggers on `agents` / `org_members` INSERT
/// (see the colleagues migration), so this trait is read-only — callers resolve
/// a satellite to its colleague or list the roster; they never insert.
#[async_trait]
pub trait ColleagueStore: fmt::Debug + Send + Sync {
    /// Roster for an org — humans and agents, alpha-sorted by display name.
    ///
    /// System is excluded by construction (it is never a row). Display names
    /// resolve live from the backing `agents` / `users` rows, so a rename is
    /// reflected on the next read.
    async fn list_for_org(&self, org_id: OrgId) -> Result<Vec<ColleagueRef>, ColleagueError>;

    /// The fully-resolved colleague for `id`, or [`ColleagueError::NotFound`].
    async fn read(&self, id: ColleagueId) -> Result<Colleague, ColleagueError>;

    /// The colleague id backing `agent_id` within `org_id`.
    ///
    /// Errors with [`ColleagueError::SatelliteUnmapped`] if the mint trigger
    /// never ran for that agent (a directory-integrity bug).
    async fn resolve_agent(
        &self,
        org_id: OrgId,
        agent_id: AgentId,
    ) -> Result<ColleagueId, ColleagueError>;

    /// The colleague id backing `user_id` within `org_id`.
    async fn resolve_user(
        &self,
        org_id: OrgId,
        user_id: UserId,
    ) -> Result<ColleagueId, ColleagueError>;
}

/// Cheap-to-clone shared handle threaded through the runtime.
pub type SharedColleagueStore = Arc<dyn ColleagueStore>;
