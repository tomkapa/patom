//! Public surface for the asset module: newtypes, content-type allowlist,
//! and the [`AssetStore`] trait. CLAUDE.md §1: ids are typed, parsing
//! happens once at the boundary via `TryFrom`.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;

use crate::types::ParseError;

use super::error::AssetError;
use super::limits::{
    ASSET_URL_MAX_LEN, MAX_AGENT_AVATAR_BYTES, MAX_AVATAR_BYTES, MAX_MCP_ICON_BYTES,
    MAX_WORKSPACE_AVATAR_BYTES, OBJECT_KEY_MAX_LEN,
};

/// Image content-type allowed at the upload boundary.
///
/// A sum type, not a `&str`, so exhaustive `match` proves we've covered
/// every variant in the SDK PutObject call, the magic-byte cross-check,
/// and the URL-extension derivation (CLAUDE.md §1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageContentType {
    Png,
    Jpeg,
    Webp,
    Svg,
}

impl ImageContentType {
    /// Wire-form `Content-Type` header value.
    #[must_use]
    pub fn as_mime(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Webp => "image/webp",
            Self::Svg => "image/svg+xml",
        }
    }

    /// Canonical file extension (no leading dot) for derived object keys.
    #[must_use]
    pub fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Webp => "webp",
            Self::Svg => "svg",
        }
    }

    /// Parse a wire-form `Content-Type` header value. Returns `None` for
    /// any value outside the allow-list — the caller maps that into
    /// [`AssetError::ContentTypeNotAllowed`].
    #[must_use]
    pub fn from_mime(raw: &str) -> Option<Self> {
        // Trim any `; charset=...` parameter the client may attach.
        let canonical = raw
            .split(';')
            .next()
            .unwrap_or(raw)
            .trim()
            .to_ascii_lowercase();
        match canonical.as_str() {
            "image/png" => Some(Self::Png),
            "image/jpeg" | "image/jpg" => Some(Self::Jpeg),
            "image/webp" => Some(Self::Webp),
            "image/svg+xml" | "image/svg" => Some(Self::Svg),
            _ => None,
        }
    }
}

/// The "kind" of asset being stored. Drives per-kind byte caps, SVG
/// allow/deny rules, and key prefixes — encoded once here instead of
/// scattered through the upload handlers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssetKind {
    /// User profile avatar. SVG denied (XSS risk if the URL is ever
    /// embedded inline on the app origin); served from the assets origin.
    Avatar,
    /// MCP catalog tile icon. SVG allowed because vendor logos are
    /// distributed as SVG and the assets origin is a separate host.
    McpCatalogIcon,
    /// Workspace (organization) avatar. SVG denied for the same XSS
    /// reason as user avatars; distinct key prefix from `Avatar` so
    /// user and workspace UUIDs cannot collide.
    WorkspaceAvatar,
    /// Per-agent avatar (issue #43). SVG denied for the same XSS reason
    /// as user/workspace avatars; distinct key prefix so agent UUIDs
    /// cannot collide with user or workspace ids.
    AgentAvatar,
}

impl AssetKind {
    /// Whether this kind accepts the given content type.
    #[must_use]
    pub fn accepts(self, content_type: ImageContentType) -> bool {
        match (self, content_type) {
            (Self::Avatar | Self::WorkspaceAvatar | Self::AgentAvatar, ImageContentType::Svg) => {
                false
            }
            (
                Self::Avatar | Self::McpCatalogIcon | Self::WorkspaceAvatar | Self::AgentAvatar,
                _,
            ) => true,
        }
    }

    /// Key prefix this kind writes under in the bucket.
    #[must_use]
    pub fn key_prefix(self) -> &'static str {
        match self {
            Self::Avatar => "avatars",
            Self::McpCatalogIcon => "mcp",
            Self::WorkspaceAvatar => "workspaces",
            Self::AgentAvatar => "agents",
        }
    }

    /// Telemetry label, used as `patom.asset.kind` on tracing spans + the
    /// `kind` attribute on upload metrics.
    #[must_use]
    pub fn telemetry_label(self) -> &'static str {
        match self {
            Self::Avatar => "avatar",
            Self::McpCatalogIcon => "mcp_icon",
            Self::WorkspaceAvatar => "workspace_avatar",
            Self::AgentAvatar => "agent_avatar",
        }
    }

    /// Maximum body bytes accepted for this kind.
    #[must_use]
    pub const fn max_bytes(self) -> usize {
        match self {
            Self::Avatar => MAX_AVATAR_BYTES,
            Self::McpCatalogIcon => MAX_MCP_ICON_BYTES,
            Self::WorkspaceAvatar => MAX_WORKSPACE_AVATAR_BYTES,
            Self::AgentAvatar => MAX_AGENT_AVATAR_BYTES,
        }
    }
}

/// A validated key into the asset bucket. CLAUDE.md §1: every id is a
/// newtype, every parse is fallible.
///
/// Allowed characters: `[A-Za-z0-9_./-]`. Specifically rejects the empty
/// string, leading `/`, and `..` so a malicious caller cannot construct a
/// path that escapes its intended prefix.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct ObjectKey(Arc<str>);

impl ObjectKey {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Build a deterministic key for an asset kind + stable id + content
    /// type. Used by the upload handlers so re-uploading replaces in
    /// place — no orphan-cleanup job needed.
    pub fn derive(
        kind: AssetKind,
        stable_id: &str,
        content_type: ImageContentType,
    ) -> Result<Self, ParseError> {
        let raw = format!(
            "{prefix}/{id}.{ext}",
            prefix = kind.key_prefix(),
            id = stable_id,
            ext = content_type.extension(),
        );
        Self::try_from(raw.as_str())
    }
}

impl TryFrom<&str> for ObjectKey {
    type Error = ParseError;

    fn try_from(raw: &str) -> Result<Self, Self::Error> {
        if raw.is_empty() {
            return Err(ParseError::Empty {
                field: "asset.object_key",
            });
        }
        if raw.len() > OBJECT_KEY_MAX_LEN {
            return Err(ParseError::TooLong {
                field: "asset.object_key",
                max: OBJECT_KEY_MAX_LEN,
                got: raw.len(),
            });
        }
        if raw.starts_with('/') {
            return Err(ParseError::Malformed {
                field: "asset.object_key",
                detail: "must not start with '/'",
            });
        }
        if raw.contains("..") {
            return Err(ParseError::Malformed {
                field: "asset.object_key",
                detail: "must not contain '..'",
            });
        }
        for b in raw.bytes() {
            let ok = b.is_ascii_alphanumeric() || matches!(b, b'/' | b'_' | b'.' | b'-');
            if !ok {
                return Err(ParseError::Malformed {
                    field: "asset.object_key",
                    detail: "only [A-Za-z0-9_./-] allowed",
                });
            }
        }
        Ok(Self(Arc::from(raw)))
    }
}

impl fmt::Debug for ObjectKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ObjectKey").field(&&*self.0).finish()
    }
}

impl fmt::Display for ObjectKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Public-facing URL for a stored asset.
///
/// CLAUDE.md §1: parse at the boundary. The smart constructor fully parses
/// the value as an absolute URL and enforces an http(s) scheme, a present
/// host, and a length cap — a prefix check would let `http://` or
/// `http://?x=1` through. `http://` is permitted so self-hosted MinIO
/// behind a plain-HTTP endpoint works; production deployments serving the
/// SPA over https should front object storage with an https reverse proxy.
#[derive(Clone, PartialEq, Eq)]
pub struct AssetUrl(Arc<str>);

impl AssetUrl {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<&str> for AssetUrl {
    type Error = ParseError;

    fn try_from(raw: &str) -> Result<Self, Self::Error> {
        if raw.is_empty() {
            return Err(ParseError::Empty { field: "asset.url" });
        }
        // Length-cap before parsing so a pathological input can't make the
        // URL parser do unbounded work (CLAUDE.md §5).
        if raw.len() > ASSET_URL_MAX_LEN {
            return Err(ParseError::TooLong {
                field: "asset.url",
                max: ASSET_URL_MAX_LEN,
                got: raw.len(),
            });
        }
        let url = url::Url::parse(raw).map_err(|_| ParseError::Malformed {
            field: "asset.url",
            detail: "must be a valid absolute url",
        })?;
        if url.scheme() != "http" && url.scheme() != "https" {
            return Err(ParseError::Malformed {
                field: "asset.url",
                detail: "must be http:// or https://",
            });
        }
        if !url.has_host() {
            return Err(ParseError::Malformed {
                field: "asset.url",
                detail: "must have a host",
            });
        }
        Ok(Self(Arc::from(raw)))
    }
}

impl fmt::Debug for AssetUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("AssetUrl").field(&&*self.0).finish()
    }
}

impl fmt::Display for AssetUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl serde::Serialize for AssetUrl {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(&self.0)
    }
}

/// Storage seam for image-style assets. Two ops cover today's needs;
/// fancier features (presigned URLs, lifecycle policies) stay out of the
/// trait until there's a concrete consumer.
#[async_trait]
pub trait AssetStore: fmt::Debug + Send + Sync + 'static {
    /// Upload `bytes` under `key` with the given content type. Returns
    /// the public URL the FE renders. Overwrites if `key` already exists.
    async fn put(
        &self,
        key: ObjectKey,
        bytes: Bytes,
        content_type: ImageContentType,
    ) -> Result<AssetUrl, AssetError>;

    /// Delete an object. Idempotent — deleting a missing key is `Ok(())`.
    async fn delete(&self, key: ObjectKey) -> Result<(), AssetError>;

    /// The public-facing base origin the FE prepends to object keys, with no
    /// trailing slash (e.g. `https://asset.example` or
    /// `http://minio:9000/<bucket>`). Validated at the config boundary
    /// ([`crate::config::ObjectStorageSettings`]).
    ///
    /// Exposed so callers that assemble a *static* asset path — e.g. the
    /// bundled default agent avatars at `/agents/agent-{n}.png` — can build
    /// the absolute URL without uploading bytes. Single source of the origin:
    /// the store already holds it for [`Self::put`].
    fn public_host(&self) -> &str;
}

pub type SharedAssetStore = Arc<dyn AssetStore>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_key_rejects_empty() {
        assert!(matches!(
            ObjectKey::try_from(""),
            Err(ParseError::Empty { .. })
        ));
    }

    #[test]
    fn object_key_rejects_leading_slash() {
        assert!(matches!(
            ObjectKey::try_from("/avatars/x.png"),
            Err(ParseError::Malformed { .. })
        ));
    }

    #[test]
    fn object_key_rejects_traversal() {
        assert!(matches!(
            ObjectKey::try_from("avatars/../etc"),
            Err(ParseError::Malformed { .. })
        ));
    }

    #[test]
    fn object_key_rejects_oversize() {
        let huge = "a".repeat(OBJECT_KEY_MAX_LEN + 1);
        assert!(matches!(
            ObjectKey::try_from(huge.as_str()),
            Err(ParseError::TooLong { .. })
        ));
    }

    #[test]
    fn object_key_rejects_bad_chars() {
        for raw in ["spa ce.png", "weird?.png", "ümlaut.png"] {
            assert!(
                matches!(ObjectKey::try_from(raw), Err(ParseError::Malformed { .. })),
                "expected malformed for {raw}"
            );
        }
    }

    #[test]
    fn object_key_accepts_valid() {
        let k = ObjectKey::try_from("avatars/abc-123_v1.png").expect("valid");
        assert_eq!(k.as_str(), "avatars/abc-123_v1.png");
    }

    #[test]
    fn derive_builds_kinded_key() {
        let k = ObjectKey::derive(AssetKind::Avatar, "abc", ImageContentType::Png).expect("ok");
        assert_eq!(k.as_str(), "avatars/abc.png");
        let k = ObjectKey::derive(AssetKind::McpCatalogIcon, "notion", ImageContentType::Svg)
            .expect("ok");
        assert_eq!(k.as_str(), "mcp/notion.svg");
    }

    #[test]
    fn asset_url_requires_http_scheme() {
        // https is accepted (CDN / R2).
        assert!(AssetUrl::try_from("https://assets.example/x.png").is_ok());
        // http is accepted too — self-hosted MinIO over plain HTTP.
        assert!(AssetUrl::try_from("http://minio:9000/patom-assets/x.png").is_ok());
        // anything without an http(s) scheme is rejected.
        assert!(matches!(
            AssetUrl::try_from("ftp://assets.example/x.png"),
            Err(ParseError::Malformed { .. })
        ));
        assert!(matches!(
            AssetUrl::try_from("/relative/x.png"),
            Err(ParseError::Malformed { .. })
        ));
        // A scheme prefix is not enough — the URL must actually parse with
        // a host. `http://` and `http://?x=1` have no authority.
        assert!(matches!(
            AssetUrl::try_from("http://"),
            Err(ParseError::Malformed { .. })
        ));
        assert!(matches!(
            AssetUrl::try_from("http://?x=1"),
            Err(ParseError::Malformed { .. })
        ));
    }

    #[test]
    fn content_type_normalises_charset() {
        assert_eq!(
            ImageContentType::from_mime("image/svg+xml; charset=utf-8"),
            Some(ImageContentType::Svg)
        );
        assert_eq!(
            ImageContentType::from_mime("image/PNG"),
            Some(ImageContentType::Png)
        );
        assert_eq!(ImageContentType::from_mime("text/plain"), None);
    }

    #[test]
    fn avatar_rejects_svg() {
        assert!(!AssetKind::Avatar.accepts(ImageContentType::Svg));
        assert!(AssetKind::Avatar.accepts(ImageContentType::Png));
        assert!(AssetKind::McpCatalogIcon.accepts(ImageContentType::Svg));
    }

    #[test]
    fn workspace_avatar_rejects_svg() {
        assert!(!AssetKind::WorkspaceAvatar.accepts(ImageContentType::Svg));
        assert!(AssetKind::WorkspaceAvatar.accepts(ImageContentType::Png));
        assert!(AssetKind::WorkspaceAvatar.accepts(ImageContentType::Jpeg));
        assert!(AssetKind::WorkspaceAvatar.accepts(ImageContentType::Webp));
    }

    #[test]
    fn workspace_avatar_uses_own_prefix() {
        let k = ObjectKey::derive(
            AssetKind::WorkspaceAvatar,
            "0123abcd",
            ImageContentType::Png,
        )
        .expect("ok");
        assert_eq!(k.as_str(), "workspaces/0123abcd.png");
    }

    #[test]
    fn agent_avatar_rejects_svg() {
        // Issue #43: agent avatars are served on the assets origin and
        // embedded in the FE / Slack, so SVG is denied like user avatars.
        assert!(!AssetKind::AgentAvatar.accepts(ImageContentType::Svg));
        assert!(AssetKind::AgentAvatar.accepts(ImageContentType::Png));
        assert!(AssetKind::AgentAvatar.accepts(ImageContentType::Jpeg));
        assert!(AssetKind::AgentAvatar.accepts(ImageContentType::Webp));
    }

    #[test]
    fn agent_avatar_uses_own_prefix() {
        // Distinct prefix so agent UUIDs cannot collide with user or
        // workspace ids in the bucket.
        let k = ObjectKey::derive(AssetKind::AgentAvatar, "0123abcd", ImageContentType::Png)
            .expect("ok");
        assert_eq!(k.as_str(), "agents/0123abcd.png");
    }
}
