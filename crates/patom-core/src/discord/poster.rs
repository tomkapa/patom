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
    DISCORD_MESSAGE_MAX, DISCORD_POST_MAX_RETRIES, DISCORD_POST_TIMEOUT,
    DISCORD_RETRY_AFTER_CAP_SECS,
};
use super::ratelimit::RateLimiter;
use super::types::{ApplicationId, ContainerId, DiscordMessageId};

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
}

#[async_trait]
pub trait DiscordPoster: fmt::Debug + Send + Sync {
    /// Post a reply, chunking to the 2000-char cap. Returns the issued message
    /// ids in order.
    async fn post(&self, req: PostRequest) -> Result<Vec<DiscordMessageId>, DiscordError>;
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
}

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
            // Only the first chunk threads under the triggering message.
            let reference = if i == 0 { req.reply_to.as_ref() } else { None };
            let id = self
                .post_one(&url, &req, token.expose(), chunk, reference)
                .await?;
            ids.push(id);
        }
        Ok(ids)
    }
}

impl HttpDiscordPoster {
    /// Post one chunk with the retry loop (429 `retry_after`, 5xx backoff).
    async fn post_one(
        &self,
        url: &str,
        req: &PostRequest,
        token: &str,
        chunk: &str,
        reply_to: Option<&DiscordMessageId>,
    ) -> Result<DiscordMessageId, DiscordError> {
        let body = serde_json::to_vec(&WireBody {
            content: chunk,
            allowed_mentions: &req.allowed_mentions,
            message_reference: reply_to.map(|m| WireMessageReference {
                message_id: m.as_str(),
                fail_if_not_exists: false,
            }),
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
        Ok(Ok(body)) => body.chars().take(512).collect(),
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
}

impl FakeDiscordPoster {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Vec::new()),
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
        };
        let v = serde_json::to_value(&body).expect("ser");
        assert!(v.get("message_reference").is_none());
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
            })
            .await
            .expect("post");
        assert_eq!(ids.len(), 1);
        assert_eq!(p.count(), 1);
        assert_eq!(p.captured()[0].content, "hello");
    }
}
