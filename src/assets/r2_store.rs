//! Cloudflare R2 (S3-compatible) implementation of [`AssetStore`].
//!
//! R2 speaks the S3 API at `https://<account_id>.r2.cloudflarestorage.com`
//! with region `auto`. The SDK's normal credential-chain resolution would
//! look at the EC2 IMDS / ECS provider / etc.; we short-circuit it with a
//! static credential built from the configured access key + secret so
//! startup is hermetic and we never accidentally pick up the host's AWS
//! creds.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use aws_config::Region;
use aws_credential_types::Credentials;
use aws_credential_types::provider::SharedCredentialsProvider;
use aws_sdk_s3::Client as S3Client;
use aws_sdk_s3::config::BehaviorVersion;
use aws_sdk_s3::primitives::ByteStream;
use bytes::Bytes;
use tokio::time::timeout;
use tracing::instrument;

use crate::config::R2Settings;

use super::error::AssetError;
use super::limits::{R2_DELETE_TIMEOUT, R2_PUT_TIMEOUT};
use super::traits::{AssetStore, AssetUrl, ImageContentType, ObjectKey};

/// Cloudflare R2 connection bundle. Cheap to clone — the inner `S3Client`
/// holds an `Arc` internally.
#[derive(Clone)]
pub struct R2AssetStore {
    client: S3Client,
    bucket: Arc<str>,
    /// Base URL the FE renders. Stored without trailing slash; `put`
    /// joins it to the object key with a single `/`.
    public_host: Arc<str>,
}

impl R2AssetStore {
    /// Construct a client from validated R2 settings. Synchronous —
    /// [`aws_config::SdkConfig`] assembly is pure data; no network
    /// round-trip happens until the first PutObject.
    ///
    /// `public_host` is already validated by the config boundary
    /// ([`R2Settings`] parsing rejects non-`https://` and trailing
    /// slashes), so this constructor doesn't re-check it.
    pub fn new(settings: &R2Settings) -> Self {
        let credentials = Credentials::new(
            settings.access_key_id.expose(),
            settings.secret_access_key.expose(),
            None,
            None,
            "relay-r2-static",
        );
        let sdk_config = aws_config::SdkConfig::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new("auto"))
            .endpoint_url(settings.endpoint())
            .credentials_provider(SharedCredentialsProvider::new(credentials))
            .build();
        let s3_config = aws_sdk_s3::config::Builder::from(&sdk_config)
            // R2 requires path-style addressing; the SDK's virtual-host
            // default would try `<bucket>.<account>.r2.cloudflarestorage.com`
            // which is not how R2 routes requests.
            .force_path_style(true)
            .build();
        Self {
            client: S3Client::from_conf(s3_config),
            bucket: Arc::from(settings.bucket.as_str()),
            public_host: Arc::from(settings.public_host.as_str()),
        }
    }

    fn build_public_url(&self, key: &ObjectKey) -> Result<AssetUrl, AssetError> {
        let raw = format!("{host}/{key}", host = self.public_host, key = key.as_str());
        AssetUrl::try_from(raw.as_str()).map_err(AssetError::from)
    }
}

impl fmt::Debug for R2AssetStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("R2AssetStore")
            .field("bucket", &&*self.bucket)
            .field("public_host", &&*self.public_host)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl AssetStore for R2AssetStore {
    #[instrument(
        name = "assets.put",
        skip(self, bytes),
        fields(
            relay.asset.bytes = bytes.len(),
            relay.asset.content_type = content_type.as_mime(),
            relay.asset.key = %key,
        )
    )]
    async fn put(
        &self,
        key: ObjectKey,
        bytes: Bytes,
        content_type: ImageContentType,
    ) -> Result<AssetUrl, AssetError> {
        // §6: assert positive *and* negative — bytes must be present and
        // bounded. The per-kind cap is enforced one layer up at the
        // multipart boundary; this assertion is defence-in-depth.
        assert!(!bytes.is_empty(), "AssetStore::put called with empty body");
        let stream = ByteStream::from(bytes);
        let put = self
            .client
            .put_object()
            .bucket(self.bucket.as_ref())
            .key(key.as_str())
            .body(stream)
            .content_type(content_type.as_mime())
            // Long-lived cache header — keys are deterministic
            // (`{prefix}/{stable_id}.{ext}`); a re-upload writes to the
            // same key so the CDN refreshes on the next miss. If we
            // need cache-busting we'll add a version segment to the key.
            .cache_control("public, max-age=31536000, immutable")
            .send();
        // Box::pin keeps the SDK's large output future off the stack
        // — clippy::large_futures otherwise flags ~21 KB of inline.
        Box::pin(run_with_timeout(R2_PUT_TIMEOUT, put))
            .await
            .map_err(|e| AssetError::R2Put(e.to_string()))?;
        self.build_public_url(&key)
    }

    #[instrument(name = "assets.delete", skip(self), fields(relay.asset.key = %key))]
    async fn delete(&self, key: ObjectKey) -> Result<(), AssetError> {
        let del = self
            .client
            .delete_object()
            .bucket(self.bucket.as_ref())
            .key(key.as_str())
            .send();
        Box::pin(run_with_timeout(R2_DELETE_TIMEOUT, del))
            .await
            .map_err(|e| AssetError::R2Delete(e.to_string()))?;
        Ok(())
    }
}

/// Wrap an async S3 future in `tokio::time::timeout` and surface the
/// SDK's full error context. Two-tier `Result` because the timeout is a
/// distinct failure from a fast SDK error.
async fn run_with_timeout<F, T, E>(dur: Duration, fut: F) -> Result<T, AssetError>
where
    F: std::future::Future<Output = Result<T, E>>,
    E: std::error::Error,
{
    match timeout(dur, fut).await {
        Err(_) => Err(AssetError::Timeout),
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => {
            // Surface the chain so operators see the inner SDK reason,
            // not just the top-level summary.
            let mut msg = e.to_string();
            let mut cause: &dyn std::error::Error = &e;
            while let Some(next) = cause.source() {
                msg.push_str(" :: ");
                msg.push_str(&next.to_string());
                cause = next;
            }
            // Map back into either R2Put / R2Delete by the caller;
            // we don't know the op here, so emit a generic-ish variant
            // and let the caller wrap.
            Err(AssetError::R2Put(msg))
        }
    }
}

/// In-memory [`AssetStore`] for tests.
///
/// Stores bytes in a `Mutex<HashMap>` and synthesises the public URL
/// the same way the R2 impl does. Lives here (not under `#[cfg(test)]`)
/// so integration tests in `tests/` can build a router without standing
/// up a real R2 bucket.
pub struct InMemoryAssetStore {
    public_host: Arc<str>,
    objects: tokio::sync::Mutex<std::collections::HashMap<String, (Bytes, ImageContentType)>>,
}

impl InMemoryAssetStore {
    /// Build a fresh store. `public_host` must already include the scheme.
    #[must_use]
    pub fn new(public_host: &str) -> Self {
        Self {
            public_host: Arc::from(public_host.trim_end_matches('/').to_owned()),
            objects: tokio::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Test helper — peek at the stored bytes + content type for a key.
    pub async fn get(&self, key: &ObjectKey) -> Option<(Bytes, ImageContentType)> {
        self.objects.lock().await.get(key.as_str()).cloned()
    }

    /// Test helper — count of objects currently stored.
    pub async fn len(&self) -> usize {
        self.objects.lock().await.len()
    }

    /// Test helper — whether the store has any objects.
    pub async fn is_empty(&self) -> bool {
        self.objects.lock().await.is_empty()
    }
}

impl fmt::Debug for InMemoryAssetStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InMemoryAssetStore")
            .field("public_host", &&*self.public_host)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl AssetStore for InMemoryAssetStore {
    async fn put(
        &self,
        key: ObjectKey,
        bytes: Bytes,
        content_type: ImageContentType,
    ) -> Result<AssetUrl, AssetError> {
        assert!(
            !bytes.is_empty(),
            "InMemoryAssetStore::put called with empty body"
        );
        self.objects
            .lock()
            .await
            .insert(key.as_str().to_owned(), (bytes, content_type));
        let raw = format!("{host}/{key}", host = self.public_host, key = key.as_str());
        AssetUrl::try_from(raw.as_str()).map_err(AssetError::from)
    }

    async fn delete(&self, key: ObjectKey) -> Result<(), AssetError> {
        self.objects.lock().await.remove(key.as_str());
        Ok(())
    }
}
