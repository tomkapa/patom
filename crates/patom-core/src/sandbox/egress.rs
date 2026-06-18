//! Per-org network egress policy for the sandbox.
//!
//! Default-deny: untrusted code runs `--network=none` unless the org has
//! explicitly allowlisted a host. The allowlist is resolved by the `run_code`
//! tool into an [`EgressPolicy`] that travels in the [`crate::sandbox::RunRequest`];
//! the host-side proxy (Stage 6 infra) enforces it.
//!
//! This module owns the validated host newtype and the per-run policy. The
//! persisted allowlist collection and its Postgres store land in a later stage;
//! every host that reaches either path is parsed here first, through the same
//! SSRF deny floor `web_fetch` uses, so an org can never allowlist `localhost`
//! or the cloud metadata endpoint.

use std::collections::BTreeSet;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use sqlx::types::Json;
use thiserror::Error;

use crate::auth::{OrgId, run_privileged};
use crate::sandbox::limits::{MAX_EGRESS_HOST_LEN, MAX_EGRESS_HOSTS_PER_ORG};
use crate::tools::{UrlError, check_egress_host};
use crate::types::ParseError;

/// A host an org has cleared for sandbox outbound traffic.
///
/// Construction reuses the `web_fetch` SSRF deny floor ([`check_host`]) so a
/// loopback / private / link-local / metadata target can never enter an
/// allowlist (§1, §5). The wire form is the bare host (no scheme, no port).
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EgressHost(String);

impl EgressHost {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for EgressHost {
    type Error = ParseError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        if raw.is_empty() {
            return Err(ParseError::Empty {
                field: "egress_host",
            });
        }
        if raw.len() > MAX_EGRESS_HOST_LEN {
            return Err(ParseError::TooLong {
                field: "egress_host",
                max: MAX_EGRESS_HOST_LEN,
                got: raw.len(),
            });
        }
        // Reuse the shared SSRF deny floor. The wrapper parses `raw` as a bare
        // host (rejecting ports, paths, and schemes), so an allowlist entry is
        // always a single resolvable host outside every blocked range.
        check_egress_host(&raw).map_err(deny_floor_to_parse)?;
        Ok(Self(raw))
    }
}

impl TryFrom<&str> for EgressHost {
    type Error = ParseError;

    fn try_from(raw: &str) -> Result<Self, Self::Error> {
        Self::try_from(raw.to_owned())
    }
}

impl std::fmt::Debug for EgressHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("EgressHost").field(&self.0).finish()
    }
}

impl std::fmt::Display for EgressHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Collapse a deny-floor [`UrlError`] into the boundary [`ParseError`]. A blocked
/// host is malformed *as an allowlist entry*; the detail string keeps the reason.
fn deny_floor_to_parse(e: UrlError) -> ParseError {
    match e {
        UrlError::HostBlocked(_) => ParseError::Malformed {
            field: "egress_host",
            detail: "host is in a blocked range (loopback / private / link-local / metadata)",
        },
        _ => ParseError::Malformed {
            field: "egress_host",
            detail: "not a valid host",
        },
    }
}

/// The network policy that travels with a single run.
///
/// A closed sum (§1): the backend matches exhaustively, so adding a future mode
/// (say, a transparent proxy) is a compile error everywhere it matters. An empty
/// org allowlist resolves to [`Self::DenyAll`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EgressPolicy {
    /// `--network=none`. The default and the floor.
    DenyAll,
    /// Outbound permitted only to these hosts, enforced host-side.
    Allow(Vec<EgressHost>),
}

impl EgressPolicy {
    /// Does this policy permit any network at all? Used by the backend to decide
    /// between `--network=none` and wiring the allowlist proxy.
    #[must_use]
    pub fn is_deny_all(&self) -> bool {
        match self {
            Self::DenyAll => true,
            Self::Allow(hosts) => hosts.is_empty(),
        }
    }
}

/// An org's persisted set of allowlisted egress hosts.
///
/// A deduplicated, capped set (§5) modeled on `AllowedMcpTools`: every host is a
/// validated [`EgressHost`], the count is bounded by [`MAX_EGRESS_HOSTS_PER_ORG`],
/// and (de)serialization funnels through the same `TryFrom` so a tampered row
/// can't smuggle a blocked host past the deny floor. An empty set is the default
/// and resolves to [`EgressPolicy::DenyAll`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EgressAllowlist(BTreeSet<EgressHost>);

impl EgressAllowlist {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Resolve the persisted set into the per-run policy. Empty ⇒ deny all.
    #[must_use]
    pub fn to_policy(&self) -> EgressPolicy {
        if self.0.is_empty() {
            EgressPolicy::DenyAll
        } else {
            EgressPolicy::Allow(self.0.iter().cloned().collect())
        }
    }
}

impl TryFrom<Vec<String>> for EgressAllowlist {
    type Error = ParseError;

    fn try_from(raw: Vec<String>) -> Result<Self, Self::Error> {
        if raw.len() > MAX_EGRESS_HOSTS_PER_ORG {
            return Err(ParseError::TooLong {
                field: "egress_allowlist",
                max: MAX_EGRESS_HOSTS_PER_ORG,
                got: raw.len(),
            });
        }
        let mut set = BTreeSet::new();
        for host in raw {
            set.insert(EgressHost::try_from(host)?);
        }
        Ok(Self(set))
    }
}

impl Serialize for EgressAllowlist {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(self.0.iter().map(EgressHost::as_str))
    }
}

impl<'de> Deserialize<'de> for EgressAllowlist {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = Vec::<String>::deserialize(deserializer)?;
        Self::try_from(raw).map_err(serde::de::Error::custom)
    }
}

/// One error per module boundary (§12) for the egress-allowlist store.
#[derive(Debug, Error)]
pub enum EgressStoreError {
    /// A persisted row failed to parse back into a valid allowlist — e.g. a
    /// host that no longer passes the deny floor. Fail closed: the caller treats
    /// this as "no network", never as "allow everything".
    #[error("stored egress allowlist is malformed: {0}")]
    Malformed(String),

    /// Underlying database failure.
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// Per-org egress-allowlist persistence. The `run_code` tool reads through this
/// to resolve the [`EgressPolicy`] for a run; admin surfaces write through it.
#[async_trait]
pub trait OrgEgressStore: std::fmt::Debug + Send + Sync + 'static {
    /// The org's allowlist, or an empty (deny-all) set if none is configured.
    async fn allowlist_for_org(&self, org: OrgId) -> Result<EgressAllowlist, EgressStoreError>;

    /// Replace the org's allowlist.
    async fn set_allowlist(
        &self,
        org: OrgId,
        allowlist: EgressAllowlist,
    ) -> Result<(), EgressStoreError>;
}

/// Cheap-clone handle threaded into the tool (mirrors `SharedSandbox`).
pub type SharedOrgEgressStore = std::sync::Arc<dyn OrgEgressStore>;

/// Postgres-backed [`OrgEgressStore`] over the `org_egress_allowlist` table.
///
/// Reads and writes run `run_privileged` with an explicit `org_id` filter: the
/// allowlist is resolved during a confined run whose org is already
/// authenticated upstream, so the query binds the tenant directly rather than
/// relying on the session principal (the RLS policy on the table is
/// defence-in-depth — memory: rls-gates-membership-not-active-org).
#[derive(Debug, Clone)]
pub struct PgOrgEgressStore {
    pool: PgPool,
}

impl PgOrgEgressStore {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl OrgEgressStore for PgOrgEgressStore {
    async fn allowlist_for_org(&self, org: OrgId) -> Result<EgressAllowlist, EgressStoreError> {
        let row: Option<(Json<Vec<String>>,)> = run_privileged(&self.pool, async |tx| {
            let row = sqlx::query_as::<_, (Json<Vec<String>>,)>(
                "SELECT hosts FROM org_egress_allowlist WHERE org_id = $1",
            )
            .bind(org)
            .fetch_optional(&mut **tx.tx_mut())
            .await?;
            Ok::<_, EgressStoreError>(row)
        })
        .await?;
        // Fail-closed: re-validate every stored host through the deny floor. A
        // tampered or now-blocked entry surfaces as `Malformed`, never as a
        // silently-allowed host.
        match row {
            None => Ok(EgressAllowlist::default()),
            Some((Json(hosts),)) => EgressAllowlist::try_from(hosts)
                .map_err(|e| EgressStoreError::Malformed(e.to_string())),
        }
    }

    async fn set_allowlist(
        &self,
        org: OrgId,
        allowlist: EgressAllowlist,
    ) -> Result<(), EgressStoreError> {
        run_privileged(&self.pool, async |tx| {
            sqlx::query(
                "INSERT INTO org_egress_allowlist (org_id, hosts, updated_at) \
                 VALUES ($1, $2, now()) \
                 ON CONFLICT (org_id) DO UPDATE SET \
                   hosts = EXCLUDED.hosts, updated_at = EXCLUDED.updated_at",
            )
            .bind(org)
            .bind(Json(&allowlist))
            .execute(&mut **tx.tx_mut())
            .await?;
            Ok::<_, EgressStoreError>(())
        })
        .await
    }
}

/// In-memory [`OrgEgressStore`] for tests. Lives outside `#[cfg(test)]` (gated on
/// `test-catalog`) so the integration-test crate can build a tool without a real
/// Postgres, mirroring `InMemoryAssetStore`.
#[cfg(any(test, feature = "test-catalog"))]
#[derive(Debug, Default)]
pub struct InMemoryOrgEgressStore {
    map: std::sync::Mutex<std::collections::HashMap<OrgId, EgressAllowlist>>,
}

#[cfg(any(test, feature = "test-catalog"))]
impl InMemoryOrgEgressStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[cfg(any(test, feature = "test-catalog"))]
#[async_trait]
impl OrgEgressStore for InMemoryOrgEgressStore {
    async fn allowlist_for_org(&self, org: OrgId) -> Result<EgressAllowlist, EgressStoreError> {
        Ok(self
            .map
            .lock()
            .expect("invariant: egress map mutex not poisoned in tests")
            .get(&org)
            .cloned()
            .unwrap_or_default())
    }

    async fn set_allowlist(
        &self,
        org: OrgId,
        allowlist: EgressAllowlist,
    ) -> Result<(), EgressStoreError> {
        self.map
            .lock()
            .expect("invariant: egress map mutex not poisoned in tests")
            .insert(org, allowlist);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn egress_host_accepts_public_domain() {
        let h = EgressHost::try_from("api.example.com").expect("public host");
        assert_eq!(h.as_str(), "api.example.com");
    }

    #[test]
    fn egress_host_rejects_localhost() {
        let err = EgressHost::try_from("localhost").expect_err("localhost blocked");
        assert!(matches!(
            err,
            ParseError::Malformed {
                field: "egress_host",
                ..
            }
        ));
    }

    #[test]
    fn egress_host_rejects_metadata_ip() {
        let err = EgressHost::try_from("169.254.169.254").expect_err("metadata blocked");
        assert!(matches!(err, ParseError::Malformed { .. }));
    }

    #[test]
    fn egress_host_rejects_private_ip() {
        let err = EgressHost::try_from("10.0.0.1").expect_err("private blocked");
        assert!(matches!(err, ParseError::Malformed { .. }));
    }

    #[test]
    fn egress_host_rejects_empty() {
        let err = EgressHost::try_from("").expect_err("empty rejected");
        assert!(matches!(err, ParseError::Empty { .. }));
    }

    #[test]
    fn deny_all_and_empty_allow_both_deny() {
        assert!(EgressPolicy::DenyAll.is_deny_all());
        assert!(EgressPolicy::Allow(vec![]).is_deny_all());
        let one = EgressPolicy::Allow(vec![EgressHost::try_from("api.example.com").expect("host")]);
        assert!(!one.is_deny_all());
    }

    #[test]
    fn empty_allowlist_resolves_to_deny_all() {
        assert_eq!(
            EgressAllowlist::default().to_policy(),
            EgressPolicy::DenyAll
        );
    }

    #[test]
    fn non_empty_allowlist_resolves_to_allow() {
        let allow = EgressAllowlist::try_from(vec!["api.example.com".to_owned()]).expect("allow");
        assert!(matches!(allow.to_policy(), EgressPolicy::Allow(hosts) if hosts.len() == 1));
    }

    #[test]
    fn allowlist_rejects_blocked_host() {
        let err = EgressAllowlist::try_from(vec!["localhost".to_owned()]).expect_err("blocked");
        assert!(matches!(err, ParseError::Malformed { .. }));
    }

    #[test]
    fn allowlist_json_round_trip() {
        let allow = EgressAllowlist::try_from(vec![
            "api.example.com".to_owned(),
            "files.example.org".to_owned(),
        ])
        .expect("allow");
        let json = serde_json::to_string(&allow).expect("serialize");
        let back: EgressAllowlist = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(allow, back);
    }

    #[test]
    fn allowlist_deserialize_rejects_blocked_host() {
        // A tampered row cannot smuggle a blocked host past the deny floor.
        let err = serde_json::from_str::<EgressAllowlist>(r#"["169.254.169.254"]"#)
            .expect_err("blocked host in stored row");
        assert!(err.to_string().contains("egress_host"));
    }

    #[tokio::test]
    async fn in_memory_store_defaults_empty_then_round_trips() {
        let store = InMemoryOrgEgressStore::new();
        let org = OrgId::new();
        assert!(
            store
                .allowlist_for_org(org)
                .await
                .expect("default")
                .is_empty()
        );

        let allow = EgressAllowlist::try_from(vec!["api.example.com".to_owned()]).expect("allow");
        store.set_allowlist(org, allow.clone()).await.expect("set");
        assert_eq!(store.allowlist_for_org(org).await.expect("get"), allow);
    }
}
