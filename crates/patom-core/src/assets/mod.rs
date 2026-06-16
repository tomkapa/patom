//! Object-storage seam for user-uploaded images.
//!
//! Two kinds of asset live here today: user avatars (`avatars/<user>.<ext>`)
//! and MCP catalog tile icons (`mcp/<catalog_id>.<ext>`). Both go through
//! one [`AssetStore`] trait + one [`S3AssetStore`] impl pointed at any
//! S3-compatible object store (MinIO, AWS S3, self-hosted, Cloudflare R2).
//!
//! Trust boundary lives in [`multipart`]: every byte coming off the wire
//! is size-capped, content-type-checked, and magic-byte-sniffed before
//! we touch object storage (CLAUDE.md §5/§6).

pub mod error;
pub mod limits;
pub mod multipart;
pub mod s3_store;
pub mod traits;

pub use error::AssetError;
pub use multipart::{
    UploadedAttachment, UploadedImage, extract_attachment_field, extract_single_image_field,
    validate_attachment_bytes, validate_image_bytes,
};
pub use s3_store::{InMemoryAssetStore, S3AssetStore};
pub use traits::{
    AssetContentType, AssetKind, AssetStore, AssetUrl, ImageContentType, ObjectKey,
    SharedAssetStore,
};
