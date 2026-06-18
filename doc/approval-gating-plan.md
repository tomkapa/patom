# Human-in-the-loop approval gating (#200)

`ask_approval` → human decision → agent resumes with the decision seeded, with a
**hard pre-execution gate** so an agent cannot perform an approval-gated action
without a matching `Approved` decision in the current DAG.

Built on the **scheduler fresh-trigger pattern** (the worker is run-to-completion
with no wait state): the `ask_approval` turn ends cleanly; a later decision
enqueues a *fresh* `NewTrigger` seeding the decision as a `SystemNote`, reusing
the original DAG root so turn budget + lineage are preserved.

## Status

### Shipped (this branch) — platform-agnostic spine, fully tested

- **Migration 90** `pending_approval` (+ `pending_approval_approvers` child for
  `OneOf`, + `agent_gated_tools` per-agent config), RLS `ENABLE`/`FORCE` +
  `app_user_is_member(org_id)` on all three. Paired up/down, rollback verified
  against the full migration chain.
- **`crates/patom-core/src/approvals/`**
  - `types.rs` — `ApprovalId`, `ActionSummary`, `ApprovalStatus`, `ApproverKind`,
    `ApproverPolicy {Anyone|Colleague|OneOf}`, `Decision`, `Platform`,
    `PlatformTarget`, `PlatformMessageId`, `ApprovalRecord`, `policy_allows`.
  - `error.rs` — one `ApprovalError` (CLAUDE.md §12) + `IntoResponse` for the
    Lark route.
  - `store.rs` + `pg_store.rs` — `ApprovalStore`: idempotent `create`
    (`ON CONFLICT (org_id, idempotency_key) DO NOTHING`), `attach_message`,
    `read`, atomic `decide` (`FOR UPDATE` → authorize → `UPDATE … WHERE
    status='pending'`, idempotent double-click), `expire_due` (bounded sweep),
    `has_approved_for_dag` (the gate's query). Tenant-side writes via
    `run_as_user`; webhook-side reads/decide via `run_privileged`.
  - `config.rs` — `GatedToolStore` (per-agent gated-tool set), same pg impl.
  - `gate.rs` — `HardApprovalGate`: `is_gated` ? `has_approved_for_dag` :
    `Blocked`. **Fail-closed** on backend error.
  - `resume.rs` — `ApprovalResumer`: seed private `SystemNote` + enqueue
    `Normal` trigger (`root_request_id = Some(original_root)`, idempotency
    `apv-resume-{id}`) + best-effort DAG-budget bump.
  - `decision.rs` — `ApprovalDecider`: authorize + `decide`, resume only on a
    newly-recorded decision. **The single seam every surface calls.**
- **`ask_approval` tool** (`tools/system/ask_approval.rs`), registered at the
  composition root. Validates approvers (human, in-org), clamps TTL, derives the
  idempotency key, creates the row, posts a visible request, `ensure_delivery`.
  `modes()=NORMAL`, posts to the **current thread** (v1).
- **Egress fix** — `tool_is_egress(name)` now covers `send_message` **and**
  `ask_approval`, so an `ask_approval`-only turn is not falsely failed
  `NoEgress`.
- **Hard gate wired** through `AgentBuilder::with_approval_gate` → `Agent` →
  `run_one_tool` (`approval_blocked`). `None` in unit tests (no gating); the
  factory installs `HardApprovalGate` on every spawned agent.
- **Prompt clause** — `<approval-gated-tools>` block in `AgentMemory` (locked
  decision 2: prompt clause + hard gate). Empty when nothing is gated, so the
  prompt-cache prefix is unchanged for the common case.
- **Tests** — `approvals_store.rs` (9), `approvals_resume.rs` (1, end-to-end
  decide→resume reusing root + double-click idempotency), `types.rs` unit (9),
  `agent.rs` render unit (2). `cargo fmt`, `cargo clippy -D warnings` (whole
  workspace), `cargo check --all-targets` all green.

### Shipped (#214) — interactive decision intake for Discord + Lark

The create-side seam, Discord intake, and Lark intake (the original plan's items
1, 2, 4) landed together in one PR — they are the three bullets below, not the
3–7 checklist that follows under "Remaining". All gates green; migration **92**
up/down/up rollback verified against the full chain.

- **Create-side seam** — `OutboundRouter` gained `resolve_target` +
  `post_approval`. `ask_approval` now resolves the thread's surface *before*
  `create` (records the real `PlatformTarget`), posts the interactive prompt via
  the owning platform router, and `attach_message`s the returned id. Web threads
  are behavior-identical (plain feed prompt). No double-post: the per-platform
  stream pumps mirror the response *stream*, not feed `Posted` rows.
- **Discord (Gateway, no migration)** — `event.rs` `DiscordEvent::Interaction` +
  `"INTERACTION_CREATE"` arm (+ `InteractionId`/`InteractionToken` newtypes);
  `poster.rs` `components: Vec<ActionRow>` + ack (type 6) / `@original` edit /
  ephemeral follow-up; `bridge.rs::handle_interaction` (ack → mint clicker →
  `ApprovalDecider::decide` → resolved-card edit; unauthorized → ephemeral). The
  `ApprovalDecider` is built once at the composition root and shared.
- **Lark (new HTTP route)** — `lark/card_actions.rs` `POST /lark/card-actions`
  (public router; verify-before-parse, challenge echo), `lark/card_verify.rs`
  (Encrypt-Key signature + Verification-Token, constant-time via `subtle`),
  `lark/card.rs` (pending/resolved card builders), `directory.colleague_for_open_id`,
  `poster.post_card` (`msg_type:"interactive"`). Per-app Encrypt Key +
  Verification Token are **sealed in `lark_apps`** (migration 92) and accepted by
  the admin register route. v1 supports the **signed-plaintext** scheme and
  **group-chat** approvals (DM cards degrade to the web prompt); body encryption
  is intentionally unsupported — validate the exact scheme against the live Lark
  console. **Admin must set the app's Message Card Request URL + subscribe
  `card.action.trigger`, and register the Encrypt Key + Verification Token.**

### Remaining — platform intake (best built + validated against the live
platforms; the spine seam `ApprovalDecider` is ready for both)

3. **`ask_approval` `to` targeting** — extract #178's
   `resolve_or_create_target` into a shared helper so `ask_approval` can route a
   refund approval to an internal channel / DM the approver (fixes the
   customer-DM leak). v1 posts to the current thread.
4. **Platform binding on create** — the posters pass `PlatformTarget::Discord` /
   `::Lark` (instead of `::Web`) once they post the interactive prompt, then call
   `attach_message`.
5. **Expiry sweeper** — a background task calling `expire_due` on a cadence
   (mirror `scheduling/scheduler.rs`), wired into the server lifecycle.
6. **Admin config UI/route** for `agent_gated_tools` (`set_gated`/`unset_gated`
   exist on the store).
7. **Gated-set per-turn load (efficiency / altitude).** Today the hard gate
   calls `is_gated` (a DB read) on *every* tool call, and the prompt builder
   calls `gated_tools_for_agent` (another DB read) on *every* turn — even for
   agents that gate nothing. The deep fix is to load the agent's gated-tool set
   **once per turn** and reuse it for both the `<approval-gated-tools>` block and
   the gate (a `HashSet::contains` instead of N DB round-trips). A TTL cache is
   *not* the right fix — staleness in the "newly gated" direction would let a
   just-gated tool run un-approved for the cache window, weakening the security
   guarantee. The read is a small indexed `SELECT EXISTS`, so this is a
   throughput optimization, not a correctness issue.

## Notes / deviations from the issue text

- Spine migration is **90**, not 86 — #178/#181/eval-harness took 86–89.
- #214 adds migration **92** (`lark_card_credentials`: sealed Encrypt Key +
  Verification Token on `lark_apps`). 91 was taken by `tool_artifacts` (#185).
- `token.rs` (signed-URL HMAC) is **still not** included: Discord uses the
  authenticated Gateway, and Lark's callback is verified with the app's Encrypt
  Key + Verification Token, so v1 has no consumer for it.
- The create-side seam lives on `OutboundRouter` (`resolve_target` +
  `post_approval`), not a parallel trait — the composite already knows which
  surface owns a thread.
