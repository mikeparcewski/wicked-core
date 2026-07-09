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
- **F9 (HIGH) — RESOLVED by spike, 2026-07-09 (GO).** The round-2 concern ("no deterministic headless
  skill-invocation exists") was **spike-tested against real `claude` v2.1.205** and is now GO: a
  `claude -p` prompt that BEGINS with `/<skill-name>` is harness-expanded deterministically (SKILL.md
  injected, in-role turn 0); an unknown `/skill` is validated + rejected, so it is a real mechanism not
  best-effort. **Correction to the original round-2 text:** the Tier-2 reviewer specialists ARE reachable
  directly by the hyphenated `/wicked-testing-<name>` slash form (NOT Tier-1-orchestrator-only), and
  `context: fork` is on EVERY wicked-testing SKILL.md (not a Tier-2 marker). PREREQS that remain (do not
  block, but are real): (1) the skills must be **installed** in `~/.claude/skills/` first (a fresh env
  provisions them — §4.2 skill-provisioner sidecar); (2) a top-level `/skill` in `-p` runs in the MAIN
  context, so reviewer independence needs **one isolated `claude -p` process per reviewer** with only
  evidence paths (not `context: fork`). Full recipe: brain `headless-skill-invocation-recipe`. So the
  skills layer is no longer spike-gated on invocation — only the per-CLI form for NON-claude seats
  remains data-to-be-authored (F11).
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

## Revision 0.5 — resolved design tensions (agent critique + operator answers, 2026-07-09)

An agent-perspective critique raised seven risks; the operator resolved each. Recorded so they are not
relitigated — these are decisions, not open questions.

1. **Verification vs. validation (ship-the-wrong-thing).** Resolved: the **agent validator** (semantic
   review, §rev0.4 pair) IS the intent-fitness gate — conformance (deterministic) + fitness (agent) are
   the two pieces on purpose. Refinement kept: at least one seat is prompted to check against the
   *original intent*, and to surface what the spec left **unspecified** (guards shared-omission).
2. **"Fixed DAG = waterfall."** Rejected: phases are the **control plane** (failure paths, fan-outs,
   re-entry, Campaign spawns — as ciq/gcp-sdlc already do), and the **execution mechanism inside a phase
   is judgment**, not a script. The DAG gives control flow; agents exercise judgment within it. Loopiness
   lives in the event flows, not in a rigid sequence.
3. **"Same-model seats aren't independent."** Rejected: independence comes from **context + framing**
   (like two humans working from different information), not just model weights — the reviewer's
   cold-evidence isolation is the mechanism; multi-CLI is an added strength. The question and context a
   seat is given drives divergence.
4. **"Who approves the validators?"** = **council territory.** Diverse seats decide (not a lone agent →
   no self-grade; not a forced human → no bottleneck), with human escalation above a threshold (see 6).
5. **"'Deterministic' is overclaimed."** Clarified: the deterministic validator asserts **any validatable
   artifact shape** — doc sections present, config keys, code shape, file/content patterns — not just
   "tests pass." That class genuinely is deterministic.
6. **"No rigor/cost dial."** Resolved: the **council determines the orchestration path** — it may assign
   to a CLI and act as the HITL itself via events, bubbling to a **real human only above a threshold**.
   The dial is the council's routing judgment, not a static config.
7. **"Event purity is dogma."** Rejected: events enable **sidecars** — standard event types subscribed
   for audit, extra processing, provisioning — attaching to workflows without touching them. That
   extensibility is the point.

**Trust model (name it, don't overclaim):** diverse-seat **agent consensus**, on a **deterministic
structural floor**, with **human escalation above a threshold**. The floor covers structural/factual
claims; meets-intent / design-sound rest on consensus. A green run means "diverse seats + the escalation
policy agreed," **not** "proven." Calibrate trust to that.

---

## Revision 0.4 — generated, grounded evidence-validators as the gate (operator direction, 2026-07-09)

**Supersedes §3 layer-1.** The deterministic gate check is NOT a generic precanned verifier chosen
from a closed enum. It is a **grounded, deterministic validation script authored by a test-strategy
agent for that specific phase/task**, stored in the vault as the phase/task's **evidence evaluator**,
versioned and **approved**. When the spec changes, the agent regenerates the validator and the update
goes through approval — validation always tracks the spec; a stale validator can never silently pass
work. This operates at **phase level, task level, or anything between**.

Why this is strictly stronger than the old ladder-1:
- **evaluator≠creator becomes deterministic + auditable.** The old evaluator was a second LLM pass you
  had to trust per run. Here the LLM authors the check **once**, grounded + reviewed; the gate then
  **re-runs the exact pinned script**. Judgment lives in a reviewed artifact, not a per-run vibe. The
  "no `llm_eval` at gate time" invariant is preserved — the LLM is offline (authoring), never at the
  gate.
- **Resolves review finding #3 (closed-enum capability leak).** `GateCond`/`GateType` need not enumerate
  every condition (threshold, cannot-reproduce, "no P0 open"). The condition logic lives **in the
  generated validator**, which returns a structured verdict the gate consumes. New condition = new
  generated script (data/artifact), never a core enum edit. This retires the 6-kind closed verifier
  set from old §3.
- **Layer-2 (structural deliverables) largely collapses in** — the validator asserts its own required
  artifacts. **Layer-3 (governance deny-dominates) unchanged.** Verdict still `= f(deterministic,
  governance)` only; the engagement dial still selects WHO confirms, never the verdict.

Mechanism (decisions committed 2026-07-09; forks flagged for operator veto):
- **Authoring:** the wicked-testing `test-strategist` / `acceptance-test-writer` skill, invoked via the
  verified headless recipe (`claude -p "/wicked-testing-<skill> …" --output-format json`, one isolated
  process per reviewer — see brain memory `headless-skill-invocation-recipe`). Grounded with the
  phase/task spec artifacts (clarify/design outputs + ACs) as input.
- **Dual validator (operator addition, 2026-07-09).** The evidence evaluator is a **pair**: (a) a
  **deterministic** validator (the grounded script — precise,
  auditable, cheap, can't judge semantics) and (b) an **agent-based** validator (judgment: "does this
  satisfy the intent?" — which no script can encode). Mirrors wicked-testing's own split:
  `acceptance-test-writer` (deterministic, evidence-gated) + `semantic-reviewer` (judges what the AC
  *means*). A phase may carry one or both — `commit-exists` needs only deterministic; "feature meets
  intent" wants both.
  - **Combination rule (preserves old §3's "a model may never *solely* approve"):** Approve requires the
    **deterministic** validator to PASS; the **agent** validator can REJECT but is never the sole
    approver. `Verdict = Approve iff deterministic==pass ∧ agent!=reject ∧ governance!=deny`; either piece
    failing → Conditional/Reject. This deliberately reintroduces an agent at gate-time, but only as a
    second, distinct-seat, run-cold piece that can fail-but-not-lone-pass — the auditable deterministic
    floor stays intact.
- **Validator form (fork 1):** the deterministic piece is a wicked-testing **acceptance scenario/plan**
  (reuses the existing executor + isolated reviewer + evidence-vault format), with a plain deterministic
  script escape hatch for non-test assertions; the agent piece is a grounded reviewer prompt/skill. Not a
  bare bash blob.
  - **BUILT + live-verified (2026-07-09, `src/validator.rs`):** `author_deterministic_validator` invokes
    the `acceptance-test-writer` skill to author a shell check; `run_validator` re-verifies it (no LLM at
    gate time); `agent_validate` + `combine_verdict` implement the agent judge + the rule. **Live-testing
    finding:** the agent piece must be a **controlled reviewer prompt, NOT the `semantic-reviewer` skill**
    — the skill imposes its own aligned/divergent/missing Gap-Report format that overrode a binary
    PASS/REJECT instruction (it emitted `"PASS — … so it diverges"` for bad work, fooling the parser). A
    controlled prompt on a distinct seat gives a clean binary; the two-strategist independence comes from
    the seat + cold framing, not the skill's format. Remaining: vault storage + content-hash pin +
    approval + wiring into the phase gate flow.
  - **Two validators check DIFFERENT substrates (composition finding, live-tested).** The deterministic
    validator runs its script in the phase's **worktree/cwd** (the filesystem artifacts); the agent
    validator judges the **work TEXT** in its own cwd with **no access to that worktree**. So a criterion
    must be framed to match: **structural/existence** ("file X exists / contains Y") is for the
    DETERMINISTIC half; **content/semantic** ("the deliverable satisfies the intent") is for the AGENT
    half. Sending an existence-framed criterion to the agent makes it search its *own* empty cwd and
    REJECT — a false negative. Also: the `acceptance-test-writer` skill wraps its shell answer in prose
    despite instructions, so extracting the command needs `extract_shell_command` (pick the last
    command-like line + strip a leaked language marker), not a naive fence strip.
- **Vault (fork 2):** **wicked-estate** holds the *approved validator artifact + pin* (content-addressed,
  injected `phase→validator` edge, durable); `.wicked-testing/evidence/<run>/` holds each run's
  *execution evidence*. Estate = source of truth, testing = run log.
- **Approval + change control (fork 3):** a regenerated validator is **diffed against the pinned one and
  re-approved** (the same evaluator≠creator + human-confirm-on-change any deliverable gets) before it can
  gate again.
- **Data:** `PhaseDef` (and optionally `WorkUnit`) gain up to two validator pins — a
  `deterministic_validator_ref` and an `agent_validator_ref` (each a **content-hash pin** so the gate runs
  the *exact* approved artifact; either may be absent). `skill_ref` names the **authoring** agent(s), not
  the check.
- **Independence — HONEST as-built (integrated review, 2026-07-09).** The two pieces are complementary
  (structural vs semantic), but as built they are **distinct PROMPTS/framing on the same runner**, not
  genuinely distinct council SEATS — so "uncorrelated blind spots from independent seats" is the
  *aspiration*, not today's guarantee. Wiring each piece to a real distinct council seat (different
  CLI/identity) is the tracked follow-up that would make the independence real.
- **Event-driven fit:** authoring the validator is itself a phase/task emitting events; "validator
  approved" is gate **state + notification** (not request/reply); running it at the gate is the
  deterministic re-verify. All consistent with Law 1.

This makes "gate wiring" = building this validator-authoring + pin + re-verify mechanism. The
`WorkflowDef`→runtime plumbing (plan_from_def driving the reducer) is orthogonal and lands underneath it.

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
| `wicked.run.requested` (workflow, problem, args) — the launch trigger | human CLI / **scheduler sidecar** / campaign | reducer |
| `wicked.run.triaged` (→ chosen `def_id`) | triage-router | reducer |
| `wicked.skill.needed` / `wicked.skill.refresh` → `wicked.skill.ready` (§4.2) | reducer | skill-provisioner sidecar |
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
- **scheduler sidecar** (operator direction, 2026-07-09) — a **publisher** that turns **schedules
  (data: cron/interval + workflow id + args)** into `wicked.run.requested {workflow, problem, args}`
  events on a timer. **Launch is an event**, so the reducer subscribes to `run.requested` and starts a
  run identically no matter who fired it — a human on the CLI, a Campaign node, or this scheduler. That
  makes "schedule agents" fall out for free (recurring nightly audit, periodic migration check,
  scheduled review): adding a schedule is a data row + a sidecar, **never a core change** — the same
  sidecar pattern (§4.2 / §rev0.5 #7) pointed at time. Idempotency (§2.4) covers a schedule that
  double-fires.

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

**VERIFIED working end-to-end (2026-07-09, runnable smoke on wicked-bus v2.2.3).** Both crew seams
round-tripped through the real bus: `wicked.run.requested {workflow, problem, args}` (emit → reducer
subscribes `wicked.run.*` → poll → nested args survive → ack) and `wicked.skill.needed → wicked.skill.ready`
(provisioner sidecar polls needed → emits ready → a reducer cursor scoped to `.ready` polls only that;
`run_id` correlation survives the chain). The substrate needs **zero new infrastructure**.

**HONEST SCOPE (integrated review, 2026-07-09) — what the bridge does NOT yet do.** `src/bus.rs` +
`Core::connect_bus` carry ONLY the **launch-trigger edge** (`wicked.run.requested` → `LaunchRun`, plus a
`wicked.run.launched` back), and it is **opt-in / env-gated** (`WICKED_BUS_DB`). The design's central
**mediation seam of Law 1** — the actor PUBLISHING `task.dispatched` for a `cli-runner` subscriber that
executes off-bus and PUBLISHES `task.completed` back, i.e. "the actor no longer calls execution directly"
— is **NOT built**: a shipped run still dispatches units in-process (`dispatch_unit` → worker thread →
`ApplyStepResult`), not over the bus. So **Law 1 is realized for the launch trigger, not for the
execution seam.** Building the `task.dispatched`/`task.completed` mediation over this proven substrate is
the tracked follow-up; today the bus is an ingress trigger + lifecycle emitter, not the control plane.
- **Real library API** (the wicked-bus README's programmatic example is STALE): `emit(db, config,
  {event_type, domain, subdomain, payload})`; `register(db, {plugin, role:'subscriber', filter,
  cursor_init})` → `{cursor_id}`; `poll(db, cursorId, {batchSize})` → **array of raw rows** (payload is a
  JSON string — `JSON.parse` it); `ack(db, cursorId, lastEventId)`. Isolate with `WICKED_BUS_DATA_DIR`.
- **Sidecars = application logic, NOT bus gaps.** The reducer + skill-provisioner are each a persistent
  `poll → handle → ack` loop (poll is one-shot; a daemon-push path exists but falls back to poll) with:
  persisted `cursor_id` across restarts (else `latest` skips backlog, `oldest` risks
  `CURSOR_BEHIND_TTL_WINDOW` past the 72h sweep); **idempotent handlers** (at-least-once re-delivers on a
  crash between handle and ack); a payload **correlation-id** convention for `needed→ready` matching (+ a
  `wicked.skill.refresh` timeout/retry); and precise filters (`wicked.skill.*` catches both `needed` and
  `ready`, so a subscriber must not consume its own emissions). See brain `bus-seam-verified-working`.

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

### 4.2 Skills fill the gaps; the skill set is a runtime allowlist; provisioning is an event (operator direction, 2026-07-09)

Skills are the capability-fill for everything the thin reducer doesn't do — phase execution, task
breakdown, the two validators (§ rev0.4), the reviewer. "Capability as data" at the *execution* layer,
exactly as `WorkflowDef` is at the *orchestration* layer. Three additions:

- **Per-phase/task skill allowlist, chosen at RUNTIME — like tool permissions.** Beyond the single
  driving `skill_ref`, a `PhaseDef` (and a task) declares `allowed_skills: [SkillRef]` — the set that
  agent may use for that step. The cli-runner passes it as the invocation's skill/tool scope (the
  `claude -p --allowedTools` + skills-dir analog), so each step runs **least-privilege**. Runtime-
  selected and pure data → a phase's capability surface changes without a core edit.
- **Provisioning + refresh are EVENTS, never a direct call (Law 1).** If a needed skill is missing or
  stale, the reducer publishes `wicked.skill.needed {skill_ref, version?, cli}` or
  `wicked.skill.refresh {scope}`; a **skill-provisioner subscriber** (wraps `wicked-testing:update`,
  which "refreshes skills across all detected AI CLIs") installs/updates it, then emits
  `wicked.skill.ready`. This decouples the engine from skill installation and solves the spike's
  prerequisite (a fresh run env needs the skills present in `~/.claude/skills/` first — see brain
  `headless-skill-invocation-recipe`). A phase blocked on a missing skill is gate **state +
  notification** (`AwaitingSkill`), not a synchronous fetch.
- **Composes with the gate model.** The deterministic-validator author, the agent-validator author,
  and the reviewer are all just skills with their own per-phase allowlists, provisioned identically.
  The two-strategist independence (rev0.4) is two skill invocations under two distinct seats.

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
