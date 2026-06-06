//! Trait-contract tests for [`patom::memory::PgMemoryStore`]:
//!
//! - journal append correctness
//! - materialized-view consistency
//! - replay rebuild from events
//! - concurrent-mutation safety
//! - `prompt_requests` migration safety (existing rows continue to dispatch
//!   as `Normal`).
//!
//! Each test uses a fresh schema so they can run in parallel.

#![allow(clippy::expect_used)]

use std::sync::Arc;

use patom::clock::SystemClock;
use patom::memory::{
    MemoryContent, MemoryEventPayload, MemoryId, MemoryKind, MemoryMutation, MemoryState,
    MemoryStore, MemoryStoreError, MutationKind, MutationSource, MutationSourceKind, PgMemoryStore,
    ResolutionOutcome, ResolutionReason, SearchFilter, run_librarian_sweep,
};
use patom::provider::embed_one;
use patom::runtime::{PromptRequestId, RequestKind};
use sqlx::PgPool;

mod common;
use common::pg::{human_to_agent_session, seed_prompt_request, seed_tenant};

fn store(pool: &PgPool) -> Arc<PgMemoryStore> {
    Arc::new(PgMemoryStore::new(
        pool.clone(),
        SystemClock::shared(),
        common::embedding::FakeEmbeddingProvider::shared(),
    ))
}

fn content(s: &str) -> MemoryContent {
    MemoryContent::try_from(s).expect("valid memory content")
}

fn write(
    seed: &common::pg::Seed,
    kind: MemoryKind,
    body: &str,
    state: MemoryState,
    pinned: bool,
) -> MemoryMutation {
    MemoryMutation::Write {
        agent: seed.agent_id,
        kind,
        content: content(body),
        state,
        pinned,
        source: MutationSource::Operator,
    }
}

#[sqlx::test]
async fn write_appends_event_and_materialised_row(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let s = store(&pool);

    let outcome = s
        .apply(write(
            &seed,
            MemoryKind::Identity,
            "I default to terse replies.",
            MemoryState::Held,
            false,
        ))
        .await
        .expect("apply write");

    let row = outcome.row.expect("write returns row");
    assert_eq!(row.id, outcome.memory_id);
    assert_eq!(row.agent_id, seed.agent_id);
    assert_eq!(row.kind, MemoryKind::Identity);
    assert_eq!(row.state, MemoryState::Held);
    assert!(!row.pinned);
    assert_eq!(row.content.as_str(), "I default to terse replies.");

    let events = s.list_events(seed.agent_id).await.expect("events");
    assert_eq!(events.len(), 1);
    let ev = &events[0];
    assert_eq!(ev.id, outcome.event_id);
    assert_eq!(ev.mutation_kind(), MutationKind::Write);
    assert_eq!(ev.target_memory_id, outcome.memory_id);
    let MemoryEventPayload::Write { content, kind, .. } = &ev.payload else {
        panic!("expected Write payload, got {:?}", ev.payload);
    };
    assert_eq!(content.as_str(), "I default to terse replies.");
    assert_eq!(*kind, MemoryKind::Identity);
    assert_eq!(ev.source.kind(), MutationSourceKind::Operator);

    let listed = s.list(seed.agent_id).await.expect("list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, outcome.memory_id);
}

#[sqlx::test]
async fn update_records_before_and_after_content(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let s = store(&pool);

    let first = s
        .apply(write(
            &seed,
            MemoryKind::Other,
            "translator is fast on European languages",
            MemoryState::Tentative,
            false,
        ))
        .await
        .expect("write");

    let updated = s
        .apply(MemoryMutation::Update {
            agent: seed.agent_id,
            target: first.memory_id,
            content: content("translator is fast on Romance languages"),
            state: MemoryState::Tentative,
            source: MutationSource::Operator,
        })
        .await
        .expect("update");

    let row = updated.row.expect("update returns row");
    assert_eq!(row.id, first.memory_id);
    assert_eq!(
        row.content.as_str(),
        "translator is fast on Romance languages"
    );

    let events = s.list_events(seed.agent_id).await.expect("events");
    assert_eq!(events.len(), 2);
    let upd = &events[1];
    assert_eq!(upd.mutation_kind(), MutationKind::Update);
    let MemoryEventPayload::Update { before, after, .. } = &upd.payload else {
        panic!("expected Update payload, got {:?}", upd.payload);
    };
    assert_eq!(before.as_str(), "translator is fast on European languages");
    assert_eq!(after.as_str(), "translator is fast on Romance languages");
}

#[sqlx::test]
async fn forget_removes_materialised_but_journal_keeps_event(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let s = store(&pool);

    let written = s
        .apply(write(
            &seed,
            MemoryKind::Procedure,
            "ask one focused clarifying question",
            MemoryState::Held,
            false,
        ))
        .await
        .expect("write");

    let forget = s
        .apply(MemoryMutation::Forget {
            agent: seed.agent_id,
            target: written.memory_id,
            source: MutationSource::Librarian,
        })
        .await
        .expect("forget");
    assert!(forget.row.is_none());

    assert!(
        s.get(written.memory_id).await.expect("get").is_none(),
        "row should be gone from materialised view"
    );

    let events = s.list_events(seed.agent_id).await.expect("events");
    assert_eq!(events.len(), 2);
    let f = &events[1];
    assert_eq!(f.mutation_kind(), MutationKind::Forget);
    let MemoryEventPayload::Forget { before } = &f.payload else {
        panic!("expected Forget payload, got {:?}", f.payload);
    };
    assert_eq!(before.as_str(), "ask one focused clarifying question");
    assert_eq!(f.source.kind(), MutationSourceKind::Librarian);
}

#[sqlx::test]
async fn update_unknown_id_is_not_found(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let s = store(&pool);

    let stranger = MemoryId::new();
    let err = s
        .apply(MemoryMutation::Update {
            agent: seed.agent_id,
            target: stranger,
            content: content("nope"),
            state: MemoryState::Tentative,
            source: MutationSource::Operator,
        })
        .await
        .expect_err("should fail");
    assert!(matches!(err, MemoryStoreError::NotFound { .. }));
}

#[sqlx::test]
async fn pinned_row_rejects_agent_update_but_accepts_operator_override(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let s = store(&pool);

    let pinned = s
        .apply(write(
            &seed,
            MemoryKind::Identity,
            "operator pinned identity",
            MemoryState::Core,
            true,
        ))
        .await
        .expect("write pinned");

    let agent_err = s
        .apply(MemoryMutation::Update {
            agent: seed.agent_id,
            target: pinned.memory_id,
            content: content("agent attempt"),
            state: MemoryState::Tentative,
            source: MutationSource::Turn(PromptRequestId::new()),
        })
        .await
        .expect_err("agent edit blocked");
    assert!(matches!(
        agent_err,
        MemoryStoreError::PinnedImmutable { .. }
    ));

    let _ok = s
        .apply(MemoryMutation::Update {
            agent: seed.agent_id,
            target: pinned.memory_id,
            content: content("operator override"),
            state: MemoryState::Core,
            source: MutationSource::Operator,
        })
        .await
        .expect("operator override succeeds");
}

#[sqlx::test]
#[allow(clippy::too_many_lines)] // mixed-mutation scenario + embedding regression check
async fn rebuild_materialized_reproduces_live_view(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let s = store(&pool);

    let a = s
        .apply(write(
            &seed,
            MemoryKind::Identity,
            "first",
            MemoryState::Held,
            false,
        ))
        .await
        .expect("a");
    let _b = s
        .apply(write(
            &seed,
            MemoryKind::Other,
            "second",
            MemoryState::Tentative,
            false,
        ))
        .await
        .expect("b");
    let c = s
        .apply(write(
            &seed,
            MemoryKind::Procedure,
            "third",
            MemoryState::Held,
            false,
        ))
        .await
        .expect("c");

    s.apply(MemoryMutation::Update {
        agent: seed.agent_id,
        target: a.memory_id,
        content: content("first revised"),
        state: MemoryState::Tentative,
        source: MutationSource::Operator,
    })
    .await
    .expect("update");

    s.apply(MemoryMutation::Forget {
        agent: seed.agent_id,
        target: c.memory_id,
        source: MutationSource::Operator,
    })
    .await
    .expect("forget");

    let live_ids: Vec<MemoryId> = s
        .list(seed.agent_id)
        .await
        .expect("list")
        .iter()
        .map(|r| r.id)
        .collect();
    let live_contents: Vec<String> = s
        .list(seed.agent_id)
        .await
        .expect("list")
        .into_iter()
        .map(|r| r.content.as_str().to_owned())
        .collect();

    // Pre-rebuild embedding search baseline. `rebuild_materialized`
    // DELETEs every materialized row and replays the journal, which by
    // itself would leave `embedding IS NULL` (the embedding column lives
    // on `agent_memories`, not in `memory_events`). The store snapshots
    // and restores embeddings around the replay so retrieval keeps
    // working — assert that here.
    let embedder = common::embedding::FakeEmbeddingProvider::new();
    let probe = embed_one(&embedder, "first revised")
        .await
        .expect("embed probe");
    let pre_hits = s
        .search_by_embedding(seed.agent_id, &probe, 8, SearchFilter::default())
        .await
        .expect("pre search");
    assert!(
        !pre_hits.is_empty(),
        "fake embedder + live writes should match at least one row before rebuild"
    );

    s.rebuild_materialized(seed.agent_id)
        .await
        .expect("rebuild");

    let rebuilt_ids: Vec<MemoryId> = s
        .list(seed.agent_id)
        .await
        .expect("list2")
        .iter()
        .map(|r| r.id)
        .collect();
    let rebuilt_contents: Vec<String> = s
        .list(seed.agent_id)
        .await
        .expect("list2")
        .into_iter()
        .map(|r| r.content.as_str().to_owned())
        .collect();

    assert_eq!(live_ids, rebuilt_ids);
    assert_eq!(live_contents, rebuilt_contents);

    let post_hits = s
        .search_by_embedding(seed.agent_id, &probe, 8, SearchFilter::default())
        .await
        .expect("post search");
    let pre_ids: Vec<MemoryId> = pre_hits.iter().map(|h| h.row.id).collect();
    let post_ids: Vec<MemoryId> = post_hits.iter().map(|h| h.row.id).collect();
    assert_eq!(
        pre_ids, post_ids,
        "rebuild_materialized must preserve embeddings — search hits diverged"
    );
}

#[sqlx::test]
async fn concurrent_writes_are_independent(pool: PgPool) {
    // Different (agent, target) tuples must not contend; the journal is
    // append-only and each write mints a fresh memory id, so two parallel
    // writers should both succeed without retries.
    let seed = seed_tenant(&pool).await;
    let s = store(&pool);

    let s1 = s.clone();
    let s2 = s.clone();
    let agent = seed.agent_id;

    let h1 = tokio::spawn(async move {
        s1.apply(MemoryMutation::Write {
            agent,
            kind: MemoryKind::Identity,
            content: content("one"),
            state: MemoryState::Held,
            pinned: false,
            source: MutationSource::Operator,
        })
        .await
    });
    let h2 = tokio::spawn(async move {
        s2.apply(MemoryMutation::Write {
            agent,
            kind: MemoryKind::Other,
            content: content("two"),
            state: MemoryState::Tentative,
            pinned: false,
            source: MutationSource::Operator,
        })
        .await
    });

    let r1 = h1.await.expect("join1").expect("apply1");
    let r2 = h2.await.expect("join2").expect("apply2");
    assert_ne!(r1.memory_id, r2.memory_id);

    let listed = s.list(agent).await.expect("list");
    assert_eq!(listed.len(), 2);
}

#[sqlx::test]
async fn prompt_requests_kind_defaults_to_normal(pool: PgPool) {
    // Pre-existing inserters do not specify `kind` or `kind_payload`; the
    // column defaults backfill `'normal'` and the empty `Normal` payload
    // so the worker dispatches them on the existing reply path. This
    // locks in the migration-safety invariant called out in §2.1:
    // existing rows continue to dispatch as `Normal`.
    let seed = seed_tenant(&pool).await;

    let session_store =
        patom::session::PgSessionStore::new(pool.clone(), patom::clock::SystemClock::shared());
    let session =
        human_to_agent_session(&pool, &session_store, seed.agent_id, seed.org_id, seed.user_id).await;
    let id = seed_prompt_request(&pool, session, seed.agent_id, seed.org_id).await;

    let (kind, payload): (RequestKind, serde_json::Value) =
        sqlx::query_as("SELECT kind, kind_payload FROM prompt_requests WHERE id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .expect("fetch");

    assert_eq!(kind, RequestKind::Normal);
    assert_eq!(
        payload,
        serde_json::json!({"kind": "normal", "data": {}}),
        "default backfill should produce the Normal payload"
    );
}

#[sqlx::test]
async fn record_validation_promotes_state(pool: PgPool) {
    use patom::memory::ValidationOrigin;

    let seed = seed_tenant(&pool).await;
    let s = store(&pool);

    // Tentative write via operator path so the source_turn_id FK is NULL.
    let outcome = s
        .apply(MemoryMutation::Write {
            agent: seed.agent_id,
            kind: MemoryKind::Identity,
            content: content("Tabs over spaces"),
            state: MemoryState::Tentative,
            pinned: false,
            source: MutationSource::Operator,
        })
        .await
        .expect("write");

    // First validation promotes Tentative -> Held.
    let row = s
        .record_validation(
            seed.agent_id,
            outcome.memory_id,
            ValidationOrigin::Operator,
            Some("operator endorsement"),
        )
        .await
        .expect("record");
    assert_eq!(row.state, MemoryState::Held);

    // Second validation promotes Held -> Validated.
    let row = s
        .record_validation(
            seed.agent_id,
            outcome.memory_id,
            ValidationOrigin::Operator,
            None,
        )
        .await
        .expect("record");
    assert_eq!(row.state, MemoryState::Validated);
}

#[sqlx::test]
async fn mature_promotes_long_lived_tentative_to_held(pool: PgPool) {
    // Tentative row past the maturation window with no unresolved
    // contradiction promotes to Held; last_validated_at stays put (the
    // validation clock is reserved for independent signals).
    let seed = seed_tenant(&pool).await;
    let s = store(&pool);

    let outcome = s
        .apply(write(
            &seed,
            MemoryKind::Identity,
            "I default to terse replies",
            MemoryState::Tentative,
            false,
        ))
        .await
        .expect("write");

    let before = s.get(outcome.memory_id).await.expect("get").expect("row");
    assert_eq!(before.state, MemoryState::Tentative);

    // Cutoff in the future treats every row as "old enough".
    let future = chrono::Utc::now() + chrono::Duration::seconds(1);
    let promoted = s
        .mature_tentative(seed.agent_id, future)
        .await
        .expect("mature");
    assert_eq!(promoted, 1);

    let after = s.get(outcome.memory_id).await.expect("get").expect("row");
    assert_eq!(after.state, MemoryState::Held);
    assert_eq!(
        after.last_validated_at, before.last_validated_at,
        "maturation must not advance the validation clock",
    );

    // Journal carries an Update event with state=Held and Librarian source.
    let events = s.list_events(seed.agent_id).await.expect("events");
    let last = events.last().expect("at least one event");
    assert_eq!(last.mutation_kind(), MutationKind::Update);
    let MemoryEventPayload::Update {
        before: payload_before,
        after: payload_after,
        state,
        ..
    } = &last.payload
    else {
        panic!("expected Update payload, got {:?}", last.payload);
    };
    assert_eq!(payload_before.as_str(), "I default to terse replies");
    assert_eq!(payload_after.as_str(), "I default to terse replies");
    assert_eq!(*state, MemoryState::Held);
    assert_eq!(last.source.kind(), MutationSourceKind::Librarian);
}

#[sqlx::test]
async fn mature_skips_recent_tentative(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let s = store(&pool);

    let outcome = s
        .apply(write(
            &seed,
            MemoryKind::Identity,
            "fresh tentative",
            MemoryState::Tentative,
            false,
        ))
        .await
        .expect("write");

    // Cutoff in the past — every row is "too young" to promote.
    let past = chrono::Utc::now() - chrono::Duration::days(30);
    let promoted = s
        .mature_tentative(seed.agent_id, past)
        .await
        .expect("mature");
    assert_eq!(promoted, 0);

    let row = s.get(outcome.memory_id).await.expect("get").expect("row");
    assert_eq!(row.state, MemoryState::Tentative);
}

#[sqlx::test]
async fn mature_skips_rows_with_unresolved_contradiction(pool: PgPool) {
    // A tentative row entangled in an unresolved contradiction stays put.
    // The resolution turn is the path forward, not passive maturation.
    let seed = seed_tenant(&pool).await;
    let s = store(&pool);

    let a = s
        .apply(write(
            &seed,
            MemoryKind::Identity,
            "ship on Friday",
            MemoryState::Tentative,
            false,
        ))
        .await
        .expect("a");
    let b = s
        .apply(write(
            &seed,
            MemoryKind::Identity,
            "don't ship on Friday",
            MemoryState::Tentative,
            false,
        ))
        .await
        .expect("b");

    s.record_contradiction(seed.agent_id, a.memory_id, b.memory_id, "test")
        .await
        .expect("contradiction");

    let future = chrono::Utc::now() + chrono::Duration::seconds(1);
    let promoted = s
        .mature_tentative(seed.agent_id, future)
        .await
        .expect("mature");
    assert_eq!(promoted, 0);

    for id in [a.memory_id, b.memory_id] {
        let row = s.get(id).await.expect("get").expect("row");
        assert_eq!(row.state, MemoryState::Tentative);
    }
}

#[sqlx::test]
async fn librarian_sweep_dedup_does_not_emit_validation(pool: PgPool) {
    // Two identical-content rows trip dedup (similarity = 1 from the fake
    // embedding). The sweep merges them — the loser is forgotten — but
    // does NOT emit a `validation_events` row: cross-session re-emergence
    // is too contaminated by memory loading to count as independent
    // evidence (doc/memory.md §1.7).
    let seed = seed_tenant(&pool).await;
    let s = store(&pool);

    s.apply(write(
        &seed,
        MemoryKind::Identity,
        "I prefer terse replies.",
        MemoryState::Held,
        false,
    ))
    .await
    .expect("a");
    s.apply(write(
        &seed,
        MemoryKind::Identity,
        "I prefer terse replies.",
        MemoryState::Held,
        false,
    ))
    .await
    .expect("b");

    let now = chrono::Utc::now();
    let report = run_librarian_sweep(s.as_ref(), seed.agent_id, now)
        .await
        .expect("sweep");
    assert_eq!(report.deduped, 1, "one duplicate should be merged");

    let validation_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM validation_events WHERE agent_id = $1")
            .bind(seed.agent_id)
            .fetch_one(&pool)
            .await
            .expect("count validation_events");
    assert_eq!(
        validation_count, 0,
        "librarian dedup must not produce a validation_events row",
    );
}

#[sqlx::test]
async fn mature_skips_pinned_rows(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let s = store(&pool);

    let outcome = s
        .apply(write(
            &seed,
            MemoryKind::Identity,
            "pinned tentative",
            MemoryState::Tentative,
            true,
        ))
        .await
        .expect("write");

    let future = chrono::Utc::now() + chrono::Duration::seconds(1);
    let promoted = s
        .mature_tentative(seed.agent_id, future)
        .await
        .expect("mature");
    assert_eq!(promoted, 0);

    let row = s.get(outcome.memory_id).await.expect("get").expect("row");
    assert_eq!(row.state, MemoryState::Tentative);
}

#[sqlx::test]
async fn revert_event_undoes_write(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let s = store(&pool);

    let outcome = s
        .apply(MemoryMutation::Write {
            agent: seed.agent_id,
            kind: MemoryKind::Identity,
            content: content("Ephemeral"),
            state: MemoryState::Tentative,
            pinned: false,
            source: MutationSource::Operator,
        })
        .await
        .expect("write");

    let revert = s
        .revert_event(seed.agent_id, outcome.event_id)
        .await
        .expect("revert");
    assert!(revert.is_none(), "row removed by revert");
}

#[sqlx::test]
async fn evict_overflow_drops_below_quota(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let s = store(&pool);

    for i in 0..5 {
        s.apply(MemoryMutation::Write {
            agent: seed.agent_id,
            kind: MemoryKind::Identity,
            content: content(&format!("memory {i}")),
            state: MemoryState::Tentative,
            pinned: false,
            source: MutationSource::Operator,
        })
        .await
        .expect("write");
    }

    let evicted = s.evict_overflow(seed.agent_id, 3).await.expect("evict");
    assert_eq!(evicted.len(), 2);
    let listed = s.list(seed.agent_id).await.expect("list");
    assert_eq!(listed.len(), 3);
}

#[sqlx::test]
async fn decay_demotes_old_validated(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let s = store(&pool);

    let outcome = s
        .apply(MemoryMutation::Write {
            agent: seed.agent_id,
            kind: MemoryKind::Identity,
            content: content("decaying"),
            state: MemoryState::Validated,
            pinned: false,
            source: MutationSource::Operator,
        })
        .await
        .expect("write");

    // Cutoff in the future demotes everything Validated.
    let future = chrono::Utc::now() + chrono::Duration::seconds(1);
    let n = s
        .decay_validated(seed.agent_id, future)
        .await
        .expect("decay");
    assert_eq!(n, 1);
    let row = s.get(outcome.memory_id).await.expect("get").expect("row");
    assert_eq!(row.state, MemoryState::Held);
}

#[sqlx::test]
async fn record_contradiction_is_idempotent(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let s = store(&pool);

    let a = s
        .apply(MemoryMutation::Write {
            agent: seed.agent_id,
            kind: MemoryKind::Identity,
            content: content("ship on Friday"),
            state: MemoryState::Held,
            pinned: false,
            source: MutationSource::Operator,
        })
        .await
        .expect("a");
    let b = s
        .apply(MemoryMutation::Write {
            agent: seed.agent_id,
            kind: MemoryKind::Identity,
            content: content("don't ship on Friday"),
            state: MemoryState::Held,
            pinned: false,
            source: MutationSource::Operator,
        })
        .await
        .expect("b");

    let id1 = s
        .record_contradiction(seed.agent_id, a.memory_id, b.memory_id, "test")
        .await
        .expect("c1");
    let id2 = s
        .record_contradiction(seed.agent_id, a.memory_id, b.memory_id, "again")
        .await
        .expect("c2");
    assert_eq!(id1, id2);
}

#[sqlx::test]
async fn set_pinned_toggles_protection(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let s = store(&pool);

    let outcome = s
        .apply(MemoryMutation::Write {
            agent: seed.agent_id,
            kind: MemoryKind::Identity,
            content: content("invariant"),
            state: MemoryState::Held,
            pinned: false,
            source: MutationSource::Operator,
        })
        .await
        .expect("w");

    let row = s
        .set_pinned(seed.agent_id, outcome.memory_id, true)
        .await
        .expect("pin");
    assert!(row.pinned);

    // Agent path now rejects.
    let err = s
        .apply(MemoryMutation::Update {
            agent: seed.agent_id,
            target: outcome.memory_id,
            content: content("changed"),
            state: MemoryState::Tentative,
            source: MutationSource::Turn(PromptRequestId::new()),
        })
        .await
        .expect_err("pinned protects");
    assert!(matches!(err, MemoryStoreError::PinnedImmutable { .. }));
}

async fn seed_contradiction(
    s: &Arc<PgMemoryStore>,
    seed: &common::pg::Seed,
) -> patom::memory::ContradictionEventId {
    let a = s
        .apply(MemoryMutation::Write {
            agent: seed.agent_id,
            kind: MemoryKind::Identity,
            content: content("ship on Friday"),
            state: MemoryState::Held,
            pinned: false,
            source: MutationSource::Operator,
        })
        .await
        .expect("a");
    let b = s
        .apply(MemoryMutation::Write {
            agent: seed.agent_id,
            kind: MemoryKind::Identity,
            content: content("don't ship on Friday"),
            state: MemoryState::Held,
            pinned: false,
            source: MutationSource::Operator,
        })
        .await
        .expect("b");
    s.record_contradiction(seed.agent_id, a.memory_id, b.memory_id, "test")
        .await
        .expect("contradiction")
}

#[sqlx::test]
async fn resolve_contradiction_mutation_close_links_event(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let s = store(&pool);
    let id = seed_contradiction(&s, &seed).await;

    // Mint a memory event to point at — any apply produces one.
    // Use Operator source so we don't need to seed a prompt_requests row.
    let event = s
        .apply(MemoryMutation::Write {
            agent: seed.agent_id,
            kind: MemoryKind::Procedure,
            content: content("revised procedure"),
            state: MemoryState::Tentative,
            pinned: false,
            source: MutationSource::Operator,
        })
        .await
        .expect("event");

    s.resolve_contradiction(id, ResolutionOutcome::Mutation(event.event_id))
        .await
        .expect("close");

    let row = s.read_contradiction(id).await.expect("read").expect("row");
    assert!(row.resolved_at.is_some());
    assert_eq!(row.resolution_event_id, Some(event.event_id));
    assert_eq!(row.resolution_reason, None);
}

#[sqlx::test]
async fn resolve_contradiction_no_action_close_persists_reason(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let s = store(&pool);
    let id = seed_contradiction(&s, &seed).await;

    let reason = ResolutionReason::try_from(
        "Both memories are correct in different contexts; no mutation needed.".to_string(),
    )
    .expect("reason");
    s.resolve_contradiction(id, ResolutionOutcome::NoAction { reason })
        .await
        .expect("close");

    let row = s.read_contradiction(id).await.expect("read").expect("row");
    assert!(row.resolved_at.is_some());
    assert_eq!(row.resolution_event_id, None);
    assert_eq!(
        row.resolution_reason.as_deref(),
        Some("Both memories are correct in different contexts; no mutation needed.")
    );
}

#[sqlx::test]
async fn resolve_contradiction_is_idempotent(pool: PgPool) {
    let seed = seed_tenant(&pool).await;
    let s = store(&pool);
    let id = seed_contradiction(&s, &seed).await;

    let reason = ResolutionReason::try_from("first close".to_string()).expect("reason");
    s.resolve_contradiction(id, ResolutionOutcome::NoAction { reason })
        .await
        .expect("close 1");

    let first = s.read_contradiction(id).await.expect("r1").expect("r1");
    let stamped_at = first.resolved_at.expect("resolved");

    // Second call should be a no-op — the stored stamp does not change.
    let again = ResolutionReason::try_from("second close attempt".to_string()).expect("reason");
    s.resolve_contradiction(id, ResolutionOutcome::NoAction { reason: again })
        .await
        .expect("close 2");

    let after = s.read_contradiction(id).await.expect("r2").expect("r2");
    assert_eq!(after.resolved_at, Some(stamped_at));
    assert_eq!(after.resolution_reason.as_deref(), Some("first close"));
}

#[test]
fn resolution_reason_rejects_empty_and_oversize() {
    assert!(ResolutionReason::try_from(String::new()).is_err());
    let max = patom::memory::CONTRADICTION_REASON_MAX_BYTES;
    let ok = "x".repeat(max);
    assert!(ResolutionReason::try_from(ok).is_ok());
    let too_long = "x".repeat(max + 1);
    assert!(ResolutionReason::try_from(too_long).is_err());
}
