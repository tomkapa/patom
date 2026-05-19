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

use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::time::sleep;

use super::error::SlackError;
use super::limits::{SLACK_POST_MAX_RETRIES, SLACK_POST_TIMEOUT};
use super::types::{SlackBotToken, SlackChannelId, SlackThreadTs, SlackTs};

/// What the bridge / stream pump hands to the poster.
#[derive(Debug, Clone)]
pub struct PostRequest {
    pub token: SlackBotToken,
    pub channel: SlackChannelId,
    pub thread_ts: SlackThreadTs,
    pub text: String,
    /// Per-message `username` override — the agent's name. Phase 1
    /// identity surface; later supplemented with `icon_url` (issue #43).
    pub username: String,
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
    thread_ts: &'a str,
    text: &'a str,
    username: &'a str,
    unfurl_links: bool,
    unfurl_media: bool,
}

#[derive(Deserialize)]
struct WirePostResponse {
    ok: bool,
    #[serde(default)]
    ts: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

#[async_trait]
impl SlackPoster for HttpSlackPoster {
    async fn post(&self, req: PostRequest) -> Result<SlackTs, SlackError> {
        let body = WirePostBody {
            channel: req.channel.as_str(),
            thread_ts: req.thread_ts.as_str(),
            text: &req.text,
            username: &req.username,
            // Bot replies should never expand link previews; the agent's
            // text is the message, attachments would be visual noise.
            unfurl_links: false,
            unfurl_media: false,
        };

        let mut attempt: u8 = 0;
        loop {
            let send = self
                .http
                .post(SLACK_POST_URL)
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
                let retry_after = parse_retry_after_secs(resp.headers()).unwrap_or(1);
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
                    let body = resp.text().await.unwrap_or_default();
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
                let body = resp.text().await.unwrap_or_default();
                return Err(SlackError::PostFailed {
                    status: status.as_u16(),
                    body,
                });
            }

            // 2xx — Slack signals success/failure inside the JSON body.
            let parsed: WirePostResponse = resp.json().await?;
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
            let ts_str = parsed.ts.ok_or_else(|| {
                SlackError::Internal("chat.postMessage 200 without ts".to_owned())
            })?;
            return Ok(SlackTs::try_from(ts_str)?);
        }
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
                thread_ts: thread_ts(),
                text: "hello".to_owned(),
                username: "researcher".to_owned(),
            })
            .await
            .expect("post");
        let ts2 = p
            .post(PostRequest {
                token: token(),
                channel: channel(),
                thread_ts: thread_ts(),
                text: "world".to_owned(),
                username: "critic".to_owned(),
            })
            .await
            .expect("post");
        assert_ne!(ts1.as_str(), ts2.as_str());
        let captured = p.captured();
        assert_eq!(captured.len(), 2);
        assert_eq!(captured[0].text, "hello");
        assert_eq!(captured[0].username, "researcher");
        assert_eq!(captured[1].text, "world");
        assert_eq!(captured[1].username, "critic");
    }

    #[test]
    fn backoff_is_bounded() {
        // Even an absurdly large attempt does not produce an unbounded
        // sleep — the cap is 2 s.
        assert!(backoff(0) < Duration::from_secs(2));
        assert!(backoff(255) <= Duration::from_secs(2));
    }
}
