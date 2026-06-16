//! `profile_write` — record a colleague's durable role / expertise /
//! preferences on the org-SHARED profile board (issue #183).
//!
//! Deliberately distinct from `memory_write`: that mints the *agent's own
//! private* note about a colleague (`agent_memories`, kind=collaborator); this
//! writes the *shared* board the whole org reads in `<participants>` and
//! `search_colleague` ranks. The subject-in-org check lives in the store
//! (`ProfileStore::upsert`), the one place it cannot be sidestepped.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::debug;

use crate::colleagues::{
    ColleagueId, ColleagueProfile, Expertise, MAX_EXPERTISE, MAX_PREFERENCES, MAX_ROLE,
    Preferences, ProfileError, Role, SharedProfileStore,
};
use crate::tools::{Tool, ToolCallContext, ToolError};
use crate::types::ToolName;

const TOOL_NAME: &str = "profile_write";

const TOOL_DESCRIPTION: &str = "Record a colleague's durable role, expertise, or working \
    preferences on the org-SHARED profile board, so every agent in your org can find them with \
    `search_colleague` and see them in your `<participants>` block.\n\
    \n\
    USE for facts about who a colleague IS in the org: their job (\"Product Manager\"), what \
    they are expert in, or how they like to work (\"async-first; call me Pa\"). For your OWN \
    private notes about working with someone, use `memory_write(kind=\"collaborator\")` instead \
    — that stays with you; this board is shared.\n\
    \n\
    Arguments: `subject` (the colleague's id from your `<colleagues>` / `<participants>` block) \
    and at least one of `role`, `expertise`, `preferences`. Writing again for the same subject \
    updates their board entry.";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    /// Colleague the profile is about — must be a colleague in your org.
    subject: ColleagueId,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    expertise: Option<String>,
    #[serde(default)]
    preferences: Option<String>,
}

#[derive(Debug, Serialize)]
struct Output {
    subject: ColleagueId,
    note: &'static str,
}

pub struct ProfileWriteTool {
    name: ToolName,
    description: &'static str,
    input_schema: Arc<Value>,
    profiles: SharedProfileStore,
}

impl std::fmt::Debug for ProfileWriteTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProfileWriteTool").finish_non_exhaustive()
    }
}

impl ProfileWriteTool {
    #[must_use]
    pub fn new(profiles: SharedProfileStore) -> Self {
        let name = ToolName::try_from(TOOL_NAME).expect("invariant: valid tool name");
        let input_schema = Arc::new(json!({
            "type": "object",
            "required": ["subject"],
            "properties": {
                "subject": { "type": "string", "format": "uuid", "description": "Colleague id (from <colleagues>/<participants>) this profile is about" },
                "role": { "type": "string", "minLength": 1, "maxLength": MAX_ROLE },
                "expertise": { "type": "string", "minLength": 1, "maxLength": MAX_EXPERTISE },
                "preferences": { "type": "string", "minLength": 1, "maxLength": MAX_PREFERENCES },
            },
            "additionalProperties": false,
        }));
        Self {
            name,
            description: TOOL_DESCRIPTION,
            input_schema,
            profiles,
        }
    }
}

#[async_trait]
impl Tool for ProfileWriteTool {
    fn name(&self) -> &ToolName {
        &self.name
    }
    fn description(&self) -> &str {
        self.description
    }
    fn input_schema(&self) -> Arc<Value> {
        self.input_schema.clone()
    }

    async fn execute(&self, input: Value, ctx: &ToolCallContext) -> Result<String, ToolError> {
        let parsed: Input = serde_json::from_value(input)?;
        // Only agents run tools; reject anything else so provenance is sound.
        if ctx.viewer.agent_id().is_none() {
            return Err(ToolError::InvalidInput(
                "profile_write must be called by an agent".into(),
            ));
        }

        // Validate each present field through its newtype bound (§1).
        let to_invalid = |e: crate::types::ParseError| ToolError::InvalidInput(e.to_string());
        let role = parsed
            .role
            .map(Role::try_from)
            .transpose()
            .map_err(to_invalid)?;
        let expertise = parsed
            .expertise
            .map(Expertise::try_from)
            .transpose()
            .map_err(to_invalid)?;
        let preferences = parsed
            .preferences
            .map(Preferences::try_from)
            .transpose()
            .map_err(to_invalid)?;
        if role.is_none() && expertise.is_none() && preferences.is_none() {
            return Err(ToolError::InvalidInput(
                "profile_write needs at least one of role, expertise, or preferences".into(),
            ));
        }

        // Provenance: the writing agent's colleague id. Org scope comes from the
        // trusted `ctx.org_id`, and the subject-in-org check happens in the store.
        let profile = ColleagueProfile::new(
            parsed.subject,
            role,
            expertise,
            preferences,
            ctx.viewer.colleague_id(),
        );
        self.profiles
            .upsert(ctx.org_id, &profile)
            .await
            .map_err(profile_err_to_tool)?;

        debug!(
            patom.claim_key = %ctx.claim_key,
            patom.colleague.subject = %parsed.subject,
            "profile_write.ok",
        );

        let out = Output {
            subject: parsed.subject,
            note: "Profile written to the shared board. The org can now find this colleague \
                   via search_colleague.",
        };
        Ok(serde_json::to_string(&out)?)
    }
}

/// Map a store failure onto the tool surface: a bad subject is the model's
/// fault (`invalid_input`, self-correctable); embedding / DB faults are not.
fn profile_err_to_tool(e: ProfileError) -> ToolError {
    match e {
        ProfileError::SubjectNotInOrg { .. } | ProfileError::Parse(_) => {
            ToolError::InvalidInput(e.to_string())
        }
        ProfileError::NotFound(_) | ProfileError::Embed(_) | ProfileError::Db(_) => {
            ToolError::Backend(e.to_string())
        }
    }
}
