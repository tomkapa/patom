//! Pre-join history reader — `GET /channels/{id}/messages`.
//!
//! Live ingest (`MESSAGE_CREATE`) only reaches messages sent while the bot is
//! present. To give the agent the conversation that predates its join, this
//! reads the channel's backlog over REST on first access (a one-shot, gated by
//! `discord_threads.backfill_complete`). The REST read self-attributes — each
//! message carries its full author object — so backfilled rows mint the same
//! shadows as live ones, and the namespaced `idempotency_key` dedups any overlap
//! with live delivery. The orchestration (paging + mirroring) lives in the
//! bridge; this module is just the single REST page.
//!
//! `READ_MESSAGE_HISTORY` is required: without it the endpoint 403s (mapped to an
//! empty page + a warning, so backfill no-ops instead of failing).
//!
//! The `MESSAGE_CONTENT` privileged intent governs *content*, not access: the
//! request still succeeds without it, but Discord blanks `content` (and
//! `embeds` / `attachments` / `components`) on every message **except** the
//! bot's own, DMs it received, and messages that @mention it — the same
//! exceptions as the Gateway. (Per the official "Get Channel Messages" docs;
//! this restriction applies to the REST read too, not only Gateway events.) So
//! on an install without the intent, this reader still mints the shadow rows but
//! their bodies are empty for ambient messages — the digest read (#199) then
//! degrades to mention/DM content. Granting the intent is the prerequisite for
//! full ambient backfill.

use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use reqwest::Client;

use super::app_store::SharedBotTokenSource;
use super::error::DiscordError;
use super::event::InboundMessage;
use super::limits::DISCORD_POST_TIMEOUT;
use super::ratelimit::RateLimiter;
use super::types::{ApplicationId, ContainerId, DiscordMessageId};

#[async_trait]
pub trait HistoryReader: fmt::Debug + Send + Sync {
    /// Fetch one page of messages from `container_id`, strictly **before**
    /// `before` (newest-first, at most `limit`). An empty page means no more
    /// history (or no read permission — logged).
    async fn fetch_before(
        &self,
        application_id: &ApplicationId,
        container_id: &ContainerId,
        before: &DiscordMessageId,
        limit: usize,
    ) -> Result<Vec<InboundMessage>, DiscordError>;
}

pub type SharedHistoryReader = Arc<dyn HistoryReader>;

/// Production reader over the shared `reqwest::Client`.
pub struct HttpDiscordHistoryReader {
    http: Client,
    api_base: String,
    tokens: SharedBotTokenSource,
    limiter: Arc<RateLimiter>,
}

impl HttpDiscordHistoryReader {
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

impl fmt::Debug for HttpDiscordHistoryReader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpDiscordHistoryReader")
            .field("api_base", &self.api_base)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl HistoryReader for HttpDiscordHistoryReader {
    async fn fetch_before(
        &self,
        application_id: &ApplicationId,
        container_id: &ContainerId,
        before: &DiscordMessageId,
        limit: usize,
    ) -> Result<Vec<InboundMessage>, DiscordError> {
        let token = self.tokens.token(application_id).await?;
        let url = format!(
            "{}/channels/{}/messages?limit={}&before={}",
            self.api_base,
            container_id.as_str(),
            limit,
            before.as_str(),
        );
        self.limiter.acquire(application_id).await;
        let send = self
            .http
            .get(&url)
            .header("Authorization", format!("Bot {}", token.expose()))
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
        // 403 == missing READ_MESSAGE_HISTORY / VIEW_CHANNEL: treat as "no
        // readable history" (an empty page) rather than a hard failure, so a
        // mis-permissioned channel doesn't wedge the bridge.
        if status.as_u16() == 403 {
            tracing::warn!(
                channel = %container_id,
                event = "discord.history.forbidden",
                "messages 403 — bot likely lacks READ_MESSAGE_HISTORY",
            );
            return Ok(Vec::new());
        }
        if !status.is_success() {
            return Err(DiscordError::PostFailed {
                status: status.as_u16(),
                body: String::new(),
            });
        }
        let bytes = match tokio::time::timeout(DISCORD_POST_TIMEOUT, resp.bytes()).await {
            Ok(Ok(b)) => b,
            Ok(Err(e)) => return Err(DiscordError::Http(e)),
            Err(_) => return Err(DiscordError::PostTimeout(DISCORD_POST_TIMEOUT)),
        };
        parse_history_page(&bytes)
    }
}

/// Parse a `GET /channels/{id}/messages` body (a JSON array of message objects)
/// into `InboundMessage`s. A single malformed message is dropped (logged) so one
/// bad row doesn't lose the whole page.
fn parse_history_page(body: &[u8]) -> Result<Vec<InboundMessage>, DiscordError> {
    let values: Vec<serde_json::Value> = serde_json::from_slice(body)?;
    let mut out = Vec::with_capacity(values.len());
    for value in values {
        match serde_json::from_value::<InboundMessage>(value) {
            Ok(msg) => out.push(msg),
            Err(e) => tracing::warn!(error = %e, event = "discord.history.message_parse_failed"),
        }
    }
    Ok(out)
}

// ──────────────────────────────────────────────────────────────────────
// Test fake
// ──────────────────────────────────────────────────────────────────────

/// Test reader: returns a scripted backlog on the first `fetch_before`, then an
/// empty page (so the bounded paging loop terminates after one page).
pub struct FakeHistoryReader {
    pages: Mutex<std::collections::VecDeque<Vec<InboundMessage>>>,
}

impl FakeHistoryReader {
    /// A reader with nothing to backfill (every fetch is empty).
    #[must_use]
    pub fn empty() -> Self {
        Self {
            pages: Mutex::new(std::collections::VecDeque::new()),
        }
    }

    /// A reader that serves `pages` in order (each `fetch_before` pops one),
    /// then empty pages thereafter.
    #[must_use]
    pub fn with_pages(pages: Vec<Vec<InboundMessage>>) -> Self {
        Self {
            pages: Mutex::new(pages.into()),
        }
    }
}

impl fmt::Debug for FakeHistoryReader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FakeHistoryReader").finish_non_exhaustive()
    }
}

#[async_trait]
impl HistoryReader for FakeHistoryReader {
    async fn fetch_before(
        &self,
        _application_id: &ApplicationId,
        _container_id: &ContainerId,
        _before: &DiscordMessageId,
        _limit: usize,
    ) -> Result<Vec<InboundMessage>, DiscordError> {
        Ok(self
            .pages
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front()
            .unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_history_page_reads_messages_newest_first() {
        let body = br#"[
            {"id":"30","channel_id":"2","author":{"id":"5","username":"carol"},"content":"third"},
            {"id":"20","channel_id":"2","author":{"id":"4","username":"bob"},"content":"second"},
            {"id":"10","channel_id":"2","author":{"id":"3","username":"alice"},"content":"first"}
        ]"#;
        let page = parse_history_page(body).expect("parse");
        assert_eq!(page.len(), 3);
        // Discord returns newest-first; the orchestrator reverses for append.
        assert_eq!(page[0].message_id.as_str(), "30");
        assert_eq!(page[2].message_id.as_str(), "10");
        assert_eq!(page[2].content, "first");
    }

    #[test]
    fn parse_history_page_drops_one_bad_message() {
        let body = br#"[
            {"id":"20","channel_id":"2","author":{"id":"4","username":"bob"},"content":"ok"},
            {"id":"not-a-snowflake","channel_id":"2","author":{"id":"4","username":"x"}}
        ]"#;
        let page = parse_history_page(body).expect("parse");
        assert_eq!(
            page.len(),
            1,
            "the malformed message is dropped, the good one kept"
        );
        assert_eq!(page[0].message_id.as_str(), "20");
    }

    #[test]
    fn parse_history_page_empty_array() {
        assert!(parse_history_page(b"[]").expect("parse").is_empty());
    }

    #[tokio::test]
    async fn fake_reader_serves_pages_then_empty() {
        let app = ApplicationId::try_from("111111111111111111").expect("app");
        let chan = ContainerId::try_from("222222222222222222").expect("chan");
        let before = DiscordMessageId::try_from("999").expect("before");
        let msg: InboundMessage = serde_json::from_value(serde_json::json!({
            "id":"10","channel_id":"2","author":{"id":"3","username":"a"},"content":"hi"
        }))
        .expect("msg");
        let reader = FakeHistoryReader::with_pages(vec![vec![msg]]);
        let first = reader
            .fetch_before(&app, &chan, &before, 100)
            .await
            .expect("first");
        assert_eq!(first.len(), 1);
        let second = reader
            .fetch_before(&app, &chan, &before, 100)
            .await
            .expect("second");
        assert!(second.is_empty(), "subsequent fetches are empty");
    }
}
