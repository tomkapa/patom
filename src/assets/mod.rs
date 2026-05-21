//! Object-storage seam for user-uploaded images.
//!
//! Two kinds of asset live here today: user avatars (`avatars/<user>.<ext>`)
//! and MCP catalog tile icons (`mcp/<catalog_id>.<ext>`). Both go through
//! one [`AssetStore`] trait + one [`R2AssetStore`] impl pointed at
//! Cloudflare R2 (the S3-compatible object store).
//!
//! Trust boundary lives in [`multipart`]: every byte coming off the wire
//! is size-capped, content-type-checked, and magic-byte-sniffed before
//! we touch R2 (CLAUDE.md §5/§6).

pub mod error;
pub mod limits;
pub mod multipart;
pub mod r2_store;
pub mod traits;

pub use error::AssetError;
pub use multipart::{UploadedImage, extract_single_image_field, validate_image_bytes};
pub use r2_store::{InMemoryAssetStore, R2AssetStore};
pub use traits::{AssetKind, AssetStore, AssetUrl, ImageContentType, ObjectKey, SharedAssetStore};
