//! Per-claim [`Agent`] resolver.
//!
//! Each worker claim carries `receiver_agent_id`; before driving the turn the
//! worker calls [`Agents::get`] to obtain the right [`Agent`] instance. Today
//! every agent shares the same collaborators (provider, sessions, tools,
//! hooks); the registry enforces that the id is a known row in the `agents`
//! table and hands back a clone of the shared agent. The seam is here so a
//! future change can specialise per-agent (model, tool subset, hooks)
//! without touching every call site.
//!
//! The cache is bounded ([`super::AGENT_PROMPT_CACHE_CAP`] / TTL constants
//! from the same module — agents and prompts share the same scaling budget).
//! On miss, [`Agents::get`] hits [`AgentStore::read`] and admits a freshly
//! built [`Agent`]; on TTL expiry the row is reloaded so an admin's edit is
//! observed within the window.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use thiserror::Error;

use crate::agent_core::Agent;
use crate::clock::SharedClock;

use super::error::AgentStoreError;
use super::store::SharedAgentStore;
use super::types::{AgentId, AgentRecord};

/// Resolve an [`Agent`] for a given [`AgentId`].
#[async_trait]
pub trait Agents: fmt::Debug + Send + Sync {
    /// Fetch the agent for `id`. Returns [`AgentsError::NotFound`] if no row
    /// matches and [`AgentsError::Store`] for any other backend failure.
    async fn get(&self, id: AgentId) -> Result<Agent, AgentsError>;
}

/// Cheap-clone handle the worker pool holds.
pub type SharedAgents = Arc<dyn Agents>;

/// Errors raised by the registry.
#[derive(Debug, Error)]
pub enum AgentsError {
    #[error("agent {0:?} not found")]
    NotFound(AgentId),

    #[error("agent store: {0}")]
    Store(#[from] AgentStoreError),
}

/// Function that turns an [`AgentRecord`] into a fully-built [`Agent`].
///
/// Holds whatever shared collaborators the deployment chose at startup. The
/// registry calls this exactly once per cache miss; after that the resulting
/// [`Agent`] is cloned (it is `Clone` because all its fields are
/// reference-counted).
pub type AgentFactory = Arc<dyn Fn(&AgentRecord) -> Agent + Send + Sync>;

/// Bounded TTL cache keyed by [`AgentId`], mapping to fully-built [`Agent`].
///
/// The factory closure decides what `Agent` to build for a given row — for
/// now every deployment passes the same closure (shared collaborators), but
/// per-agent customisation can replace it without touching the worker pool.
pub struct CachedAgents {
    store: SharedAgentStore,
    factory: AgentFactory,
    cache: Mutex<HashMap<AgentId, Entry>>,
    cap: usize,
    ttl: Duration,
    clock: SharedClock,
}

#[derive(Clone)]
struct Entry {
    agent: Agent,
    fetched_at: Instant,
}

impl CachedAgents {
    /// Construct with explicit caps. Both `cap` and `ttl` must be non-zero;
    /// CLAUDE.md §6 says zero means caller error.
    #[must_use]
    pub fn new(
        store: SharedAgentStore,
        factory: AgentFactory,
        cap: usize,
        ttl: Duration,
        clock: SharedClock,
    ) -> Self {
        assert!(cap > 0, "invariant: CachedAgents cap must be > 0");
        assert!(!ttl.is_zero(), "invariant: CachedAgents ttl must be > 0");
        Self {
            store,
            factory,
            cache: Mutex::new(HashMap::new()),
            cap,
            ttl,
            clock,
        }
    }

    fn lookup_fresh(&self, id: AgentId, now: Instant) -> Option<Agent> {
        let cache = self
            .cache
            .lock()
            .expect("invariant: CachedAgents mutex never poisoned");
        let entry = cache.get(&id)?;
        if now.saturating_duration_since(entry.fetched_at) >= self.ttl {
            return None;
        }
        Some(entry.agent.clone())
    }

    fn admit(&self, id: AgentId, agent: Agent, now: Instant) {
        let mut cache = self
            .cache
            .lock()
            .expect("invariant: CachedAgents mutex never poisoned");
        if cache.len() >= self.cap && !cache.contains_key(&id) {
            evict_one(&mut cache, now, self.ttl);
            assert!(
                cache.len() < self.cap,
                "invariant: CachedAgents eviction made room"
            );
        }
        cache.insert(
            id,
            Entry {
                agent,
                fetched_at: now,
            },
        );
    }
}

#[async_trait]
impl Agents for CachedAgents {
    async fn get(&self, id: AgentId) -> Result<Agent, AgentsError> {
        let now = self.clock.now();
        if let Some(agent) = self.lookup_fresh(id, now) {
            return Ok(agent);
        }
        // Cache miss / expiry — reload under no lock.
        let record = match self.store.read(id).await {
            Ok(rec) => rec,
            Err(AgentStoreError::NotFound(_)) => return Err(AgentsError::NotFound(id)),
            Err(e) => return Err(AgentsError::Store(e)),
        };
        let agent = (self.factory)(&record);
        self.admit(id, agent.clone(), now);
        Ok(agent)
    }
}

impl fmt::Debug for CachedAgents {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CachedAgents")
            .field("cap", &self.cap)
            .field("ttl", &self.ttl)
            .finish_non_exhaustive()
    }
}

/// Evict the oldest expired entry, falling back to the absolute oldest when
/// nothing has expired. Bounded scan over `cap` entries.
fn evict_one(cache: &mut HashMap<AgentId, Entry>, now: Instant, ttl: Duration) {
    let mut oldest_expired: Option<AgentId> = None;
    let mut oldest_overall: Option<(AgentId, Instant)> = None;
    for (id, entry) in cache.iter() {
        if now.saturating_duration_since(entry.fetched_at) >= ttl {
            oldest_expired = Some(*id);
            break;
        }
        match oldest_overall {
            None => oldest_overall = Some((*id, entry.fetched_at)),
            Some((_, ts)) if entry.fetched_at < ts => {
                oldest_overall = Some((*id, entry.fetched_at));
            }
            _ => {}
        }
    }
    let victim = oldest_expired.or_else(|| oldest_overall.map(|(id, _)| id));
    if let Some(id) = victim {
        cache.remove(&id);
    }
}
