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

### Remaining — platform intake (best built + validated against the live
platforms; the spine seam `ApprovalDecider` is ready for both)

1. **Discord (Gateway)** — `discord/event.rs`: add
   `DiscordEvent::Interaction(Box<InteractionCreate>)` + `"INTERACTION_CREATE"`
   parse arm (+ unit tests). `discord/bridge.rs`: `handle_interaction` — parse
   `custom_id` `apv:{id}:{a|d}`; **ack within 3 s** (callback type 6
   DEFERRED_UPDATE_MESSAGE) *before* DB work; `member.user.id` →
   `DiscordDirectory::resolve_or_mint` → `ColleagueId`; `ApprovalDecider::decide`;
   `PATCH …/@original` to the resolved view; unauthorized → ephemeral `flags:64`.
   `discord/poster.rs`: optional `components: Vec<ActionRow>` on `PostRequest`
   (`skip_serializing_if`) + a message-edit method. Wire `ApprovalDecider` into
   `BridgeDeps`. The Gateway is authenticated → no HMAC needed.
2. **Lark (new HTTP route)** — `lark/card_actions.rs`: `POST /lark/card-actions`
   merged into the public router (template = `slack/events.rs`: raw `Bytes`,
   `DefaultBodyLimit`, verify-before-parse, challenge echo). New
   `lark/card_verify.rs` (encrypt-key + verification-token, constant-time via
   `subtle`). Handler (≤3 s, 200 + JSON): verify; parse `value` →
   approval_id+decision + operator `open_id`; new
   `lark/directory.rs::colleague_for_open_id` (reverse lookup on
   `lark_user_handles`); `ApprovalDecider::decide`; respond `{"card": <resolved>}`
   (+ toast). `lark/poster.rs`: `msg_type:"interactive"` + `card` JSON + a
   card-update method. Admin configures the app's **Message Card Request URL** +
   `card.action.trigger`.
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

- Migration is **90**, not 86 — #178/#181/eval-harness took 86–89.
- `token.rs` (HMAC) is **not** included: Discord (Gateway) and Lark (verification
  token) are both platform-authenticated, so v1 has no consumer for it; add it
  only when a signed-URL surface lands (CLAUDE.md zero-dead-code).
