---
name: DES-EXEC-001-event-driven-workflow-execution
title: The crew execution layer — event-driven, data-defined multi-agent workflows
status: draft
version: 0.1
date: 2026-07-08
author: mike.parcewski@gmail.com
review-required: true
grounded-in:
  - the fresh code-only functional review of wicked-crew + wicked-core (the "60% exposed" verdict)
  - the 3-project design mine (~/Projects/{command_iq,gcp-sdlc,rai-aws}) — see the design brief
  - scratch/.cross-pollination/concept-catalog.part-a-process.md (the migration-factory concepts)
  - scratch/handoffs/HANDOFF.md (the wicked-agent lifecycle + trust-boundary intent)
first-principles:
  - EVENT-DRIVEN, no direct calls between components — only publishers/subscribers. The AI CLIs
    (not event-aware) are mediated by custom subscribers we own.
  - Capability lands as DATA + REGISTRATION. Adding a workflow/gate/deliverable MUST NOT edit core.
    (command_iq's stability law — four prior redesigns died because capability was hardcoded.)
---

# DES-EXEC-001 — The crew execution layer

> Turns wicked-core from "sentence-split → one CLI → default-allow" into a **data-defined,
> event-driven, evidence-gated multi-agent workflow engine** — reusing the plumbing that already
> exists, wired onto wicked-bus, with the CLIs mediated by custom subscribers.

---

## Revision 0.2 — corrections from adversarial review round 1 (2026-07-08)

An adversarial review checked every "verified by reading the code" claim below against the actual
source and found the v0.1 factual base unreliable. These corrections OVERRIDE the sections below;
the sections are kept for the architecture shape (which held up) but their code claims are amended here.

**Corrected code reality (what actually exists):**
- `src/workflow.rs` is a **STUB** (only `StepInput/StepStatus/StepOutput/HumanDecision/StepRunner/
  StubStepRunner`). **`WorkflowDef`, `GateSpec`, `StageSpec` exist NOWHERE.** `StageKind` lives in
  `src/domain.rs:183` as a **closed enum `{Recon, Build, Review, Test}`**, assigned by a **keyword
  heuristic** `StageKind::classify()` — not declared per-workflow. `register_workflow` stores only
  `(id, description)` pairs (`pipeline.rs:194`).
- **There is no `CliPolicy::AdversarialPair`.** Real evaluator≠creator = `distribute.rs:71
  enforce_evaluator_distinct` (reassigns a Review/Test seat off the builder, **only if ≥2 seats**) +
  `pipeline.rs:241` a second **governance** decision under a different identity string. **No CLI re-run
  over the artifact; no artifact-passing between units** (units are independently decomposed +
  distributed).
- The gate is **fail-closed** (`execute.rs`/`pipeline.rs`; deny is reachable via a registered policy) —
  v0.1's "defaults to allow" was wrong. The true limitation: **no deny fires without a registered
  policy.**
- **No wicked-bus integration and no `rusqlite`/raw-SQL dep in wicked-core** (only the estate
  `SqliteStore` graph abstraction, which cannot write the bus `events` table). The cited
  `emit_event_to` precedent **does not exist**. `CoreEvent`s carry **no ids** (so v0.1's "attempt
  folded into event ids" describes nothing).
- **Campaign DAG is the one real reuse** — genuinely built + careful (`campaign.rs`, cycle detection,
  attempt-keyed run ids, atomic persist-then-launch, crash-resume reconciliation). §5 stands.

**Re-scoped consequences:**
1. **The first real deliverable is BUILDING the `WorkflowDef` value + a registry + a data-driven
   reducer** that dispatches on def-supplied data — with `StageKind`/`HumanConfirm` **demoted to data
   fields**. This is net-new core work (satisfying Law 2 requires it), NOT a data addition on an
   existing spine. Say so plainly wherever §1/§4/§7 imply "promote existing data."
2. **Evaluator≠creator "made real" = new artifact-passing**: feed unit N's `work_output` node into the
   review unit's prompt + keep the real seat-distinct guard (`distribute.rs`, note the ≥2-seat
   degradation). Delete every `AdversarialPair` reference in §3/§7.
3. **Bus publish goes through the wicked-bus CLI/library** (subprocess or napi), never a raw SQLite
   INSERT — wicked-bus `emit()` owns validation + payload CAS-offload + causality + the v3 migration
   gate. Also: the bus `idempotency_key` UNIQUE is only a **24h window**, so it is NOT the durable
   exactly-once guarantee — **durable dedup is the reducer's cursor/status monotonicity**, not the bus.
4. **Idempotency fixes:** (a) the idempotency key MUST include the **event type** (else
   `task.completed`/`evidence.recorded`/`gate.decided` for one unit collide on the UNIQUE key →
   data-loss). (b) Make `apply_step_result` a **single store transaction** (unit-status + cursor
   advance atomic) OR make `dispatch_unit` **skip a `Done` unit** — closes the crash-then-resume
   double-execute window. (c) §2.4's "reducer dedups on the key" is wrong — the real mechanism is
   cursor/status monotonicity (`actor.rs:776`, `:1055`, `:1120`).
5. **Law 1 needs an explicit edge table** (below). Becoming a real bus subscriber is a **reducer
   async control-flow rewrite** (today it holds a synchronous `runner: Arc<dyn StepRunner>` and calls
   it inline, `actor.rs:994`); and `campaign↔actor` are already direct in-process calls
   (`campaign.rs:739`) — those stay calls, honestly scoped.

**Edge table — what becomes bus pub/sub vs stays an in-process call:**

| Interaction | Decision | Why |
|---|---|---|
| reducer → CLI execution (`task.dispatched` → cli-runner → `task.completed`) | **bus pub/sub** | the mediation seam; the one edge that must decouple |
| gate needs human (`gate.pending`) / human answers (`gate.decided`) | **bus notification + command** | state+notification, not request/reply (per the priors) |
| campaign ↔ actor (dispatch node, confirm campaign gate) | **in-process call** (unchanged) | already built, deterministic, single-process |
| live CLI output (`CliOutputDelta`) | **in-process broadcast** (unchanged) | low-latency studio feed |
| studio observing everything | **bus subscribe** (mirror) | ecosystem-wide, multi-subscriber |

**CORRECTED build order (de-risked — bus LAST, engine FIRST, all bus-free-provable):**
1. **`WorkflowDef` value + registry + data-driven reducer + the `feature` def**, proven on the existing
   **in-process `CoreEvent` stream** + **print-mode** in `cargo test`. No bus, no napi. Proves Law 2.
2. **Real adversarial-reviewer 2nd run with artifact-passing** (unit N `work_output` → review prompt)
   + the seat-distinct guard. Proves the evaluator fix.
3. **Print-mode control-plane test** asserting the event sequence + reducer state (CI-safe, no CLI).
4. **THEN the bus** — as a *mirror* of already-working events, via the wicked-bus **CLI/library**.
5. **THEN bridge Campaign through napi** (`launch_run` is bridged; Campaign is not) → reachable from studio.

**§4.1 SKILLS — corrected by adversarial review round 2 (findings 9–12). The skills layer is a
DEFERRED, SPIKE-GATED refinement, NOT a slice-1 foundation. Corrections:**
- **No deterministic headless skill-invocation exists (F9, HIGH).** Skills are a Claude Code *plugin*
  (`wicked-testing/.claude-plugin/plugin.json`) invoked by slash-command (`/wicked-testing:…`) or
  interactive auto-match — both require the plugin *installed* in the env; `claude -p "{prompt}"`
  (crew's real template, `registry.rs:76`) has NO machinery to install/name/load a skill. The 40
  Tier-2 reviewer specialists are `context:fork`-only — **not invocable from outside**; you must invoke
  a Tier-1 orchestrator that forks them. → **SPIKE required** to prove one deterministic headless
  recipe (env plugin install + exact invocation form) before ANY design commitment to skills.
- **Reviewer isolation is NOT inherited as claimed (F10).** wicked-testing's enforced cold-evidence
  isolation is a property of Claude Code's fork + its `.wicked-testing/evidence/{run}/…` manifest
  format. On non-Claude seats (`agy`/`pi`) `allowed-tools` is **advisory only**; crew's
  `execute_wrapped` emits **raw stdout**, not the manifest. So dispatching a reviewer to a distinct
  seat gives **process-level isolation (a real 2nd independent run — keep this)** but NOT the enforced
  wicked-testing contract unless we also run on a Claude seat + bridge the evidence format.
- **Skill runtime is core code, not pure data (F11).** `SkillRef` routing (id→cli+skill) is data, but
  plugin provisioning + evidence-format + host-enforcement level are an environmental contract the
  runner must satisfy in code. `SkillRef` must carry the runtime contract (required plugin, evidence
  format, enforcement level) and the cli-runner validates/provisions it.
- **"Reuse ~247 skills, author no prompts" is FALSE for slice-1 (F12, HIGH).** It's **47** skills
  (7 invocable), not 247. `plan` is *test* planning (not general task-breakdown); **there is NO
  existing skill for the `design` or `build` phases** — the core "do the work" of `feature`. Only
  `review`/`test` map cleanly. → **build + design need NEW authored crew capabilities**; the engine
  slice-1 uses **authored prompts** on the existing `claude -p` path.

**Net:** slice-1's engine (WorkflowDef + data-driven reducer + `feature` + real artifact-passing 2nd
run) uses authored prompts on the current CLI path and is fully bus-free / skills-free / cargo-testable.
Skills are a separate track: **(spike) prove headless named-skill invocation → author the missing
build/design skills → bridge the evidence format for enforced reviewer isolation.** Not a blocker.

---

## 0. The two laws (do not violate)

1. **Event-driven.** No component calls another. Everything publishes/subscribes on the bus. The
   only durable, single-writer, *ordering* authority is the **reducer** (the existing actor). Every
   other participant — CLI-runners, gate-evaluator, adversarial reviewer, triage-router, human-gate
   UI — is a decoupled subscriber. The CLIs, which are not event-aware, are wrapped by **custom
   subscribers we own** (the "cli-runner" seam).
2. **Capability is data.** A workflow is a `WorkflowDef` value + registration — never new core code.
   Adding `feature`/`bug`/`migration` or a gate check must be *data*, not a core edit. If it requires
   touching the reducer, the interface is wrong.

*(Law 1 is the user's hard constraint; Law 2 is command_iq's `WORKFLOW-CONSTITUTION.md` — the design
that survived after four hardcoded redesigns died in 3 days.)*

---

## 1. What exists today (grounded, not aspirational)

Verified by reading the code:
- **Rich `CoreEvent` stream** (`src/event.rs`): SessionStarted · UnitPlanned · UnitDistributed ·
  UnitExecuting · CliOutputDelta · GateDecided · UnitDone/Denied · AwaitingHuman · Resumed ·
  SessionCompleted/Failed · full Campaign* events. **But it is observability OUTPUT**, broadcast via
  `subscribe()`, not the control substrate — execution is a direct in-actor dispatch to a worker
  thread that posts `ApplyStepResult` back.
- **Single-writer `StoreActor`** (`src/actor.rs`) — the ordering authority; idempotent apply of
  worker results (unit_ix + not-already-Done guard). This *is* the reducer we keep central.
- **Typed workflow spine** (`src/workflow.rs`): `WorkflowDef`/`StageKind`/`GateSpec`. Default is
  `[Recon, Build, AdversarialReview, FunctionalTest]` with HumanConfirm gates.
- **Council multi-CLI** (`crates/wicked-council`) with an evaluator≠creator seat guard.
- **Campaign DAG** — built + wired into the actor (`LaunchCampaign/ResumeCampaign/ConfirmCampaignGate`,
  `src/campaign.rs`, `actor.rs`): a DAG of Runs with edge conditions, failure policies, concurrency
  cap, gate decisions. **This is our cross-workflow composition primitive, already done.**
- **Wrapped CLI execution** (`src/execute_wrapped.rs`) — governed subprocess in a git worktree,
  stdout captured, streamed as `CliOutputDelta`.
- **Weak spots (the review's findings):** planning is a **sentence-splitter** (`src/plan.rs`);
  evaluator≠creator is a *label* on a governance claim, not a real 2nd run; the gate defaults to
  **allow** (no deny-policy reachable); Campaign/memory/governance are **not bridged through napi**;
  **no wicked-bus integration at all**.

**Conclusion:** this is not a rebuild. It's (a) promote the event stream to the control plane,
(b) add real workflow *data*, (c) make the planner + reviewer real, (d) bridge to wicked-bus + napi.

---

## 2. The event-driven architecture

### 2.1 The hybrid the priors proved (and why)
All three prior platforms keep the **workflow state + advance in a central durable reducer**
(SQLite/Firestore/DynamoDB) and make **only the CLI/agent execution a pure pub/sub seam**. Reason:
an at-least-once bus gives no cross-type ordering and can double-deliver; a central reducer +
idempotent dedup is how you get exactly-once *effect*. Gates/HITL are **state + notification**, not a
request/reply. We adopt this exactly: **the actor is the one durable subscriber (reducer); the
CLI-runner is the pure pub/sub seam; gates are state + a `gate.pending` notification.**

### 2.2 Event catalog (`wicked.<noun>.<verb>`, past-tense)
Published to wicked-bus AND mirrored to the in-process `CoreEvent` stream (studio subscribes to bus):

| Event | Publisher | Consumer(s) |
|---|---|---|
| `wicked.signal.received` | ingress (CLI/API/webhook) | triage-router |
| `wicked.run.triaged` (→ chosen `def_id`) | triage-router | reducer |
| `wicked.run.launched` · `wicked.stage.started` (stage_ix, kind, select_key) | reducer | — |
| `wicked.task.dispatched` (prompt, workdir, cli, attempt, role) | reducer | **cli-runner** (filtered by cli) |
| `wicked.task.output.delta` | cli-runner | studio |
| `wicked.task.completed` / `wicked.task.failed` (work_output ref, exit) | cli-runner | reducer |
| `wicked.evidence.recorded` (evidence_kind, envelope_hash) | reducer / cli-runner | gate-evaluator |
| `wicked.gate.pending` (notification for HITL) | reducer | human-gate UI |
| `wicked.gate.decided` (Approve{amend}\|Reject\|verdict) — a **command** to the reducer | gate-evaluator / human | reducer |
| `wicked.stage.completed` (verdict) · `wicked.run.completed`/`failed` | reducer | — |
| `wicked.campaign.node.finished` (maps to existing `CampaignRunFinished`) | reducer | reducer (dispatch next ready node) |

### 2.3 Subscribers (each decoupled; zero direct calls)
- **triage-router** — `signal.received` → rule table → `run.triaged`. (New; small.)
- **reducer** (the actor) — the ONE central, single-writer, durable subscriber. Consumes `*.triaged`,
  `task.completed/failed`, `gate.decided`, `campaign.node.finished`; owns state; publishes
  `stage.started`, `task.dispatched`, `gate.pending`, `run.completed`.
- **cli-runner** (one per CLI or a pool) — `task.dispatched` (filtered) → runs the wrapped subprocess
  in the run's worktree (**reuse `execute_wrapped.rs`**) → `task.output.delta` → `task.completed/failed`.
  **This is the "custom subscriber mediates the non-event-aware CLI" seam** (gcp's auto-runner / rai's
  SQS consumer analogue).
- **gate-evaluator** — `evidence.recorded` → runs the gate ladder (§3) with an identity **distinct
  from the creator** → `gate.pending` (needs human) or auto `gate.decided`.
- **adversarial reviewer** — the `AdversarialReview` stage's evaluator seat; consumes the creator's
  `task.completed`, publishes a critique as evidence (a REAL 2nd CLI run — fixes the "label" bug).
- **human-gate UI** — `gate.pending` → renders → human answer becomes a `gate.decided` command.

### 2.4 The one hard requirement — idempotency
The bus is at-least-once + unordered. The reducer MUST be idempotent. Every event carries a
**deterministic `idempotency_key = hash(run_id, stage_ix, unit_ix, attempt)`** (attempt is already
folded into event ids); the reducer dedups on it (it already dedups worker results). The single-writer
actor gives ordering; the bus is pure transport + fan-out. This reproduces the priors' CAS guard.

### 2.5 The wicked-bus bridge (Rust ↔ SQLite pub/sub)
wicked-bus is a local-first **SQLite** event log (`wicked.<noun>.<verb>`, domain/subdomain, JSON
payload, UNIQUE `idempotency_key` dedup, cursor-poll, at-least-once). wicked-core (Rust) publishes by
**writing rows to the same SQLite events table** (a thin Rust publisher; precedent: apps-core's native
`emit_event_to(store,…)` replaced the Node shell-out). Subscribers cursor-poll. No JS bridge on the
hot path. The in-process `CoreEvent` broadcast stays as the low-latency studio feed; bus is the
durable, ecosystem-wide, multi-subscriber substrate.

---

## 3. Gates, evidence, evaluator≠creator (make the fakes real)

- **Gate ladder** (from command_iq's constitution): (1) **deterministic evidence** — re-run the
  pinned verifier, **never trust the cached status**; (2) **structural check** — required deliverables
  present/typed; (3) **governance** deny-dominates (`wicked-governance`); **verdict = f(1,2,3) only**.
  `Verdict ∈ {Approve, Conditional, Reject}`.
- **Engagement dial** `{just-finish | balanced | ask-first}` — the cardinal invariant: it selects
  **WHO confirms**, never **WHAT the verdict is**. (Lint-enforced separation in all three priors.)
- **evaluator≠creator, structurally** — the gate-evaluator and the adversarial reviewer run under a
  **different council seat/identity** than the creator (reuse `CliPolicy::AdversarialPair`). The
  reviewer is a REAL second CLI run over the cold artifact — not a relabeled claim. "A model may fail
  a gate; it may never solely approve one."
- **Evidence** — per-phase `work_output` node with typed `evidence_kind`, `schema_version`, and a
  **re-verified envelope hash** (SHA-256 tamper seal, recorded-at floor); immutable; the gate
  re-derives status at eval time. Verifier kinds are a closed enum
  (`exit_code_eq, regex_match, not_contains, commit_exists, structural_eq, content_present`) —
  **no `llm_eval`** as a primary verifier.

---

## 4. The workflows as DATA (`WorkflowDef` values, not code)

A `WorkflowDef` = `{ id, phases: [PhaseDef] }`; a `PhaseDef` = `{ id, gate_type ∈ {value|strategy|
execution|null}, executes_code, verified_evidence, required_deliverables, depends_on[], min/max_agents,
role_hints[] }`. Registered in a registry; the reducer reads the def, never branches on `id`.

Phase shapes (converged from all three priors):
- **feature** — `clarify(value gate) → design(strategy) → build(execution) → adversarial-review →
  test(FunctionalTest) → review`. `build.executes_code`, `test.verified_evidence`. HumanConfirm after
  clarify + after adversarial-review; `HumanConfirmIf(verdict≠PASS)` on test.
- **bug** — `triage(value) → reproduce(value) → fix(execution) → verify(FunctionalTest)`.
  `fix.depends_on=[reproduce]` — reproduce-first; not fixed until the repro goes red→green.
- **migration** — `plan(strategy) → execute/backfill(execution) → cutover(execution, UNCONDITIONAL
  HumanConfirm the dial can never downgrade) → verify(execution) → cleanup(advisory)`.

**Real task-breakdown replaces the sentence-splitter:** the `clarify`/`plan` phase is a bounded CLI
task ("decompose this intent into work units with acceptance criteria"), council-routed to a
cheapest-capable seat — its output is the unit list, gated like any phase.

### 4.1 Phase execution = a SKILL invocation, not a raw prompt (consistent control)

A phase's work is driven by a **skill** — a structured, versioned, allowed-tools-scoped capability
definition — **not** an ad-hoc prompt. This is how we control the CLIs *consistently* (a raw prompt
varies every run; a skill is a fixed contract) and it keeps Law 2: a phase references a `skill_ref`
(data), never embeds behavior in core.

- **`PhaseDef` gains `skill_ref` (+ optional `role: Creator|Evaluator`).** The `task.dispatched`
  event carries `{ skill_ref, inputs, workdir, cli, role, attempt }`. The **cli-runner subscriber
  invokes the CLI with that skill loaded** (e.g. `claude` with the wicked-* plugin available,
  invoking the named skill) — the skill, not the runner, defines the behavior + tool scope + evidence
  contract for the phase.
- **REUSE the ~247 skills the ecosystem already ships** — crew orchestrates them; it does not author
  new prompts. Phase → skill mapping (first slice):

  | Phase | Skill (existing) | Source |
  |---|---|---|
  | clarify / plan / task-breakdown | `plan` | wicked-testing |
  | design | `authoring` (design mode) / a garden archetype skill | wicked-testing / garden |
  | build | `authoring` / `execution` | wicked-testing |
  | adversarial-review (Evaluator seat) | `review` · `semantic-reviewer` · `acceptance-test-reviewer` | wicked-testing |
  | test / verify | `execution` · the 3-agent acceptance pipeline | wicked-testing |
  | bug: reproduce | `acceptance-test-writer` / `execution` | wicked-testing |

- **evaluator≠creator becomes REAL via skills, not a label.** The build phase runs an authoring/build
  skill under the **Creator** seat; the review/gate phase runs `acceptance-test-reviewer` /
  `semantic-reviewer` under a **distinct Evaluator** seat (different CLI/identity) that reads **cold
  evidence only** — wicked-testing's whole design is *enforced reviewer isolation*, which is exactly
  the guarantee we need. So the gate's independent verdict is a real second skill run over the
  artifact, dispatched as its own `task.dispatched` event. This closes the review's "evaluator≠creator
  is just a label" finding by construction.
- **Skills are registered as DATA** (a `SkillRef` table: id → CLI + skill name + evidence contract),
  so adding/swapping a phase's skill is a data edit. New wicked-* skills become new phase options with
  zero core change.

---

## 5. Composition — self-contained by default, Campaign for cross-workflow

Evidence correction: in all three priors **every workflow is self-contained** — feature/bug/migration
need **zero** other workflows. The only real cross-workflow spawn is command_iq's `ops→feature`.
So:
- **Default:** triage pins ONE `def_id`; the Run is self-contained. Matches the priors fully.
- **Optional cross-workflow composition = a Campaign** (already built): "feature verify fails → spawn
  a `bug` Run" is a Campaign node with an `OnTerminal` edge under `ContinueIndependent`/
  `HumanGateOnFailure`; "migration per unit" is a Campaign fan-out with a concurrency cap. Purely
  event-driven (`campaign.node.finished` → dispatch next ready). **This makes crew more composable
  than any prior — for free.**

---

## 6. Build order (the first slice) + proof

1. **Bus spine** — the event catalog (§2.2); the Rust wicked-bus publisher (§2.5); promote the actor
   to consume control events; deterministic `idempotency_key`.
2. **cli-runner subscriber** — `task.dispatched` → `execute_wrapped` → `task.completed`. The mediation
   seam. (Prove the actor no longer calls execution directly — it publishes.)
3. **`feature` WorkflowDef** as data + the real `clarify` task-breakdown + the gate ladder (§3) with a
   real adversarial-reviewer 2nd run.
4. **Proof:** a **print-mode** run (stub cli-runner, no real CLIs — the whole event flow runs
   deterministically, CI-safe) + **one real-CLI e2e** (`claude -p`) end to end.
5. Then `bug`, then `migration`, then Campaign composition, then **bridge through napi → studio**
   (Campaign + these workflows become reachable from the product — closes the review's "built but
   unexposed" gap).

**Test strategy:** print-mode is the control-plane unit test (concept #10 — no AI provider, asserts
the event sequence + reducer state). Real-CLI e2e is `#[ignore]`'d in CI, codifies the recipe. Gate:
`cargo build/test/clippy -D warnings` green per change (wicked-core CI already enforces this).

---

## 7. Reuse vs build

| Need | Status | Action |
|---|---|---|
| Durable reducer / ordering | **built** (actor) | Promote to consume control events; keep single-writer |
| Typed stage machine + gates | **built** (`workflow.rs`) | Add `value`/`strategy` gate types; workflow defs as data |
| CLI execution (mediated) | **built** (`execute_wrapped.rs`) | Wrap as the `cli-runner` bus subscriber |
| evaluator≠creator | **guard designed** (council `AdversarialPair`) | Make the reviewer a REAL 2nd run; reuse the seat guard |
| Cross-workflow composition | **built + wired** (Campaign DAG) | Use for optional feature→bug / migration fan-out |
| Event bus | wicked-bus ships (SQLite) | Add the `wicked.<noun>.<verb>` catalog; Rust publisher; idempotency_key |
| Real planning | **sentence-splitter** (`plan.rs`) | Replace with a bounded CLI task-breakdown phase |
| Signal→triage routing | **absent** | Build the triage-router subscriber (rule table) |
| napi bridge (Campaign/workflows/governance) | **not bridged** | Expose after the slice proves out — closes the product gap |
| Dimensional governance (gcp 8-dim, lanes) | **absent** (deny-dominates only) | Deferred; add if risk-tiered auto/human routing is wanted |

---

## 8. Open questions
1. **Rust wicked-bus publisher** — write to wicked-bus's SQLite table directly (schema-couple) vs a
   tiny FFI/CLI. Prefer direct SQLite write (precedent: apps-core native emit). Confirm the events
   table schema is stable enough to couple to.
2. **Print-mode stub** — a `StubCliRunner` subscriber returning schema-shaped output, so the whole
   event flow runs with no CLI. Confirm this is the CI-safe default (it is, per concept #10).
3. **Where triage-router + cli-runner live** — Rust subscribers in-process, or the TS crew layer
   subscribing on wicked-bus? Leaning Rust for the reducer-adjacent ones (idempotency), TS-optional
   for ingress. Resolve in the first slice.
