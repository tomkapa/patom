use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;
use thiserror::Error;

use super::routes::turns::TurnDetailError;
use crate::agents::{AgentStoreError, PromptVersionError};
use crate::assets::AssetError;
use crate::auth::AuthError;
use crate::billing::BillingError;
use crate::entitlements::LicenseError;
use crate::mcp::McpError;
use crate::orgs::OrgError;
use crate::runtime::{PromptError, ResponseError};
use crate::types::ParseError;

/// One error type for the HTTP boundary. CLAUDE.md §12: `IntoResponse` lives next to
/// the variants so the HTTP mapping cannot drift from the variant set.
#[derive(Debug, Error)]
pub enum HttpError {
    #[error("invalid request: {0}")]
    BadRequest(String),

    #[error("not found")]
    NotFound,

    /// 403 with a fixed reason string. Use for role-gated routes (e.g.
    /// owner/admin-only mutations) where the failure mode is purely
    /// "your role isn't high enough", not an unknown resource.
    #[error("forbidden: {0}")]
    Forbidden(&'static str),

    #[error("conflict: {0}")]
    Conflict(String),

    #[error("payload too large")]
    PayloadTooLarge,

    #[error("too many requests")]
    TooManyRequests,

    #[error("parse: {0}")]
    Parse(#[from] ParseError),

    #[error("agent: {0}")]
    Agent(#[from] AgentStoreError),

    #[error("prompt version: {0}")]
    PromptVersion(#[from] PromptVersionError),

    #[error("prompt pipeline: {0}")]
    Prompt(#[from] PromptError),

    #[error("response stream: {0}")]
    Response(#[from] ResponseError),

    #[error("mcp: {0}")]
    Mcp(#[from] McpError),

    /// BYO provider-credential store failure (#141). Every variant is an
    /// internal fault (crypto / corruption / Db) — boundary parse failures
    /// surface as [`Self::Parse`] before the store is reached — so this maps
    /// to 500 with no detail (never leak key material, CLAUDE.md §2).
    #[error("provider credential store error")]
    ProviderCredential(#[from] crate::provider::ProviderCredentialError),

    #[error("auth: {0}")]
    Auth(#[from] AuthError),

    #[error("billing: {0}")]
    Billing(#[from] BillingError),

    /// Inner failure on the per-turn detail route. 4xx variants (NotFound /
    /// MetricsMissing / PromptVersionMissing) are bridged to `Self::NotFound`
    /// at the route, so the only variant that reaches this seat is the 5xx
    /// sqlx fall-through.
    #[error("turn detail: {0}")]
    TurnDetail(TurnDetailError),

    #[error("org: {0}")]
    Org(#[from] OrgError),

    #[error("colleague: {0}")]
    Colleague(#[from] crate::colleagues::ColleagueError),

    /// Entitlement gate refused the action (agent cap hit, or an unlicensed
    /// feature). Maps to 402 Payment Required so the FE can prompt an upgrade.
    #[error("license: {0}")]
    License(#[from] LicenseError),

    #[error("asset: {0}")]
    Asset(#[from] AssetError),

    /// Upload endpoint reached while `AppState.assets` is `None` — i.e.
    /// the operator hasn't configured R2. Distinct from `Internal` so
    /// the dashboard can dedicate an alert to it.
    #[error("asset storage not configured")]
    AssetStorageMissing,

    #[error("internal error")]
    Internal,
}

impl IntoResponse for HttpError {
    // One big exhaustive match — adding a variant must touch this
    // function. Splitting into helpers would split the contract.
    #[allow(clippy::too_many_lines)]
    fn into_response(self) -> Response {
        let (status, message) = match &self {
            Self::BadRequest(m) => (StatusCode::BAD_REQUEST, m.clone()),
            Self::NotFound => (StatusCode::NOT_FOUND, "not found".into()),
            Self::Forbidden(reason) => (StatusCode::FORBIDDEN, (*reason).into()),
            Self::Conflict(m) => (StatusCode::CONFLICT, m.clone()),
            Self::PayloadTooLarge => (StatusCode::PAYLOAD_TOO_LARGE, "too large".into()),
            Self::TooManyRequests => (StatusCode::TOO_MANY_REQUESTS, "too many requests".into()),
            Self::Parse(e) | Self::Auth(AuthError::Parse(e)) => {
                (StatusCode::BAD_REQUEST, e.to_string())
            }
            Self::Agent(AgentStoreError::NotFound(_)) => {
                (StatusCode::BAD_REQUEST, "unknown agent_id".into())
            }
            Self::Agent(AgentStoreError::NameNotFound(_)) => {
                (StatusCode::NOT_FOUND, self.to_string())
            }
            Self::Mcp(McpError::ServerCapExceeded { .. })
            | Self::Billing(BillingError::Exceeded { .. }) => {
                (StatusCode::TOO_MANY_REQUESTS, self.to_string())
            }
            // Out of platform credit → 402, the same code the agent-cap license
            // gate uses. Distinct from the 429 monthly-cap case above.
            Self::Billing(BillingError::OutOfCredit { .. }) => {
                (StatusCode::PAYMENT_REQUIRED, self.to_string())
            }
            Self::Billing(BillingError::Db(_)) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "billing store error".into(),
            ),
            Self::Colleague(crate::colleagues::ColleagueError::NotFound(_)) => {
                (StatusCode::NOT_FOUND, "colleague not found".into())
            }
            Self::Colleague(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "colleague directory error".into(),
            ),
            Self::Agent(AgentStoreError::InUse(_) | AgentStoreError::NameTaken(_))
            | Self::Mcp(McpError::CatalogIdTaken(_) | McpError::CatalogIdShadowsGlobal(_)) => {
                (StatusCode::CONFLICT, self.to_string())
            }
            Self::Agent(AgentStoreError::Parse(_))
            | Self::Mcp(
                McpError::Parse(_) | McpError::InvalidConfig(_) | McpError::CatalogIdUnknown(_),
            ) => (StatusCode::BAD_REQUEST, self.to_string()),
            Self::Agent(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "agent store error".into(),
            ),
            Self::PromptVersion(PromptVersionError::Parse(_)) => {
                (StatusCode::BAD_REQUEST, self.to_string())
            }
            // Body matches `NotFound` above, but the variant set is
            // intentionally split — adding a 4xx prompt-version mode
            // later (e.g. ConflictedRestore) should only force the new
            // arm, not the catch-all 500.
            #[allow(clippy::match_same_arms)]
            Self::PromptVersion(
                PromptVersionError::VersionNotFound { .. } | PromptVersionError::AgentNotFound(_),
            ) => (StatusCode::NOT_FOUND, "not found".into()),
            Self::PromptVersion(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "prompt version error".into(),
            ),
            Self::Prompt(PromptError::RequestNotFound(_)) => {
                (StatusCode::NOT_FOUND, "not found".into())
            }
            Self::Prompt(e) => (StatusCode::BAD_REQUEST, e.to_string()),
            Self::Response(ResponseError::NotFound(_)) => {
                (StatusCode::NOT_FOUND, "stream not found".into())
            }
            Self::Response(e) => (StatusCode::BAD_GATEWAY, e.to_string()),
            Self::Mcp(McpError::NotFound(_)) => {
                (StatusCode::NOT_FOUND, "mcp server not found".into())
            }
            Self::Mcp(_) => (StatusCode::INTERNAL_SERVER_ERROR, "mcp store error".into()),
            Self::ProviderCredential(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "provider credential store error".into(),
            ),
            Self::Auth(
                AuthError::Unauthenticated | AuthError::Jwt(_) | AuthError::OAuthStateInvalid,
            ) => (StatusCode::UNAUTHORIZED, "unauthorized".into()),
            Self::Auth(AuthError::EmailUnverified) => (
                StatusCode::FORBIDDEN,
                "email not verified by provider".into(),
            ),
            Self::Auth(AuthError::NotMember(_)) => (StatusCode::FORBIDDEN, self.to_string()),
            Self::Auth(AuthError::OrgLimitReached { .. }) => {
                (StatusCode::CONFLICT, "org.limit_reached".into())
            }
            Self::Auth(AuthError::OAuthProvider(_)) => {
                (StatusCode::BAD_GATEWAY, "oauth provider unavailable".into())
            }
            Self::Auth(_) => (StatusCode::INTERNAL_SERVER_ERROR, "auth error".into()),
            Self::TurnDetail(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "turn detail error".into(),
            ),
            Self::Asset(AssetError::TooLarge { .. }) => {
                (StatusCode::PAYLOAD_TOO_LARGE, self.to_string())
            }
            Self::Asset(AssetError::Timeout) => {
                (StatusCode::GATEWAY_TIMEOUT, "asset upload timed out".into())
            }
            Self::Asset(AssetError::StoragePut(_) | AssetError::StorageDelete(_)) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "asset storage error".into(),
            ),
            // Validation-style asset failures map to 400. The
            // `Asset(Parse)` arm shares its body with the
            // `Agent(Parse) | Mcp(Parse|...)` arm above; clippy
            // suggests merging the two but they sit in different
            // domains and we keep them split for readability.
            #[allow(clippy::match_same_arms)]
            Self::Asset(
                AssetError::ContentTypeNotAllowed(_)
                | AssetError::MagicByteMismatch { .. }
                | AssetError::UnknownFileType
                | AssetError::MissingField
                | AssetError::TooManyFields
                | AssetError::Multipart(_)
                | AssetError::Parse(_),
            ) => (StatusCode::BAD_REQUEST, self.to_string()),
            Self::AssetStorageMissing => (
                StatusCode::SERVICE_UNAVAILABLE,
                "asset storage not configured".into(),
            ),
            Self::Org(OrgError::SlugTaken) => (StatusCode::CONFLICT, "org_slug.taken".into()),
            Self::Org(OrgError::LastOwnerProtected) => {
                (StatusCode::CONFLICT, "org.last_owner".into())
            }
            #[allow(clippy::match_same_arms)]
            Self::Org(OrgError::NotFound) => (StatusCode::NOT_FOUND, "not found".into()),
            Self::Org(OrgError::InviteExpired) => (StatusCode::GONE, "invite.expired".into()),
            Self::Org(OrgError::InviteAlreadyConsumed) => {
                (StatusCode::CONFLICT, "invite.consumed".into())
            }
            #[allow(clippy::match_same_arms)]
            Self::Org(OrgError::InviteBatchTooLarge { .. }) => {
                (StatusCode::PAYLOAD_TOO_LARGE, self.to_string())
            }
            Self::Org(OrgError::Parse(e)) => (StatusCode::BAD_REQUEST, e.to_string()),
            Self::Org(OrgError::Auth(e)) => {
                (StatusCode::INTERNAL_SERVER_ERROR, format!("auth: {e}"))
            }
            Self::Org(OrgError::Db(_)) => {
                (StatusCode::INTERNAL_SERVER_ERROR, "org store error".into())
            }
            // Both license failures are "your plan doesn't cover this" → 402.
            // Matched explicitly (not `License(_)`) so a future LicenseError
            // variant with a different status forces an edit here (§1).
            Self::License(
                LicenseError::FeatureNotLicensed { .. } | LicenseError::AgentLimitReached { .. },
            ) => (StatusCode::PAYMENT_REQUIRED, self.to_string()),
            Self::Internal => (StatusCode::INTERNAL_SERVER_ERROR, "internal error".into()),
        };
        // CLAUDE.md §2: every error response that maps to a 5xx is a
        // server-side fault — emit a tracing::error event so operators
        // see the underlying error variant rather than the bare wire
        // message. The handler span (via `TraceLayer`) gets the status.
        if status.is_server_error() {
            tracing::error!(
                event = "http.error.5xx",
                http.response.status_code = status.as_u16(),
                error = ?self,
            );
        }
        (status, Json(json!({ "error": message }))).into_response()
    }
}
