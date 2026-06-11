//! Per-thread display-name overrides.
//!
//! A person's name is surface-specific — their Patom/IdP name, their
//! Slack handle, their Lark name — but their [`ColleagueId`] is the one
//! canonical identity every subsystem (memory, `send_message`, the feed)
//! keys on. This seam lets the agent's roster *render* the right name for
//! the platform a thread lives on without touching that identity: given a
//! thread, it returns `colleague_id → display label` overrides, and the
//! renderer substitutes them over the canonical colleague names.
//!
//! The agent core depends only on this trait, so it stays
//! platform-agnostic; each adapter (Slack today, Lark later) provides its
//! own impl. Deployments without any adapter use [`NoThreadDisplayNames`].

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;

use crate::threads::ThreadId;

use super::{ColleagueId, ColleagueName};

#[async_trait]
pub trait ThreadDisplayNames: std::fmt::Debug + Send + Sync {
    /// Display-name overrides for a thread, keyed by colleague id. Empty
    /// when the thread has no external-platform identity (e.g. a web
    /// thread) or on a lookup failure — the renderer then falls back to
    /// the canonical colleague names. Best-effort: never errors, so a
    /// directory hiccup degrades the label, not the turn.
    async fn overrides_for_thread(&self, thread: ThreadId) -> HashMap<ColleagueId, ColleagueName>;
}

pub type SharedThreadDisplayNames = Arc<dyn ThreadDisplayNames>;

/// No-op overrides — every thread renders canonical colleague names. The
/// default for deployments with no platform adapter wired.
#[derive(Debug, Default)]
pub struct NoThreadDisplayNames;

#[async_trait]
impl ThreadDisplayNames for NoThreadDisplayNames {
    async fn overrides_for_thread(&self, _thread: ThreadId) -> HashMap<ColleagueId, ColleagueName> {
        HashMap::new()
    }
}
