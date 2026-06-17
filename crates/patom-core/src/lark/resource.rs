//! Downloading inbound Lark message resources (images and files).
//!
//! Unlike Discord, a Lark message does not carry a download URL — it carries an
//! `image_key`/`file_key`. The bytes are pulled from
//! `GET /open-apis/im/v1/messages/{message_id}/resources/{file_key}?type=…`
//! with a tenant-access-token bearer. The `type` query param is `image` for an
//! image key and `file` for everything else.
//!
//! [`ResourceFetcher`] is a trait so the bridge is testable without real HTTP:
//! production uses [`HttpResourceFetcher`] over the shared `reqwest::Client`,
//! tests inject [`FakeResourceFetcher`].

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;

use crate::provider::ingest::{FetchedBytes, get_capped};
use crate::provider::limits::MAX_ATTACHMENT_FILE_BYTES;

use super::error::LarkError;

/// Which resource bucket a key lives in — selects the `?type=` query param.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LarkResourceKind {
    /// An `image_key` (standalone image message or a post-embedded image).
    Image,
    /// A `file_key` (file message). Audio/video share this bucket but are not
    /// requested — the model can't consume them.
    File,
}

impl LarkResourceKind {
    /// The `type` query-param value the resource endpoint requires.
    #[must_use]
    pub const fn type_param(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::File => "file",
        }
    }
}

/// Fetches the bytes behind a Lark message resource key.
#[async_trait]
pub trait ResourceFetcher: fmt::Debug + Send + Sync {
    /// GET the resource `file_key` of `message_id`, bounded in size and time.
    /// `token` is a tenant-access-token secret used as the bearer.
    async fn fetch(
        &self,
        token: &str,
        message_id: &str,
        file_key: &str,
        kind: LarkResourceKind,
    ) -> Result<FetchedBytes, LarkError>;
}

/// Cheap-clone handle to a [`ResourceFetcher`].
pub type SharedResourceFetcher = Arc<dyn ResourceFetcher>;

/// Production fetcher over the shared `reqwest::Client`.
#[derive(Debug, Clone)]
pub struct HttpResourceFetcher {
    http: reqwest::Client,
    api_base: String,
}

impl HttpResourceFetcher {
    #[must_use]
    pub fn new(http: reqwest::Client, api_base: String) -> Self {
        Self { http, api_base }
    }
}

#[async_trait]
impl ResourceFetcher for HttpResourceFetcher {
    async fn fetch(
        &self,
        token: &str,
        message_id: &str,
        file_key: &str,
        kind: LarkResourceKind,
    ) -> Result<FetchedBytes, LarkError> {
        let url = format!(
            "{base}/open-apis/im/v1/messages/{mid}/resources/{key}",
            base = self.api_base,
            mid = message_id,
            key = file_key,
        );
        let req = self
            .http
            .get(&url)
            .query(&[("type", kind.type_param())])
            .bearer_auth(token);
        get_capped(req, MAX_ATTACHMENT_FILE_BYTES)
            .await
            .map_err(|e| LarkError::ResourceFetch(e.to_string()))
    }
}

/// Canned `(bytes, content_type)` keyed by resource `file_key`.
type StubMap = std::sync::Mutex<std::collections::HashMap<String, (Vec<u8>, Option<String>)>>;

/// Test fetcher returning canned bytes per `file_key`.
///
/// Not `#[cfg(test)]` so the integration tests in `tests/` can inject it.
#[derive(Debug, Default)]
pub struct FakeResourceFetcher {
    items: StubMap,
}

impl FakeResourceFetcher {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `bytes` (and an optional response `Content-Type`) for `file_key`.
    #[must_use]
    pub fn with(self, file_key: &str, bytes: Vec<u8>, content_type: Option<&str>) -> Self {
        self.insert(file_key, bytes, content_type);
        self
    }

    /// Register canned bytes for `file_key` after construction (for tests that
    /// build the fetcher first, then populate it).
    pub fn insert(&self, file_key: &str, bytes: Vec<u8>, content_type: Option<&str>) {
        self.items
            .lock()
            .expect("invariant: fake-fetcher mutex poisoned")
            .insert(
                file_key.to_owned(),
                (bytes, content_type.map(ToOwned::to_owned)),
            );
    }
}

#[async_trait]
impl ResourceFetcher for FakeResourceFetcher {
    async fn fetch(
        &self,
        _token: &str,
        _message_id: &str,
        file_key: &str,
        _kind: LarkResourceKind,
    ) -> Result<FetchedBytes, LarkError> {
        let (bytes, content_type) = self
            .items
            .lock()
            .expect("invariant: fake-fetcher mutex poisoned")
            .get(file_key)
            .cloned()
            .ok_or_else(|| LarkError::ResourceFetch(format!("no stub for {file_key}")))?;
        Ok(FetchedBytes {
            bytes: bytes.into(),
            content_type,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_param_maps_kind() {
        assert_eq!(LarkResourceKind::Image.type_param(), "image");
        assert_eq!(LarkResourceKind::File.type_param(), "file");
    }
}
