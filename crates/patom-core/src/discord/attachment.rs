//! Downloading inbound Discord message attachments.
//!
//! A `MESSAGE_CREATE` carries each attachment's signed `cdn.discordapp.com`
//! URL, valid when the Gateway delivers the message and needing no auth. The
//! bridge fetches those bytes (size- and time-bounded) and re-hosts the
//! supported ones in the asset store as model input.
//!
//! [`AttachmentFetcher`] is a trait so the bridge is testable without real HTTP:
//! production uses [`HttpAttachmentFetcher`] over the shared `reqwest::Client`,
//! tests inject [`FakeAttachmentFetcher`].

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;

use crate::provider::ingest::{FetchedBytes, get_capped};
use crate::provider::limits::MAX_ATTACHMENT_FILE_BYTES;

use super::error::DiscordError;

/// Fetches the bytes behind a Discord attachment CDN URL.
#[async_trait]
pub trait AttachmentFetcher: fmt::Debug + Send + Sync {
    /// GET `url`, bounded in size and time. The URL is self-authorizing (signed
    /// CDN link), so no credentials are attached.
    async fn fetch(&self, url: &str) -> Result<FetchedBytes, DiscordError>;
}

/// Cheap-clone handle to an [`AttachmentFetcher`].
pub type SharedAttachmentFetcher = Arc<dyn AttachmentFetcher>;

/// Production fetcher over the shared `reqwest::Client`.
#[derive(Debug, Clone)]
pub struct HttpAttachmentFetcher {
    http: reqwest::Client,
}

impl HttpAttachmentFetcher {
    #[must_use]
    pub fn new(http: reqwest::Client) -> Self {
        Self { http }
    }
}

#[async_trait]
impl AttachmentFetcher for HttpAttachmentFetcher {
    async fn fetch(&self, url: &str) -> Result<FetchedBytes, DiscordError> {
        get_capped(self.http.get(url), MAX_ATTACHMENT_FILE_BYTES)
            .await
            .map_err(|e| DiscordError::AttachmentFetch(e.to_string()))
    }
}

/// Canned `(bytes, content_type)` keyed by attachment URL.
type StubMap = std::sync::Mutex<std::collections::HashMap<String, (Vec<u8>, Option<String>)>>;

/// Test fetcher returning canned bytes per URL.
///
/// Not `#[cfg(test)]` so the integration tests in `tests/` can inject it. A miss
/// is an error, so a test that registers nothing exercises the "no attachments"
/// path without any network.
#[derive(Debug, Default)]
pub struct FakeAttachmentFetcher {
    items: StubMap,
}

impl FakeAttachmentFetcher {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `bytes` (and an optional response `Content-Type`) for `url`.
    #[must_use]
    pub fn with(self, url: &str, bytes: Vec<u8>, content_type: Option<&str>) -> Self {
        self.insert(url, bytes, content_type);
        self
    }

    /// Register canned bytes for `url` after construction (for tests that build
    /// the fetcher first, then populate it).
    pub fn insert(&self, url: &str, bytes: Vec<u8>, content_type: Option<&str>) {
        self.items
            .lock()
            .expect("invariant: fake-fetcher mutex poisoned")
            .insert(url.to_owned(), (bytes, content_type.map(ToOwned::to_owned)));
    }
}

#[async_trait]
impl AttachmentFetcher for FakeAttachmentFetcher {
    async fn fetch(&self, url: &str) -> Result<FetchedBytes, DiscordError> {
        let (bytes, content_type) = self
            .items
            .lock()
            .expect("invariant: fake-fetcher mutex poisoned")
            .get(url)
            .cloned()
            .ok_or_else(|| DiscordError::AttachmentFetch(format!("no stub for {url}")))?;
        Ok(FetchedBytes {
            bytes: bytes.into(),
            content_type,
        })
    }
}
