# Marketing Patom with Patom

A plan for using Patom's multi-agent runtime to do Patom's own marketing.
The operator is a solo OSS founder with no marketing background. The
agents are the marketing team the operator doesn't have. Target audience:
developers — particularly those building with Rust, agent systems, and
OSS infrastructure. Goal: make Patom popular.

This is a plan for real recurring marketing work, not a staged demo.
Every role, MCP, and workflow described here exists because there is a
concrete marketing job that needs doing.

---

## 1. Operating model

Once the system is running, the weekly loop is:

- **Friday morning.** Scheduled task fires on content-strategist. The
  strategist reads the week's GitHub activity (commits, issues, PRs,
  star delta), the prior newsletter's reception (if measurable), and
  proposes next week's angle. Hands off to technical-writer (drafts the
  newsletter issue), social-writer (drafts teasers per channel), and
  designer (drafts one diagram or OG image).
- **Weekend.** Drafts settle in Notion / Beehiiv. No operator action.
- **Monday morning.** Operator opens Notion + Beehiiv, reviews drafts.
  Leaves comments. Never edits.
- **Monday afternoon → Tuesday morning.** Agents revise on the comments,
  do a self-critique pass against their taste memory, present revised
  drafts. Operator approves.
- **Tuesday.** Operator posts manually — newsletter goes out via Beehiiv,
  teasers to X / HN / Reddit / LinkedIn from operator's accounts.
- **Continuously.** community-manager triages GitHub activity as it
  arrives. Surfaces FAQ candidates, contributor-thank-you list, and
  draft replies to Notion for the operator to review and paste manually.

The system runs week over week without operator intervention to *start*
the loop — only to review, approve, and post.

---

## 2. Roles

Five agents. Each owns a real recurring job; each has a memory scope and
MCP allowlist sized to that job.

### 2.1 content-strategist

Owns the *what* and the *when*. Decides what to write about, when to
publish, which channel for which angle.

- **MCPs.** Notion (r/w), GitHub (read).
- **Scheduled tasks.** Weekly Friday-morning fire to plan the next
  issue. Optional event trigger on Patom release tags.
- **Memory scope.** Positioning, target persona, channel priorities,
  what angles landed, what bombed, competitive landscape, brand voice
  rules. Seeded from the operator's deep research (§4).

### 2.2 technical-writer

Owns long-form. Newsletter issues on Beehiiv, blog-style deep-dives,
README marketing sections.

- **MCPs.** Notion (r/w), Beehiiv (r/w drafts), GitHub (read, for code
  references).
- **Memory scope.** Voice samples (lifted from the operator's chosen
  exemplar), `do` rules and `don't` rules extracted from those samples,
  what technical narrative threads exist, what's been explained, what's
  still un-explained, source-of-truth code anchors.
- **Output shape.** Polished drafts on Beehiiv. Operator reviews via
  comments; writer revises; operator publishes.

### 2.3 social-writer

Owns short-form distribution. Drafts X threads, HN-comment angles,
Reddit post copy, LinkedIn shorts, dev.to summaries — all queued to
Notion for the operator to post manually.

- **MCPs.** Notion (r/w).
- **Memory scope.** Per-channel tone (X vs HN vs Reddit vs LinkedIn —
  they read differently), hooks that worked, drafts queued. Inherits
  voice samples and `do`/`don't` rules from the shared writer-voice
  memory layer.
- **Output shape.** One Notion page per draft, channel tagged,
  ready to copy-paste.

### 2.4 designer

Owns visuals. OG images for newsletter issues, social cards, diagrams
for technical posts, occasional demo GIFs.

- **MCPs.** Pencil, Notion (write).
- **Memory scope.** Visual system (palette, typography, recurring
  motifs), what diagrams exist already, which directions the operator
  rejected.
- **Output shape.** Pencil documents; mirrored to Notion as image
  exports so the writer agents can reference them in drafts.

### 2.5 community-manager

Owns the GitHub repo's social surface. Surfaces issue/PR patterns,
drafts replies, identifies FAQ candidates, maintains a contributor
list for newsletter mentions.

- **MCPs.** GitHub (read), Notion (r/w).
- **Memory scope.** Contributor history, recurring questions (FAQ
  candidates), people to thank, issues that look like feature pitches,
  sentiment patterns.
- **Output shape.** Notion pages — draft replies the operator pastes
  into GitHub manually, FAQ candidates the strategist converts into
  content, thank-you list the writer weaves into newsletter intros.

---

## 3. MCP wiring

| MCP     | Status      | Used by                                                      | Mode      |
|---------|-------------|--------------------------------------------------------------|-----------|
| Notion  | ready       | all 5 agents (sub-tree per agent; see §3.1)                  | r/w       |
| Pencil  | ready       | designer                                                     | r/w       |
| Beehiiv | in-flight   | technical-writer                                             | r/w drafts; operator publishes |
| GitHub  | in-flight   | content-strategist, technical-writer, community-manager      | read-only |

Three deliberate scoping choices, each justified:

- **GitHub is read-only.** Agents draft replies and FAQ entries to
  Notion; the operator pastes into GitHub manually. The public repo is
  the brand's most visible surface — no agent writes to it unreviewed.
- **All publishing is manual.** Beehiiv send, X post, HN submit, Reddit
  post, LinkedIn share — every one happens from the operator's account
  after operator approval. Two reasons: (a) most platforms restrict bot
  posting; (b) the published artefact must feel human, which requires a
  human hand on the publish button.
- **No Gmail / Calendar.** Removed from the design. They served a prior
  fictional-agency demo and have no job in real-world marketing of
  Patom.

### 3.1 Notion sub-tree per agent

Each agent reads and writes its own sub-tree to prevent cross-agent
overwrites:

- `Patom / Strategy` — content-strategist's pages (positioning, angles,
  competitive notes).
- `Patom / Newsletter` — technical-writer's drafts and reference pages.
  Beehiiv issues are the canonical artefact; Notion holds working
  copies.
- `Patom / Social` — social-writer's per-channel draft queue.
- `Patom / Visuals` — designer's exported assets and design-system
  reference.
- `Patom / Community` — community-manager's FAQ drafts, contributor
  list, draft GitHub replies.

A workspace audit (`last_edited_by` per page) catches any agent writing
outside its sub-tree.

---

## 4. Memory bootstrap

The agents are only as good as their seeded memory. Cold-start drafts
with empty memory will read like generic LLM output. The operator's
deep research is the seeding step — done once before the system goes
live, refreshed periodically.

### 4.1 Research the operator owns

Produced before any agent runs. Each item lives as a Notion page and a
distilled set of `core` memory rows on the relevant agent(s).

1. **Positioning.** What Patom is, who it's for, what it's against.
   3–5 rules pinned as `core` on content-strategist.
2. **Target persona.** What the developer-reader already knows, what
   they're skeptical of, what would make them try Patom. Key rules
   pinned on content-strategist.
3. **Voice exemplar.** A specific person or brand whose writing fits
   Patom's zone (terse + opinionated + technically dense + dry).
   Candidates to evaluate: Dan Luu, Hillel Wayne, Tigerbeetle blog,
   fly.io blog, Tinygrad's notes. The research output names one chosen
   exemplar + 5–10 voice samples lifted verbatim + extracted `do`
   rules + extracted `don't` rules. Voice samples and rules pinned as
   `core` on technical-writer; inherited by social-writer.
4. **Channel best practices.** What works on each channel (Beehiiv
   subject lines, X opening hooks, HN title patterns, Reddit subreddit
   norms, LinkedIn shorts). Channel-specific rules pinned on
   social-writer.
5. **Competitive landscape.** Who else is in the space (LangChain,
   Mastra, Pydantic AI, etc.), how Patom is different, what *not* to
   claim. Key contrasts pinned on content-strategist.

### 4.2 Why a borrowed voice is correct here

The operator does not have a writing corpus to mine. Starting from a
named exemplar is how real writers begin — imitate someone whose work
fits, until your own taste emerges through correction. The expected
shape is: month 1 drafts read like the exemplar; over months of review
comments compounding into memory (§5), drafts drift away from the
exemplar toward the operator's actual preferences. The operator
discovers their voice *through* reviewing, not by writing.

### 4.3 What lives where

- **Notion pages.** Full research, browsable, the source of truth the
  agents can re-read.
- **`core` memory rows.** Distilled rules, rendered into every prompt,
  cannot be overridden by drift. Operator owns these via the Memory
  HTTP routes (`doc/memory.md` §1.9).
- **`held` memory rows.** Mature over time from review-comment
  patterns via the librarian (§5).
- **`tentative` memory rows.** Automatic writes during turns, awaiting
  maturation or operator review.

---

## 5. Review-and-revise loop

The operator's role at runtime is review-only. They never edit drafts;
they leave comments, the writer revises, the operator approves. Final
posting is manual.

For "human taste, not AI taste" to hold over time, **review comments
must compound into memory**. Otherwise the operator writes the same
five comments every week forever, AI taste never recedes, and the
system fails its core promise. Three modes feed memory together,
mirroring how a real teammate learns — none replace the others:

1. **Automatic (`tentative`).** When the writer revises on a review
   comment, it does an in-turn `memory_write` capturing the rule it
   extracted from the comment. Starts low-trust.
2. **Librarian-mediated (`held`).** The librarian (`doc/memory.md`
   §1.8) periodically reflects on review-comment patterns. If the
   operator has corrected the same phrasing pattern several times in a
   month, the librarian proposes promoting it to `held`.
3. **Explicit (`core`).** The operator marks a review comment as a
   permanent rule directly via the Memory HTTP routes. Outranks
   everything.

All three running together is what makes the agents feel like
teammates rather than a workflow tool. This is the load-bearing
design decision behind the system.

### 5.1 Self-critique pre-show pass

Before the writer presents a draft to the operator, it does a critique
pass against its own taste memory and rewrites any violations it
detects. Prevents the operator wasting reviews on rules already taught,
and accelerates the rate at which drafts converge on the operator's
voice.

---

## 6. Cadence

The newsletter is the cadence anchor; everything else orbits it.

- **Weekly.** Newsletter issue. content-strategist fires Friday
  morning; drafts settle over the weekend; operator reviews Monday;
  publishes Tuesday.
- **Per release (event-triggered).** content-strategist fires on
  Patom tag-push; drafts a release fan-out (newsletter special,
  social teasers, release-notes polish).
- **Continuous.** community-manager runs on inbound GitHub activity.
  No fixed schedule.

---

## 7. Build order

To ship newsletter issue #1:

1. **Operator does the deep research (§4).** Five Notion pages, then
   distilled `core` memory rows pinned on the relevant agents. This is
   the gating step — every downstream agent depends on it.
2. **Wire Beehiiv MCP.** Required for technical-writer to produce
   drafts the operator can publish from.
3. **Wire GitHub MCP (read-only).** Required for content-strategist's
   Friday fire to read weekly activity, and for community-manager to
   triage.
4. **Define the 5 agent system prompts.** Role boundaries, allowed
   MCPs, memory-write cues, self-critique instructions. Built as a
   shared pre-request module so prompts can iterate without editing
   each agent file.
5. **Schedule content-strategist's Friday task.** Hand-test the first
   fire end-to-end: strategist proposes angle, hands off, writers +
   designer produce drafts in Notion + Beehiiv.
6. **Operator runs review-and-revise on issue #1.** Treat the first
   round of comments as the seeding of `tentative` taste rules.
7. **Publish issue #1 manually.** Beehiiv send. Social teasers posted
   manually from operator accounts.

After issue #1, the loop runs on its own cadence. Iterate prompts and
memory based on what reviews surface.

---

## 8. Operational risks

| Risk | Handling |
|---|---|
| Beehiiv or GitHub MCP doesn't land in time. | Both are in-flight. If Beehiiv slips, technical-writer drafts to Notion only and the operator pastes into Beehiiv. If GitHub MCP slips, the strategist's Friday fire reads a manually-pasted "this week" summary from a Notion page; community-manager pauses until the MCP lands. |
| Operator review backlog — drafts pile up faster than the operator can review. | The Monday review window is the rate limiter. If a backlog forms, content-strategist reduces issue cadence (biweekly) until cleared. Better to ship fewer good issues than queue a month of unreviewed drafts. |
| Taste-memory drift toward operator blind spots — the system learns the operator's preferences perfectly, including the bad ones. | Operator periodically reads the writer agent's pinned `core` rules and prunes any that look like bad habits. The Notion research pages (§4.1) act as the canonical anchor — if the agent's learned rules drift away from the research, the research wins. |
| Friday scheduled task fails silently — no drafts on Monday. | Scheduled-task observability: an alert if the Friday fire produces zero `prompt_requests` rows by Friday noon. The operator manually kicks the strategist if the alert fires. |
| Agent posts to GitHub by accident. | Mitigated by structure: GitHub MCP is read-only, so this is impossible by construction. |
| Voice-exemplar choice turns out wrong — month 1 drafts read poorly. | Cheap to re-pick. Swap the exemplar Notion page, refresh the writer's pinned voice samples + rules, restart from issue #2 onward. The first issue may need extra rounds of review; that's the cost of bootstrapping.|

---

## 9. What this is not

To prevent scope creep:

- **Not a sales pitch.** No staged scenarios, no fictional clients,
  no "watch the timer tick" moments. The system runs because Patom
  needs marketing, not because it needs to be demonstrated.
- **Not a CMS.** Notion is the agents' knowledge base, not the
  publishing surface. Publishing happens manually on Beehiiv / X / HN
  / Reddit / LinkedIn.
- **Not autonomous publishing.** The system drafts and queues; the
  operator publishes. Always.
- **Not multi-tenant.** This is one operator marketing one product.
  If Patom later sells this configuration to other OSS founders,
  that's a separate productization, not in scope here.
