//! Domain newtypes and enums for the approval subsystem.
//!
//! Every value carrying an invariant is wrapped and crosses into the typed world
//! exactly once via `TryFrom` (CLAUDE.md §1); the inner fields are private.

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::agents::AgentId;
use crate::auth::OrgId;
use crate::colleagues::ColleagueId;
use crate::runtime::PromptRequestId;
use crate::threads::ThreadId;
use crate::types::{ParseError, ToolName};

use super::limits::{APPROVAL_SUMMARY_MAX, MAX_APPROVERS};

crate::uuid_newtype! {
    /// Opaque identifier for a `pending_approval` row.
    pub ApprovalId
}

impl TryFrom<&str> for ApprovalId {
    type Error = ParseError;
    fn try_from(raw: &str) -> Result<Self, Self::Error> {
        Uuid::parse_str(raw)
            .map(Self::from)
            .map_err(|_| ParseError::Malformed {
                field: "approval_id",
                detail: "expected uuid",
            })
    }
}

/// Human-readable description of the gated action, shown to the approver.
/// `1..=APPROVAL_SUMMARY_MAX` bytes, matching the column `CHECK`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionSummary(String);

impl ActionSummary {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ActionSummary {
    type Error = ParseError;
    fn try_from(raw: String) -> Result<Self, Self::Error> {
        if raw.is_empty() {
            return Err(ParseError::Empty {
                field: "action_summary",
            });
        }
        let got = raw.len();
        if got > APPROVAL_SUMMARY_MAX {
            return Err(ParseError::TooLong {
                field: "action_summary",
                max: APPROVAL_SUMMARY_MAX,
                got,
            });
        }
        Ok(Self(raw))
    }
}

impl TryFrom<&str> for ActionSummary {
    type Error = ParseError;
    fn try_from(raw: &str) -> Result<Self, Self::Error> {
        Self::try_from(raw.to_owned())
    }
}

/// A platform-issued message id recorded after the interactive prompt is posted.
///
/// Discord snowflake / Lark `message_id`; the resolve path edits this message.
/// Kept as an opaque bounded string — the shape differs per platform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformMessageId(String);

/// Upper bound on a stored platform message id. Discord snowflakes are ≤20
/// chars; Lark `om_*` ids are short. 128 is generous headroom with a real cap.
const PLATFORM_MESSAGE_ID_MAX: usize = 128;

impl PlatformMessageId {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for PlatformMessageId {
    type Error = ParseError;
    fn try_from(raw: String) -> Result<Self, Self::Error> {
        if raw.is_empty() {
            return Err(ParseError::Empty {
                field: "platform_message_id",
            });
        }
        let got = raw.len();
        if got > PLATFORM_MESSAGE_ID_MAX {
            return Err(ParseError::TooLong {
                field: "platform_message_id",
                max: PLATFORM_MESSAGE_ID_MAX,
                got,
            });
        }
        Ok(Self(raw))
    }
}

crate::str_enum! {
    /// Lifecycle state of a `pending_approval` row.
    pub enum ApprovalStatus {
        Pending  => "pending",
        Approved => "approved",
        Denied   => "denied",
        Expired  => "expired",
    }
}

crate::str_enum! {
    /// The chat surface an approval prompt was posted to.
    pub enum Platform {
        Discord => "discord",
        Lark    => "lark",
        Web     => "web",
    }
}

crate::str_enum! {
    /// Stored discriminant of the [`ApproverPolicy`]. `Anyone`/`OneOf` pin no
    /// single colleague; `Colleague` pins exactly one (the `approver_colleague`
    /// column).
    pub enum ApproverKind {
        Anyone    => "anyone",
        Colleague => "colleague",
        OneOf     => "one_of",
    }
}

/// Who may decide an approval.
///
/// Keyed by [`ColleagueId`], which transparently backs a real *or* shadow user,
/// so an agent can whitelist an approver who has never logged into Patom (see
/// the shadow-identity model).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApproverPolicy {
    /// Any colleague in the org. The callback is org-bound, so "anyone" still
    /// means "anyone in this tenant", never the public.
    Anyone,
    /// Exactly one named colleague.
    Colleague(ColleagueId),
    /// One of an explicit set (`1..=MAX_APPROVERS`).
    OneOf(Vec<ColleagueId>),
}

impl ApproverPolicy {
    /// Build the policy from the tool's optional `approvers` list: omitted /
    /// empty ⇒ [`Anyone`](Self::Anyone); a non-empty list ⇒ [`OneOf`](Self::OneOf)
    /// after de-duplicating and enforcing the cap.
    pub fn from_ids(ids: Vec<ColleagueId>) -> Result<Self, ParseError> {
        if ids.is_empty() {
            return Ok(Self::Anyone);
        }
        // De-duplicate in one pass, preserving the order the agent listed them.
        let mut seen = std::collections::HashSet::with_capacity(ids.len());
        let deduped: Vec<ColleagueId> = ids.into_iter().filter(|id| seen.insert(*id)).collect();
        if deduped.len() > MAX_APPROVERS {
            return Err(ParseError::OutOfRange {
                field: "approvers",
                detail: "too many approvers",
            });
        }
        Ok(Self::OneOf(deduped))
    }

    #[must_use]
    pub fn kind(&self) -> ApproverKind {
        match self {
            Self::Anyone => ApproverKind::Anyone,
            Self::Colleague(_) => ApproverKind::Colleague,
            Self::OneOf(_) => ApproverKind::OneOf,
        }
    }

    /// The single pinned colleague for the `Colleague` variant — the value of
    /// the `approver_colleague` column. `None` for `Anyone`/`OneOf`.
    #[must_use]
    pub fn pinned(&self) -> Option<ColleagueId> {
        match self {
            Self::Colleague(c) => Some(*c),
            Self::Anyone | Self::OneOf(_) => None,
        }
    }

    /// The explicit whitelist for the `OneOf` variant (the child-table rows).
    /// Empty for the other variants.
    #[must_use]
    pub fn members(&self) -> &[ColleagueId] {
        match self {
            Self::OneOf(set) => set,
            Self::Anyone | Self::Colleague(_) => &[],
        }
    }
}

/// Server-side authorization: may `clicker` decide an approval with this policy?
///
/// `Anyone` ⇒ any org colleague (the callback already proved org membership);
/// `Colleague` ⇒ exact match; `OneOf` ⇒ membership in the set. The `OneOf` set
/// is read from the child table by the store and passed in here.
#[must_use]
pub fn policy_allows(
    kind: ApproverKind,
    pinned: Option<ColleagueId>,
    one_of: &[ColleagueId],
    clicker: ColleagueId,
) -> bool {
    match kind {
        ApproverKind::Anyone => true,
        ApproverKind::Colleague => pinned == Some(clicker),
        ApproverKind::OneOf => one_of.contains(&clicker),
    }
}

/// The decision a human made on a click. Maps to the resolved
/// [`ApprovalStatus`]; the compact wire form (`a`/`d`) rides Discord's ≤100-char
/// `custom_id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Approved,
    Denied,
}

impl Decision {
    #[must_use]
    pub fn status(self) -> ApprovalStatus {
        match self {
            Self::Approved => ApprovalStatus::Approved,
            Self::Denied => ApprovalStatus::Denied,
        }
    }

    /// One-char wire tag used in the Discord `custom_id` (`apv:{id}:a|d`) and the
    /// Lark callback `value`.
    #[must_use]
    pub fn tag(self) -> char {
        match self {
            Self::Approved => 'a',
            Self::Denied => 'd',
        }
    }

    /// Inverse of [`Self::tag`].
    #[must_use]
    pub fn from_tag(tag: &str) -> Option<Self> {
        match tag {
            "a" => Some(Self::Approved),
            "d" => Some(Self::Denied),
            _ => None,
        }
    }
}

/// Where an approval prompt was delivered, decomposed for the platform columns.
///
/// `Web` carries no external binding (in-thread / web-UI approval); the platform
/// posters supply `Discord`/`Lark` with the container needed to post and edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlatformTarget {
    Web,
    Discord {
        application_id: String,
        container_id: String,
        reply_to: Option<String>,
    },
    Lark {
        app_id: String,
        chat_id: String,
        reply_to: Option<String>,
    },
}

impl PlatformTarget {
    #[must_use]
    pub fn platform(&self) -> Platform {
        match self {
            Self::Web => Platform::Web,
            Self::Discord { .. } => Platform::Discord,
            Self::Lark { .. } => Platform::Lark,
        }
    }

    /// `(platform_app_id, platform_container, platform_reply_to)` for the row.
    #[must_use]
    pub fn columns(&self) -> (Option<&str>, Option<&str>, Option<&str>) {
        match self {
            Self::Web => (None, None, None),
            Self::Discord {
                application_id,
                container_id,
                reply_to,
            } => (
                Some(application_id.as_str()),
                Some(container_id.as_str()),
                reply_to.as_deref(),
            ),
            Self::Lark {
                app_id,
                chat_id,
                reply_to,
            } => (
                Some(app_id.as_str()),
                Some(chat_id.as_str()),
                reply_to.as_deref(),
            ),
        }
    }
}

/// The full read model of a `pending_approval` row, parsed at the store boundary
/// (CLAUDE.md §1) so nothing downstream sees a raw row.
#[derive(Debug, Clone)]
pub struct ApprovalRecord {
    pub id: ApprovalId,
    pub org_id: OrgId,
    pub thread_id: ThreadId,
    pub requesting_agent_id: AgentId,
    pub requesting_colleague_id: ColleagueId,
    pub root_request_id: PromptRequestId,
    pub action_summary: ActionSummary,
    pub gated_tool: ToolName,
    pub approver_kind: ApproverKind,
    pub approver_colleague: Option<ColleagueId>,
    pub status: ApprovalStatus,
    pub platform: Platform,
    pub platform_app_id: Option<String>,
    pub platform_container: Option<String>,
    pub platform_reply_to: Option<String>,
    pub platform_message_id: Option<PlatformMessageId>,
    pub decided_by_colleague: Option<ColleagueId>,
    pub decided_at: Option<DateTime<Utc>>,
    pub expires_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_summary_rejects_empty_and_oversize() {
        assert!(ActionSummary::try_from(String::new()).is_err());
        let too_long = "x".repeat(APPROVAL_SUMMARY_MAX + 1);
        assert!(ActionSummary::try_from(too_long).is_err());
        let ok = ActionSummary::try_from("Refund $40 to customer #12").expect("valid");
        assert_eq!(ok.as_str(), "Refund $40 to customer #12");
    }

    #[test]
    fn approval_id_parses_uuid_and_rejects_garbage() {
        assert!(ApprovalId::try_from("not-a-uuid").is_err());
        let raw = "00000000-0000-0000-0000-000000000001";
        assert_eq!(ApprovalId::try_from(raw).expect("valid").to_string(), raw);
    }

    #[test]
    fn policy_from_ids_empty_is_anyone() {
        assert_eq!(
            ApproverPolicy::from_ids(vec![]).expect("ok"),
            ApproverPolicy::Anyone
        );
    }

    #[test]
    fn policy_from_ids_dedupes_and_caps() {
        let a = ColleagueId::new();
        let b = ColleagueId::new();
        let policy = ApproverPolicy::from_ids(vec![a, a, b]).expect("ok");
        assert_eq!(policy, ApproverPolicy::OneOf(vec![a, b]));

        let too_many: Vec<ColleagueId> = (0..=MAX_APPROVERS).map(|_| ColleagueId::new()).collect();
        assert!(ApproverPolicy::from_ids(too_many).is_err());
    }

    #[test]
    fn anyone_allows_any_org_colleague() {
        let clicker = ColleagueId::new();
        assert!(policy_allows(ApproverKind::Anyone, None, &[], clicker));
    }

    #[test]
    fn colleague_policy_allows_only_the_pinned_one() {
        let pinned = ColleagueId::new();
        let other = ColleagueId::new();
        assert!(policy_allows(
            ApproverKind::Colleague,
            Some(pinned),
            &[],
            pinned
        ));
        assert!(!policy_allows(
            ApproverKind::Colleague,
            Some(pinned),
            &[],
            other
        ));
    }

    #[test]
    fn one_of_policy_allows_only_members() {
        let a = ColleagueId::new();
        let b = ColleagueId::new();
        let outsider = ColleagueId::new();
        let set = [a, b];
        assert!(policy_allows(ApproverKind::OneOf, None, &set, b));
        assert!(!policy_allows(ApproverKind::OneOf, None, &set, outsider));
    }

    #[test]
    fn decision_tag_roundtrips() {
        assert_eq!(Decision::from_tag("a"), Some(Decision::Approved));
        assert_eq!(Decision::from_tag("d"), Some(Decision::Denied));
        assert_eq!(Decision::from_tag("x"), None);
        assert_eq!(Decision::Approved.tag(), 'a');
        assert_eq!(Decision::Denied.status(), ApprovalStatus::Denied);
    }

    #[test]
    fn platform_target_decomposes_to_columns() {
        let web = PlatformTarget::Web;
        assert_eq!(web.platform(), Platform::Web);
        assert_eq!(web.columns(), (None, None, None));

        let discord = PlatformTarget::Discord {
            application_id: "app1".into(),
            container_id: "chan1".into(),
            reply_to: Some("msg1".into()),
        };
        assert_eq!(discord.platform(), Platform::Discord);
        assert_eq!(
            discord.columns(),
            (Some("app1"), Some("chan1"), Some("msg1"))
        );
    }
}
