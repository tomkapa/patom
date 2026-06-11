//! Generic S3-compatible implementation of [`AssetStore`].
//!
//! Works against any S3 API endpoint — MinIO, AWS S3, self-hosted, or
//! Cloudflare R2 — using the explicit endpoint + region resolved at the
//! config boundary ([`ObjectStorageSettings`]). The SDK's normal
//! credential-chain resolution would look at the EC2 IMDS / ECS provider /
//! etc.; we short-circuit it with a static credential built from the
//! configured access key + secret so startup is hermetic and we never
//! accidentally pick up the host's AWS creds.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use aws_config::Region;
use aws_credential_types::Credentials;
use aws_credential_types::provider::SharedCredentialsProvider;
use aws_sdk_s3::Client as S3Client;
use aws_sdk_s3::config::{BehaviorVersion, RequestChecksumCalculation, ResponseChecksumValidation};
use aws_sdk_s3::primitives::ByteStream;
use bytes::Bytes;
use tokio::time::timeout;
use tracing::instrument;

use crate::config::ObjectStorageSettings;

use super::error::AssetError;
use super::limits::{STORAGE_DELETE_TIMEOUT, STORAGE_PUT_TIMEOUT};
use super::traits::{AssetStore, AssetUrl, ImageContentType, ObjectKey};

/// S3-compatible connection bundle. Cheap to clone — the inner `S3Client`
/// holds an `Arc` internally.
#[derive(Clone)]
pub struct S3AssetStore {
    client: S3Client,
    bucket: Arc<str>,
    /// Base URL the FE renders. Stored without trailing slash; `put`
    /// joins it to the object key with a single `/`.
    public_host: Arc<str>,
}

impl S3AssetStore {
    /// Construct a client from validated object-storage settings.
    /// Synchronous — [`aws_config::SdkConfig`] assembly is pure data; no
    /// network round-trip happens until the first PutObject.
    ///
    /// `endpoint` and `public_host` are already validated by the config
    /// boundary ([`ObjectStorageSettings`] parsing rejects non-origin URLs
    /// and trailing slashes), so this constructor doesn't re-check them.
    #[must_use]
    pub fn new(settings: &ObjectStorageSettings) -> Self {
        Self {
            client: S3Client::from_conf(Self::build_s3_config(settings)),
            bucket: Arc::from(settings.bucket.as_str()),
            public_host: Arc::from(settings.public_host.as_str()),
        }
    }

    /// Assemble the S3 client config from validated settings. Split out of
    /// [`Self::new`] so the addressing + checksum invariants are unit-testable
    /// without standing up a live client (config assembly is pure data — no
    /// network round-trip happens until the first PutObject).
    fn build_s3_config(settings: &ObjectStorageSettings) -> aws_sdk_s3::config::Config {
        let credentials = Credentials::new(
            settings.access_key_id.expose(),
            settings.secret_access_key.expose(),
            None,
            None,
            "patom-s3-static",
        );
        let sdk_config = aws_config::SdkConfig::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new(settings.region.clone()))
            .endpoint_url(&settings.endpoint)
            .credentials_provider(SharedCredentialsProvider::new(credentials))
            .build();
        aws_sdk_s3::config::Builder::from(&sdk_config)
            // Path-style addressing (`<endpoint>/<bucket>/<key>`) works
            // universally — MinIO and R2 require it, AWS still supports it.
            // The SDK's virtual-host default (`<bucket>.<endpoint>`) breaks
            // MinIO/R2, so we force path-style for every backend.
            .force_path_style(true)
            // Scope flexible checksums to `when_required`. With
            // `behavior-version-latest` the SDK otherwise defaults to
            // `when_supported`, attaching a CRC32 to every PutObject — which
            // Cloudflare R2 rejects with a fast 4xx, surfacing as
            // `AssetError::StoragePut` → HTTP 500 on avatar upload. AWS S3 is
            // unaffected by the narrower setting; R2/MinIO need it.
            .request_checksum_calculation(RequestChecksumCalculation::WhenRequired)
            .response_checksum_validation(ResponseChecksumValidation::WhenRequired)
            .build()
    }

    fn build_public_url(&self, key: &ObjectKey) -> Result<AssetUrl, AssetError> {
        let raw = format!("{host}/{key}", host = self.public_host, key = key.as_str());
        AssetUrl::try_from(raw.as_str()).map_err(AssetError::from)
    }
}

impl fmt::Debug for S3AssetStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("S3AssetStore")
            .field("bucket", &&*self.bucket)
            .field("public_host", &&*self.public_host)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl AssetStore for S3AssetStore {
    #[instrument(
        name = "assets.put",
        skip(self, bytes),
        fields(
            patom.asset.bytes = bytes.len(),
            patom.asset.content_type = content_type.as_mime(),
            patom.asset.key = %key,
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
        Box::pin(run_with_timeout(STORAGE_PUT_TIMEOUT, put))
            .await
            .map_err(SdkOutcome::into_put_error)?;
        self.build_public_url(&key)
    }

    #[instrument(name = "assets.delete", skip(self), fields(patom.asset.key = %key))]
    async fn delete(&self, key: ObjectKey) -> Result<(), AssetError> {
        let del = self
            .client
            .delete_object()
            .bucket(self.bucket.as_ref())
            .key(key.as_str())
            .send();
        Box::pin(run_with_timeout(STORAGE_DELETE_TIMEOUT, del))
            .await
            .map_err(SdkOutcome::into_delete_error)?;
        Ok(())
    }

    fn public_host(&self) -> &str {
        &self.public_host
    }
}

/// Per-op-neutral outcome from [`run_with_timeout`]. The caller maps
/// this onto its specific [`AssetError`] variant (StoragePut vs
/// StorageDelete) so errors carry the correct operation label.
enum SdkOutcome {
    Timeout,
    Sdk(String),
}

impl SdkOutcome {
    fn into_put_error(self) -> AssetError {
        match self {
            Self::Timeout => AssetError::Timeout,
            Self::Sdk(msg) => AssetError::StoragePut(msg),
        }
    }

    fn into_delete_error(self) -> AssetError {
        match self {
            Self::Timeout => AssetError::Timeout,
            Self::Sdk(msg) => AssetError::StorageDelete(msg),
        }
    }
}

/// Wrap an async S3 future in `tokio::time::timeout` and surface the
/// SDK's full error chain. Returns [`SdkOutcome`] — the caller maps it
/// to the operation-specific [`AssetError`] variant so a delete failure
/// doesn't surface as `StoragePut` and vice versa.
async fn run_with_timeout<F, T, E>(dur: Duration, fut: F) -> Result<T, SdkOutcome>
where
    F: std::future::Future<Output = Result<T, E>>,
    E: std::error::Error,
{
    match timeout(dur, fut).await {
        Err(_) => Err(SdkOutcome::Timeout),
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
            Err(SdkOutcome::Sdk(msg))
        }
    }
}

/// In-memory [`AssetStore`] for tests.
///
/// Stores bytes in a `Mutex<HashMap>` and synthesises the public URL
/// the same way the S3 impl does. Lives here (not under `#[cfg(test)]`)
/// so integration tests in `tests/` can build a router without standing
/// up a real object-storage bucket.
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

    fn public_host(&self) -> &str {
        &self.public_host
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::SecretString;
    use aws_sdk_s3::config::{RequestChecksumCalculation, ResponseChecksumValidation};

    fn r2_settings() -> ObjectStorageSettings {
        ObjectStorageSettings {
            endpoint: "https://acct.r2.cloudflarestorage.com".to_owned(),
            region: "auto".to_owned(),
            bucket: "patom-assets".to_owned(),
            access_key_id: SecretString::try_from("AKIDEXAMPLE".to_owned())
                .expect("non-empty access key id"),
            secret_access_key: SecretString::try_from("secret-access-key".to_owned())
                .expect("non-empty secret access key"),
            public_host: "https://asset.example".to_owned(),
        }
    }

    // Cloudflare R2 rejects the AWS SDK's default flexible checksums that
    // `behavior-version-latest` enables (`when_supported` attaches a CRC32 to
    // every PutObject), failing avatar uploads with a fast 4xx that surfaces as
    // `AssetError::StoragePut` → HTTP 500. The client must pin both checksum
    // knobs to `when_required` so we only send/verify a checksum when the
    // operation model demands one.
    #[test]
    fn s3_config_scopes_checksums_to_when_required() {
        let config = S3AssetStore::build_s3_config(&r2_settings());
        assert_eq!(
            config.request_checksum_calculation(),
            Some(&RequestChecksumCalculation::WhenRequired),
            "request checksum calculation must be when_required for R2",
        );
        assert_eq!(
            config.response_checksum_validation(),
            Some(&ResponseChecksumValidation::WhenRequired),
            "response checksum validation must be when_required for R2",
        );
    }
}
