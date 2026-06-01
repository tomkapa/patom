use std::fmt;
use std::sync::Arc;

use super::error::ParseError;

/// Cap on the persisted avatar URL string length.
///
/// Mirrors the `octet_length(avatar_url) <= 2048` CHECK on both
/// `users.avatar_url` (migration 14) and `organizations.avatar_url`
/// (migration 48), so the boundary refuses a value the DB would also
/// reject.
pub const AVATAR_URL_MAX_BYTES: usize = 2048;

/// A validated avatar URL shared by the user- and org-facing DTOs.
///
/// CLAUDE.md §1: parse at the boundary. Two write paths feed this type —
/// a self-uploaded asset (an [`crate::assets::AssetUrl`] off the object
/// store) and an OIDC `picture` claim — so the smart constructor mirrors
/// [`crate::assets::AssetUrl`] exactly: it fully parses the value as an
/// absolute URL and enforces an `http`(s) scheme, a present host, and the
/// length cap. `http://` is permitted so a self-hosted MinIO endpoint
/// served over plain HTTP round-trips through both newtypes.
///
/// `Arc<str>` keeps clones cheap — these flow through `/me` and the
/// members list, both of which clone per row.
#[derive(Clone, PartialEq, Eq)]
pub struct AvatarUrl(Arc<str>);

impl AvatarUrl {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<&str> for AvatarUrl {
    type Error = ParseError;

    fn try_from(raw: &str) -> Result<Self, Self::Error> {
        if raw.is_empty() {
            return Err(ParseError::Empty {
                field: "avatar.url",
            });
        }
        // Length-cap before parsing so a pathological input can't make the
        // URL parser do unbounded work (CLAUDE.md §5).
        if raw.len() > AVATAR_URL_MAX_BYTES {
            return Err(ParseError::TooLong {
                field: "avatar.url",
                max: AVATAR_URL_MAX_BYTES,
                got: raw.len(),
            });
        }
        let url = url::Url::parse(raw).map_err(|_| ParseError::Malformed {
            field: "avatar.url",
            detail: "must be a valid absolute url",
        })?;
        if url.scheme() != "http" && url.scheme() != "https" {
            return Err(ParseError::Malformed {
                field: "avatar.url",
                detail: "must be http:// or https://",
            });
        }
        if !url.has_host() {
            return Err(ParseError::Malformed {
                field: "avatar.url",
                detail: "must have a host",
            });
        }
        Ok(Self(Arc::from(raw)))
    }
}

impl TryFrom<String> for AvatarUrl {
    type Error = ParseError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::try_from(raw.as_str())
    }
}

impl fmt::Debug for AvatarUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("AvatarUrl").field(&&*self.0).finish()
    }
}

impl fmt::Display for AvatarUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl serde::Serialize for AvatarUrl {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty() {
        assert!(matches!(
            AvatarUrl::try_from(""),
            Err(ParseError::Empty { .. })
        ));
    }

    #[test]
    fn rejects_oversize() {
        let huge = format!("https://h.test/{}", "a".repeat(AVATAR_URL_MAX_BYTES));
        assert!(matches!(
            AvatarUrl::try_from(huge.as_str()),
            Err(ParseError::TooLong { .. })
        ));
    }

    #[test]
    fn accepts_http_and_https() {
        // https — an IdP picture claim or a CDN-fronted asset.
        let a = AvatarUrl::try_from("https://lh3.googleusercontent.com/a/x=s96")
            .expect("https accepted");
        assert_eq!(a.as_str(), "https://lh3.googleusercontent.com/a/x=s96");
        // http — a self-hosted MinIO endpoint, same as `AssetUrl`.
        assert!(AvatarUrl::try_from("http://minio:9000/patom-assets/avatars/u.png").is_ok());
    }

    #[test]
    fn rejects_non_http_scheme_and_relative() {
        assert!(matches!(
            AvatarUrl::try_from("ftp://assets.example/x.png"),
            Err(ParseError::Malformed { .. })
        ));
        assert!(matches!(
            AvatarUrl::try_from("/relative/x.png"),
            Err(ParseError::Malformed { .. })
        ));
        // A scheme prefix is not enough — the URL must parse with a host.
        assert!(matches!(
            AvatarUrl::try_from("https://"),
            Err(ParseError::Malformed { .. })
        ));
    }

    #[test]
    fn serializes_as_plain_string() {
        let a = AvatarUrl::try_from("https://h.test/x.png").expect("valid");
        let json = serde_json::to_string(&a).expect("serialize");
        assert_eq!(json, "\"https://h.test/x.png\"");
    }
}
