//! Test-only [`OrgLanguageResolver`] that returns a fixed [`Language`].
//!
//! Avoids the dance of `PgOrgLanguageResolver` (needs a real
//! `SharedUserStore` + `SharedAgentStore` and reads from the DB) for the
//! many tests that only care about which prompt body the language picks,
//! not how the language was resolved. Constructed with a starting
//! language; [`StaticOrgLanguageResolver::set`] swaps it at runtime so
//! invalidation paths can be exercised.

use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;

use patom::agents::AgentId;
use patom::auth::{
    Language, LanguageResolverError, OrgLanguageResolver, SharedOrgLanguageResolver,
};
use patom::prompts::Prompts;

#[derive(Debug)]
pub struct StaticOrgLanguageResolver {
    language: Mutex<Language>,
    invalidations: Mutex<usize>,
}

impl StaticOrgLanguageResolver {
    pub fn new(language: Language) -> Self {
        Self {
            language: Mutex::new(language),
            invalidations: Mutex::new(0),
        }
    }

    pub fn set(&self, language: Language) {
        *self.language.lock().expect("lock") = language;
    }

    pub fn invalidations(&self) -> usize {
        *self.invalidations.lock().expect("lock")
    }
}

#[async_trait]
impl OrgLanguageResolver for StaticOrgLanguageResolver {
    async fn language_for_agent(&self, _agent: AgentId) -> Result<Language, LanguageResolverError> {
        Ok(*self.language.lock().expect("lock"))
    }

    fn invalidate_all(&self) {
        *self.invalidations.lock().expect("lock") += 1;
    }
}

/// Convenience: cheap-clone English resolver wrapped in the trait
/// handle the composition root uses. Tests that wire a full `AppState`
/// reach for this.
pub fn english_resolver() -> SharedOrgLanguageResolver {
    Arc::new(StaticOrgLanguageResolver::new(Language::En))
}

/// Convenience: load the shipped per-language prompt registry. The
/// registry is process-cheap; tests construct one each time rather than
/// memoizing to keep teardown clean.
pub fn prompts() -> Arc<Prompts> {
    Arc::new(Prompts::load())
}
