//! Agent memory subsystem (doc/memory.md).
//!
//! Two seams live side by side:
//!
//! - [`Memory`] / [`StaticMemory`] / [`AgentMemory`] — per-turn system-prompt
//!   assembly (`<core>` + `<role>` + composed `<memory>` section). The agent
//!   loop calls into this once per turn.
//! - [`MemoryStore`] — the agent's persistent, journaled memory rows; one
//!   transactional mutation function ([`MemoryStore::apply`]) is the only
//!   write path.
//!
//! Composition: [`MemorySectionLoader`] turns rows into a rendered
//! [`MemorySection`] keyed by `(session, agent)`; the renderer +
//! `MemoryToolDeps` share one loader so the cached value is identical
//! regardless of which path performed the load.

mod agent;
mod composer;
mod librarian;
mod limits;
mod loader;
mod pg_store;
mod reflection_scheduler;
mod session_cache;
mod r#static;
mod store;
mod traits;
mod types;

// --- per-turn system-prompt assembly --------------------------------------
pub use agent::{
    AgentMemory, CORE_TAG_CLOSE, CORE_TAG_OPEN, DATE_FORMAT, DATE_TAG_CLOSE, DATE_TAG_OPEN,
    LANGUAGE_TAG_CLOSE, LANGUAGE_TAG_OPEN, ORG_RULE_TAG_CLOSE, ORG_RULE_TAG_OPEN, ROLE_TAG_CLOSE,
    ROLE_TAG_OPEN,
};
pub use r#static::StaticMemory;
pub use traits::{Memory, MemoryError, SharedMemory};

// --- composition + caching ------------------------------------------------
pub use composer::{
    MEMORY_TAG_CLOSE, MEMORY_TAG_OPEN, MemorySection, SubjectNames, compose_memory_section,
};
pub use loader::MemorySectionLoader;
pub use session_cache::SessionMemoryCache;

// --- storage --------------------------------------------------------------
pub use pg_store::PgMemoryStore;
pub use store::{
    ContradictionEventRow, MemoryEvent, MemoryEventPayload, MemoryEvidence, MemoryMutation,
    MemoryRow, MemoryStore, MemoryStoreError, MutationOutcome, MutationSource, PairCandidate,
    ResolutionOutcome, ResolutionReason, ScoredMemoryRow, SearchFilter, SharedMemoryStore,
    ValidationOrigin, ValidationSource,
};

// --- background work -----------------------------------------------------
pub use librarian::{LibrarianScheduler, LibrarianSweepReport, run_librarian_sweep};
pub use reflection_scheduler::ReflectionScheduler;

// --- newtypes / value types ----------------------------------------------
pub use types::{
    ContradictionEventId, MemoryContent, MemoryEventId, MemoryHandle, MemoryId, MemoryKind,
    MemoryState, MutationKind, MutationSourceKind, RecallLimit, ValidationEventId,
};

// --- caps / tunables ------------------------------------------------------
pub use limits::{
    CONTEXTUAL_LAYER_MAX_BYTES, CONTEXTUAL_TOP_K, CONTRADICTION_REASON_MAX_BYTES,
    CONTRADICTION_SIMILARITY_THRESHOLD, DEDUP_SIMILARITY_THRESHOLD, LIBRARIAN_BATCH_LIMIT,
    LIBRARIAN_POLL_SECS, MATURATION_WINDOW, MAX_EVENTS_PER_PAGE, MAX_MEMORIES_PER_AGENT,
    MAX_MEMORY_MUTATIONS_PER_REFLECTION, MAX_MEMORY_MUTATIONS_PER_TURN, MAX_RECALL_CALLS_PER_TURN,
    MAX_SIMILAR_PAIRS_PER_AGENT, MEMORY_CONTENT_MAX_BYTES, OPERATOR_AUDIT_PAGE_LIMIT,
    RECALL_DEFAULT_RESULTS, RECALL_MAX_RESULTS, REFLECTION_IDLE_TIMEOUT_SECS,
    REFLECTION_SCHEDULER_BATCH_LIMIT, REFLECTION_SCHEDULER_POLL_SECS, SESSION_MEMORY_CACHE_CAP,
    SESSION_MEMORY_CACHE_TTL_SECS, STABLE_LAYER_MAX_BYTES, VALIDATION_DECAY,
    VALIDATION_EVIDENCE_MAX_BYTES,
};
