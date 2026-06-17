//! Outbound `im/v1/messages` sender for the Lark adapter.
//!
//! One trait — [`LarkPoster`] — fronts every message Patom sends a Lark
//! tenant. Two impls:
//!
//! - [`HttpLarkPoster`]: production. Uses the shared `reqwest::Client` to
//!   `POST {api_base}/open-apis/im/v1/messages` (or the per-message
//!   `.../{message_id}/reply` endpoint when threading a reply) with a
//!   `tenant_access_token` bearer. Honours `Retry-After` on HTTP 429 and
//!   does bounded exponential backoff on 5xx, mirroring the Slack poster.
//!
//! - [`FakeLarkPoster`]: records every [`PostRequest`] it sees without
//!   touching the network. Lives next to the production impl (not under
//!   `#[cfg(test)]`) because the e2e integration test in `tests/` is a
//!   separate crate.
//!
//! ## Wire shape
//!
//! Lark wraps the textual payload twice: the outer body carries
//! `msg_type: "text"`, and `content` is itself a JSON *string* holding
//! `{"text": "..."}`. We build the inner object with `serde_json::json!`
//! and serialise it to a string so escaping stays correct.

use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::time::sleep;

use super::error::LarkError;
use super::limits::{
    LARK_MAX_POST_CHARS, LARK_POST_MAX_RETRIES, LARK_POST_TIMEOUT, LARK_RETRY_AFTER_CAP_SECS,
};
use super::token::TenantAccessToken;
use super::types::{LarkChatId, LarkMessageId, LarkOpenId};

/// What the bridge / stream pump hands to the poster.
///
/// `reply_to` is `Some(_)` when the message must thread under an existing
/// Lark message (the common agent-reply case); the poster then targets the
/// `.../{message_id}/reply` endpoint so the post lands inside that thread.
/// `None` is a fresh top-level message in `chat_id`.
#[derive(Debug, Clone)]
pub struct PostRequest {
    pub token: TenantAccessToken,
    pub chat_id: LarkChatId,
    pub reply_to: Option<LarkMessageId>,
    pub text: String,
}

#[async_trait]
pub trait LarkPoster: fmt::Debug + Send + Sync {
    /// Post a single message. Returns the Lark-issued `message_id` on success.
    async fn post(&self, req: PostRequest) -> Result<LarkMessageId, LarkError>;

    /// Send a DM to a recipient by `open_id` (`receive_id_type=open_id`), which
    /// Lark routes to the (auto-created) p2p chat — no chat pre-creation. Used
    /// by the outbound router's DM arm (#178). No threading: a DM is flat.
    async fn post_dm(
        &self,
        token: TenantAccessToken,
        open_id: &LarkOpenId,
        text: &str,
    ) -> Result<LarkMessageId, LarkError>;
}

/// Shared handle to a [`LarkPoster`].
pub type SharedLarkPoster = Arc<dyn LarkPoster>;

// ──────────────────────────────────────────────────────────────────────
// Production impl
// ──────────────────────────────────────────────────────────────────────

const CREATE_PATH: &str = "/open-apis/im/v1/messages?receive_id_type=chat_id";
const CREATE_OPEN_PATH: &str = "/open-apis/im/v1/messages?receive_id_type=open_id";
const REPLY_PATH_PREFIX: &str = "/open-apis/im/v1/messages/";
const REPLY_PATH_SUFFIX: &str = "/reply";

/// Production poster over the shared `reqwest::Client`.
pub struct HttpLarkPoster {
    http: Client,
    api_base: String,
}

impl HttpLarkPoster {
    #[must_use]
    pub fn new(http: Client, api_base: String) -> Self {
        Self { http, api_base }
    }
}

impl fmt::Debug for HttpLarkPoster {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpLarkPoster")
            .field("api_base", &self.api_base)
            .finish_non_exhaustive()
    }
}

/// Wire body for the `create` endpoint — `receive_id` names the chat.
#[derive(Serialize)]
struct WireCreateBody<'a> {
    receive_id: &'a str,
    msg_type: &'a str,
    content: &'a str,
}

/// Wire body for the `reply` endpoint — the parent message id is in the path.
#[derive(Serialize)]
struct WireReplyBody<'a> {
    msg_type: &'a str,
    content: &'a str,
    /// Reply **into the thread/topic** rather than as an inline quote-reply.
    /// Lark defaults this to `false` (a "Reply to X" quote at channel top
    /// level); we always thread the agent's reply under the triggering message.
    reply_in_thread: bool,
}

#[derive(Deserialize)]
struct WirePostResponse {
    code: i64,
    #[serde(default)]
    msg: String,
    #[serde(default)]
    data: Option<WirePostData>,
}

#[derive(Deserialize)]
struct WirePostData {
    #[serde(default)]
    message_id: Option<String>,
}

#[async_trait]
impl LarkPoster for HttpLarkPoster {
    async fn post(&self, req: PostRequest) -> Result<LarkMessageId, LarkError> {
        // Lark wraps the text payload in a JSON *string* under `content`.
        let clipped = clip_chars(&req.text, LARK_MAX_POST_CHARS);
        let content = json!({ "text": clipped }).to_string();

        let (url, json_body) = if let Some(parent) = req.reply_to.as_ref() {
            let url = format!(
                "{}{REPLY_PATH_PREFIX}{}{REPLY_PATH_SUFFIX}",
                self.api_base,
                parent.as_str()
            );
            let body = serde_json::to_vec(&WireReplyBody {
                msg_type: "text",
                content: &content,
                reply_in_thread: true,
            })?;
            (url, body)
        } else {
            let url = format!("{}{CREATE_PATH}", self.api_base);
            let body = serde_json::to_vec(&WireCreateBody {
                receive_id: req.chat_id.as_str(),
                msg_type: "text",
                content: &content,
            })?;
            (url, body)
        };
        self.send_with_retry(&url, &json_body, &req.token).await
    }

    async fn post_dm(
        &self,
        token: TenantAccessToken,
        open_id: &LarkOpenId,
        text: &str,
    ) -> Result<LarkMessageId, LarkError> {
        let clipped = clip_chars(text, LARK_MAX_POST_CHARS);
        let content = json!({ "text": clipped }).to_string();
        let url = format!("{}{CREATE_OPEN_PATH}", self.api_base);
        let body = serde_json::to_vec(&WireCreateBody {
            receive_id: open_id.as_str(),
            msg_type: "text",
            content: &content,
        })?;
        self.send_with_retry(&url, &body, &token).await
    }
}

impl HttpLarkPoster {
    /// POST `json_body` to `url` with the bot bearer, retrying 429 (Retry-After)
    /// and 5xx (bounded backoff). Shared by `post` and `post_dm`.
    async fn send_with_retry(
        &self,
        url: &str,
        json_body: &[u8],
        token: &TenantAccessToken,
    ) -> Result<LarkMessageId, LarkError> {
        let mut attempt: u8 = 0;
        loop {
            assert!(
                attempt <= LARK_POST_MAX_RETRIES,
                "retry counter overran cap"
            );
            let send = self
                .http
                .post(url)
                .bearer_auth(token.expose())
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(json_body.to_vec())
                .send();
            let resp = match tokio::time::timeout(LARK_POST_TIMEOUT, send).await {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => return Err(e.into()),
                Err(_) => return Err(LarkError::PostTimeout(LARK_POST_TIMEOUT)),
            };

            let status = resp.status();
            // Retry on 429 (Retry-After) and 5xx.
            if status.as_u16() == 429 {
                let raw_retry_after = parse_retry_after_secs(resp.headers()).unwrap_or(1);
                // Cap the upstream-supplied header value so a hostile or
                // misbehaving proxy cannot stall us indefinitely.
                let retry_after = raw_retry_after.min(LARK_RETRY_AFTER_CAP_SECS);
                if attempt >= LARK_POST_MAX_RETRIES {
                    return Err(LarkError::RateLimited {
                        retry_after_secs: retry_after,
                    });
                }
                attempt = attempt.saturating_add(1);
                sleep(Duration::from_secs(u64::from(retry_after))).await;
                continue;
            }
            if status.is_server_error() {
                if attempt >= LARK_POST_MAX_RETRIES {
                    let body = read_text_with_timeout(resp).await;
                    return Err(LarkError::PostFailed {
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
                return Err(LarkError::PostFailed {
                    status: status.as_u16(),
                    body,
                });
            }

            // 2xx — Lark signals success/failure inside the JSON body via `code`.
            let bytes = match tokio::time::timeout(LARK_POST_TIMEOUT, resp.bytes()).await {
                Ok(Ok(b)) => b,
                Ok(Err(e)) => return Err(LarkError::Http(e)),
                Err(_) => return Err(LarkError::PostTimeout(LARK_POST_TIMEOUT)),
            };
            return parse_post_response(&bytes);
        }
    }
}

/// Pure parse of a 2xx `im/v1/messages` body into the issued message id.
///
/// A non-zero `code` is the documented Lark application-error path that rides
/// inside an HTTP 200, so it maps to [`LarkError::PostFailed`] rather than a
/// transport error.
fn parse_post_response(body: &[u8]) -> Result<LarkMessageId, LarkError> {
    let parsed: WirePostResponse = serde_json::from_slice(body)?;
    if parsed.code != 0 {
        let detail = if parsed.msg.is_empty() {
            format!("im/v1/messages code {}", parsed.code)
        } else {
            parsed.msg
        };
        return Err(LarkError::PostFailed {
            status: 200,
            body: detail,
        });
    }
    let id = parsed
        .data
        .and_then(|d| d.message_id)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| LarkError::Internal("im/v1/messages ok without message_id".to_owned()))?;
    Ok(LarkMessageId::try_from(id)?)
}

/// Clip `text` to at most `max_chars` characters on a UTF-8 boundary.
///
/// Borrows when no clipping is needed so the common short-message path
/// allocates nothing.
fn clip_chars(text: &str, max_chars: usize) -> &str {
    match text.char_indices().nth(max_chars) {
        Some((end, _)) => &text[..end],
        None => text,
    }
}

/// Best-effort body read for diagnostic logging — bounded by
/// [`LARK_POST_TIMEOUT`]. On timeout or transport failure we surface an empty
/// body rather than blocking the pump, since the caller has already classified
/// the response as a failure.
async fn read_text_with_timeout(resp: reqwest::Response) -> String {
    match tokio::time::timeout(LARK_POST_TIMEOUT, resp.text()).await {
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

/// Capped exponential backoff: 0.25 s, 0.5 s, 1.0 s. Keeps the per-pump task
/// wall-clock cost bounded even if Lark is degraded.
fn backoff(attempt: u8) -> Duration {
    let shift = attempt.min(3);
    let ms = 250u64.checked_shl(u32::from(shift)).unwrap_or(2_000);
    Duration::from_millis(ms.min(2_000))
}

// ──────────────────────────────────────────────────────────────────────
// Test fake
// ──────────────────────────────────────────────────────────────────────

/// Test poster that records every request and returns a fixed stub id.
///
/// Exposed (not `#[cfg(test)]`) so integration tests in `tests/` can reach it.
pub struct FakeLarkPoster {
    inner: Mutex<Vec<PostRequest>>,
    dms: Mutex<Vec<(LarkOpenId, String)>>,
}

impl FakeLarkPoster {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Vec::new()),
            dms: Mutex::new(Vec::new()),
        }
    }

    /// Snapshot every captured post in arrival order.
    #[must_use]
    pub fn captured(&self) -> Vec<PostRequest> {
        self.inner
            .lock()
            .expect("invariant: fake-poster mutex poisoned")
            .clone()
    }

    /// Number of captured posts. Cheap helper for assertions.
    #[must_use]
    pub fn count(&self) -> usize {
        self.inner
            .lock()
            .expect("invariant: fake-poster mutex poisoned")
            .len()
    }

    /// Every `(open_id, text)` a `post_dm` was sent for.
    #[must_use]
    pub fn dms(&self) -> Vec<(LarkOpenId, String)> {
        self.dms
            .lock()
            .expect("invariant: fake-poster mutex poisoned")
            .clone()
    }
}

impl Default for FakeLarkPoster {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for FakeLarkPoster {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FakeLarkPoster")
            .field("count", &self.count())
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl LarkPoster for FakeLarkPoster {
    async fn post(&self, req: PostRequest) -> Result<LarkMessageId, LarkError> {
        self.inner
            .lock()
            .expect("invariant: fake-poster mutex poisoned")
            .push(req);
        Ok(LarkMessageId::try_from("om_fake_stub_message_id")?)
    }

    async fn post_dm(
        &self,
        _token: TenantAccessToken,
        open_id: &LarkOpenId,
        text: &str,
    ) -> Result<LarkMessageId, LarkError> {
        self.dms
            .lock()
            .expect("invariant: fake-poster mutex poisoned")
            .push((open_id.clone(), text.to_owned()));
        Ok(LarkMessageId::try_from("om_fake_stub_message_id")?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    async fn token() -> TenantAccessToken {
        // The token's internal fields are private; the `FakeTokenProvider`
        // mints a never-expiring one so request construction never re-mints.
        use super::super::token::TokenProvider;
        let provider = super::super::token::FakeTokenProvider::new("t-test");
        let app = super::super::types::LarkAppId::try_from("cli_test123").expect("valid app id");
        provider.token(&app).await.expect("fake token")
    }

    fn chat() -> LarkChatId {
        LarkChatId::try_from("oc_chat123").expect("valid chat id")
    }

    fn message() -> LarkMessageId {
        LarkMessageId::try_from("om_parent123").expect("valid message id")
    }

    #[tokio::test]
    async fn fake_poster_records_and_returns_message_id() {
        let p = FakeLarkPoster::new();
        let id1 = p
            .post(PostRequest {
                token: token().await,
                chat_id: chat(),
                reply_to: None,
                text: "hello".to_owned(),
            })
            .await
            .expect("post");
        let id2 = p
            .post(PostRequest {
                token: token().await,
                chat_id: chat(),
                reply_to: Some(message()),
                text: "world".to_owned(),
            })
            .await
            .expect("post");
        assert_eq!(id1.as_str(), "om_fake_stub_message_id");
        assert_eq!(id2.as_str(), "om_fake_stub_message_id");
        let captured = p.captured();
        assert_eq!(captured.len(), 2);
        assert_eq!(p.count(), 2);
        assert_eq!(captured[0].text, "hello");
        assert!(captured[0].reply_to.is_none());
        assert_eq!(captured[1].text, "world");
        assert_eq!(
            captured[1].reply_to.as_ref().map(LarkMessageId::as_str),
            Some("om_parent123"),
            "fake must preserve the reply target for assertions"
        );
    }

    #[test]
    fn parse_post_response_extracts_message_id() {
        let body = br#"{"code":0,"msg":"success","data":{"message_id":"om_abc123"}}"#;
        let id = parse_post_response(body).expect("ok");
        assert_eq!(id.as_str(), "om_abc123");
    }

    #[test]
    fn parse_post_response_non_zero_code_is_post_failed() {
        let body = br#"{"code":230001,"msg":"invalid receive_id"}"#;
        let err = parse_post_response(body).expect_err("err");
        assert!(matches!(
            err,
            LarkError::PostFailed { status, body } if status == 200 && body == "invalid receive_id"
        ));
    }

    #[test]
    fn parse_post_response_ok_without_message_id_is_internal() {
        let body = br#"{"code":0,"msg":"ok","data":{}}"#;
        let err = parse_post_response(body).expect_err("err");
        assert!(matches!(err, LarkError::Internal(_)));
    }

    #[test]
    fn clip_chars_clips_on_char_boundary() {
        // Multi-byte chars must not be split mid-codepoint.
        let s = "héllo wörld";
        assert_eq!(clip_chars(s, 4), "héll");
        assert_eq!(clip_chars(s, 100), s);
        assert_eq!(clip_chars(s, 0), "");
    }

    #[test]
    fn clip_chars_respects_max_post_chars() {
        let big = "x".repeat(LARK_MAX_POST_CHARS + 50);
        let clipped = clip_chars(&big, LARK_MAX_POST_CHARS);
        assert_eq!(clipped.chars().count(), LARK_MAX_POST_CHARS);
    }

    #[test]
    fn backoff_is_bounded() {
        // Even an absurdly large attempt does not produce an unbounded sleep —
        // the cap is 2 s.
        assert!(backoff(0) < Duration::from_secs(2));
        assert!(backoff(255) <= Duration::from_secs(2));
    }

    #[test]
    fn create_wire_body_serialises_receive_id() {
        let inner = json!({ "text": "hi there" }).to_string();
        let body = WireCreateBody {
            receive_id: "oc_chat123",
            msg_type: "text",
            content: &inner,
        };
        let value = serde_json::to_value(&body).expect("ser");
        assert_eq!(value["receive_id"], "oc_chat123");
        assert_eq!(value["msg_type"], "text");
        // `content` is a JSON *string*, not a nested object.
        assert!(value["content"].is_string());
        assert_eq!(value["content"], "{\"text\":\"hi there\"}");
    }

    #[test]
    fn reply_wire_body_omits_receive_id() {
        let inner = json!({ "text": "reply" }).to_string();
        let body = WireReplyBody {
            msg_type: "text",
            content: &inner,
            reply_in_thread: true,
        };
        let value = serde_json::to_value(&body).expect("ser");
        assert!(
            value.get("receive_id").is_none(),
            "reply wire body addresses the parent via the URL path, not receive_id"
        );
        assert_eq!(
            value
                .get("reply_in_thread")
                .and_then(serde_json::Value::as_bool),
            Some(true),
            "the reply must thread, not quote-reply at channel top level",
        );
        assert!(value["content"].is_string());
    }

    #[tokio::test]
    async fn fake_token_is_fresh() {
        // Guard the test helper: the fake token must read as fresh so request
        // construction never re-mints inside these unit tests.
        let t = token().await;
        assert!(t.is_fresh(Instant::now()));
    }
}
