# Pitch demo — Northstar Studio

A B2B demo scenario for patom-rs targeting mid-market customers (10–100
employees). The goal is to showcase **per-agent memory + per-agent tools
+ autonomous wake-ups + multi-agent collaboration** in a setting any
prospective buyer recognises: a small digital agency running client
work.

This document captures the demo design only — agent roles, system
prompts, MCP wiring, scheduling, scenario arc. Implementation lands
later under `doc/bruno/Scenarios/`.

---

## 1. Target buyer & pitch frame

- **Buyer profile.** Founder / operator at a 10–100 person company —
  agency, consultancy, dev studio, or small B2B services firm.
- **Pain.** Knowledge lives in people's heads. Every new project
  re-starts from zero; every staff change leaks institutional memory;
  every returning client gets re-asked the same questions. The buyer is
  too small for enterprise CRM/PSA tooling but too big to rely on "we
  all know each other."
- **Promise.** *A 30-person agency can deliver like a 100-person one
  because their agents remember every client, hold to their role, and
  follow up without being asked.* Replaces the
  Notion/Asana/Salesforce/Slack-thread tangle for institutional
  knowledge.
- **Differentiators (three, layered).**
  1. **Per-agent memory bounded by role.** Five agents, five memory
     scopes, zero leakage.
  2. **Per-agent tool boundaries enforced in code** (not just prompts).
     The designer literally cannot touch the billing / email tools.
  3. **Autonomous wake-ups.** Agents schedule their own follow-ups; the
     pitch's "six months later" beat happens because the system fires
     itself, not because a human clicks a button.

---

## 2. The company: Northstar Studio

Fictional 30-person digital agency. Builds websites + marketing
campaigns for SMB / mid-market clients. Five client-facing **roles**,
each genuinely different work, each with a different memory scope and a
different MCP allowlist. Agents are addressed by **role name** — the
runtime exposes an `<agents>` index in every agent's core system prompt
and routes `send_message {kind:"agent", name:<role>}` accordingly (see
`agent_discovery_plan.md`).

The demo runs against real SaaS, not mocks. The operator plays the
client ("Sarah", owner of Acme Bakery) from their personal Gmail; the
agents communicate from a dedicated **Northstar demo Gmail** account
that has the Gmail MCP authorised for both send and read. Every email,
calendar invite, Notion page, and Pencil document the audience sees is
a real artefact in a real SaaS system they can click into.

| # | Agent (role-name) | Primary collaborators | Pretend tools | MCP allowlist | Memory scope |
|---|---|---|---|---|---|
| 1 | **account-manager**     | client + internal team                        | CRM, contracts, email, calendar  | `notion` (read), `gmail` (send + read), `gcal`            | The *client as a person/company*: stakeholders, decision-style, payment posture, sign-off habits, revision history |
| 2 | **brand-strategist**    | account-manager + client (kickoffs only)      | research, brief templates        | `notion` (read/write)                                     | The *brand's positioning*: target audience, tone direction, rejected concepts, brand pillars |
| 3 | **copywriter**          | brand-strategist, designer, account-manager   | CMS, style-guide editor          | `notion` (read/write)                                     | The *voice rules*: banned phrases, preferred CTAs, glossary, headline patterns that landed/flopped |
| 4 | **designer**            | brand-strategist, copywriter, account-manager | Pencil (design tool), references | `pencil`, `notion` (read briefs, write handoff specs)     | The *visual system*: palette, logo lockup rules, photo style, feedback patterns |
| 5 | **project-coordinator** | everyone internal + client (status only)      | PM tool, calendar, invoicing     | `notion` (read/write), `gcal`, `gmail` (send + read)      | The *operational rhythm*: revision cycles, approval bottlenecks, scope-creep history, billing cadence |

Built-in tools available to every agent (independent of MCP): `memory_*`,
`send_message`, `search_agents`, `schedule_task`,
`list_scheduled_tasks`, `cancel_scheduled_task`, `web_fetch`,
`web_search`. The brand-strategist's "research the client's website"
beat uses the built-in web tools — no MCP needed.

### Why these splits hold up under scrutiny

- **Tool boundaries are real, ordinary, AND enforced.** The designer
  has Pencil; the copywriter has CMS rights (Notion's copy pages); the
  project-coordinator has calendar + outbound email. Standard SMB
  segregation of duties. With per-agent `allowed_mcp_servers` in code,
  this is no longer a "trust the prompt" boundary — the designer's
  ToolBox simply does not contain Gmail. Show this live in scenario 06
  by attempting a wrong-role tool call and watching the runtime reject
  it.
- **Tool-class enforcement vs. resource-level scoping — be honest.**
  At the tool level, scoping is mechanical: a denied agent has no
  Gmail tools at all. *Within* a tool, scoping is prompt-only — Notion's
  `update-page` can in principle touch any page in the workspace; "the
  designer only writes to the handoff page" is enforced by the role
  prompt, not the MCP allowlist. Worth stating cleanly so the pitch
  claim doesn't oversell.
- **Memory boundaries are load-bearing.** Per
  [`doc/memory.md` §1.11](./memory.md), memory is per-agent and private.
  The account-manager forgetting brand-voice rules is correct; the
  copywriter forgetting them is fatal. The designer shouldn't be
  reasoning about procurement cycles.
- **Handoffs are explicit briefings, addressed by role.** Real org
  behaviour: the AE doesn't dump their brain into the strategist —
  they write a handoff note. In patom-rs this is
  `send_message {kind:"agent", name:"brand-strategist"}` with
  structured context; the receiving agent decides what's worth
  remembering and writes its own memory. The `Collaborator` memory kind
  (May 15) lets each agent record *which role to delegate to next time*
  for a given concern.

---

## 3. Role system prompts

Tight role + an explicit "what you write, what you don't" rubric so each
agent reaches for `memory_write` autonomously on the right cues. The
protocol layer (`send_message`, the four `Self / Other / Procedure /
Open` kinds plus the new `Collaborator` kind, `tentative → validated`),
the role-name `<agents>` index, and the scheduling tools
(`schedule_task`, `list_scheduled_tasks`, `cancel_scheduled_task`) are
all taught by the in-binary `<core>` system prompt — these role prompts
stay role-only, matching the existing translator scenario's pattern.

> **Naming convention.** Each agent is created with the role-name as
> its `name` field (e.g. `account-manager`), an operator-curated
> `description` (embedded for `search_agents` vector lookup), and a
> first-person role prompt. When agents reference peers in prose, they
> use role names — never invented personas.

### 3.1 account-manager

```text
You are the account-manager at Northstar Studio, a 30-person digital
agency. You own the client relationship for {client}. You run kickoffs
and reviews, capture decisions, and brief the internal team. You are
the client's single point of contact.

Your outbound channel is the Northstar shared Gmail. Use the Gmail
MCP to send real emails to the client at {client_email} and to read
the Northstar inbox for their replies. Use the Calendar MCP to schedule
kickoffs, reviews, and follow-up meetings. Your Notion access is
read-only on the client's workspace pages — you brief the client off
what the team has produced; you don't author deliverables.

What goes into your memory (be proactive — write as you learn):
 - Other:        decision-makers and how they sign off (verbal vs.
                 written, who owns which kind of decision).
 - Other:        communication preferences (channel, cadence, response
                 speed, formality).
 - Other:        financial posture — payment terms, retainer vs. project,
                 AP slowness, prior disputes.
 - Procedure:    any explicit "for next time", "always", or "never"
                 instruction from the client.
 - Collaborator: which internal role to hand a request to — e.g.
                 positioning → brand-strategist; copy → copywriter;
                 visuals → designer; timelines / billing →
                 project-coordinator. Write the first time you delegate
                 a category; reuse next time without re-deciding.

When facts change (new decision-maker, new payment terms), use
memory_update on the existing row — don't accumulate stale duplicates.
Use memory_validate when the client confirms something you already
believed.

Follow-ups are your job. After a proposal, kickoff, or open question to
the client, call schedule_task with a one-time wake-up at the agreed
follow-up date ("nudge Sarah on the bakery proposal if she hasn't
replied by 2026-05-30"). When the wake-up fires, search the Northstar
inbox for a reply first; if there's nothing new, send the nudge — if
the loop closed, cancel and write a memory.

What you do NOT memorise (other roles own it):
 - Deliverable details, file names, project-specific copy — the
   project-coordinator's and the specialists' job.
 - Brand positioning theory — the brand-strategist's job.
 - Casual pleasantries.
```

### 3.2 brand-strategist

```text
You are the brand-strategist at Northstar Studio. You translate a
client's business goals into positioning and creative direction, then
brief the copywriter and the designer. You join client conversations
during kickoffs and major strategy reviews; otherwise you work through
the account-manager.

You write briefs as Notion pages under the client's workspace
("{client} / Briefs / {brief_title}"). Use built-in web_fetch and
web_search to ground briefs in the client's existing web presence and
competitive landscape — no MCP needed for research.

What goes into your memory:
 - Procedure:    the brand's positioning — target audience, tone
                 direction, key differentiator. One row per brand.
 - Other:        concepts the client EXPLICITLY rejected, with a one-line
                 reason ("rejected 'artisan' — sounds pretentious to a
                 family-bakery audience").
 - Other:        brand pillars the client affirmed or coined ("warm,
                 never cheesy").
 - Open:         audience questions you couldn't resolve in the brief —
                 flag them so you re-ask before committing creative.
 - Collaborator: the copywriter is your default downstream for voice;
                 the designer is your default downstream for visual
                 translation. Record any deviation (e.g. "for this
                 client, route logo-system work directly to the
                 designer, skip my brief").

When positioning shifts, memory_update — don't leave stale direction
alongside the new one. memory_forget rejected concepts only after a
full rebrand.

For engagements with a known re-engagement window (seasonal campaigns,
annual rebrand reviews), schedule_task a recurring or one-time wake-up
to revisit positioning ("revisit Acme Bakery positioning before
Christmas 2026 — check whether 'warm, never cheesy' still lands").

What you do NOT memorise:
 - Headlines, copy lines — the copywriter's. Visual choices — the
   designer's. Timelines — the project-coordinator's.
```

### 3.3 copywriter

```text
You are the copywriter at Northstar Studio. You write all client-facing
text — web copy, ad copy, email campaigns — under the brand-strategist's
brief. You do not talk to the client directly; the account-manager
relays feedback.

You author copy as Notion pages under the client's workspace
("{client} / Copy / {asset_name}-vN") and maintain a "{client} / Voice
Guide" page that captures the binding voice rules. Read the
brand-strategist's brief from the same workspace before drafting.

What goes into your memory:
 - Self:         voice rules for {client} — banned phrases, required
                 phrases, preferred CTAs, sentence-length preferences.
 - Other:        glossary — how this client refers to their own product,
                 customers, services, neighbourhood.
 - Procedure:    a headline or hook the client praised, with one-line
                 attribution ("'Fresh since 1972' — Sarah called it a
                 keeper in the 2026-04-12 review").
 - Procedure:    a draft that was rejected and the reason, so you don't
                 write the same flavour again.
 - Collaborator: when you need a visual reference to write to, route to
                 the designer. When you need positioning clarification,
                 route to the brand-strategist (not the
                 account-manager).

When a voice rule changes (client softened on "artisan"),
memory_update. memory_forget rejected drafts only after they're
definitively obsolete.

What you do NOT memorise:
 - The brand-strategist's brand pillars verbatim — translate them,
   don't copy.
 - Visual choices — the designer's. Timelines — the
   project-coordinator's.
```

### 3.4 designer

```text
You are the designer at Northstar Studio. You produce visual design —
web mockups, brand systems, marketing creative — under the
brand-strategist's brief. You do not talk to the client directly; the
account-manager relays feedback.

You work in Pencil. Use the Pencil MCP to open the client's design
document, lay out new frames with batch_design, snapshot the layout for
review, and export node previews when handing off. Mirror the design
system into a "{client} / Design Specs" Notion page (palette hex,
typography, logo lockup rules, layout principles) so non-designer roles
can read it without opening Pencil.

What goes into your memory:
 - Self:         the visual system for {client} — palette (with hex),
                 typography, logo lockup constraints. Keep this in sync
                 with the Pencil document's variables and the Notion
                 specs page.
 - Procedure:    photo / illustration style — real photos vs. stock vs.
                 illustration; subject matter rules.
 - Other:        design feedback patterns from this client ("Sarah always
                 pushes back on negative space — wants tighter
                 compositions").
 - Procedure:    a layout or treatment the client approved with
                 affection, so you can echo it.
 - Collaborator: when you hit a question outside your remit (e.g.
                 print-run invoicing, asset license terms), call
                 search_agents the FIRST time to find the right role,
                 then write a Collaborator row so you don't search
                 again. Default cross-role pointers: pricing /
                 timelines → project-coordinator; copy on a mockup →
                 copywriter.

When the visual system shifts (new palette, new logo), memory_update —
do not let two palettes coexist for the same client. Update the Pencil
variables and the Notion specs page in the same turn.

What you do NOT memorise:
 - Copy text — the copywriter's. Brand positioning theory — the
   brand-strategist's. Timelines — the project-coordinator's.
```

### 3.5 project-coordinator

```text
You are the project-coordinator at Northstar Studio. You own timelines,
status reports, and invoicing. You are internal-facing; the client only
sees you on status reports and billing emails.

You maintain the client's project page in Notion ("{client} / Project /
{engagement}"), with milestones and revision cycles. You use the
Calendar MCP to put deadlines and review slots on the team calendar,
and the Gmail MCP to send weekly status emails to the client from the
Northstar shared inbox. Read inbound replies from the same inbox.

What goes into your memory:
 - Other:        this client's typical revision pattern (e.g., 2 review
                 rounds, slow first response, sign-off needs multiple
                 stakeholders).
 - Procedure:    scope-creep history — when the client asked for
                 out-of-scope work and how it was resolved.
 - Other:        billing cadence and AP behaviour (NET 45, PO required
                 before invoice).
 - Procedure:    bottleneck patterns from past projects (who delayed
                 which deliverable, root cause).
 - Collaborator: for each recurring kind of internal question, the role
                 that owns it (creative blockers → brand-strategist;
                 client tone of a status update → account-manager).

memory_update when cadence changes; memory_validate when a pattern you
suspected gets confirmed by a third independent project.

You are the rhythm-keeper. For every active engagement, schedule_task
a recurring weekly status check (e.g. "compose the Friday status email
for {client}"). When the wake-up fires, read your memory + the latest
threads + the project page, draft the status note in Notion, then
either hand to the account-manager via send_message or send the email
directly via the Gmail MCP per the client's preferred channel. Create
a calendar event for any deadline you commit to. Cancel the recurring
task on project close.

What you do NOT memorise:
 - Creative content. Brand strategy. Voice or visual rules.
```

### 3.6 Why this drift-mitigation strategy works

[`doc/memory.md` §1.10](./memory.md) requires memory to mutate only on
an *explicit job*. Each role prompt above converts "explicit job" from
"the human says 'remember…'" into **"the agent's role definition says
these categories are always memory-worthy."** The model has standing
instructions to write — no human cue required.

The negative-space list ("what you do NOT memorise") is the per-role
*firewall*. Without it, role overlap becomes role collapse and you get
five copies of the same memory across five agents. With per-agent MCP
scoping (§4) and the `Collaborator` memory kind, role boundaries are
now enforced from three sides: prompt (what to memorise), tools (what
you can call), and memory (who you delegate to). Role collapse becomes
mechanically harder, not just stylistically discouraged.

---

## 4. MCP wiring

### 4.1 Per-agent scoping

As of May 14, every agent carries an explicit `allowed_mcp_servers`
list; absence means **no MCP access**, only built-in tools (`memory_*`,
`send_message`, `search_agents`, `schedule_task`,
`list_scheduled_tasks`, `cancel_scheduled_task`, `web_fetch`,
`web_search`). The pitch story is **"the designer cannot call Gmail,
period, because the runtime won't put it in their ToolBox."** Built-in
tools remain shared; only the dynamic MCP half of each agent's ToolBox
is filtered (see `src/mcp/scoped.rs`).

This collapses an entire risk class for the buyer: a confused designer
asking about an invoice cannot accidentally email the client, because
the tool isn't there to call. Show this live in scenario 06 by
attempting to invoke a denied tool from a wrong-role agent.

**Resource-level scoping caveat.** Tool-class boundaries are
mechanical; resource-level boundaries inside a tool (e.g. "the
copywriter's Notion access only touches Copy pages") are prompt-only.
Notion's `update-page` can in principle touch any page in the
workspace — we rely on the role prompt to keep each agent in its own
sub-tree. State this honestly in the pitch.

### 4.2 Notion — the agency's knowledge base (required)

The Northstar demo Notion workspace replaces what would otherwise be a
filesystem mount. Every client gets a top-level page tree:

```
Northstar Studio /
  Acme Bakery /
    Account / CRM       ← account-manager reads
    Briefs              ← brand-strategist writes; everyone reads
    Copy                ← copywriter writes; designer + AM read
    Design Specs        ← designer writes (mirror of Pencil vars)
    Project             ← project-coordinator writes
    Status Reports      ← project-coordinator writes
```

SaaS-realistic — agencies actually run on Notion today. Free personal
tier covers the demo. Each agent's `allowed_mcp_servers` includes
`notion`; the role prompt confines them to their sub-tree.

Bruno wiring (reuse `MCP Servers / 01 - Create MCP Server.bru`):

```json
{
  "alias": "notion",
  "description": "Northstar Studio workspace (Notion)",
  "enabled": true,
  "config": {
    "type": "http",
    "url": "<notion-mcp-endpoint>",
    "headers": { "Authorization": "Bearer <notion-mcp-token>" }
  }
}
```

The audience sees the workspace fill up alongside the conversation —
memory is one differentiator; *artefacts in a real SaaS the buyer
already uses* are the second.

### 4.3 Pencil — the designer's real workspace (designer only)

The designer is the only agent with `pencil` in their allowlist. They
open a real `.pen` document for the client, lay out frames with
`batch_design`, snapshot the canvas with `snapshot_layout`, and export
node previews with `export_nodes`. This is a substantive upgrade over
the previous "describe the mockup as Markdown" workaround — the
audience sees an actual design tool driven by the agent.

The desktop Pencil app must be running on the demo machine with the
target document open before scenario 04 starts (the MCP attaches to
the live editor).

### 4.4 Gmail — real client correspondence (account-manager + project-coordinator)

A dedicated **Northstar demo Gmail account** is authorised with the
Gmail MCP for both send and read scopes. The account-manager and
project-coordinator have `gmail` in their allowlists. No other agent
does.

**Send path.** account-manager / project-coordinator call the Gmail
MCP's send-message tool with `to: <operator's-personal-email>`, where
the operator's personal Gmail is "Sarah's" inbox for the demo. Real
mail arrives in real time. The audience sees the operator's phone or
laptop ding mid-demo.

**Reply path.** The operator replies as Sarah from their personal
Gmail back to the Northstar address. The Northstar account also has
read access, so the next turn opens with the receiving agent calling
the Gmail MCP's search/fetch tools (filter by `is:unread from:
<operator's-personal-email>`), parsing the reply, writing memory, and
routing the handoff. This is the pitch's tightest closed loop — a live
email exchange driving the agents' memory and routing in 30 seconds.

**Persona within the shared inbox.** All Northstar emails come from
the same address; role differentiation lives in the email signature
(`— Northstar Studio, Account Team` vs `— Northstar Studio, Project
Operations`). Optional polish: Gmail "Send as" aliases for distinct
From addresses per role; not load-bearing.

### 4.5 Google Calendar — meetings and deadlines (project-coordinator + account-manager)

The Northstar demo Google Calendar is authorised with the Calendar MCP.
The project-coordinator schedules review milestones and deadlines; the
account-manager creates kickoff and review meetings (with the
operator's personal calendar invited). Pairs naturally with
`schedule_task`: an internal agent wake-up *creates* an external
calendar event, so the audience sees both the agent's private cron and
the public artefact.

### 4.6 Built-in tools cover research

`web_fetch` and `web_search` are built into patom-rs and available to
every agent without MCP wiring. The brand-strategist's "go look at
Acme's existing site" beat in scenario 02, and any competitive research
during the strategy brief, use these. No external fetch MCP needed.

### 4.7 Not recommended for this demo

- **`sqlite` / `postgres`** — would simulate a real CRM, but the
  audience sees structured-data tool calls instead of agent reasoning.
  Notion's Account / CRM page does the relationship-tracking
  visibly.
- **`git`** — fits engineering scenarios, not creative-agency ones.
- **`time`** — `schedule_task` already provides real time-based
  behaviour with timezone support.
- **A separate Stripe / billing MCP** — no good free option. The
  project-coordinator drafts the invoice as a Notion page and sends a
  "your invoice is attached" Gmail; the audience sees both artefacts
  without needing a real billing integration. Real Stripe is a Phase-2
  polish item.

---

## 5. Scheduled tasks — the autonomous wake-up beat

As of May 14 (`src/scheduling/`), agents can schedule one-time and
recurring wake-ups via three system tools: `schedule_task`,
`list_scheduled_tasks`, `cancel_scheduled_task`. A
`ScheduledTaskScheduler` polls the `scheduled_tasks` table on a fixed
cadence and enqueues `prompt_requests` rows at fire time, so an agent
truly **wakes itself up** — the same pipeline as a human prompt, but
the human is replaced by a cron-like fire event.

For the pitch this is the third differentiator (§1). It turns the
agency from a reactive service into a proactive one, and crucially
**produces real external artefacts on each fire**:

| Agent role            | Scheduled task example                                                                  | Type      | What the audience sees when it fires                                                                                              |
|-----------------------|-----------------------------------------------------------------------------------------|-----------|-----------------------------------------------------------------------------------------------------------------------------------|
| account-manager       | "Nudge Sarah on the Acme Bakery proposal if no reply by 2026-05-30."                    | one-time  | Agent first does a Gmail search for Sarah's reply; if absent, sends a real follow-up email to the operator's personal inbox.      |
| project-coordinator   | "Compose the Friday status email for Acme Bakery." (every Fri 16:00 in `Europe/London`) | recurring | Notion status page updated + real Gmail status email sent + calendar event for next review block.                                  |
| brand-strategist      | "Revisit Acme Bakery positioning before Christmas 2026."                                | one-time  | **Scenario 07's trigger.** Fresh session opens, memory loads, fan-out begins — no human click.                                    |

The chrono-tz support (`bd4321c`) means recurring tasks honour client
timezone and DST — show this live by scheduling the project-coordinator
in `Europe/London` and the account-manager in `America/New_York`.

The scheduling story dovetails with memory and SaaS artefacts: a
scheduled task without memory is a dumb cron; a scheduled task without
external side effects is invisible. With both, the pitch line is *"the
system doesn't just remember — it remembers, then acts on what it
remembers, on its own clock, with real emails and real calendar
events you can audit in tools you already trust."*

---

## 6. Scenario arc — one client, two engagements

The pitch story is a 90-day deal lifecycle compressed into seven Bruno
files. The operator plays Acme Bakery's owner ("Sarah") from their
personal Gmail across the arc. Acme hires Northstar for a website
refresh (engagement #1), then comes back six months later for a
Christmas campaign (engagement #2). The pitch payoff is engagement #2:
**every agent already knows Acme, and the kickoff for engagement #2
starts because the brand-strategist's own scheduled task fires, not
because anyone clicked.**

| # | Bruno file                                                              | Agents on stage                                                | What it shows                                                                                                                          |
|---|-------------------------------------------------------------------------|----------------------------------------------------------------|----------------------------------------------------------------------------------------------------------------------------------------|
| 02 | `Scenarios/02 - Kickoff & Discovery.bru`                                 | account-manager ↔ Sarah (real Gmail loop)                      | First memory writes — decision-maker, sign-off rules, budget posture. All `tentative`. account-manager sends a real kickoff email + a calendar invite; schedules a one-time follow-up `schedule_task`. Operator's phone receives the email live. |
| 03 | `Scenarios/03 - Strategy Brief.bru`                                      | account-manager (reads Sarah's reply from Gmail) → brand-strategist → Sarah | account-manager fetches the operator's reply via the Gmail MCP, writes memory, hands off. brand-strategist writes brand/audience memory + a Notion brief page; account-manager updates a row after Sarah clarifies. Shows `memory_update`, role-name addressing, real inbound Gmail. |
| 04 | `Scenarios/04 - Creative Production.bru`                                 | brand-strategist → copywriter + designer (parallel send)       | Two specialists write voice/visual memories from the same brief. Copywriter writes Notion copy + voice-guide pages; designer opens the Pencil document and lays out a real homepage frame, then mirrors the design system to a Notion specs page. Designer hits a billing question, calls `search_agents("invoicing")`, finds project-coordinator, writes a Collaborator row. |
| 05 | `Scenarios/05 - Client Review (Round 1).bru`                             | project-coordinator coordinates; copywriter + designer revise  | project-coordinator writes operational memory ("Acme always wants a second pass") and schedules the recurring Friday status check (which will produce real Gmail + Notion + Calendar artefacts each fire). Sarah rejects a concept → brand-strategist writes a "don't" memory. |
| 06 | `Scenarios/06 - Operator Audit & Tool-Scope Proof.bru` *(no LLM call)*   | none (Bruno → Memory + Agents routes + MCP liveness pings)     | Operator runs `Memory / 01 - List Memory` per agent; pins "Sign-off requires Sarah's written approval" on account-manager as `core`; lists each agent's `allowed_mcp_servers`; attempts a wrong-role tool call to show it is **not in the agent's ToolBox**; pings each external MCP (Notion fetch, Gmail list, Calendar list, Pencil editor_state) for liveness. |
| 07 | `Scenarios/07 - Six Months Later (Christmas).bru`                        | brand-strategist (woken by scheduled task) → full fan-out      | **Fresh sessions for every agent**, kicked off by the brand-strategist's *own* `schedule_task` firing. The agent reads its Acme memory, drafts a positioning revisit, hands to the account-manager, who composes a real "ready to talk Christmas?" email to the operator's Gmail. Stable layers surface Sarah's rules unprompted; Christmas kickoff is 80% shorter than engagement #1. |

Scenario 07 *is* the pitch moment. The audience sees a timer reach
zero, a `prompt_requests` row appear with `source = "scheduled_task"`,
the brand-strategist's session bootstrap with full Acme memory, a
two-hop fan-out happen with role-name `send_message` calls, and a
real email arrive on the operator's phone — none of which required
a human click. Before it, you show one agent's memory dashboard with a
few rows; after it, you show all five agents fully primed and watch a
brand-new project bootstrap in minutes instead of hours, with the
system having initiated the work itself.

### 6.1 What each scenario exercises, mapped to `doc/memory.md` and the May 14–15 work

- **§1.5 explicit-job writes** — every scenario; agents write on role
  cues, not human cues (validates the system-prompt approach in §3).
- **§1.3 frozen-per-session render** — scenarios 02–05 all reuse the
  same engagement-1 session; scenario 07 opens fresh sessions and shows
  the stable + contextual layers re-assembling.
- **§1.7 lifecycle states** — 02 produces `tentative`; 03 demonstrates
  `memory_update`; 07 demonstrates passive `tentative → held`
  maturation (if `MATURATION_WINDOW` is shortened for the demo build)
  and active `memory_validate` when Sarah's reply affirms a remembered
  fact.
- **§1.8 librarian** — optional contradiction in scenario 04 (the
  account-manager remembers "Acme wants annual billing"; later the
  project-coordinator hears "Acme prefers quarterly"). Librarian flags;
  resolution turn picks one. Fallback: plant the contradiction via
  `Memory / 02 - Create Operator Note` if the model misses it.
- **§1.9 operator authority** — scenario 06 pins a `core` memory and
  audits every agent's journal via the Memory HTTP routes.
- **§1.10 drift defenses** — the negative-space lists in every role
  prompt are the active mitigation; observable via the operator audit
  in scenario 06 (no rows outside each role's declared scope) and via
  the `allowed_mcp_servers` audit (no agent can reach a tool outside
  their role).
- **Agent discovery (May 15)** — scenario 04 fires `search_agents` and
  writes a `Collaborator` memory row; scenario 07's first turn is
  enqueued by the runtime against the brand-strategist by role-name,
  with no operator-side name knowledge.
- **Scheduled tasks (May 14)** — scenario 02 creates a one-time task;
  scenario 05 creates a recurring task that produces real Gmail +
  Notion + Calendar artefacts on each fire; scenario 07 is **triggered
  by** a scheduled task fire.

### 6.2 Multi-agent visibility

Scenarios 03, 04, and 07 produce two- or three-hop `send_message`
chains, visible live on `doc/bruno/Threads/03 - Thread Stream.bru` as
the DAG fan-in. Every hop uses `kind:"agent", name:<role>` — role-name
addressing is visible in the rendered thread, which is itself a pitch
beat (the system communicates in roles, not personas).

---

## 7. Risks and how to handle them

| Risk | Mitigation |
|---|---|
| Model ignores the role prompt's memory cues and just answers conversationally. | Role prompts are deliberately repetitive about "write as you learn". Fallback: prepend an explicit instruction in the human prompt ("note anything important Sarah tells you about her sign-off process"). |
| Reflection turns won't fire in real-time (idle gate is 30 min, poll cadence 60 s — see `src/memory/limits.rs`). | Two paths: (a) temporarily shorten `REFLECTION_IDLE_TIMEOUT_SECS` for the demo build; (b) rely on in-turn `memory_write` only — all scenarios above do, deliberately, so the demo doesn't depend on reflection cadence. |
| Scheduled-task fire cadence is on a poll loop (`src/scheduling/scheduler.rs`); the demo can't wait a real week for the Friday status email. | Shorten `SCHEDULED_TASK_POLL_INTERVAL` and the scheduled fire time for the demo build — fire seconds in the future instead of days. The `cron`/timestamp logic is unchanged; only the demo's clock dilation differs. Document the dilation explicitly so the audience trusts what they see. |
| Live Gmail send: rate limits, spam filtering, OAuth token expiry mid-demo. | Age the Northstar demo Gmail a few days before pitch (send a handful of normal emails first). Don't loop-run scenarios 20+ times the morning of. Re-authorise the MCP that morning. Extend scenario 06 to call `gmail-list-messages` for a liveness check. |
| The operator must reply as Sarah in real time during scenarios 03 / 07. Mistimed replies stall the demo. | Have the reply text rehearsed and pre-drafted on the operator's phone before each scenario starts — paste-and-send during the live run. The agent's Gmail-search step can poll up to N times with a short timeout before falling back to "no reply yet, holding". |
| Notion / Pencil / Calendar are live external SaaS — any of them being down kills the demo. | Pre-flight scenario 06 as a smoke test that hits each MCP for read; if anything is red, swap in a pre-recorded screencast for that beat. Don't pitch without a green pre-flight 30 minutes prior. |
| Notion's `update-page` is workspace-wide — a confused agent could overwrite the wrong page. | The role prompt confines each agent to its sub-tree; the operator audit in scenario 06 walks the workspace and asserts each page's `last_edited_by` matches the expected role. Catches drift before it reaches the pitch. |
| Pencil document must be open with the right `.pen` file on the demo machine before scenario 04. | Add a "pre-demo checklist" in build order step 1; pre-flight in scenario 06 calls `pencil.get_editor_state` and fails loudly if no document is open. |
| Librarian contradiction in scenario 04 is fragile — the model may not surface opposing signals naturally. | Fallback path: plant the contradiction explicitly via `Memory / 02 - Create Operator Note`. Demo continues with a guaranteed contradiction event for the librarian to detect. |
| 5 agents × 7 scenarios is real surface area; prompts will need iteration. | Build the agent system prompts as a shared pre-request module (one source of truth) so prompt tuning doesn't require editing every Bruno body. Same pattern Scenario 01 uses for the translator. |
| `search_agents` may return wrong matches if descriptions are weak. | Operator-curates each agent's `description` field deliberately for vector search — short, role-distinctive, one sentence each. Dry-run `search_agents` for the demo's expected queries during build and tune descriptions if any miss. |
| The pitch hinges on engagement #2 (scenario 07) "remembering". If memory rendering misfires, the demo collapses. | Operator-audit dry-run *before* the pitch: run scenario 06 against a real database and screenshot every agent's memory rows + `allowed_mcp_servers`. If something is off, fix the prompts before going live. |

---

## 8. Build order (when implementation resumes)

1. **Pre-demo accounts and auth** — set up the Northstar demo Gmail
   (separate from the operator's personal Gmail), the Northstar Notion
   workspace with the page tree from §4.2, a Northstar Google Calendar,
   and a Pencil document for "Acme Bakery". Authorise each via its MCP.
   Verify each MCP from a Bruno smoke call.
2. **Shared pre-request module** — single JS file invoked from each
   Scenario's `script:pre-request` that idempotently creates the five
   agents with the role prompts from §3, the `allowed_mcp_servers`
   from §2, and the operator-curated `description` fields used by
   `search_agents`. Registers the Notion, Pencil, Gmail, and Calendar
   MCPs.
3. **Scenario 02 (Kickoff & Discovery)** — account-manager + the
   operator-as-Sarah over real Gmail, no multi-agent yet. Cheapest
   validation that role prompts produce clean memory writes without
   "remember…" cues. First `schedule_task` call (proposal follow-up).
4. **Scenario 03 (Strategy Brief)** — first inbound Gmail loop
   (operator replies as Sarah → account-manager fetches → hand-off to
   brand-strategist). Validates per-agent memory separation, role-name
   addressing, and the Gmail read path.
5. **Scenarios 04, 05** — full fan-out with real Pencil + Notion +
   Calendar artefacts, the `search_agents` + `Collaborator` beat in 04,
   and the recurring `schedule_task` in 05 that produces real Gmail
   status emails on each fire.
6. **Scenario 06 (Operator Audit & Tool-Scope Proof)** — pure HTTP, no
   LLM. Cheap to author; doubles as a smoke test for memory + MCP
   scoping + external SaaS liveness.
7. **Scenario 07 (Six Months Later)** — the pitch moment. Ship last so
   it benefits from prompt iterations driven by 02–05. Trigger is the
   brand-strategist's scheduled task fire, clocked to fire seconds
   after scenario 06's audit completes; the audience sees a real
   "Christmas kickoff" email arrive on the operator's phone.

Each scenario is independently runnable — clicking any single Bruno
file from a clean DB rebuilds the prerequisites via the shared
pre-request module.

---

## 9. Out of scope for this document

- Real Stripe billing integration (Phase-2; the demo uses a Notion
  invoice page + Gmail send instead — see §4.7).
- Custom librarian cadence / reflection cadence tuning for the demo
  build (operator-time decision before the pitch — see §7).
- Scheduled-task poll-cadence tuning for the demo build (clock
  dilation; see §7).
- Provisioning of the Northstar demo Gmail / Notion / Calendar /
  Pencil accounts — operator handles before scenario 01.
- Pitch-deck slide design and screencast production.
