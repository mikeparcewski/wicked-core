# DES-TEAM-001 — Real-time teaming: peer monitors on the live feed, a worker that can ask for help, and the gate as the only decider

- **Status:** PROPOSED — **NOT BUILT.** No seam below is implemented. This document records an intended design and the evidence behind it; it does not describe engine behaviour. Where a component already exists it is cited as an existing seam the design would *consume*, not as this design shipping. Two seams (S1's trigger surface, S5's routing variant) are the subject of in-flight implementation work at the time of writing; S2, S3, S4 and S6 are not started.
- **Date:** 2026-09-22
- **Scope (intended):** wicked-core `src/distribute.rs`, `src/domain.rs` (`RoutingInfo`), `src/acp_runner.rs` (elicitation trigger surface), `src/actor.rs` (monitor fan-out); wicked-crew `packages/crew/src/api/` (feed + gate presentation); wicked-studio (gate adjudication UI)
- **Source:** wicked-core#590 (filed 2026-09-22 by the operator, directly after run `aee254f1`)
- **Related:** DES-002 (ACP elicitation — the worker-asks channel, landed; see §2.3), DES-EXEC-001 (the execution layer and the two laws), core#212 / core#233 / core#234 (elicitation revert and re-land), core#537 (`ballot_load_factor`), core#585 (agy credential path), DES-SEAT-001 (seat-instance identity — makes a monitor fleet affordable)

---

## 1. Problem

A council votes **before** work (routing) or **after** it (a verdict). Neither moment is when a defect is observable. The engine convenes a council per phase regardless of whether anything interesting happened, and the convening degrades **silently**: seats bench, consensus fails, and the run still produces a verdict that reads identical to a real one.

### 1.1 Measured — run `aee254f1` (B18, wicked-studio, 2026-09-22)

The following are the operator's recorded observations from that run as filed in #590. **They are run-ledger figures, not re-derived here** — this document did not have access to the rig's ledger for that run and does not assert them as independently reproduced.

| Signal | Value |
|---|---|
| `councilSeatFailed` | 12 — all one seat (`copilot`), 5× `quota_exhausted` + 7 bench-cascade |
| timeout-attributed failures | 0 |
| `consensus: false` | 2 of 4 units |
| ord 4 `agentVerdict` | `"skipped"` — the judge never ran |
| Gates | 4/4 passed — on `deterministicPass` + `evaluatorPass`, **not** on council agreement |

The reading #590 draws: the council contributed nothing to the verdict and convened a known-dry seat twelve times to collect twelve failures.

What did catch defects on that run was an independent reviewer reading the finished PR — five findings including a HIGH (a coverage fetch in an `onClick` with no cancellation, so clicking retire on a 1-memory scope then a 500-memory scope displays "1 memory will be erased" before a destructive action). That defect was visible the instant the handler was written and instead survived the creator, the evaluator, a human-approved deliver gate and a PR.

### 1.2 Why the shape, not the tuning, is the problem

Convening is periodic and phase-aligned; defects are continuous and edit-aligned. No amount of seat-health tuning moves a vote to the moment the handler is written. The design below moves the observation, not the vote.

---

## 2. What already exists, and what is genuinely new

This section is the honest inventory. Everything marked ✅ is an **existing seam this design would consume** — none of it is this design being built.

### 2.1 The live feed — ✅ exists

- `CoreEvent::UnitOutputDelta` (`src/event.rs:244`), `CoreEvent::GovernanceHookFired` (`src/event.rs:583`), `CoreEvent::WorkerToolCallDenied` (`src/event.rs:884`).
- The daemon fan-out: `app.get('/ws', { websocket: true }, …)` (`wicked-crew` `packages/crew/src/api/server.ts:1439`) and the per-PTY channel `/ws/terminals/:id` (`packages/crew/src/api/server.ts:1444`, authorised `operator` at `packages/crew/src/api/auth.ts:555`).

A monitor has something to subscribe to. It has no way to be *summoned*, and nothing consumes its output.

### 2.2 The agent→client question channel — ✅ exists

ACP `session/request_permission` is in production: rewritten into the Claude `PreToolUse` shape at `src/acp_permission.rs:318`, and judged on every such request of a turn (`src/acp_runner.rs:975`).

### 2.3 Worker asks for help mid-turn — ✅ **landed, contrary to #590's status table**

**#590 records this row as "⚠️ Built and reverted (#212 → reverted in #233; teardown wiring tracked in #234)". That is stale as of `origin/main`.** The Rust half was re-landed and then refined:

- `64279b3` — *"feat(acp): ACP elicitation maps — Rust half (core#234 reland) (#258)"*
- `b300d5f` — *"fix: elicitation capability is one predicate — what a seat advertises is what its turns serve (#346)"*

At `origin/main` the machinery is present, not parked:

| Piece | Location |
|---|---|
| `pub struct ElicitationMaps` | `src/acp_runner.rs:171` |
| `ElicitationMaps::pending` registration | `src/acp_runner.rs:138`, impl at `:224` |
| The `'elicit` dual-poll loop in `exec_turn_acp` | `src/acp_runner.rs:3929` (interval const at `:2994`) |
| Turn-time gate | `let elicitation_enabled = epoch > 0 && proc.elicitation_advertised;` — `src/acp_runner.rs:3750` |
| `CoreEvent::ElicitationCreated` / `ElicitationResolved` | `src/event.rs:1134` / `:1153` |
| Crew cache + routes | `packages/crew/src/api/elicitation-cache.ts`; `GET`/`POST ${V}/runs/:id/elicitation` at `packages/crew/src/api/routes.ts:3251` |

Crew's own comment states the completion honestly: *"the LIVE elicitation wire is complete (create → cache → resolve, crew#357/#358)"* (`packages/crew/src/api/routes.ts:3262-3263`).

**This changes S1 materially.** S1 is **not** "re-land elicitation". The transport is landed. What is missing is a **trigger a worker can reach**:

1. `elicitation_enabled = epoch > 0 && proc.elicitation_advertised` (`src/acp_runner.rs:3750`) restricts suspension to ACP-carrier units whose adapter advertised the capability at `initialize` (`elicitation_advertised` stored at `src/acp_runner.rs:2730`). Chat turns pass `epoch=0` and are never suspended (`src/acp_runner.rs:3749`, comment). The wrapped carrier has no elicitation path at all.
2. `elicitation/create` is raised by an **MCP server running inside the adapter**, not by the model deciding it is stuck. A worker cannot today *choose* to ask for help; it can only call a tool that happens to raise a form.

So the worker-asks-for-help capability needs a **tool the worker can deliberately call** ("ask the team"), plus a carrier story for wrapped units — not a revert-reversal. #590's own framing ("only one piece is novel … the rest is re-landing parked work") **understates the remaining S1 work in one direction and overstates it in another**: the parked work is already back, but the affordance it was meant to provide does not exist.

Note also that #590's premise *"#234's stated reason for parking elicitation — 'nothing currently triggers this and no consumer is stranded' — is now void; this design is the stranded consumer"* remains **correct and is in fact the live situation**: the wire is complete and still nothing triggers it.

### 2.4 Routing is already plural — ✅ exists

`pub enum RoutingInfo` (`src/domain.rs:690`) with `Tool`, `Degraded`, `Council`, `EvaluatorDistinct` — constructed at `src/distribute.rs:196`, `:652`, `:1064`/`:1310`, `:875` respectively. Adding a variant is an additive change to a shape the engine already branches on (`src/distribute.rs:714-719`).

There is a recorded precedent for **not** shipping a routing mode the engine cannot produce:

> *"a historical-ranking fast path once lived here, but distribution always runs with an IN-MEMORY council estate … Rankings therefore never persist across runs, so the fast path could never fire; it was removed rather than ship a `RoutingInfo::Ranked` mode the engine can't actually produce. Every unit convenes."* — `src/distribute.rs:1101-1105`

**That precedent binds this design.** `RoutingInfo::Teamed` must not be added until the engine can actually produce a teamed distribution; a variant that never fires is the exact mistake `:1105` records removing.

### 2.5 `evaluator ≠ creator` — ✅ exists, and is out of scope

`enforce_evaluator_distinct` (`src/distribute.rs:820`) is independent of the council and survives this change untouched. It is the real governance property and it does not depend on any of this. **Deliberately out of scope.**

### 2.6 Monitor→worker feedback mid-turn — ❌ the genuinely new engineering

Nothing in the engine delivers a third party's finding into a turn in progress. `InjectWorkerMessage` exists as an operator-driven inject, but there is no monitor role, no severity contract, no dedup, and no confirmation-against-settled-diff step. This is S3 and it is the only part with no existing home.

---

## 3. Design

### 3.1 The model

Seats stop being a voting bloc and become **teammates with different vantage points, working at the same time**.

1. **Peer monitors on the live feed.** A monitor subscribes to the run's stream (§2.1) and the worktree diff, and surfaces findings *while the work happens*.
2. **Bidirectional.** Monitors feed findings back in real time; the working CLI can ask questions or ask for help when it is unsure (§2.3).
3. **Complexity-scaled.** Monitor-pool size and depth key off signal: diff size, files touched, subsystem criticality, and whether a **destructive path** is involved. A docs change summons nothing; a memory-erase path summons a second pair of eyes automatically.
4. **End reviewers where impact outweighs cost.** Keep the independent post-hoc reviewer for high-impact changes; drop it where it is waste. #590 records the evidence from this program: 13/13 behavioural PRs came back with findings; 6/6 docs PRs landed clean first time. (As with §1.1, this is the operator's program tally, not re-derived here.)

### 3.2 Authority model — the load-bearing part

**A monitor is not always right. Peers advise; the worker may refute with evidence; the GATE decides.**

Reviewers lose the big picture — a fix that is correct locally can be wrong for the system. #590 records three occasions in this program where a reviewer was wrong and a worker's refusal was right:

- A reviewer's proposed fix on core#587 would have widened `matched`, so a build failing the version pin today would start passing — **loosening an admission gate** to satisfy a review comment. The worker refused with evidence.
- A reviewer's sub-claim on core#579 about `max_concurrency` was refuted from `campaign.rs:325-327`, which documents the exclusion as the specification.
- A human-authored review brief commissioned a test asserting `toolChildrenKilled >= 1`; four probes proved the assertion unreachable (`AwaitingHuman` and a registered tool child are mutually exclusive). The implementer refused rather than ship a vacuous assertion.

Therefore, as a design invariant:

| # | Invariant |
|---|---|
| I-1 | A monitor finding is **input**, never a verdict. No monitor output may terminate, fail or deny a unit on its own. |
| I-2 | A worker may **refute** a finding on the record. The refutation is evidence and rides with the finding. |
| I-3 | The **gate** is the only party that sees the diff, the findings, the refutations and the system context together — so the gate decides. |
| I-4 | Making monitors authoritative would merely relocate the single point of judgment. Explicitly rejected. |
| I-5 | `evaluator ≠ creator` (`src/distribute.rs:820`) is untouched. Teaming adds observers; it does not alter who may judge. |

### 3.3 Seams

Each seam is stated with what it would change and what it depends on. **None is built.**

- **S1 — the worker-asks-for-help affordance.** *Revised from #590 by the §2.3 finding.* Not a re-land: the elicitation transport is present at `origin/main`. S1 is (a) a tool a worker can deliberately call to raise a question, (b) a decision on the wrapped carrier, which has no elicitation path, and (c) lifting or justifying the `epoch > 0 && proc.elicitation_advertised` restriction (`src/acp_runner.rs:3750`). **In flight at time of writing.**
- **S2 — monitor subscription.** A read-only seat attached to the run feed (§2.1) plus the worktree diff, emitting severity-tagged findings. Not started.
- **S3 — monitor→worker injection.** The novel piece (§2.6). Not started.
- **S4 — complexity policy.** What summons how many monitors (§3.1 item 3). Not started.
- **S5 — `RoutingInfo::Teamed` + a run-level/config mode selector**, so the per-phase council remains a **user choice** rather than being deleted. Gated on §2.4's precedent: do not add the variant before the engine can produce it. **In flight at time of writing.**
- **S6 — gate adjudication.** Findings, refutations and evidence presented together at the existing gate. Not started.

### 3.4 Risks to design against

| # | Risk | Intended mitigation |
|---|---|---|
| R-1 | **Stream volume.** #590 records 380 `UnitOutputDelta` in one run; per-event monitoring is expensive. | Batch at semantic checkpoints (tool-call boundaries, file writes), not tokens. |
| R-2 | **Monitor noise.** A monitor that flags everything is as useless as a test that cannot fail. | Severity bar + dedup, specified in S2 before S3 delivers anything into a turn. |
| R-3 | **Premature findings.** A monitor sees intent before the diff settles; findings then become source claims with no `file:line` — a failure mode this program has already had. | Confirm against the **settled** diff before surfacing. |
| R-4 | **Monitors becoming authoritative.** | I-1…I-4 above. |
| R-5 | **Cost of the monitor pool.** N monitors means N more heavy CLI subprocesses on the spikiest thing the platform does. | Depends on DES-SEAT-001: cheap, instance-distinct, differently-modelled monitors. Without it, a monitor fleet is capped at one instance per signed-in vendor. |
| R-6 | **A `Teamed` variant that never fires**, repeating `src/distribute.rs:1105`. | S5 lands only behind a producible distribution. |

---

## 4. Acceptance (intended — nothing here has been run)

1. **S2:** a monitor seat attached to a live run emits at least one severity-tagged finding carrying a `file:line`, confirmed against the settled worktree diff, and emits **nothing** on a docs-only change.
2. **S3:** a monitor finding reaches the working CLI mid-turn and appears in the unit transcript, without the monitor being able to terminate, fail or deny the unit (I-1).
3. **S1:** a worker deliberately raises a question mid-turn and receives an answer, on both carriers or with the wrapped-carrier limitation stated.
4. **S6:** a gate presents a finding, the worker's refutation and the evidence together, and the gate decision — not the finding — determines the unit outcome (I-3).
5. **S5:** a run configured for council routing still convenes a council and reports `RoutingInfo::Council`; a run configured for teaming reports `RoutingInfo::Teamed`; neither variant can be produced without a distribution that actually took that path (§2.4).
6. **No regression:** `enforce_evaluator_distinct` (`src/distribute.rs:820`) behaviour is unchanged, including the `distinctness_fallback: "creator_seat"` disclosure (`src/distribute.rs:169`, `:173`).
7. **Re-run the defect that motivated this.** The `onClick` cancellation defect from `aee254f1` (§1.1) is surfaced by a monitor at authoring time, not by a reviewer after the PR.

---

## 5. Out of scope

- `evaluator ≠ creator` (§2.5).
- Deleting the per-phase council. S5 makes it a mode, not a casualty.
- Seat-instance identity and the worker pool — that is DES-SEAT-001, which this design depends on for R-5 but does not contain.

---

## 6. Open questions

- **OQ-TEAM-1.** Can a worker ask for help on the **wrapped** carrier at all, or is the affordance ACP-only for v1? `src/acp_runner.rs:3750` restricts it to ACP units with an advertising adapter today.
- **OQ-TEAM-2.** Is `epoch > 0` (`src/acp_runner.rs:3750`) a deliberate exclusion of chat turns that should persist under teaming, or an artifact of the elicitation design that teaming should lift?
- **OQ-TEAM-3.** What is the severity bar (R-2) and who sets it — policy data, or per-monitor skill text?
- **OQ-TEAM-4.** Does a monitor consume the worktree diff directly, or a digest? Direct diff access for a read-only seat interacts with the OS sandbox floor (DES-INPUT-GOV-008 Boundary 1).
- **OQ-TEAM-5.** How does a finding that arrives *after* the unit ends get adjudicated — dropped, or routed to the gate as a late finding?
- **OQ-TEAM-6.** #590's measured figures (§1.1) and program tally (§3.1 item 4) are recorded from the operator's ledger and were **not** re-derived for this document. Before S4's complexity policy is calibrated on them, they should be re-derived from the run ledger.

---

## 7. Also retired alongside this — the load-~10 operating rule

The operating rule *"do not launch a governed run above load ~10"* is **obsolete**, and this is verifiable at source independently of anything above.

`crates/wicked-council/src/dispatch.rs:205` already scales the ballot budget by host load:

```rust
pub(crate) fn ballot_load_factor(load1: Option<f64>, load5: Option<f64>, cpus: usize) -> f64 {
```

- The multiplier is `clamp(max(load1, load5) / cpus, 1, BALLOT_LOAD_FACTOR_CAP)` (`dispatch.rs:199` doc comment; the clamp itself at `:213`).
- `pub const BALLOT_LOAD_FACTOR_CAP: f64 = 5.0;` — `dispatch.rs:168`.
- It is applied at `dispatch.rs:727` (`let factor = ballot_load_factor(load1, load5, cpus);`).
- The derivation rides the timeout error message via `ballot_timeout_note` (`dispatch.rs:221`), e.g. `"40s × 2.40, 1-min load 33.6 / 14 cpus"`.
- The `max(1-min, 5-min)` choice is documented from the measurement that produced it (`dispatch.rs:200-204`): S6/S8 runs on 2026-09-20 had 1-min load ~10.9 on 14 cpus (factor 1.00, budget unchanged) while the 5-min was 15.6 (factor 1.11 → 44.6 s budget); pi answered in 40.8 s — inside the 5-min-driven budget but not the 1-min one. Tests pin all four behaviours: `dispatch.rs:2708`, `:2714`, `:2721`, `:2738`, `:2750`.

The rule predates core#537 and was still being enforced by operators in 2026-09. **The engine now absorbs the load the rule was invented to avoid.** Doc surfaces still carrying the rule are catalogued in `PLAN-DOCS-TEAMING-SEAT.md` §2; unlike the rest of this document, that correction is safe to make **now**, because it describes shipped behaviour rather than this design.
