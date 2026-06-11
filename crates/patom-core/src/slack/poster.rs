//! Outbound `chat.postMessage` wrapper.
//!
//! One trait — `SlackPoster` — fronts every outbound message Slack sees.
//! Two impls:
//!
//! - `HttpSlackPoster`: production. Uses the shared `reqwest::Client` to
//!   `POST https://slack.com/api/chat.postMessage` with per-message
//!   `username` override (the per-agent identity surface for Phase 1 —
//!   no `icon_url` yet; see issue #43). Honours `Retry-After` on 429 and
//!   does bounded exponential backoff on 5xx and `error: ratelimited`
//!   bodies.
//!
//! - `FakeSlackPoster` (test-only): records every `PostRequest` it sees
//!   without touching the network. Lives next to the production impl
//!   instead of inside `#[cfg(test)]` because the e2e integration test
//!   in `tests/` is a separate crate.
//!
//! ## Body shape
//!
//! `PostRequest.body: PostBody` is a sum type so the same wire path
//! serves both plain text (the common `Done` / `AgentMessage` case) and
//! Block Kit cards (the `WireMcpRequest` connection card). Retry +
//! rate-limit paths stay shared.

use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::time::sleep;

use super::error::SlackError;
use super::limits::{
    SLACK_POST_BODY_TIMEOUT, SLACK_POST_MAX_RETRIES, SLACK_POST_TIMEOUT, SLACK_RETRY_AFTER_CAP_SECS,
};
use super::types::{SlackBotToken, SlackChannelId, SlackThreadTs, SlackTs, SlackUserId};

/// What the caller posts.
///
/// `Blocks` carries a `fallback_text` that Slack uses for notification
/// body + accessibility — mandatory per the Block Kit API so
/// non-Block-Kit clients (mobile lock-screen previews, screen readers)
/// still see a meaningful summary.
#[derive(Debug, Clone)]
pub enum PostBody {
    /// Plain text post — single `text` field on the wire.
    Text(String),
    /// Block Kit payload — `text` falls through to `fallback_text` so
    /// notification body + accessibility have something to render.
    Blocks {
        fallback_text: String,
        blocks: Value,
    },
}

impl PostBody {
    /// The fallback / notification text the receiver sees. Used by the
    /// stream pump to apply the `SLACK_MAX_POST_CHARS` cap on a single
    /// known-textual surface before constructing the body. For
    /// `PostBody::Blocks` the blocks themselves are not clipped here —
    /// individual builders (`connection_card.rs`) own that responsibility.
    #[must_use]
    pub fn fallback_text(&self) -> &str {
        match self {
            Self::Text(s) => s.as_str(),
            Self::Blocks { fallback_text, .. } => fallback_text.as_str(),
        }
    }
}

/// What the bridge / stream pump hands to the poster.
///
/// `thread_ts` is `Some(_)` for the common case — an agent reply that
/// must thread under the user's originating message. It is `None`
/// only for the `/patom` slash command's synthetic prompt mirror,
/// which lands as a top-level channel post to become the thread root
/// itself. Slack's `chat.postMessage` API omits the `thread_ts` field
/// from the wire body in that case.
#[derive(Debug, Clone)]
pub struct PostRequest {
    pub token: SlackBotToken,
    pub channel: SlackChannelId,
    pub thread_ts: Option<SlackThreadTs>,
    pub body: PostBody,
    /// Per-message `username` override — the agent's name on outbound
    /// agent posts, or the human's workspace display name on the
    /// slash-command synthetic prompt mirror.
    pub username: String,
    /// Per-message `icon_url` override. `Some` for the slash-command
    /// prompt mirror (carries the sender's `users.info` avatar URL so
    /// Slack does not fall back to the app default); `None` elsewhere
    /// (agent posts have no avatar yet — issue #43).
    pub icon_url: Option<String>,
    /// When `Some(user)`, the message is posted via `chat.postEphemeral`
    /// and is visible only to that Slack user (it is never persisted in
    /// the channel). Used for the "connect your account" nudge to an
    /// unlinked user. `None` is the normal `chat.postMessage` path.
    pub ephemeral_to: Option<SlackUserId>,
}

#[async_trait]
pub trait SlackPoster: fmt::Debug + Send + Sync {
    /// Post a single message. Returns the Slack-issued `ts` on success.
    async fn post(&self, req: PostRequest) -> Result<SlackTs, SlackError>;
}

pub type SharedSlackPoster = Arc<dyn SlackPoster>;

// ──────────────────────────────────────────────────────────────────────
// Production impl
// ──────────────────────────────────────────────────────────────────────

const SLACK_POST_URL: &str = "https://slack.com/api/chat.postMessage";
const SLACK_POST_EPHEMERAL_URL: &str = "https://slack.com/api/chat.postEphemeral";

pub struct HttpSlackPoster {
    http: Client,
}

impl HttpSlackPoster {
    #[must_use]
    pub fn new(http: Client) -> Self {
        Self { http }
    }
}

impl fmt::Debug for HttpSlackPoster {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpSlackPoster").finish_non_exhaustive()
    }
}

#[derive(Serialize)]
struct WirePostBody<'a> {
    channel: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    thread_ts: Option<&'a str>,
    /// `chat.postEphemeral` requires the target `user`; omitted for the
    /// normal `chat.postMessage` path.
    #[serde(skip_serializing_if = "Option::is_none")]
    user: Option<&'a str>,
    text: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    blocks: Option<&'a Value>,
    username: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    icon_url: Option<&'a str>,
    unfurl_links: bool,
    unfurl_media: bool,
}

#[derive(Deserialize)]
struct WirePostResponse {
    ok: bool,
    /// `chat.postMessage` returns `ts`; `chat.postEphemeral` returns
    /// `message_ts`. We accept either so both paths resolve a `SlackTs`.
    #[serde(default)]
    ts: Option<String>,
    #[serde(default)]
    message_ts: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

#[async_trait]
impl SlackPoster for HttpSlackPoster {
    async fn post(&self, req: PostRequest) -> Result<SlackTs, SlackError> {
        let (text, blocks) = match &req.body {
            PostBody::Text(s) => (s.as_str(), None),
            PostBody::Blocks {
                fallback_text,
                blocks,
            } => (fallback_text.as_str(), Some(blocks)),
        };
        let body = WirePostBody {
            channel: req.channel.as_str(),
            thread_ts: req
                .thread_ts
                .as_ref()
                .map(super::types::SlackThreadTs::as_str),
            user: req.ephemeral_to.as_ref().map(SlackUserId::as_str),
            text,
            blocks,
            username: &req.username,
            icon_url: req.icon_url.as_deref(),
            // Bot replies should never expand link previews; the agent's
            // text is the message, attachments would be visual noise.
            unfurl_links: false,
            unfurl_media: false,
        };
        let url = if req.ephemeral_to.is_some() {
            SLACK_POST_EPHEMERAL_URL
        } else {
            SLACK_POST_URL
        };

        let mut attempt: u8 = 0;
        loop {
            let send = self
                .http
                .post(url)
                .bearer_auth(req.token.expose())
                .json(&body)
                .send();
            let resp = match tokio::time::timeout(SLACK_POST_TIMEOUT, send).await {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => return Err(e.into()),
                Err(_) => return Err(SlackError::PostTimeout(SLACK_POST_TIMEOUT)),
            };

            let status = resp.status();
            // Retry on 429 (Retry-After) and 5xx.
            if status.as_u16() == 429 {
                let raw_retry_after = parse_retry_after_secs(resp.headers()).unwrap_or(1);
                // Cap the upstream-supplied header value so a hostile or
                // misbehaving proxy cannot stall us indefinitely.
                let retry_after = raw_retry_after.min(SLACK_RETRY_AFTER_CAP_SECS);
                if attempt >= SLACK_POST_MAX_RETRIES {
                    return Err(SlackError::RateLimited {
                        retry_after_secs: retry_after,
                    });
                }
                attempt = attempt.saturating_add(1);
                sleep(Duration::from_secs(u64::from(retry_after))).await;
                continue;
            }
            if status.is_server_error() {
                if attempt >= SLACK_POST_MAX_RETRIES {
                    let body = read_text_with_timeout(resp).await;
                    return Err(SlackError::PostFailed {
                        status: status.as_u16(),
                        body,
                    });
                }
                attempt = attempt.saturating_add(1);
                sleep(backoff(attempt)).await;
                continue;
            }
            if !status.is_success() {
                let body = read_text_with_timeout(resp).await;
                return Err(SlackError::PostFailed {
                    status: status.as_u16(),
                    body,
                });
            }

            // 2xx — Slack signals success/failure inside the JSON body.
            let parsed: WirePostResponse =
                match tokio::time::timeout(SLACK_POST_BODY_TIMEOUT, resp.json()).await {
                    Ok(Ok(p)) => p,
                    Ok(Err(e)) => return Err(SlackError::Http(e)),
                    Err(_) => return Err(SlackError::PostTimeout(SLACK_POST_BODY_TIMEOUT)),
                };
            if !parsed.ok {
                // `ratelimited` inside a 200 body is the documented
                // Slack-side rate-limit path that bypasses HTTP 429.
                if parsed.error.as_deref() == Some("ratelimited") {
                    if attempt >= SLACK_POST_MAX_RETRIES {
                        return Err(SlackError::RateLimited {
                            retry_after_secs: 1,
                        });
                    }
                    attempt = attempt.saturating_add(1);
                    sleep(backoff(attempt)).await;
                    continue;
                }
                return Err(SlackError::PostFailed {
                    status: 200,
                    body: parsed.error.unwrap_or_else(|| "unknown".to_owned()),
                });
            }
            let ts_str = parsed.ts.or(parsed.message_ts).ok_or_else(|| {
                SlackError::Internal("chat.post* 200 without ts/message_ts".to_owned())
            })?;
            return Ok(SlackTs::try_from(ts_str)?);
        }
    }
}

/// Best-effort body read for diagnostic logging — bounded by
/// `SLACK_POST_BODY_TIMEOUT`. On timeout or transport failure we
/// surface an empty body rather than blocking the pump, since the
/// caller has already classified the response as a failure.
async fn read_text_with_timeout(resp: reqwest::Response) -> String {
    match tokio::time::timeout(SLACK_POST_BODY_TIMEOUT, resp.text()).await {
        Ok(Ok(body)) => body,
        Ok(Err(_)) | Err(_) => String::new(),
    }
}

fn parse_retry_after_secs(headers: &reqwest::header::HeaderMap) -> Option<u32> {
    headers
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u32>().ok())
}

/// Capped exponential backoff: 0.25 s, 0.5 s, 1.0 s. Keeps the per-pump
/// task wall-clock cost bounded even if Slack is degraded.
fn backoff(attempt: u8) -> Duration {
    let shift = attempt.min(3);
    let ms = 250u64.checked_shl(u32::from(shift)).unwrap_or(2_000);
    Duration::from_millis(ms.min(2_000))
}

// ──────────────────────────────────────────────────────────────────────
// Test fake
// ──────────────────────────────────────────────────────────────────────

/// Test poster that records every request and returns a stub `ts`.
/// Exposed (not `#[cfg(test)]`) so integration tests in `tests/` can
/// reach it.
pub struct FakeSlackPoster {
    inner: Mutex<FakeInner>,
}

struct FakeInner {
    posts: Vec<PostRequest>,
    next_ts_micros: u64,
}

impl FakeSlackPoster {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(FakeInner {
                posts: Vec::new(),
                next_ts_micros: 1,
            }),
        }
    }

    /// Snapshot every captured post in arrival order.
    #[must_use]
    pub fn captured(&self) -> Vec<PostRequest> {
        self.inner
            .lock()
            .expect("invariant: fake-poster mutex poisoned")
            .posts
            .clone()
    }

    /// Number of captured posts. Cheap helper for assertions.
    #[must_use]
    pub fn count(&self) -> usize {
        self.inner
            .lock()
            .expect("invariant: fake-poster mutex poisoned")
            .posts
            .len()
    }
}

impl Default for FakeSlackPoster {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for FakeSlackPoster {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FakeSlackPoster")
            .field("count", &self.count())
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl SlackPoster for FakeSlackPoster {
    async fn post(&self, req: PostRequest) -> Result<SlackTs, SlackError> {
        let mut guard = self
            .inner
            .lock()
            .expect("invariant: fake-poster mutex poisoned");
        guard.posts.push(req);
        let micros = guard.next_ts_micros;
        guard.next_ts_micros = guard.next_ts_micros.saturating_add(1);
        // Build a canonical "1700000000.<6-digit micros>" ts.
        let ts_str = format!("1700000000.{micros:06}");
        SlackTs::try_from(ts_str).map_err(SlackError::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn token() -> SlackBotToken {
        SlackBotToken::try_from("xoxb-test-12345".to_string()).expect("valid")
    }

    fn channel() -> SlackChannelId {
        SlackChannelId::try_from("C012345").expect("valid")
    }

    fn thread_ts() -> SlackThreadTs {
        SlackThreadTs::try_from("1234567890.000100").expect("valid")
    }

    #[tokio::test]
    async fn fake_poster_records_and_returns_ts() {
        let p = FakeSlackPoster::new();
        let ts1 = p
            .post(PostRequest {
                token: token(),
                channel: channel(),
                thread_ts: Some(thread_ts()),
                body: PostBody::Text("hello".to_owned()),
                username: "researcher".to_owned(),
                icon_url: None,
                ephemeral_to: None,
            })
            .await
            .expect("post");
        let ts2 = p
            .post(PostRequest {
                token: token(),
                channel: channel(),
                thread_ts: Some(thread_ts()),
                body: PostBody::Text("world".to_owned()),
                username: "critic".to_owned(),
                icon_url: None,
                ephemeral_to: None,
            })
            .await
            .expect("post");
        assert_ne!(ts1.as_str(), ts2.as_str());
        let captured = p.captured();
        assert_eq!(captured.len(), 2);
        assert_eq!(captured[0].body.fallback_text(), "hello");
        assert_eq!(captured[0].username, "researcher");
        assert_eq!(captured[1].body.fallback_text(), "world");
        assert_eq!(captured[1].username, "critic");
    }

    #[tokio::test]
    async fn fake_poster_records_blocks_body() {
        let p = FakeSlackPoster::new();
        let blocks = json!([{"type": "section", "text": {"type": "mrkdwn", "text": "hi"}}]);
        p.post(PostRequest {
            token: token(),
            channel: channel(),
            thread_ts: Some(thread_ts()),
            body: PostBody::Blocks {
                fallback_text: "connection requested".to_owned(),
                blocks: blocks.clone(),
            },
            username: "recruiter".to_owned(),
            icon_url: None,
            ephemeral_to: None,
        })
        .await
        .expect("post");
        let captured = p.captured();
        assert_eq!(captured.len(), 1);
        match &captured[0].body {
            PostBody::Blocks {
                fallback_text,
                blocks: b,
            } => {
                assert_eq!(fallback_text, "connection requested");
                assert_eq!(b, &blocks);
            }
            PostBody::Text(_) => panic!("expected Blocks variant"),
        }
    }

    #[test]
    fn backoff_is_bounded() {
        // Even an absurdly large attempt does not produce an unbounded
        // sleep — the cap is 2 s.
        assert!(backoff(0) < Duration::from_secs(2));
        assert!(backoff(255) <= Duration::from_secs(2));
    }

    #[test]
    fn wire_body_serialises_blocks_only_when_present() {
        let text_body = WirePostBody {
            channel: "C1",
            thread_ts: None,
            user: None,
            text: "hello",
            blocks: None,
            username: "agent",
            icon_url: None,
            unfurl_links: false,
            unfurl_media: false,
        };
        let json = serde_json::to_value(&text_body).expect("ser");
        assert!(
            json.get("blocks").is_none(),
            "text-only wire should omit blocks"
        );
        assert!(
            json.get("user").is_none(),
            "non-ephemeral wire should omit user"
        );

        let blocks_val = json!([{"type": "section"}]);
        let blocks_body = WirePostBody {
            channel: "C1",
            thread_ts: None,
            user: None,
            text: "fallback",
            blocks: Some(&blocks_val),
            username: "agent",
            icon_url: Some("https://example.com/avatar.png"),
            unfurl_links: false,
            unfurl_media: false,
        };
        let json = serde_json::to_value(&blocks_body).expect("ser");
        assert_eq!(json["blocks"], blocks_val);
        assert_eq!(json["text"], "fallback");
        assert_eq!(json["icon_url"], "https://example.com/avatar.png");
        // Slack rejects `blocks` if it's nested under another object —
        // regression guard for the prior `{ "blocks": [...] }` wrap.
        assert!(
            json["blocks"].is_array(),
            "wire-body blocks must be a JSON array, not an object"
        );
    }

    #[test]
    fn wire_body_serialises_user_only_for_ephemeral() {
        let ephemeral = WirePostBody {
            channel: "C1",
            thread_ts: None,
            user: Some("U0USER1"),
            text: "connect your account",
            blocks: None,
            username: "Patom",
            icon_url: None,
            unfurl_links: false,
            unfurl_media: false,
        };
        let json = serde_json::to_value(&ephemeral).expect("ser");
        assert_eq!(
            json["user"], "U0USER1",
            "chat.postEphemeral requires the target user on the wire"
        );
    }

    #[tokio::test]
    async fn fake_poster_records_ephemeral_target() {
        let p = FakeSlackPoster::new();
        let user = SlackUserId::try_from("U0USER1").expect("valid");
        p.post(PostRequest {
            token: token(),
            channel: channel(),
            thread_ts: None,
            body: PostBody::Text("run /patom".to_owned()),
            username: "Patom".to_owned(),
            icon_url: None,
            ephemeral_to: Some(user.clone()),
        })
        .await
        .expect("post");
        let captured = p.captured();
        assert_eq!(captured.len(), 1);
        assert_eq!(
            captured[0].ephemeral_to.as_ref().map(SlackUserId::as_str),
            Some(user.as_str()),
            "fake must preserve the ephemeral target for assertions"
        );
    }
}
