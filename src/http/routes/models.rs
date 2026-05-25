//! Read-only catalog of LLM models the workspace can pin per-agent.
//!
//! `GET /models` — returns every entry from
//! [`crate::provider::catalog::MODEL_CATALOG`] (plus the test-catalog
//! extension when the cargo feature is on). The FE consumes this to
//! populate the agent-detail model picker — the same names are
//! parse-validated against the catalog on `PUT /agents/{id}`.
//!
//! Authenticated read; no per-org filtering — the catalog is workspace-wide.

use axum::Json;
use axum::Router;
use axum::routing::get;
use serde::Serialize;

use crate::provider::Model;

use super::super::state::AppState;

pub(super) fn router() -> Router<AppState> {
    Router::new().route("/models", get(list_models))
}

/// One catalog row on the wire. Mirrors
/// [`crate::provider::catalog::CatalogEntry`] but flattens the provider
/// to its `snake_case` discriminator so the FE can render a chip without
/// holding a provider enum of its own.
#[derive(Debug, Serialize)]
struct ModelEntry {
    /// Catalog id the agent's `model` field accepts on PUT, e.g.
    /// `"claude-sonnet-4-6"`. The FE sends this verbatim.
    id: &'static str,
    /// `snake_case` provider name (`"anthropic"`, `"openai"`, …) for
    /// rendering a per-row chip on the picker.
    provider: &'static str,
}

#[tracing::instrument(name = "models.list", skip_all)]
async fn list_models() -> Json<Vec<ModelEntry>> {
    let out: Vec<ModelEntry> = Model::all()
        .map(|m| ModelEntry {
            id: m.as_str(),
            provider: m.provider().as_str(),
        })
        .collect();
    Json(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn list_models_returns_every_catalog_entry() {
        let Json(out) = list_models().await;
        assert!(!out.is_empty(), "catalog must not be empty");

        // Known production rows present.
        let ids: Vec<&str> = out.iter().map(|e| e.id).collect();
        assert!(
            ids.contains(&"claude-sonnet-4-6"),
            "expected sonnet 4.6 in catalog, got: {ids:?}"
        );

        // Every row carries a non-empty provider name from the closed enum.
        for entry in &out {
            assert!(!entry.id.is_empty());
            assert!(
                matches!(entry.provider, "anthropic" | "openai" | "deepseek"),
                "unexpected provider: {}",
                entry.provider
            );
        }
    }

    #[tokio::test]
    async fn list_models_count_matches_catalog_iterator() {
        let Json(out) = list_models().await;
        let direct = Model::all().count();
        assert_eq!(out.len(), direct);
    }
}
