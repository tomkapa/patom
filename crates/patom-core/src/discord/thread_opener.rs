//! Opens a Discord thread on a message — `POST /channels/{ch}/messages/{msg}/threads`.
//!
//! When a member `@`-mentions the bot at a channel's **top level**, the bridge
//! opens a thread rooted on that message and the whole exchange lives there,
//! keeping the channel clean. This is the single REST seam for that; the
//! orchestration (when to open, how to bind the Patom thread) lives in the
//! bridge.
//!
//! Any error is the caller's signal to **fall back to a plain channel reply** —
//! opening a thread can legitimately fail (a message already inside a thread, a
//! forum channel, missing `CREATE_PUBLIC_THREADS`), so the bridge treats every
//! `Err` as "can't thread → degrade", never a hard failure.
//!
//! Two impls mirror [`crate::discord::history`]:
//! - [`HttpDiscordThreadOpener`]: production, over the shared `reqwest::Client`
//!   and the same [`RateLimiter`] as the poster/history reader (one egress
//!   budget). One request, no retry — a transient failure just falls back.
//! - [`FakeThreadOpener`]: returns a configured thread id (or an error) and
//!   records every call, for the bridge tests.

use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};

use super::app_store::SharedBotTokenSource;
use super::error::DiscordError;
use super::limits::{
    DISCORD_POST_TIMEOUT, DISCORD_RETRY_AFTER_CAP_SECS, DISCORD_THREAD_AUTO_ARCHIVE_MINUTES,
    DISCORD_THREAD_NAME_MAX,
};
use super::ratelimit::RateLimiter;
use super::types::{ApplicationId, ContainerId, DiscordMessageId};

#[async_trait]
pub trait ThreadOpener: fmt::Debug + Send + Sync {
    /// Open a thread on `message_id` in `channel_id`, named `name`, and return
    /// the new thread's container id (a Discord thread IS a channel). An `Err`
    /// means the caller should fall back to a plain channel reply.
    async fn open_from_message(
        &self,
        application_id: &ApplicationId,
        channel_id: &ContainerId,
        message_id: &DiscordMessageId,
        name: &str,
    ) -> Result<ContainerId, DiscordError>;
}

pub type SharedThreadOpener = Arc<dyn ThreadOpener>;

// ──────────────────────────────────────────────────────────────────────
// Production impl
// ──────────────────────────────────────────────────────────────────────

/// Production opener over the shared `reqwest::Client`.
pub struct HttpDiscordThreadOpener {
    http: Client,
    api_base: String,
    tokens: SharedBotTokenSource,
    limiter: Arc<RateLimiter>,
}

impl HttpDiscordThreadOpener {
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

impl fmt::Debug for HttpDiscordThreadOpener {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpDiscordThreadOpener")
            .field("api_base", &self.api_base)
            .finish_non_exhaustive()
    }
}

#[derive(Serialize)]
struct WireCreateThread<'a> {
    name: &'a str,
    auto_archive_duration: u32,
}

#[derive(Deserialize)]
struct WireThreadResponse {
    id: String,
}

#[async_trait]
impl ThreadOpener for HttpDiscordThreadOpener {
    async fn open_from_message(
        &self,
        application_id: &ApplicationId,
        channel_id: &ContainerId,
        message_id: &DiscordMessageId,
        name: &str,
    ) -> Result<ContainerId, DiscordError> {
        // The caller (`bridge::thread_name`) guarantees a non-empty, ≤100-char
        // name; a violation is a programmer error, not an operating condition.
        assert!(!name.is_empty(), "invariant: thread name must be non-empty");
        assert!(
            name.chars().count() <= DISCORD_THREAD_NAME_MAX,
            "invariant: thread name exceeds the Discord cap"
        );
        if self.limiter.invalid_budget_exhausted() {
            // A Cloudflare ban is imminent/active on the shared egress; do not
            // add to the invalid-request count.
            return Err(DiscordError::RateLimited {
                retry_after_secs: DISCORD_RETRY_AFTER_CAP_SECS,
            });
        }
        let token = self.tokens.token(application_id).await?;
        let url = format!(
            "{}/channels/{}/messages/{}/threads",
            self.api_base,
            channel_id.as_str(),
            message_id.as_str()
        );
        let body = serde_json::to_vec(&WireCreateThread {
            name,
            auto_archive_duration: DISCORD_THREAD_AUTO_ARCHIVE_MINUTES,
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
        parse_created_thread_id(&bytes)
    }
}

/// Parse the created-thread response (a channel object) into its container id.
fn parse_created_thread_id(bytes: &[u8]) -> Result<ContainerId, DiscordError> {
    let parsed: WireThreadResponse = serde_json::from_slice(bytes)?;
    Ok(ContainerId::try_from(parsed.id)?)
}

/// Read up to 512 chars of an error body (bounded), best-effort.
async fn read_text(resp: reqwest::Response) -> String {
    match tokio::time::timeout(DISCORD_POST_TIMEOUT, resp.text()).await {
        Ok(Ok(body)) => body.chars().take(512).collect(),
        Ok(Err(_)) | Err(_) => String::new(),
    }
}

// ──────────────────────────────────────────────────────────────────────
// Test fake
// ──────────────────────────────────────────────────────────────────────

/// Records every `(channel, message, name)` open call.
///
/// Returns a configured thread id, or — when built via
/// [`FakeThreadOpener::failing`] — an error so a test can exercise the bridge's
/// channel-reply fallback.
pub struct FakeThreadOpener {
    /// `Ok(id)` returns that thread id; `Err(())` returns a `PostFailed`.
    outcome: Result<ContainerId, ()>,
    calls: Mutex<Vec<(ContainerId, DiscordMessageId, String)>>,
}

impl FakeThreadOpener {
    /// An opener that always returns `thread_id`.
    #[must_use]
    pub fn returning(thread_id: ContainerId) -> Self {
        Self {
            outcome: Ok(thread_id),
            calls: Mutex::new(Vec::new()),
        }
    }

    /// An opener that always fails (Discord can't start a thread here) — drives
    /// the bridge's fall-back-to-channel-reply path.
    #[must_use]
    pub fn failing() -> Self {
        Self {
            outcome: Err(()),
            calls: Mutex::new(Vec::new()),
        }
    }

    /// How many times `open_from_message` was called.
    #[must_use]
    pub fn call_count(&self) -> usize {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// The recorded `(channel, message, name)` calls in order.
    #[must_use]
    pub fn calls(&self) -> Vec<(ContainerId, DiscordMessageId, String)> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl fmt::Debug for FakeThreadOpener {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FakeThreadOpener")
            .field("calls", &self.call_count())
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl ThreadOpener for FakeThreadOpener {
    async fn open_from_message(
        &self,
        _application_id: &ApplicationId,
        channel_id: &ContainerId,
        message_id: &DiscordMessageId,
        name: &str,
    ) -> Result<ContainerId, DiscordError> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((channel_id.clone(), message_id.clone(), name.to_owned()));
        self.outcome.clone().map_err(|()| DiscordError::PostFailed {
            status: 400,
            body: "fake thread-open failure".to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_created_thread_id_from_channel_object() {
        // The create-thread endpoint returns the new thread as a channel object.
        let body = serde_json::json!({
            "id": "555000000000000001",
            "type": 11,
            "parent_id": "333333333333333333",
            "name": "draft a JD",
        })
        .to_string();
        let id = parse_created_thread_id(body.as_bytes()).expect("parse");
        assert_eq!(id.as_str(), "555000000000000001");
    }

    #[test]
    fn rejects_non_snowflake_thread_id() {
        let body = serde_json::json!({ "id": "not-a-snowflake" }).to_string();
        assert!(parse_created_thread_id(body.as_bytes()).is_err());
    }

    #[tokio::test]
    async fn fake_returning_records_call_and_returns_id() {
        let app = ApplicationId::try_from("111111111111111111").expect("app");
        let chan = ContainerId::try_from("333333333333333333").expect("chan");
        let msg = DiscordMessageId::try_from("444444444444444444").expect("msg");
        let want = ContainerId::try_from("555000000000000001").expect("thread");
        let opener = FakeThreadOpener::returning(want.clone());

        let got = opener
            .open_from_message(&app, &chan, &msg, "draft a JD")
            .await
            .expect("open");

        assert_eq!(got, want);
        assert_eq!(opener.call_count(), 1);
        let calls = opener.calls();
        assert_eq!(calls[0].0, chan);
        assert_eq!(calls[0].2, "draft a JD");
    }

    #[tokio::test]
    async fn fake_failing_returns_err_but_still_records() {
        let app = ApplicationId::try_from("111111111111111111").expect("app");
        let chan = ContainerId::try_from("333333333333333333").expect("chan");
        let msg = DiscordMessageId::try_from("444444444444444444").expect("msg");
        let opener = FakeThreadOpener::failing();

        let result = opener.open_from_message(&app, &chan, &msg, "x").await;

        assert!(
            result.is_err(),
            "failing opener errors so the bridge falls back"
        );
        assert_eq!(opener.call_count(), 1, "the call is still recorded");
    }
}
