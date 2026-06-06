//! Validated outbound URL with SSRF guard.
//!
//! `web_fetch` accepts arbitrary URLs from the model. Without filtering, the model can
//! reach internal targets — cloud metadata endpoints (`169.254.169.254`), localhost
//! services, RFC1918 hosts. Every URL the agent dereferences MUST go through
//! [`FetchUrl::try_from`] first.

use std::net::IpAddr;

use thiserror::Error;
use url::{Host, Url};

use crate::types::{PROMPT_MAX_BYTES, ParseError};

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum UrlError {
    #[error("malformed url: {0}")]
    Malformed(String),

    #[error("scheme `{0}` not allowed (only https)")]
    DisallowedScheme(String),

    #[error("host missing or unresolvable")]
    HostMissing,

    #[error("host `{0}` is in a blocked range (loopback / private / link-local / metadata)")]
    HostBlocked(String),

    #[error("url too long: max {max} bytes, got {got}")]
    TooLong { max: usize, got: usize },
}

impl From<ParseError> for UrlError {
    fn from(e: ParseError) -> Self {
        Self::Malformed(e.to_string())
    }
}

/// A URL the agent has cleared for outbound HTTPS traffic.
///
/// Construction enforces:
/// 1. Length cap (matches the prompt cap so a hostile prompt cannot stuff arbitrary bytes
///    here).
/// 2. `https://` only — `http`, `file`, `gopher`, etc. are rejected.
/// 3. Hostname is present and is not a numeric address inside loopback / private /
///    link-local / multicast / unspecified ranges.
///
/// We do *not* perform DNS resolution at construction time — that would block at parse
/// time and double the network footprint of every call. The contract is "any URL we
/// dereference is then re-checked by the HTTP client's redirect policy" (see
/// `WebFetchTool` for the redirect-time guard).
#[derive(Debug, Clone)]
pub struct FetchUrl(Url);

impl FetchUrl {
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    #[must_use]
    pub fn into_inner(self) -> Url {
        self.0
    }

    #[must_use]
    pub fn host_str(&self) -> Option<&str> {
        self.0.host_str()
    }
}

impl TryFrom<&str> for FetchUrl {
    type Error = UrlError;

    fn try_from(raw: &str) -> Result<Self, Self::Error> {
        if raw.len() > PROMPT_MAX_BYTES {
            return Err(UrlError::TooLong {
                max: PROMPT_MAX_BYTES,
                got: raw.len(),
            });
        }
        let url = Url::parse(raw).map_err(|e| UrlError::Malformed(e.to_string()))?;
        if url.scheme() != "https" {
            return Err(UrlError::DisallowedScheme(url.scheme().to_string()));
        }
        let host = url.host().ok_or(UrlError::HostMissing)?;
        check_host(&host)?;
        Ok(Self(url))
    }
}

/// Re-check a host after it has been resolved or after a redirect lands on a new
/// destination. Public so the HTTP redirect policy can call it.
pub(super) fn check_host(host: &Host<&str>) -> Result<(), UrlError> {
    match host {
        Host::Domain(name) => {
            // Block trivially obvious internal names. Real DNS-time defense lives in the
            // socket-level guard (a TODO worth taking seriously when the agent talks to
            // arbitrary networks).
            let lowered = name.to_ascii_lowercase();
            if lowered == "localhost"
                || lowered.ends_with(".localhost")
                || lowered.ends_with(".internal")
                || lowered.ends_with(".local")
            {
                return Err(UrlError::HostBlocked(name.to_string()));
            }
            Ok(())
        }
        Host::Ipv4(ip) => check_ip(IpAddr::V4(*ip)),
        Host::Ipv6(ip) => check_ip(IpAddr::V6(*ip)),
    }
}

fn check_ip(ip: IpAddr) -> Result<(), UrlError> {
    let blocked = match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_multicast()
                || v4.is_unspecified()
                || v4.is_documentation()
                // Cloud metadata services
                || v4.octets() == [169, 254, 169, 254]
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                // ULA fc00::/7
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                // Link-local fe80::/10
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                // IPv4-mapped — re-check the embedded v4 to defeat the obvious bypass
                || v6.to_ipv4_mapped().is_some_and(|m| {
                    m.is_loopback()
                        || m.is_private()
                        || m.is_link_local()
                        || m.is_unspecified()
                        || m.octets() == [169, 254, 169, 254]
                })
        }
    };
    if blocked {
        return Err(UrlError::HostBlocked(ip.to_string()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_https() {
        assert!(matches!(
            FetchUrl::try_from("http://example.com").expect_err("must reject"),
            UrlError::DisallowedScheme(_)
        ));
        assert!(matches!(
            FetchUrl::try_from("file:///etc/passwd").expect_err("must reject"),
            UrlError::DisallowedScheme(_)
        ));
    }

    #[test]
    fn rejects_loopback_and_private_and_metadata() {
        for raw in [
            "https://127.0.0.1/",
            "https://10.0.0.1/",
            "https://192.168.1.1/",
            "https://169.254.169.254/latest/meta-data/",
            "https://[::1]/",
            "https://[fe80::1]/",
        ] {
            let err = FetchUrl::try_from(raw).expect_err("must reject");
            assert!(
                matches!(err, UrlError::HostBlocked(_)),
                "{raw} not blocked: {err:?}"
            );
        }
    }

    #[test]
    fn rejects_localhost_names() {
        for raw in [
            "https://localhost/",
            "https://app.localhost/",
            "https://service.internal/",
            "https://my.local/",
        ] {
            let err = FetchUrl::try_from(raw).expect_err("must reject");
            assert!(
                matches!(err, UrlError::HostBlocked(_)),
                "{raw} not blocked: {err:?}"
            );
        }
    }

    #[test]
    fn accepts_public_https() {
        assert!(FetchUrl::try_from("https://example.com/").is_ok());
        assert!(FetchUrl::try_from("https://api.example.com/v1/users?id=42#frag").is_ok());
    }

    #[test]
    fn rejects_ipv4_mapped_loopback() {
        // ::ffff:127.0.0.1 must not bypass the v4 loopback check.
        assert!(matches!(
            FetchUrl::try_from("https://[::ffff:127.0.0.1]/").expect_err("must reject"),
            UrlError::HostBlocked(_)
        ));
    }
}
