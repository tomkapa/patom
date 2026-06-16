//! The [`ProfileStore`] boundary — the org-shared profile board's read/write
//! surface. One error type (`ProfileError`, §12); every method exhaustively
//! handled by callers.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;

use crate::auth::OrgId;
use crate::colleagues::ColleagueId;

use super::error::ProfileError;
use super::types::{ColleagueMatch, ColleagueProfile};

/// Read/write access to `colleague_profiles`.
///
/// Writes are org-scoped against a *trusted* `org` (derived from the authed
/// actor), not against anything the caller supplies in the profile — the
/// subject-in-org check is the one place that rule cannot be sidestepped.
#[async_trait]
pub trait ProfileStore: std::fmt::Debug + Send + Sync {
    /// Upsert `profile` onto the board within `org`. The subject
    /// (`profile.colleague_id()`) must be a colleague in `org`, else
    /// [`ProfileError::SubjectNotInOrg`]. Re-embeds the composed `profile_text`
    /// unless it is byte-identical to the stored one and an embedding already
    /// exists (then the stored vector is kept).
    async fn upsert(&self, org: OrgId, profile: &ColleagueProfile) -> Result<(), ProfileError>;

    /// Batch-fetch profiles for the given colleague ids (capped at
    /// [`super::limits::MAX_PROFILE_FETCH`]). Missing ids are simply absent from
    /// the map — a colleague without a profile is not an error.
    async fn get_many(
        &self,
        ids: &[ColleagueId],
    ) -> Result<HashMap<ColleagueId, ColleagueProfile>, ProfileError>;

    /// Unified semantic search across agents (`agents.description_embedding`)
    /// and profiled humans (`colleague_profiles.embedding`), org-scoped to the
    /// `viewer`'s tenant and excluding the viewer, ranked by cosine distance.
    async fn search_colleagues(
        &self,
        embedding: &[f32],
        viewer: ColleagueId,
        k: usize,
    ) -> Result<Vec<ColleagueMatch>, ProfileError>;
}

/// Shared handle threaded through the runtime, mirroring the other store seams.
pub type SharedProfileStore = Arc<dyn ProfileStore>;
