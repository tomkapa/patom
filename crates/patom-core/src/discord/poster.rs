//! Outbound `POST /channels/{id}/messages` sender for the Discord adapter.
//!
//! One trait — [`DiscordPoster`] — fronts every message Patom sends. Two impls:
//!
//! - [`HttpDiscordPoster`]: production. Chunks the reply to Discord's 2000-char
//!   cap, posts each chunk via the shared `reqwest::Client` with an
//!   `Authorization: Bot <token>` header, threads the first chunk under the
//!   triggering message via `message_reference` (`fail_if_not_exists: false`, so
//!   a since-deleted parent degrades to a plain post), honours 429 `retry_after`
//!   and 5xx backoff, and acquires a global rate-limit token per request.
//! - [`FakeDiscordPoster`]: records every [`PostRequest`] without a network.
//!
//! **`allowed_mentions` is mandatory and cannot be constructed unset.** Discord
//! re-resolves `@everyone`/role/user pings from the raw `content` unless an
//! `allowed_mentions` object constrains them; [`AllowedMentions`] always
//! serializes `"parse": []`, so an accidental `@everyone` echoed in agent text is
//! structurally impossible — the only pings are the user ids deliberately listed.

use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize, Serializer};
use tokio::time::sleep;

use super::app_store::SharedBotTokenSource;
use super::error::DiscordError;
use super::limits::{
    DISCORD_ACTION_ROW_MAX, DISCORD_BUTTON_LABEL_MAX, DISCORD_CUSTOM_ID_MAX,
    DISCORD_MESSAGE_ACTION_ROWS_MAX,
};
use super::limits::{
    DISCORD_MESSAGE_MAX, DISCORD_POST_ERROR_BODY_MAX, DISCORD_POST_MAX_RETRIES,
    DISCORD_POST_TIMEOUT, DISCORD_RETRY_AFTER_CAP_SECS,
};
use super::ratelimit::RateLimiter;
use super::types::{ApplicationId, ContainerId, DiscordMessageId, InteractionId, InteractionToken};

/// The `allowed_mentions` safety object — cannot be constructed with `parse`
/// unset.
///
/// Every constructor leaves `parse = []`, so a raw `@everyone` in echoed agent
/// text never resolves — only the explicit `users` ping does.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AllowedMentions {
    users: Vec<String>,
}

impl AllowedMentions {
    /// Ping nobody — the safe default for a reply that addresses no one.
    #[must_use]
    pub fn none() -> Self {
        Self { users: Vec::new() }
    }

    /// Ping exactly these user snowflakes (and no one else).
    #[must_use]
    pub fn users(ids: &[DiscordUserId]) -> Self {
        Self {
            users: ids.iter().map(|i| i.as_str().to_owned()).collect(),
        }
    }
}

use super::types::DiscordUserId;

impl Serialize for AllowedMentions {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut s = ser.serialize_struct("allowed_mentions", 2)?;
        // `parse: []` is load-bearing — it disables @everyone/role/user
        // auto-resolution from `content`. Never omit it.
        let empty: [&str; 0] = [];
        s.serialize_field("parse", &empty)?;
        s.serialize_field("users", &self.users)?;
        s.end()
    }
}

/// Style of an interactive button (the subset Patom posts).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonStyle {
    /// Green — the affirmative action (Approve). Discord style `3`.
    Success,
    /// Red — the destructive action (Deny). Discord style `4`.
    Danger,
}

impl ButtonStyle {
    const fn wire(self) -> u8 {
        match self {
            Self::Success => 3,
            Self::Danger => 4,
        }
    }
}

/// One interactive button. The `custom_id` (≤[`DISCORD_CUSTOM_ID_MAX`]) is echoed
/// back verbatim on `INTERACTION_CREATE` when a user clicks it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Button {
    style: ButtonStyle,
    label: String,
    custom_id: String,
}

impl Button {
    /// Build a button, asserting the `label` and `custom_id` are within Discord's
    /// caps (§5 — every string crossing the boundary is bounded).
    #[must_use]
    pub fn new(style: ButtonStyle, label: impl Into<String>, custom_id: String) -> Self {
        let label = label.into();
        let label_len = label.chars().count();
        assert!(label_len >= 1, "button label must be non-empty");
        assert!(
            label_len <= DISCORD_BUTTON_LABEL_MAX,
            "button label exceeds Discord's 80-char cap"
        );
        let len = custom_id.chars().count();
        assert!(len >= 1, "button custom_id must be non-empty");
        assert!(
            len <= DISCORD_CUSTOM_ID_MAX,
            "button custom_id exceeds Discord's 100-char cap"
        );
        Self {
            style,
            label,
            custom_id,
        }
    }
}

impl Serialize for Button {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut s = ser.serialize_struct("button", 4)?;
        s.serialize_field("type", &2u8)?; // 2 = Button
        s.serialize_field("style", &self.style.wire())?;
        s.serialize_field("label", &self.label)?;
        s.serialize_field("custom_id", &self.custom_id)?;
        s.end()
    }
}

/// A row of interactive buttons (Discord caps a row at [`DISCORD_ACTION_ROW_MAX`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionRow {
    buttons: Vec<Button>,
}

impl ActionRow {
    /// Build a row, asserting it carries 1..=5 buttons (§5).
    #[must_use]
    pub fn new(buttons: Vec<Button>) -> Self {
        assert!(!buttons.is_empty(), "action row must carry a button");
        assert!(
            buttons.len() <= DISCORD_ACTION_ROW_MAX,
            "action row exceeds Discord's five-button cap"
        );
        Self { buttons }
    }
}

impl Serialize for ActionRow {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut s = ser.serialize_struct("action_row", 2)?;
        s.serialize_field("type", &1u8)?; // 1 = Action Row
        s.serialize_field("components", &self.buttons)?;
        s.end()
    }
}

/// What the stream pump hands the poster.
#[derive(Debug, Clone)]
pub struct PostRequest {
    /// The bot to post as (rate-limit key + which token to fetch).
    pub application_id: ApplicationId,
    /// The channel or thread to post into.
    pub container_id: ContainerId,
    /// The message to reply under (threads the first chunk); `None` posts plain.
    pub reply_to: Option<DiscordMessageId>,
    pub content: String,
    pub allowed_mentions: AllowedMentions,
    /// Interactive components (e.g. approval buttons) attached to the first
    /// chunk. Empty for ordinary replies — then the wire body omits the field.
    pub components: Vec<ActionRow>,
}

#[async_trait]
pub trait DiscordPoster: fmt::Debug + Send + Sync {
    /// Post a reply, chunking to the 2000-char cap. Returns the issued message
    /// ids in order.
    async fn post(&self, req: PostRequest) -> Result<Vec<DiscordMessageId>, DiscordError>;

    /// Open (or fetch the existing) DM channel with `recipient` for `bot`
    /// (`POST /users/@me/channels`), returning the DM channel snowflake. The
    /// outbound router (#178, arm 3) binds the returned id so subsequent turns
    /// post to the same channel without re-opening it.
    async fn create_dm(
        &self,
        application_id: &ApplicationId,
        recipient: &DiscordUserId,
    ) -> Result<ContainerId, DiscordError>;

    /// Acknowledge a component interaction within Discord's 3-second deadline
    /// with a DEFERRED_UPDATE_MESSAGE (callback type 6): the card stays as-is
    /// while the decision is recorded, then [`Self::edit_interaction_message`]
    /// renders the resolved view. The interaction token authorizes the call, so
    /// no bot token is sent. Must be invoked *before* any DB work.
    async fn ack_interaction(
        &self,
        interaction_id: &InteractionId,
        interaction_token: &InteractionToken,
    ) -> Result<(), DiscordError>;

    /// Edit the original interaction message (the approval card) to its resolved
    /// view via `PATCH /webhooks/{app}/{token}/messages/@original`. Empty
    /// `components` strips the buttons.
    async fn edit_interaction_message(
        &self,
        application_id: &ApplicationId,
        interaction_token: &InteractionToken,
        content: &str,
        components: &[ActionRow],
    ) -> Result<(), DiscordError>;

    /// Post an ephemeral follow-up (only the clicker sees it; `flags: 64`) via
    /// `POST /webhooks/{app}/{token}`. Used to tell an unauthorized clicker the
    /// click was rejected without mutating the shared card.
    async fn followup_ephemeral(
        &self,
        application_id: &ApplicationId,
        interaction_token: &InteractionToken,
        content: &str,
    ) -> Result<(), DiscordError>;
}

pub type SharedDiscordPoster = Arc<dyn DiscordPoster>;

// ──────────────────────────────────────────────────────────────────────
// Production impl
// ──────────────────────────────────────────────────────────────────────

/// Production poster over the shared `reqwest::Client`.
pub struct HttpDiscordPoster {
    http: Client,
    api_base: String,
    tokens: SharedBotTokenSource,
    limiter: Arc<RateLimiter>,
}

impl HttpDiscordPoster {
    #[must_use]
    pub fn new(
        http: Client,
        api_base: String,
        tokens: SharedBotTokenSource,
        limiter: Arc<RateLimiter>,
    ) -> Self {
        Self {
            http,
            api_base,
            tokens,
            limiter,
        }
    }
}

impl fmt::Debug for HttpDiscordPoster {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpDiscordPoster")
            .field("api_base", &self.api_base)
            .finish_non_exhaustive()
    }
}

#[derive(Serialize)]
struct WireMessageReference<'a> {
    message_id: &'a str,
    /// `false` → if the parent was deleted, degrade to a plain post rather than
    /// erroring (the OutboundRouter "scheduled task ran, nothing posted" fix).
    fail_if_not_exists: bool,
}

#[derive(Serialize)]
struct WireBody<'a> {
    content: &'a str,
    allowed_mentions: &'a AllowedMentions,
    #[serde(skip_serializing_if = "Option::is_none")]
    message_reference: Option<WireMessageReference<'a>>,
    /// Interactive components. Omitted when empty so an ordinary reply's body is
    /// byte-for-byte unchanged.
    #[serde(skip_serializing_if = "<[ActionRow]>::is_empty")]
    components: &'a [ActionRow],
}

#[derive(Serialize)]
struct WireCreateDm<'a> {
    recipient_id: &'a str,
}

/// `POST /interactions/{id}/{token}/callback` body. Type 6 =
/// DEFERRED_UPDATE_MESSAGE (ack now, edit the message later).
#[derive(Serialize)]
struct WireCallback {
    #[serde(rename = "type")]
    callback_type: u8,
}

/// `PATCH /webhooks/{app}/{token}/messages/@original` body — the resolved card.
#[derive(Serialize)]
struct WireEdit<'a> {
    content: &'a str,
    components: &'a [ActionRow],
}

/// `POST /webhooks/{app}/{token}` follow-up body. `flags: 64` = EPHEMERAL.
#[derive(Serialize)]
struct WireFollowup<'a> {
    content: &'a str,
    flags: u8,
}

/// Discord interaction callback type for DEFERRED_UPDATE_MESSAGE.
const DISCORD_CALLBACK_DEFERRED_UPDATE: u8 = 6;

/// The EPHEMERAL message flag (`1 << 6`): only the interacting user sees it.
const DISCORD_FLAG_EPHEMERAL: u8 = 64;

#[derive(Deserialize)]
struct WirePostResponse {
    id: String,
}

#[derive(Deserialize)]
struct WireRateLimit {
    #[serde(default)]
    retry_after: f64,
}

#[async_trait]
impl DiscordPoster for HttpDiscordPoster {
    async fn post(&self, req: PostRequest) -> Result<Vec<DiscordMessageId>, DiscordError> {
        // The components batch is bounded so a future caller can't over-fill it
        // and burn the shared invalid-request budget on a 400 (§5).
        assert!(
            req.components.len() <= DISCORD_MESSAGE_ACTION_ROWS_MAX,
            "message action rows exceed Discord's cap"
        );
        // An empty body is a Discord 400 that would burn the shared
        // invalid-request budget for nothing — there is no message to send.
        if req.content.is_empty() {
            return Ok(Vec::new());
        }
        if self.limiter.invalid_budget_exhausted() {
            // A Cloudflare ban is imminent/active on the shared egress; do not
            // add to the invalid-request count.
            return Err(DiscordError::RateLimited {
                retry_after_secs: DISCORD_RETRY_AFTER_CAP_SECS,
            });
        }
        let token = self.tokens.token(&req.application_id).await?;
        let url = format!(
            "{}/channels/{}/messages",
            self.api_base,
            req.container_id.as_str()
        );
        let chunks = chunk_content(&req.content, DISCORD_MESSAGE_MAX);
        let mut ids = Vec::with_capacity(chunks.len());
        for (i, chunk) in chunks.iter().enumerate() {
            // Only the first chunk threads under the triggering message and
            // carries the interactive components (the buttons attach once).
            let reference = if i == 0 { req.reply_to.as_ref() } else { None };
            let components: &[ActionRow] = if i == 0 { &req.components } else { &[] };
            let id = self
                .post_one(&url, &req, token.expose(), chunk, reference, components)
                .await?;
            ids.push(id);
        }
        Ok(ids)
    }

    async fn create_dm(
        &self,
        application_id: &ApplicationId,
        recipient: &DiscordUserId,
    ) -> Result<ContainerId, DiscordError> {
        if self.limiter.invalid_budget_exhausted() {
            return Err(DiscordError::RateLimited {
                retry_after_secs: DISCORD_RETRY_AFTER_CAP_SECS,
            });
        }
        let token = self.tokens.token(application_id).await?;
        let url = format!("{}/users/@me/channels", self.api_base);
        let body = serde_json::to_vec(&WireCreateDm {
            recipient_id: recipient.as_str(),
        })?;
        self.limiter.acquire(application_id).await;
        let send = self
            .http
            .post(&url)
            .header("Authorization", format!("Bot {}", token.expose()))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
            .send();
        let resp = match tokio::time::timeout(DISCORD_POST_TIMEOUT, send).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => return Err(e.into()),
            Err(_) => return Err(DiscordError::PostTimeout(DISCORD_POST_TIMEOUT)),
        };
        let status = resp.status();
        if matches!(status.as_u16(), 401 | 403 | 429) {
            self.limiter.record_invalid();
        }
        if !status.is_success() {
            return Err(DiscordError::PostFailed {
                status: status.as_u16(),
                body: read_text(resp).await,
            });
        }
        let bytes = match tokio::time::timeout(DISCORD_POST_TIMEOUT, resp.bytes()).await {
            Ok(Ok(b)) => b,
            Ok(Err(e)) => return Err(DiscordError::Http(e)),
            Err(_) => return Err(DiscordError::PostTimeout(DISCORD_POST_TIMEOUT)),
        };
        let parsed: WirePostResponse = serde_json::from_slice(&bytes)?;
        Ok(ContainerId::try_from(parsed.id)?)
    }

    async fn ack_interaction(
        &self,
        interaction_id: &InteractionId,
        interaction_token: &InteractionToken,
    ) -> Result<(), DiscordError> {
        let url = format!(
            "{}/interactions/{}/{}/callback",
            self.api_base,
            interaction_id.as_str(),
            interaction_token.expose(),
        );
        let body = serde_json::to_vec(&WireCallback {
            callback_type: DISCORD_CALLBACK_DEFERRED_UPDATE,
        })?;
        self.interaction_call(reqwest::Method::POST, url, body)
            .await
    }

    async fn edit_interaction_message(
        &self,
        application_id: &ApplicationId,
        interaction_token: &InteractionToken,
        content: &str,
        components: &[ActionRow],
    ) -> Result<(), DiscordError> {
        // This path doesn't chunk like `post`, so the single message + its
        // components batch are capped before the body is built (§5).
        assert!(
            content.chars().count() <= DISCORD_MESSAGE_MAX,
            "interaction edit content exceeds Discord's message cap"
        );
        assert!(
            components.len() <= DISCORD_MESSAGE_ACTION_ROWS_MAX,
            "interaction edit action rows exceed Discord's cap"
        );
        let url = format!(
            "{}/webhooks/{}/{}/messages/@original",
            self.api_base,
            application_id.as_str(),
            interaction_token.expose(),
        );
        let body = serde_json::to_vec(&WireEdit {
            content,
            components,
        })?;
        self.interaction_call(reqwest::Method::PATCH, url, body)
            .await
    }

    async fn followup_ephemeral(
        &self,
        application_id: &ApplicationId,
        interaction_token: &InteractionToken,
        content: &str,
    ) -> Result<(), DiscordError> {
        assert!(
            content.chars().count() <= DISCORD_MESSAGE_MAX,
            "ephemeral followup content exceeds Discord's message cap"
        );
        let url = format!(
            "{}/webhooks/{}/{}",
            self.api_base,
            application_id.as_str(),
            interaction_token.expose(),
        );
        let body = serde_json::to_vec(&WireFollowup {
            content,
            flags: DISCORD_FLAG_EPHEMERAL,
        })?;
        self.interaction_call(reqwest::Method::POST, url, body)
            .await
    }
}

impl HttpDiscordPoster {
    /// Send an interaction-scoped request (callback / `@original` edit / ephemeral
    /// follow-up). These endpoints are authorized by the interaction token in the
    /// URL, so no bot token is attached; the call is timeout-bounded and any
    /// non-2xx becomes a typed [`DiscordError::PostFailed`].
    async fn interaction_call(
        &self,
        method: reqwest::Method,
        url: String,
        body: Vec<u8>,
    ) -> Result<(), DiscordError> {
        if self.limiter.invalid_budget_exhausted() {
            // A Cloudflare ban is imminent/active on the shared egress; do not
            // add to the invalid-request count (same guard as `post_one`).
            return Err(DiscordError::RateLimited {
                retry_after_secs: DISCORD_RETRY_AFTER_CAP_SECS,
            });
        }
        let send = self
            .http
            .request(method, &url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
            .send();
        let resp = match tokio::time::timeout(DISCORD_POST_TIMEOUT, send).await {
            Ok(Ok(r)) => r,
            // The URL embeds the short-lived interaction token; `without_url`
            // strips it so a transport error's `Debug` can't leak it into logs.
            Ok(Err(e)) => return Err(e.without_url().into()),
            Err(_) => return Err(DiscordError::PostTimeout(DISCORD_POST_TIMEOUT)),
        };
        let status = resp.status();
        // An expired interaction token / 429 counts toward the shared invalid-
        // request budget, exactly as the message + DM paths record it.
        if matches!(status.as_u16(), 401 | 403 | 429) {
            self.limiter.record_invalid();
        }
        if !status.is_success() {
            return Err(DiscordError::PostFailed {
                status: status.as_u16(),
                body: read_text(resp).await,
            });
        }
        Ok(())
    }

    /// Post one chunk with the retry loop (429 `retry_after`, 5xx backoff).
    async fn post_one(
        &self,
        url: &str,
        req: &PostRequest,
        token: &str,
        chunk: &str,
        reply_to: Option<&DiscordMessageId>,
        components: &[ActionRow],
    ) -> Result<DiscordMessageId, DiscordError> {
        let body = serde_json::to_vec(&WireBody {
            content: chunk,
            allowed_mentions: &req.allowed_mentions,
            message_reference: reply_to.map(|m| WireMessageReference {
                message_id: m.as_str(),
                fail_if_not_exists: false,
            }),
            components,
        })?;
        let mut attempt: u8 = 0;
        loop {
            assert!(
                attempt <= DISCORD_POST_MAX_RETRIES,
                "retry counter overran cap"
            );
            self.limiter.acquire(&req.application_id).await;
            let send = self
                .http
                .post(url)
                .header("Authorization", format!("Bot {token}"))
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body.clone())
                .send();
            let resp = match tokio::time::timeout(DISCORD_POST_TIMEOUT, send).await {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => return Err(e.into()),
                Err(_) => return Err(DiscordError::PostTimeout(DISCORD_POST_TIMEOUT)),
            };
            let status = resp.status();
            if matches!(status.as_u16(), 401 | 403 | 429) {
                self.limiter.record_invalid();
            }
            if status.as_u16() == 429 {
                let retry_after = read_retry_after(resp).await;
                if attempt >= DISCORD_POST_MAX_RETRIES {
                    return Err(DiscordError::RateLimited {
                        retry_after_secs: retry_after,
                    });
                }
                attempt = attempt.saturating_add(1);
                sleep(Duration::from_secs(u64::from(retry_after))).await;
                continue;
            }
            if status.is_server_error() {
                if attempt >= DISCORD_POST_MAX_RETRIES {
                    return Err(DiscordError::PostFailed {
                        status: status.as_u16(),
                        body: read_text(resp).await,
                    });
                }
                attempt = attempt.saturating_add(1);
                sleep(backoff(attempt)).await;
                continue;
            }
            if !status.is_success() {
                return Err(DiscordError::PostFailed {
                    status: status.as_u16(),
                    body: read_text(resp).await,
                });
            }
            let bytes = match tokio::time::timeout(DISCORD_POST_TIMEOUT, resp.bytes()).await {
                Ok(Ok(b)) => b,
                Ok(Err(e)) => return Err(DiscordError::Http(e)),
                Err(_) => return Err(DiscordError::PostTimeout(DISCORD_POST_TIMEOUT)),
            };
            let parsed: WirePostResponse = serde_json::from_slice(&bytes)?;
            return Ok(DiscordMessageId::try_from(parsed.id)?);
        }
    }
}

/// Split `content` into chunks of at most `max` characters, breaking at the last
/// newline inside the window when there is one (so a paragraph is not split
/// mid-line), else hard-splitting on a char boundary.
fn chunk_content(content: &str, max: usize) -> Vec<String> {
    assert!(max > 0, "chunk size must be positive");
    if content.is_empty() {
        return vec![String::new()];
    }
    let mut chunks = Vec::new();
    let mut rest = content;
    while rest.chars().count() > max {
        // The byte offset of the `max`-th char (the hard split point).
        let hard = rest
            .char_indices()
            .nth(max)
            .map_or(rest.len(), |(idx, _)| idx);
        let window = &rest[..hard];
        // Prefer the last newline in the window (but not a zero-length head).
        let split = window.rfind('\n').filter(|&p| p > 0).unwrap_or(hard);
        chunks.push(rest[..split].to_owned());
        // Skip the newline we split on.
        rest = rest[split..].trim_start_matches('\n');
    }
    if !rest.is_empty() {
        chunks.push(rest.to_owned());
    }
    chunks
}

async fn read_retry_after(resp: reqwest::Response) -> u32 {
    // Prefer the JSON body's float `retry_after`; fall back to 1s. Cap so a
    // hostile proxy cannot stall the pump.
    let secs = match tokio::time::timeout(DISCORD_POST_TIMEOUT, resp.bytes()).await {
        Ok(Ok(b)) => serde_json::from_slice::<WireRateLimit>(&b).map_or(1.0, |r| r.retry_after),
        _ => 1.0,
    };
    let secs = if secs.is_finite() && secs >= 0.0 {
        secs
    } else {
        1.0
    };
    // Convert via `Duration` (no `as` cast), rounding up to whole seconds, capped.
    let dur = Duration::from_secs_f64(secs.min(f64::from(DISCORD_RETRY_AFTER_CAP_SECS)));
    let ceil_secs = dur.as_secs() + u64::from(dur.subsec_nanos() > 0);
    u32::try_from(ceil_secs.max(1))
        .unwrap_or(DISCORD_RETRY_AFTER_CAP_SECS)
        .min(DISCORD_RETRY_AFTER_CAP_SECS)
}

async fn read_text(resp: reqwest::Response) -> String {
    match tokio::time::timeout(DISCORD_POST_TIMEOUT, resp.text()).await {
        Ok(Ok(body)) => body.chars().take(DISCORD_POST_ERROR_BODY_MAX).collect(),
        Ok(Err(_)) | Err(_) => String::new(),
    }
}

/// Capped exponential backoff: 0.25 s, 0.5 s, 1.0 s (cap 2 s).
fn backoff(attempt: u8) -> Duration {
    let shift = attempt.min(3);
    let ms = 250u64.checked_shl(u32::from(shift)).unwrap_or(2_000);
    Duration::from_millis(ms.min(2_000))
}

// ──────────────────────────────────────────────────────────────────────
// Test fake
// ──────────────────────────────────────────────────────────────────────

/// Test poster that records every request and returns stub ids.
pub struct FakeDiscordPoster {
    inner: Mutex<Vec<PostRequest>>,
    dms: Mutex<Vec<(ApplicationId, DiscordUserId)>>,
    acks: Mutex<Vec<InteractionId>>,
    edits: Mutex<Vec<(ApplicationId, String)>>,
    followups: Mutex<Vec<(ApplicationId, String)>>,
}

/// The deterministic DM channel snowflake the fake returns from `create_dm`.
const FAKE_DM_CHANNEL: &str = "900000000000000001";

impl FakeDiscordPoster {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Vec::new()),
            dms: Mutex::new(Vec::new()),
            acks: Mutex::new(Vec::new()),
            edits: Mutex::new(Vec::new()),
            followups: Mutex::new(Vec::new()),
        }
    }

    #[must_use]
    pub fn captured(&self) -> Vec<PostRequest> {
        self.inner
            .lock()
            .expect("invariant: fake-poster mutex poisoned")
            .clone()
    }

    #[must_use]
    pub fn count(&self) -> usize {
        self.inner
            .lock()
            .expect("invariant: fake-poster mutex poisoned")
            .len()
    }

    /// Every `(bot, recipient)` a `create_dm` was requested for.
    #[must_use]
    pub fn dm_opens(&self) -> Vec<(ApplicationId, DiscordUserId)> {
        self.dms
            .lock()
            .expect("invariant: fake-poster mutex poisoned")
            .clone()
    }

    /// Every interaction id acknowledged via `ack_interaction`.
    #[must_use]
    pub fn acks(&self) -> Vec<InteractionId> {
        self.acks
            .lock()
            .expect("invariant: fake-poster mutex poisoned")
            .clone()
    }

    /// Every `(bot, content)` an `edit_interaction_message` rendered.
    #[must_use]
    pub fn edits(&self) -> Vec<(ApplicationId, String)> {
        self.edits
            .lock()
            .expect("invariant: fake-poster mutex poisoned")
            .clone()
    }

    /// Every `(bot, content)` a `followup_ephemeral` posted.
    #[must_use]
    pub fn followups(&self) -> Vec<(ApplicationId, String)> {
        self.followups
            .lock()
            .expect("invariant: fake-poster mutex poisoned")
            .clone()
    }
}

impl Default for FakeDiscordPoster {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for FakeDiscordPoster {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FakeDiscordPoster")
            .field("count", &self.count())
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl DiscordPoster for FakeDiscordPoster {
    async fn post(&self, req: PostRequest) -> Result<Vec<DiscordMessageId>, DiscordError> {
        self.inner
            .lock()
            .expect("invariant: fake-poster mutex poisoned")
            .push(req);
        Ok(vec![DiscordMessageId::try_from("1234567890123456789")?])
    }

    async fn create_dm(
        &self,
        application_id: &ApplicationId,
        recipient: &DiscordUserId,
    ) -> Result<ContainerId, DiscordError> {
        self.dms
            .lock()
            .expect("invariant: fake-poster mutex poisoned")
            .push((application_id.clone(), recipient.clone()));
        Ok(ContainerId::try_from(FAKE_DM_CHANNEL)?)
    }

    async fn ack_interaction(
        &self,
        interaction_id: &InteractionId,
        _interaction_token: &InteractionToken,
    ) -> Result<(), DiscordError> {
        self.acks
            .lock()
            .expect("invariant: fake-poster mutex poisoned")
            .push(interaction_id.clone());
        Ok(())
    }

    async fn edit_interaction_message(
        &self,
        application_id: &ApplicationId,
        _interaction_token: &InteractionToken,
        content: &str,
        _components: &[ActionRow],
    ) -> Result<(), DiscordError> {
        self.edits
            .lock()
            .expect("invariant: fake-poster mutex poisoned")
            .push((application_id.clone(), content.to_owned()));
        Ok(())
    }

    async fn followup_ephemeral(
        &self,
        application_id: &ApplicationId,
        _interaction_token: &InteractionToken,
        content: &str,
    ) -> Result<(), DiscordError> {
        self.followups
            .lock()
            .expect("invariant: fake-poster mutex poisoned")
            .push((application_id.clone(), content.to_owned()));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> DiscordUserId {
        DiscordUserId::try_from(s).expect("snowflake")
    }

    #[test]
    fn allowed_mentions_always_serializes_empty_parse() {
        // The structural @everyone defense: parse is [] no matter what.
        let none = serde_json::to_value(AllowedMentions::none()).expect("ser");
        assert_eq!(none["parse"], serde_json::json!([]));
        assert_eq!(none["users"], serde_json::json!([]));

        let some =
            serde_json::to_value(AllowedMentions::users(&[id("111"), id("222")])).expect("ser");
        assert_eq!(
            some["parse"],
            serde_json::json!([]),
            "parse stays empty even with users set"
        );
        assert_eq!(some["users"], serde_json::json!(["111", "222"]));
    }

    #[test]
    fn wire_body_includes_reference_with_fail_if_not_exists_false() {
        let am = AllowedMentions::none();
        let body = WireBody {
            content: "hi",
            allowed_mentions: &am,
            message_reference: Some(WireMessageReference {
                message_id: "555",
                fail_if_not_exists: false,
            }),
            components: &[],
        };
        let v = serde_json::to_value(&body).expect("ser");
        assert_eq!(v["content"], "hi");
        assert_eq!(v["message_reference"]["message_id"], "555");
        assert_eq!(v["message_reference"]["fail_if_not_exists"], false);
    }

    #[test]
    fn wire_body_omits_reference_when_none() {
        let am = AllowedMentions::none();
        let body = WireBody {
            content: "hi",
            allowed_mentions: &am,
            message_reference: None,
            components: &[],
        };
        let v = serde_json::to_value(&body).expect("ser");
        assert!(v.get("message_reference").is_none());
    }

    #[test]
    fn wire_body_omits_components_when_empty_and_includes_when_set() {
        let am = AllowedMentions::none();
        let plain = WireBody {
            content: "hi",
            allowed_mentions: &am,
            message_reference: None,
            components: &[],
        };
        assert!(
            serde_json::to_value(&plain)
                .expect("ser")
                .get("components")
                .is_none(),
            "empty components are omitted so ordinary replies are unchanged"
        );

        let row = ActionRow::new(vec![
            Button::new(ButtonStyle::Success, "Approve", "apv:abc:a".to_owned()),
            Button::new(ButtonStyle::Danger, "Deny", "apv:abc:d".to_owned()),
        ]);
        let rows = [row];
        let card = WireBody {
            content: "Approve?",
            allowed_mentions: &am,
            message_reference: None,
            components: &rows,
        };
        let v = serde_json::to_value(&card).expect("ser");
        assert_eq!(v["components"][0]["type"], 1);
        assert_eq!(v["components"][0]["components"][0]["type"], 2);
        assert_eq!(v["components"][0]["components"][0]["style"], 3);
        assert_eq!(v["components"][0]["components"][0]["label"], "Approve");
        assert_eq!(
            v["components"][0]["components"][0]["custom_id"],
            "apv:abc:a"
        );
        assert_eq!(v["components"][0]["components"][1]["style"], 4);
    }

    #[test]
    fn chunk_short_content_is_one_chunk() {
        assert_eq!(chunk_content("hello", 2000), vec!["hello".to_owned()]);
        assert_eq!(chunk_content("", 2000), vec![String::new()]);
    }

    #[test]
    fn chunk_long_content_splits_at_cap() {
        let big = "x".repeat(4500);
        let chunks = chunk_content(&big, 2000);
        assert_eq!(chunks.len(), 3);
        assert!(chunks.iter().all(|c| c.chars().count() <= 2000));
        assert_eq!(chunks.concat().chars().count(), 4500);
    }

    #[test]
    fn chunk_prefers_newline_break() {
        // A newline inside the window → break there, not mid-line.
        let content = format!("{}\nsecond part", "a".repeat(1990));
        let chunks = chunk_content(&content, 2000);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0], "a".repeat(1990));
        assert_eq!(chunks[1], "second part");
    }

    #[test]
    fn chunk_respects_char_boundaries() {
        let s = "é".repeat(2500); // 2-byte chars
        let chunks = chunk_content(&s, 2000);
        // No panic (mid-codepoint split) and every chunk is valid UTF-8.
        assert_eq!(
            chunks.iter().map(|c| c.chars().count()).sum::<usize>(),
            2500
        );
    }

    #[test]
    fn backoff_is_bounded() {
        assert!(backoff(0) < Duration::from_secs(2));
        assert!(backoff(255) <= Duration::from_secs(2));
    }

    #[tokio::test]
    async fn fake_poster_records_request() {
        let p = FakeDiscordPoster::new();
        let ids = p
            .post(PostRequest {
                application_id: ApplicationId::try_from("123456789012345678").expect("app"),
                container_id: ContainerId::try_from("987654321098765432").expect("chan"),
                reply_to: None,
                content: "hello".to_owned(),
                allowed_mentions: AllowedMentions::none(),
                components: Vec::new(),
            })
            .await
            .expect("post");
        assert_eq!(ids.len(), 1);
        assert_eq!(p.count(), 1);
        assert_eq!(p.captured()[0].content, "hello");
    }
}
