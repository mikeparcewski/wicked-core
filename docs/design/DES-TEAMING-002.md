# DES-TEAMING-002 — The team model on the bus: one transport, one grammar

- **Status:** DRAFT (rev 2). Three items are open with the operator, each written as an `[OPERATOR DECISION n]` block with a default and one alternative, so this document finalizes by deleting one branch per block (§8.3, §8.4, §8.5).
- **Date:** 2026-09-23
- **Rev 2 (2026-09-23):** review on #612 at `06db4cd`, three MEDIUMs, each verified at the code. (1) §4.1 states the idempotency-key algorithm exactly as `deterministic_key` computes it: every part NUL-terminated, 16 bytes, lowercase hex. It adds test vectors and the JS transcription. (2) Member work needs an explicit `owner` field, because `PhaseDef` is `deny_unknown_fields`. §8.3 adds `PhaseDef.owner: StepOwner`, carried by `plan_from_def` into `WorkUnit.owner`. (3) The step-boundary dedup keys on `outcome:"injected"` on any channel; `boundary` is a channel, not an outcome. §4.2, §7, §8.6 and T3 now agree. Enum sweep: the plan example now uses the real `GateSpec` JSON form and a library pin id; the no-bus case uses a local `transport:"none"` snapshot; every brace shorthand names its field; `disposition`, `verdict`, `role` and `status` declare their enums.
- **Supersedes:** the transport and orchestration parts of DES-TEAMING-001 (§14 lists every clause). DES-001's landed seams stay: S1 elicitation (#599), S3's steer mechanism over `_session/steering` (#607), S5 `RoutingInfo::Teamed` + `Core::convene_decision` (#608), S4's impact score (#600), S2's instance seats and monitor host (#605; #609 merged as `7f07428`), the batching/confirmation/dedup rules (DES-001 §4.3–§4.6), the gate render and judge exclusion (§6.2), and the unresolved-HIGH → council → continue-or-pause ruling (§6.3, §6.7).
- **Scope:** wicked-core (publisher, supervisor, carrier, fold), wicked-crew (bus handoff, relay, read route), wicked-studio (feed + gate panel), wicked-bus (no change — read as the transport contract).
- **Related:** #590 and its operator decisions (council only for a concrete dispute; every unaccepted HIGH is unresolved; impact scoring by blast radius), #604/#610 (DES-001), #609 (S2, merged as `7f07428`), #600 (S4, open).
- **Evidence base:** wicked-core `main` at `7f07428` (which includes S2, #609); `origin/feat/590-s4-review-scale` at `c9dfdfa` (S4, open, cited as "S4"); wicked-crew `main` at `fc079ec`; wicked-bus `main` at `d309d56`; wicked-studio `main` at `340e92e`. Every `file:line` was opened at that revision.

## 0. The operator's model (the contract this document implements)

1. Start a path.
2. The user chooses the starting CLI, or takes a random selection.
3. The user submits the request.
4. The primary agent (**PA**) scores it per spec (S4 impact score).
5. From score + intent the PA lays out a workplan: how many monitors (score + the PA's own ask), which steps (formal test planning? design? architecture? …), anything else execution needs.
6. Monitors read from the stream and interact with the PA via events.
7. Monitors are team members: they can do work, give guidance, or provide support.
8. If the team feels strongly the PA is heading the wrong way, the council is called in — a three-way conversation (PA, team, council).
9. At the review gate the reviewers have the outcomes **and** all team comms.

"One transport, one grammar": every team interaction is a `wicked.team.<noun>.<verb>` event on the durable bus. No direct channel exists beside it.

## 1. Problem: two fabrics and three direct channels

DES-001 was written against the engine's in-process fan-out and added direct channels where the fan-out could not carry state. On `main` and the two open branches the team model therefore rides **four** mechanisms:

| Mechanism | What rides it | Evidence |
|---|---|---|
| The engine's in-process `CoreEvent` fan-out (`EventSink`: record to the per-run JSONL log, then fan out to `mpsc` subscribers) | `adviceDelivered`, `workerAdviceResponse` ; `unitCheckpoint`, `monitorAttached`, `monitorFinding` (S2) | `src/event_log.rs:466-495`; `src/event.rs:1180-1201`, `:2409`, `:2426`; `src/event.rs:1210`, `:1225`, `:1241` |
| A direct `TeamCmd` channel from the worker-thread seam to the supervisor (`Attach`, `Finish`, `RunComplete`) plus a `TeamHandle` installed on the runner | attach context, the final-pass trigger, the run-complete sweep | `src/team.rs:1437-1443`, `:1453-1470`, `:1498-1510`; `src/lib.rs:573` (`runner.install_team(...)`); `src/workflow.rs:418` (`StepRunner::team_finish` default), called at `src/cli_runner.rs:766` |
| A `SteerMailbox` (`Arc<Mutex<HashMap<(run,ord,attempt), Vec<Advice>>>>`) written by the supervisor and drained by the ACP carrier | HIGH findings on their way to `_session/steering` | `src/team.rs:1730-1741`, `:1754-1776`, `:1792`; `src/acp_runner.rs:4903-4917`, `:5552`, `:8128` |
| The wicked-bus SQLite log, written by `BusDb::emit` | `wicked.crew.run.requested/launched`, `wicked.crew.task.dispatched/completed`, `wicked.gate.eval.requested/responded` | `src/bus.rs:1-16`, `:288`; `src/cli_runner.rs:80-89`, `:402`, `:1375` |

The bus is already the fabric with the properties the team model needs and the fan-out lacks: durable rows (`src/bus.rs:288-345`), a deterministic idempotency key that makes a re-publish a no-op (`:319-345`, `:545`), a total order (`ORDER BY event_id ASC`, `:497-499`), per-consumer durable cursors (`:371`, `:385`), a filter grammar shared with the JS side (`:562`; `wicked-bus lib/poll.js:29-76`), and a request/response pattern the gate already uses off-actor (`bus_request_agent_verdict`, `src/cli_runner.rs:351-445`). This document moves the team model onto it and deletes the other three.

## 2. What exists today (verified at the cited lines)

| Fact | Evidence |
|---|---|
| The bus row shape core writes is the wicked-bus `events` table: `event_type`, `domain` (publisher identity, e.g. `wicked-core`), `subdomain`, `payload`, `schema_version`, `idempotency_key`, `emitted_at`, `expires_at`, `dedup_expires_at`. A duplicate key resolves to the existing row's id (at-least-once, not an error). | `src/bus.rs:149-158`, `:288-345`; `wicked-bus lib/schema.sql:7-14` |
| `BusDb::poll(filter, after_event_id, batch)` returns unexpired rows strictly after a floor, oldest first, matched by `matches_filter`. `load_cursor`/`save_cursor` persist a named consumer's floor. One connection per thread. | `src/bus.rs:495-541`, `:371`, `:385`, `:239-241` |
| The gate already does a bus round-trip off-actor: publish a keyed request, poll for the paired response by id from the request's own `event_id`, deny fail-closed on timeout. | `src/cli_runner.rs:351-445` (`:402` emit, `:417-445` poll loop, `GATE_EVAL_TIMEOUT` `:93`) |
| The exec seam's keyed dispatch and its per-process consumer names. | `src/cli_runner.rs:1367-1375` (`task_key` `:315`), `:1407` (`consumer_name`), `:1588-1608` |
| The bus poll loop never runs on the actor thread; pollers reach the actor only by `Sender<Command>`. | `src/bus.rs:27-33`, `:693` |
| The engine is handed a bus db only through `WICKED_BUS_DB`, and crew sets it **only** when `--engine-exec` is on. Crew resolves a **different** default bus for its own seams: the `<core db>.bus/bus.db` sidecar. | `src/actor.rs:985-995`, `:1001-1015`; `wicked-crew packages/crew/src/core/adapter.ts:1183-1191`; `packages/crew/src/cli/index.ts:99-113`, `:125-137` |
| Crew's `/ws` is fed by the **one** in-process napi subscription, not by the bus; `broadcast` fans a frame to every socket. | `adapter.ts:1213-1216`, `:1297`; `packages/crew/src/api/server.ts:31`, `:1202`, `:1235`, `:1458`; `packages/crew/src/events/bus.ts:10-35`; `crates/wicked-core-ts/src/lib.rs:804` |
| Crew already relays a bus family onto `/ws` as an envelope frame: `bus.subscribe({plugin, filter:"wicked.interactive.**", cursor_init:"latest", maxRetries:0})` → `broadcast({type:"interactiveEvent", event})`. Studio folds that envelope defensively. | `packages/crew/src/interactive/ws-relay.ts:47-55`, `:148-162`; `wicked-studio src/store/runtime.ts:219-230` |
| Crew's durable consumers are idempotent by construction and keyed by a plugin name; emits carry a deterministic `idempotency_key` and treat `WB-002` as "already happened". | `packages/crew/src/qe/gate-events.ts:41-47`, `:77-79`; `packages/crew/src/projects/events.ts:92-108`, `:114-135` |
| `GET /api/v1/runs/:id/events` serves the engine's per-run JSONL log (`run_events`), which records **CoreEvents only**. High-volume variants are excluded from the log. | `packages/crew/src/api/routes.ts:3415-3431`; `adapter.ts:1825`; `src/lib.rs:1240`; `src/event_log.rs:149`, `:370-380`, `:448` |
| The grammar: `wicked.<domain>.<noun>.<past-tense-verb>`, four segments, lowercase; validated by `/^wicked\.[a-z0-9_]+(\.[a-z0-9_]+)*$/`. The `<domain>` gloss says "the producing product's short name", and core already publishes under a functional domain (`wicked.gate.eval.requested`). | `wicked-bus reqs/SPEC.md:369`, `:379-397`; `lib/validate.js:9`; `src/cli_runner.rs:88` |
| Filters: SPEC v1 text documents exact and single-level `.*` and defers `**`; the shipped code matches `prefix.**` (one-or-more segments) and crew relies on it. Core mirrors the code. | `reqs/SPEC.md:700-727`; `lib/poll.js:50-59`; `packages/crew/src/qe/gate-events.ts:43`; `src/bus.rs:555-562` |
| Retention: `expires_at` (72 h) hides a row from polls; `dedup_expires_at` (24 h) is the **row deletion** trigger for the v1 sweep. The v2 sweep moves TTL'd rows to warm monthly buckets and `pollResolve` reads across tiers. The `retention: forever` column exists in DESIGN-v2 only, not in the shipped schema. Neither crew nor core runs a sweep. | `reqs/SPEC.md:804-818`; `lib/sweep.js:53`; `lib/sweep-v2.js:2-3`; `lib/query.js:41`; `DESIGN-v2.md:97` vs `lib/schema.sql` (no `retention` column) |
| At-least-once is the contract; idempotency is the consumer's responsibility. | `reqs/SPEC.md:751-771`, `:780-798`; `DESIGN-v2.md:64` |
| `wicked_apps_core::emit_event_to` writes an EVENT **node on the estate graph**, not a bus row. It is not a transport. | `crates/wicked-apps-core/src/emit.rs:172-186`, `:199` |
| The shared worker-thread seam every carrier's unit passes through: run, guard's first look, `work_for_agent`, judge; callers in-process and bus. | `src/cli_runner.rs:748-761`, `:876`, `:881-901`; `src/actor.rs:7034` (via `run_unit_and_judge` `:682`), `src/cli_runner.rs:1976-2018` |
| The judge excludes the work author on all three paths; monitor exclusion is DES-001 §6.2. | `src/cli_runner.rs:912-930`, `:937-942`, `:1020-1025`; `src/validator.rs:1506`, `:1585` |
| The fold's emission order: `GateEvaluated` → `GateDecided` → `UnitDone`/`UnitDenied`; the gate's logic is `combine_verdict`. | `src/pipeline.rs:806`, `:1519-1556`; `src/validator.rs:2263` |
| The human pause is durable state plus one `awaitingHuman{gate_kind}` event; resumption is `confirm_gate`; a rework carries `rework_amendment` as prior context. | `src/actor.rs:6022-6060`, `:7736`, `:8052`, `:6624-6650` |
| The council entry point: `DecisionRequest {session_id, ord, question, options, evidence}` → `DecisionVerdict {winner, consensus, agreement_pct, returned, seated, dissent, no_ruling_reason}`; ballots run off-actor and relay `CouncilConvened`/`CouncilDeliberated`/`CouncilSeatFailed`/`CouncilVoted` through the emit point. | `src/decision.rs:276-310`; `src/lib.rs:1283-1293`; `src/actor.rs:2525-2544`, `:3432`; `src/event.rs:172`, `:180`, `:194`, `:223` |
| Routing is deterministic per unit (`RoutingInfo::Teamed { winner }`), then the evaluator≠creator fence moves review/test units off builder seats. | `src/distribute.rs:174`, `:387`, `:547` |
| A workflow is data: `WorkflowDef { id, phases: Vec<PhaseDef>, base_skill_ref }`; a `PhaseDef` carries `kind` (Recon/Build/Review/Test), `gate_type`, `gate`, `role`, `skill_ref`, `validator_pin`, `depends_on`, `executes_code`, with `deny_unknown_fields`. `Core::register_workflow` validates and registers at runtime; `plan_from_def` yields one unit per phase. | `src/workflow.rs:640-647`, `:686-708`, `:800-803`; `src/domain.rs:630-640`; `src/lib.rs:1296`; `src/actor.rs:2545-2556`; `src/plan.rs:94` |
| Launch input: `LaunchSpec { problem, clis, entity_mode, session_id, human_confirm, auto_deliver, repo_ref, base_ref, … }`. Crew's `POST /runs` takes `clis`. | `src/lib.rs:190-216`; `routes.ts:222` |
| Operator inject on ACP: queued per `(run, cli)`, delivered as a `[operator message]` prior-context block on the next matching unit's prompt. Wrapped CLIs run with `stdin` null. | `src/actor.rs:2557-2600`; `src/acp_runner.rs:5548`, `:6750-6781`, `:7017-7021`; `src/execute_wrapped.rs:3468` |
| The only mid-turn channel is the ACP adapter's `_session/steering`, read once per process, delivered at a terminal `tool_call_update` in the `session/update` arm. | `src/acp_runner.rs:2835`, `:3910`, `:4440`, `:4903`; `src/team.rs:1550` |
| S4 (branch): `signals_from_diff`, `graph_age`, `assess(&ChangeSignals, Graph, Option<&dyn ModelAssessment>) -> Assessment { deterministic, score, reasons, model, signals, plan }`; `plan_for(score)`; bands `monitors` 0/1/2/3. | S4 `src/review_scale.rs:635`, `:375`, `:563`, `:169-176`, `:552`, `:223-241`, `:135-139` |
| S2 (on main since `7f07428`): `TeamPlan { monitors, candidates }`, `AttachCtx`, `MonitorScope`, `MonitorHost` trait, `Emit`, the supervisor consuming **only** `UnitCheckpoint` from the fan-out, and a hold-checkpoints-before-attach buffer that exists because the fan-out and the direct channel have no shared order. | `src/team.rs:122-128`, `:625-645`, `:651`, `:659-670`, `:675`, `:1498-1510`, `:1150-1200` |
| `serde_json` has no `preserve_order`; fixtures compare values. | `Cargo.toml:59` |

## 3. Shape in one paragraph

A run starts a **path**. The launch names the PA seat (chosen or random) and publishes `wicked.team.path.started`. The PA runs the path's first step on its own seat: it names the touch set it expects, the engine scores that set against the repo graph (S4) and publishes `wicked.team.path.scored`; the PA composes a workplan from the phase library and publishes `wicked.team.plan.proposed`; the engine validates and registers it as the run's `WorkflowDef` and publishes `wicked.team.plan.accepted` (non-blocking by default). Every step the PA takes is bracketed by `step.claimed` and `step.completed` on the bus, with `checkpoint.reached` at each terminal tool call in between. Monitors are **team members** with a durable cursor on the run's `wicked.team.**` stream: they raise `finding.raised`, answer `help.requested`, and may `step.claimed` a step of the plan themselves. Advice reaches the PA **uniformly at its next step boundary** from the stream (every carrier) and, on the Claude ACP carrier only, mid-turn through S3's steer at the next tool-call boundary (the steer's source is the stream, not a mailbox). The PA answers every finding on the record (`advice.answered`). After a step's turn the supervisor runs the final pass (settled review, re-confirmation, hold round → `finding.settled`), convenes a one-off council for every unresolved HIGH (`council.called` / `council.ruled`, the three-way transcript), folds the attempt's stream into one `TeamLedger`, and publishes `wicked.team.gate.opened`. The worker thread waits for that event (bounded, fail-closed) and hands the ledger to the engine gate exactly as DES-001 §6.2; the fold's decision is published as `wicked.team.gate.decided`. Crew relays `wicked.team.**` onto `/ws` as `teamEvent` frames and serves the run's transcript from the bus; studio renders the feed and shows the comms at the gate. The bus is the record of the conversation; the `gate.opened` snapshot persisted on the unit is the record of what the gate saw.

## 4. Transport: the bus is the one fabric

### 4.1 Publishing

- **Where:** `BusDb::emit(&BusEmit)` (`src/bus.rs:288`) on a connection owned by the publishing thread (`:239-241`). `domain` = `CORE_DOMAIN` (`wicked-core`, `src/cli_runner.rs:73`); `subdomain` = `core.team`.
- **Publishers and their threads.** Off-actor threads publish directly on their own connection: the worker thread (`run_unit_and_judge_with_roster`, `src/cli_runner.rs:748`), the supervisor thread (S2's, re-homed), the ACP carrier's turn loop (`src/acp_runner.rs:3910`, which already writes stdin under a lock inside the turn), and the council thread (`src/actor.rs:2525-2544`). The actor thread **never opens the bus**: the four actor-originated events (`path.started`, `plan.accepted`, `path.ended`, `gate.decided`) go through a `TeamPublisher` — one thread, one connection, one `mpsc::Sender<BusEmit>` the actor holds — so a busy bus can never stall the single writer (`src/bus.rs:27-33` applied to writes). Fire-and-forget from the actor; the publisher logs a failed emit and drops it (the fold snapshot, not the bus, is the gate's record, §4.4).
- **Idempotency key.** Every team event pins `deterministic_key(&["team", <event_type>, <run_id>, <entity ids…>])` (`src/bus.rs:545`), so a re-publish after a retry, a restart or a duplicate delivery resolves to the existing row (`:319-345`). The key parts per event are in §6. **The algorithm, exactly as `deterministic_key` computes it (`src/bus.rs:545-553`):** SHA-256 over the concatenation, for **every** part in order, of the part's UTF-8 bytes followed by one `0x00` byte (so the last part is NUL-terminated too; this is not a `\0`-join); keep the first **16** bytes of the digest; encode each as two **lowercase** hex digits (`{:02x}`), giving a 32-character key. Crew's JS publishers (§7) must reproduce it byte for byte: `const h = createHash('sha256'); for (const p of parts) { h.update(Buffer.from(p, 'utf8')); h.update(Buffer.from([0])); } key = h.digest().subarray(0, 16).toString('hex');`.
  - **Test vector.** parts `["team", "wicked.team.finding.raised", "run-1", "f-3fa9c2e1d0b4a7e6"]` → `f8289d402fc42823fd875fcf4456bd8f`. parts `["team", "wicked.team.path.started", "run-1"]` → `25ea4932f42b6e22f1aacd16bc3dcdd9`. A `\0`-join **without** the trailing NUL gives `fed61d5909428bb7aec506ad09dc86d1` for the first vector, which is wrong, and that mismatch is the failure the vector exists to catch. The values were computed by a byte-for-byte transcription of `deterministic_key` (no build was run for this draft). T1 pins them in a Rust unit test against `crate::bus::deterministic_key`, and in a crew test against the JS helper, so the two implementations cannot drift.
- **One db.** The engine needs a bus path whether or not exec mediation is on. Crew hands the engine **its own cross-product bus** (the sidecar `resolveCrewBus` already resolves, `cli/index.ts:129-137`) as `WICKED_BUS_DB` on every boot, not only under `--engine-exec` (`adapter.ts:1183-1191`). Exec mediation keeps its `WICKED_BUS_EXEC` switch; it now mediates over the same file. A daemon with no usable bus runs **un-teamed and says so**: nothing can be published, so the worker thread builds the unit's snapshot locally (not published) with `transport:"none"`, an empty ledger and an empty transcript, and the gate renders it as un-teamed (§8.8). It never silently falls back to an in-process channel.

### 4.2 Subscribing

Every consumer is a named cursor with a filter, and every consumer is idempotent on the event's identity (the payload's ids, never the row id) because delivery is at-least-once (`reqs/SPEC.md:751-798`).

| Consumer | Where | Cursor name | Filter | Floor on start | Idempotency |
|---|---|---|---|---|---|
| Team supervisor (monitors' host) | core thread (S2's, re-homed) | `team-supervisor-<actor_process_gen>` (`consumer_name` pattern, `src/cli_runner.rs:1407`) | `wicked.team.**` | `tail_event_id()` at spawn (`src/bus.rs:348`): a restart starts at "now"; the attempts it was tracking are re-attached from `step.claimed` **replayed for live runs** (§8.5) | per `(run, ord, attempt)` state keyed by `finding_id`, `step_id`, `checkpoint.seq` |
| Steer point (S3) | ACP carrier, per attempt | none persisted — the attempt's floor is the `event_id` of its own `step.claimed` | `wicked.team.finding.raised` | the attempt's `step.claimed` id | a per-attempt `delivered: BTreeSet<finding_id>` (replaces the mailbox `records`) |
| Step-boundary injector | worker thread, before `run_unit_streaming` (`src/cli_runner.rs:761`) | none — one bounded read per step | `wicked.team.**` for `(run)` | the run's `path.started` id | renders only items for which no `advice.delivered` row with `outcome:"injected"` exists yet, on **any** `channel` (`acp_steering` or `boundary`) |
| Gate wait (worker thread) | `src/cli_runner.rs:761` after the run returns | none — the `bus_request_agent_verdict` pattern (`:417-440`) | `wicked.team.gate.opened` | the attempt's `step.completed` id | matches `run_id, ord, attempt` |
| Crew `/ws` relay | daemon | plugin `wicked-crew-team-relay` (the `ws-relay.ts:47-55` pattern) | `wicked.team.**` | `cursor_init: "latest"`, `maxRetries: 0` | none needed: it broadcasts, never re-emits |
| Crew read route | `GET /api/v1/runs/:id/team` | none — `pollResolve(liveDb, archDir, {lastEventId: 0, filter})` (`lib/query.js:41`), paged, filtered by `payload.run_id` | `wicked.team.**` | 0 | dedup by `event_id` across tiers (`lib/query.js:184`) |
| Studio | `/ws` `teamEvent` frames + the read route on late join | — | — | — | folds by the payload ids |

### 4.3 Ordering

The bus `event_id` is the order (`ORDER BY event_id ASC`, `src/bus.rs:497-499`). No per-run team sequence is minted. Causal references are carried **in the payload** (`re: <event_type>#<entity id>`, e.g. `re: "finding.raised#f-3fa9…"`), not in the bus causality columns: core's `BusEmit` writes only the base columns (`src/bus.rs:22-24`, `:299-303`) and `withContext` is JS-only (`lib/index.js:39`). A consumer that needs "was X before Y" compares `event_id`s it has read; a consumer that needs "X happened" checks the entity id. Because publishers write in program order on one connection each, the events one thread publishes for one attempt are ordered; cross-thread order (a checkpoint from the carrier vs. a finding from the supervisor) is whatever the bus assigned, which is fine for every consumer in §4.2 — none of them keys on cross-thread order, which is why S2's hold-checkpoints-before-attach buffer (`src/team.rs:1150-1200`) is deleted: `step.claimed` is published by the worker thread **before** `run_unit_streaming` starts the turn, so it always has a lower `event_id` than any checkpoint of that attempt.

### 4.4 Retention, and what the record is

The bus is the live record and the transcript's source for at least the TTL window (72 h visible; rows deleted at 24 h by the v1 sweep, moved to warm buckets by the v2 sweep, kept forever when no sweep runs — `reqs/SPEC.md:804-818`, `lib/sweep.js:53`, `lib/sweep-v2.js:2-3`). A per-event TTL cannot extend a row's life (`dedup_expires_at` is config-level, `src/bus.rs:294`; the `retention` column is design-only). So the **durable evidence** of what the gate saw is the fold's snapshot, persisted exactly as DES-001 §6.1 (`WorkUnit.team_ledger`, `UnitEvidence.team`, the evidence bundle at `routes.ts:2879`), with the difference that the snapshot now carries the attempt's **transcript** (every team event of the attempt, capped, §6 `gate.opened`). The ledger IS the stream: the snapshot is a deterministic fold of the stream (`team::fold(events) -> TeamLedger`, a pure function tested on fixtures), and the studio transcript view reads the bus while it can and the snapshot afterwards.

### 4.5 The relay to `/ws`

One relay, the `interactiveEvent` pattern verbatim (`ws-relay.ts:148-162`): `bus.subscribe({plugin:"wicked-crew-team-relay", filter:"wicked.team.**", cursor_init:"latest", maxRetries:0, handler: e => broadcast({type:"teamEvent", event: e})})`. Frames carry `project_id` when the run's membership files it, as every other frame does (`server.ts:1233-1235`). No `CoreEvent` variant is added for any team event.

### 4.6 What stays on the engine fan-out, and why that is not "two mechanisms"

The `CoreEvent` fan-out (`src/event_log.rs:492`) remains the engine's telemetry: `unitDispatched`, `unitDistributed`, `gateEvaluated`, `gateDecided`, `awaitingHuman`, the council's ballot-level events (`CouncilConvened`/`Deliberated`/`SeatFailed`/`Voted`), chat, terminals. The rule is **one mechanism per concern**: engine state changes are CoreEvents; team communication is `wicked.team.*`. No concern rides both. The six DES-001 team CoreEvents are deleted, not mirrored (§10).

## 5. Grammar and envelope

- **Type:** `wicked.team.<noun>.<past-tense-verb>` — four segments, lowercase (`reqs/SPEC.md:379-393`). The domain segment `team` is a functional domain like `gate` in `wicked.gate.eval.requested` (`src/cli_runner.rs:88`); the `domain` column (publisher identity) is `wicked-core` for engine-published events and `wicked-crew` for the human-originated ones (§7).
- **Envelope (every payload):**

```jsonc
{
  "run_id": "<run>",           // always
  "ord": 3,                    // the unit; null on run-level events
  "attempt": 1,                // null on run-level events
  "by": "claude#1",            // seat instance | "engine" | "human" | "council:<task_id>"
  "at": 1758600000000,         // publisher's epoch ms (the bus stamps emitted_at too)
  "re": null                   // causal reference: "<noun>.<verb>#<entity id>" or null
  // …event fields
}
```

- **Caps:** strings capped as stated per event, at a UTF-8 boundary (`cap_utf8`, `src/team.rs:131`). Payloads stay far under the 1 MB bus cap (`reqs/SPEC.md:371`); the one large event (`gate.opened`) caps its transcript at 256 KB and says so.
- **Catalog:** every type below is added to `crates/wicked-governance/seed/event-catalog-annotations.json` (the generator fails on an unknown key, `seed/README.md:23-24`), and `gen_event_catalog.py --check` stays green.

## 6. The events

Twenty types. `Key` is the idempotency key's parts after `["team", <type>, run_id]`. `Pub` is who publishes; `Sub` who consumes (S = supervisor, C = ACP carrier steer point, I = step-boundary injector, G = gate wait / fold, R = crew relay + read route, U = studio).

| # | Type | Pub | Sub | Key parts | When |
|---|---|---|---|---|---|
| 1 | `wicked.team.path.started` | engine (actor → publisher) | S, I, R, U | — | launch admitted; PA seat known |
| 2 | `wicked.team.path.scored` | worker thread (S4) | S, R, U | `basis`, `score_seq` | at plan time (`basis:"intent"`) and whenever a settled-diff re-score changes the plan (`basis:"diff"`) |
| 3 | `wicked.team.plan.proposed` | worker thread (PA's plan step) / crew (human edit) | engine, R, U | `plan_rev` | the PA's plan output parsed; a human amendment |
| 4 | `wicked.team.plan.accepted` | engine | S, R, U | `plan_rev` | registered as the run's `WorkflowDef` |
| 5 | `wicked.team.member.joined` | S | R, U | `member_id` | a monitor session opened (or failed) |
| 6 | `wicked.team.member.left` | S | R, U | `member_id` | budget exhausted, failed, closed |
| 7 | `wicked.team.step.claimed` | worker thread (PA) / S (member) | S, C, I, R, U | `step_id`, `attempt`, `by` | before the step's turn starts |
| 8 | `wicked.team.checkpoint.reached` | C | S, R, U | `attempt`, `seq` | terminal `tool_call_update` of a teamed unit |
| 9 | `wicked.team.finding.raised` | S | C, I, R, U | `finding_id` | confirmed, above-bar, first-seen finding |
| 10 | `wicked.team.advice.delivered` | C (mid-turn) / I (boundary) / S (sweep) | S, R, U | `finding_id`, `channel` | a finding reached (or could not reach) the PA |
| 11 | `wicked.team.advice.answered` | worker thread (parsed `ADVICE` lines) | S, R, U | `finding_id`, `attempt` | end of the PA's turn |
| 12 | `wicked.team.help.requested` | worker thread (parsed `HELP` line) / C (mid-turn, ACP) | S, R, U | `help_id` | the PA asks the team |
| 13 | `wicked.team.help.answered` | S (a member's turn) / crew (human) | I, C, R, U | `help_id`, `by` | a member answers |
| 14 | `wicked.team.step.completed` | worker thread / S (member) | S, G, R, U | `step_id`, `attempt`, `by` | the step's turn returned |
| 15 | `wicked.team.step.reviewed` | worker thread (PA's `STEP` line) | S, R, U | `step_id`, `attempt` | the PA reviewed a member's step (Decision 3) |
| 16 | `wicked.team.finding.settled` | S (hold round) | G, R, U | `finding_id`, `attempt` | hold / withdraw / superseded |
| 17 | `wicked.team.council.called` | S | R, U | `finding_id`, `attempt` | an unresolved HIGH (DES-001 §6.3) |
| 18 | `wicked.team.council.ruled` | council thread | S, I, R, U | `finding_id`, `attempt` | `convene_decision` returned |
| 19 | `wicked.team.gate.opened` | S | G, R, U | `attempt` | the final pass folded |
| 20 | `wicked.team.gate.decided` | engine / crew (human) | R, U | `attempt`, `decision` | fold decided, or a human resolved a `team_dispute` gate |
| — | `wicked.team.path.ended` | engine | S, R, U | — | the run reached a terminal state |

(`path.ended` is the twenty-first row; it brackets the stream so the supervisor and the read route know a run is closed. It replaces `TeamCmd::RunComplete`.)

Exact payloads (envelope fields omitted after the first):

```jsonc
// 1 — engine, at launch admission (LaunchSpec.primary resolved, §8.1)
{"run_id":"r1","ord":null,"attempt":null,"by":"engine","at":…,"re":null,
 "cli":"claude#1",                 // the PA seat instance
 "selection":"chosen",             // "chosen" | "random"
 "roster":["claude#1","claude#2","codex"],
 "request":"…",                    // the problem text, ≤8 KB
 "workflow":"feature"}             // the launch's workflow id or null (the plan may replace it, §8.3)

// 2 — worker thread; S4 assess() on the predicted touch set, then on each settled diff
{"basis":"intent",                 // "intent" | "diff"
 "score_seq":1,                    // per run, monotonic
 "score":70,"deterministic":70,"reasons":["destructive ⇒ floor 70","reach 21-100 dependents: +60"],
 "model":null,                     // {"add":10,"rationale":"…"} or null
 "signals":{"changed_symbols":4,"dependents":37,"products":1,"contract_change":false,"test_gap":0.5,
            "critical":false,"destructive":true,"truncated":false},   // null when the graph was unusable
 "plan":{"monitors":3,"depth":"deep","post_hoc_reviewer":true,"post_hoc_other_cli":true},
 "tree":null}                      // the T_k the diff was taken at; null for "intent"

// 3 — worker thread (PA plan step) or crew (human edit); the plan IS a WorkflowDef composition (Decision 1)
{"plan_rev":1,
 "monitors":{"from_score":3,"asked":1,"target":3},   // target = min(max(from_score, asked), TeamLimits.max_monitors)
 "steps":[{"id":"design","kind":"recon","role":"neutral","gate":"auto","depends_on":[],"skill_ref":null,"validator_pin":null,"executes_code":false,"instructions":"…"},   // owner omitted = "pa"
          {"id":"build","kind":"build","role":"creator","gate":"auto","depends_on":["design"],"executes_code":true,"validator_pin":"e2e7af1db9e48454","instructions":"…"},   // a pin id the library already carries (workflows/feature.json)
          {"id":"test-plan","kind":"recon","role":"neutral","gate":"auto","depends_on":["design"],"owner":"team"},   // owner: "pa" (default) | "team"
          {"id":"review","kind":"review","role":"evaluator","gate":{"human_confirm_if":"verdict_not_pass"},"depends_on":["build"]},   // GateSpec is externally tagged (workflows/feature.json:8-11)
          {"id":"test","kind":"test","role":"evaluator","gate":"auto","depends_on":["build"]}],
 "asks":["a codex seat to cross-check the migration"],   // free text, ≤2 KB each, ≤8
 "touch":["src/retire.ts","src/coverage.ts"],            // predicted touch set, ≤64 paths
 "rationale":"…"}                                        // ≤4 KB

// 4 — engine, after Core::register_workflow validated it
{"plan_rev":1,"workflow_id":"r1:plan-1","by":"engine",   // "human" when a human accepted an edit (Decision 2 alternative)
 "blocking":false,
 "refused":null}                   // or {"reason":"…"} when validation refused the proposal (the run keeps the previous plan)

// 5 / 6 — supervisor
{"member_id":"m1","seat":"claude#2","role":"monitor","status":"joined",   // role: "monitor" (the only value today); status: "joined" | "failed"
 "reason":"path.scored#2 monitors=3","error":null}
{"member_id":"m1","seat":"claude#2","status":"budget_exhausted",         // "completed" | "budget_exhausted" | "failed" | "timed_out"
 "batches":10,"error":null}

// 7 — worker thread (PA) or supervisor (a member taking a plan step)
{"ord":3,"attempt":1,"by":"claude#1",
 "step_id":"build","role":"creator","kind":"build","phase":"build",
 "criterion":"…",                  // ≤2 KB
 "baseline_tree":"<tree id>",      // null when unbound (then: not monitored, disclosed)
 "repo":{"workdir":"…","git_dir":"…"},
 "code_graph_db":"…"}              // what AttachCtx carried (S2 src/team.rs:625-645)

// 8 — ACP carrier, only for a unit with a team context (unchanged from DES-001 §7 unitCheckpoint)
{"ord":3,"attempt":1,"by":"claude#1","seq":17,"tool_call_id":"toolu_…","kind":"edit",
 "title":"Edit src/retire.ts","status":"completed","paths":["src/retire.ts"]}

// 9 — supervisor (DES-001 §4.6 unchanged: parse → bar → confirm → dedup)
{"ord":3,"attempt":1,"by":"claude#2","re":"checkpoint.reached#17",
 "finding_id":"f-3fa9c2e1d0b4a7e6","member_id":"m1","line_key":"l-9c0e4b7a1d2f3e58",
 "anchor":"retire","anchor_source":"graph","severity":"high","path":"src/retire.ts","line":41,
 "evidence":"fetchCoverage(scope).then(setCount)","claim":"…","suggestion":null,
 "tree":"<T_k>","in_diff":true,"corroborated_by":[]}

// 10 — the carrier (mid-turn), the injector (next step boundary) or the supervisor's sweep
{"ord":3,"attempt":1,"by":"engine","re":"finding.raised#f-3fa9c2e1d0b4a7e6",
 "finding_ids":["f-3fa9c2e1d0b4a7e6"],
 "channel":"acp_steering",         // "acp_steering" | "boundary" | "none"
 "outcome":"injected",             // "injected" | "turn_ended" | "refused" | "not_delivered"
 "detail":null}

// 11 — worker thread, from the PA's `ADVICE <id>: ACCEPT|DECLINE — <reason>` lines (S3 parser, src/team.rs:1989)
{"ord":3,"attempt":1,"by":"claude#1","re":"finding.raised#f-3fa9c2e1d0b4a7e6",
 "finding_id":"f-3fa9c2e1d0b4a7e6","disposition":"declined","reason":"campaign.rs:325 documents the exclusion"}   // "accepted" | "declined"

// 12 / 13 — the PA asks; a member (or a human through crew) answers
{"ord":3,"attempt":1,"by":"claude#1","help_id":"h-1a2b…","question":"…","context":"…"}   // ≤4 KB each
{"ord":3,"attempt":1,"by":"claude#2","re":"help.requested#h-1a2b…","help_id":"h-1a2b…","answer":"…","evidence":["src/x.rs:41"]}

// 14 — the worker thread after run_unit_streaming returns; a member's step from the supervisor
{"ord":3,"attempt":1,"by":"claude#1","step_id":"build","status":"ok",   // "ok" | "failed" | "cancelled": the wire spelling of StepStatus (src/workflow.rs:175, which has no serde derive)
 "tree":"<T_final>","output_bytes":12345,"output_ref":"unit:r1:3:1"}    // where get_work_output finds it

// 15 — the PA's `STEP <id>: ACCEPT|REJECT — <reason>` line at its next turn (Decision 3)
{"ord":4,"attempt":1,"by":"claude#1","re":"step.completed#test-plan","step_id":"test-plan",
 "verdict":"accepted","reason":"…"}   // "accepted" | "rejected"

// 16 — supervisor, hold round (DES-001 §4.7 step 5); also "superseded" from re-confirmation
{"ord":3,"attempt":1,"by":"claude#2","re":"advice.answered#f-3fa9…","finding_id":"f-3fa9…",
 "status":"held",                  // "held" | "withdrawn" | "superseded"
 "reason":"no reply (counted as hold)","final_line":43}

// 17 — supervisor, one per unresolved HIGH (DES-001 §6.3 input, verbatim)
{"ord":3,"attempt":1,"by":"engine","re":"finding.settled#f-3fa9…","finding_id":"f-3fa9…",
 "trigger":"unresolved_high",      // the only trigger today
 "question":"…","positions":[{"by":"worker claude#1","position":"YES — the refusal stands","reason":"…"},
                             {"by":"monitor claude#2","position":"NO — the finding stands","reason":"…"}],
 "evidence":"…",                   // ≤16 KB: finding, T_final hunk, criterion, tree id
 "excluded_seats":["claude#1","claude#2"],
 "transcript":[1201,1207,1215,1220]}   // event_ids of raised/delivered/answered/settled for this finding

// 18 — the council thread (DecisionVerdict, src/decision.rs:292-310)
{"ord":3,"attempt":1,"by":"council:<task_id>","re":"council.called#f-3fa9…","finding_id":"f-3fa9…",
 "verdict":"no",                   // "yes" | "no" | "no_verdict"
 "reason":null,                    // no_verdict: "no_quorum" | "seats_benched" | "error" | "timeout" | "cap"
 "agreement_pct":67,"dissent":["…"],"seats":["codex","pi"],"returned":3,"seated":3}

// 19 — supervisor, once per attempt: the fold of this attempt's stream (DES-001 §7 teamLedger + transcript)
{"ord":3,"attempt":1,"by":"engine","final_pass":"completed",   // "completed" | "timed_out" | "skipped"
 "ledger":{ /* TeamLedger exactly as DES-001 §7: monitors[], findings[] (status, delivery, monitor_reply, dispute), rejected{}, team_pause */ },
 "transport":"bus",               // "bus" | "none" (the local no-bus snapshot, §4.1)
 "transcript":{"from_event_id":1180,"to_event_id":1290,"count":31,
               "events":[ /* the attempt's wicked.team.* rows, event_id-ordered, ≤256 KB; "truncated":true past the cap */ ]}}

// 20 — the fold (engine) or a human resolving a team_dispute gate (crew)
{"ord":3,"attempt":1,"by":"engine","re":"gate.opened#1",
 "decision":"paused",              // "allow" | "deny" | "paused" | "human_approved" | "human_amended" | "human_rejected"
 "combined":true,"team_pause":true,"unresolved":["f-3fa9…"]}

// — engine, at finalize/fail/cancel
{"run_id":"r1","ord":null,"attempt":null,"by":"engine","status":"completed"}   // "completed" | "failed" | "cancelled"
```

## 7. Who reads what

| Consumer | Reads | Produces |
|---|---|---|
| Supervisor (core) | `path.started` (arm a run), `plan.accepted` (member targets), `step.claimed` (track an attempt; open monitors lazily), `checkpoint.reached` (batch trigger, DES-001 §4.3), `advice.answered` (dispositions into the next batch prompt), `help.requested` (a member turn), `step.completed` (final pass), `council.ruled` (into the ledger), `path.ended` (forget) | `member.joined/left`, `finding.raised`, `advice.delivered{channel:"none"}` (sweep), `help.answered`, `step.claimed/completed` for a member's step, `finding.settled`, `council.called`, `gate.opened` |
| ACP carrier steer point (core) | `finding.raised{severity:"high"}` for its attempt, `help.answered` for its open asks | `checkpoint.reached`, `advice.delivered{channel:"acp_steering"}`, `help.requested` (mid-turn `HELP` line) |
| Step-boundary injector (core, worker thread) | `finding.raised` with no `advice.delivered{outcome:"injected"}` on any channel, `help.answered`, `council.ruled`, a member's `step.completed` awaiting review | `advice.delivered{channel:"boundary", outcome:"injected"}` |
| Gate wait + fold (core, worker thread) | `gate.opened` for its attempt | `UnitEvidence.team` (the snapshot), then the engine gate as DES-001 §6.2 |
| Engine (actor) | `plan.proposed` (validate → register) | `path.started`, `plan.accepted`, `gate.decided`, `path.ended` |
| Crew | the relay; the read route | `plan.proposed{by:"human"}` (plan edit, Decision 2), `help.answered{by:"human"}`, `gate.decided{by:"human"}` — all through crew's existing JS `bus.emit` with the deterministic key (`projects/events.ts:92-108`) |
| Studio | `teamEvent` frames; `GET /runs/:id/team` on late join | POSTs to crew only |

## 8. Lifecycle, step by step

### 8.1 Start a path; choose the CLI (operator steps 1–3)

- `LaunchSpec` gains `primary: Option<String>` (`src/lib.rs:190-216`), a seat-instance key from `clis`. `None` ⇒ the engine picks uniformly at random among the eligible seats (the `seat_candidates` set S5 computes, `src/distribute.rs:11`) and records `selection:"random"`. Crew's `POST /runs` body gains `primary` (`routes.ts:222`); studio's launch form gains a picker with a **Random** option.
- The PA seat is the run's **creator seat**: `teamed_distribution` (`src/distribute.rs:174`) pins `winner` to the PA for every creator-role unit; `enforce_evaluator_distinct` (`:387`, `:547`) runs unchanged, so evaluator units land on a seat distinct from the PA, and the plan is refused (`NoEligibleSeat`) when none exists — evaluator ≠ creator holds per step, structurally, exactly as today.
- The actor publishes `path.started` through the `TeamPublisher` when the launch is admitted (the same moment `unitDistributed` fires for ord 0).

### 8.2 The PA scores the path (operator step 4)

- The path's first step is the **plan step**: a Recon-kind unit on the PA seat whose required deliverable is the plan (§8.3). Its prompt asks for the predicted touch set (`touch[]`) and the plan.
- Scoring is S4, not the model: the engine builds `ChangeSignals` from the predicted touch set (a new `signals_from_paths(&[path]) -> ChangeSignals`, beside `signals_from_diff`, S4 `src/review_scale.rs:635`) and calls `assess(&signals, Graph::Ready{store, base_commit}, model_hook)` (`:563`) → `path.scored{basis:"intent"}`. No graph or a stale graph scores 100 (fail closed, S4's rule). The optional model hook (`:145`) may add 0/10/20 and never subtracts; the PA's own view of risk enters **only** through that hook and through `asks`.
- Re-scoring: every settled-diff batch (DES-001 §4.3) re-runs `assess` on the real diff; when `plan.monitors` grows, the supervisor publishes `path.scored{basis:"diff"}` and summons the extra members. The count only grows within a run.

### 8.3 The workplan (operator step 5)

The PA's plan output is a fenced JSON block matching `plan.proposed`. The worker thread parses it, publishes `plan.proposed`, and the actor validates it.

**[OPERATOR DECISION 1] — the workplan COMPOSES from the step library (default) vs. replaces workflow JSON.**

- **Default — compose.** `steps[]` is a `WorkflowDef` composition: each step is a `PhaseDef` chosen from the **phase library** (the phases of the registered workflow defs, keyed by `id`, plus their `kind`, `gate_type`, `gate`, `role`, `skill_ref`, `validator_pin`, `depends_on`, `executes_code`; `src/workflow.rs:640-708`). The PA may pick, order, drop, and add `instructions` to library phases, and may set each step's `owner` (Decision 3); it may **not** mint a validator pin, a gate, a floor or a skill ref that the library does not carry — `deny_unknown_fields` (`:640`) and the registry's validation (`src/actor.rs:2545-2556`) refuse the rest. The accepted plan is registered as `WorkflowDef { id: "<run>:plan-<rev>", phases, base_skill_ref }` through `Core::register_workflow` (`src/lib.rs:1296`) and planned by `plan_from_def` (`src/plan.rs:94`) for the remaining steps. **The one schema addition is `owner`.** `PhaseDef` is `#[serde(deny_unknown_fields)]` (`src/workflow.rs:640-641`), so a plan step with an unknown key is refused. The field is therefore added to `PhaseDef` itself: `#[serde(default, skip_serializing_if = "StepOwner::is_pa")] pub owner: StepOwner`, where `enum StepOwner { #[default] Pa, Team }` is `rename_all = "snake_case"`. The shipped workflow JSON stays byte-identical, the same additive pattern `base_skill_ref` uses (`src/workflow.rs:1780-1790`). The planning path carries it the way it carries `role`: `plan_from_def` (`src/plan.rs:94`, which copies `phase.role` at `:155`) copies `phase.owner` into a new `WorkUnit.owner` (`#[serde(default)]`, beside `role` at `src/domain.rs:451`). The PA pin in `teamed_distribution` (§8.1) skips a unit whose `owner` is `Team`, and the supervisor assigns that unit's seat (§8.5). Gates, floors, pins, base skills, `depends_on` handoffs (`src/actor.rs:6601-6620`) and the evaluator≠creator fence keep working because they are per-phase data the engine already enforces. A refused proposal publishes `plan.accepted{refused:{reason}}` and the run continues on the launch's workflow.
- **Alternative — replace.** `steps[]` is free-form; the engine synthesizes `WorkUnit`s directly from it, with no pins, gate ladder or library validation. Rejected as the default because it deletes the deterministic floor for any step the PA forgets to gate, and because every shipped workflow's floors would have to be re-expressed as PA output to survive.

### 8.4 The plan is accepted (operator step 5, cont.)

**[OPERATOR DECISION 2] — the plan is visible and editable in studio but NON-blocking (default) vs. a human gate.**

- **Default — non-blocking.** The actor publishes `plan.accepted{by:"engine", blocking:false}` immediately after validation and dispatches step 1. Studio shows the plan (a *Plan* card on the run page, from `teamEvent` frames / the read route) with **Edit** and **Stop**. Edit = `POST /api/v1/runs/:id/plan` → crew publishes `plan.proposed{by:"human", plan_rev:n+1}`; the engine validates, registers, re-plans the **remaining** steps (units not yet dispatched) and publishes `plan.accepted{plan_rev:n+1}`; dispatched units are untouched. Stop = the existing cancel. A run launched with `human_confirm` policies pauses where the *phases* say (`GateSpec`, `src/workflow.rs:531`), not on the plan.
- **Alternative — human gate.** After validation the actor calls `pause_for_human(gate_kind:"plan")` (`src/actor.rs:6022`) before step 1; `plan.accepted{by:"human"}` is published from `confirm_gate` (`:7736`), which gains a `plan` arm (approve = accept; approve+amend = `plan.proposed` rev n+1 then accept; reject = cancel). Every run would then need a human before its first step, including the autonomous ones.

### 8.5 Members work, advise, support (operator steps 6–7)

- **Members** are the monitors S2 built (read-only ACP sessions on distinct seat instances, `MonitorHost`, `src/team.rs:659-670`; opened lazily at the first due batch, DES-001 §4.1). Their **input** is the stream: the supervisor's cursor (§4.2) feeds each member the settled diffs at checkpoints exactly as DES-001 §4.3–§4.5, plus every `advice.answered`, `help.requested` and `council.ruled` since its last turn, so a member never re-raises what the PA answered and sees what the PA asked.
- **Guidance** = `finding.raised` (the DES-001 contract, unchanged: parse, bar, mechanical confirmation at `path:line` in the snapshot tree, dedup by `finding_id`, `corroborated_by`).
- **Support** = `help.answered`. The PA asks with one output line `HELP: <question>` (parsed at the step boundary, the `ADVICE` parser family, `src/team.rs:1989`), or mid-turn on ACP via the same line in a `session/update` agent message chunk. The supervisor gives one member turn per open ask (a bounded budget, `TeamLimits`) and publishes the answer. The PA's questions to a **human** stay with S1 (`AskUserQuestion` → `ElicitationCreated`, `src/event.rs:1145`): a member never becomes the decider on the PA's uncertainty.
- **Work** = a member takes a plan step. A plan step carries `owner: "pa" | "team"` (§8.3: `PhaseDef.owner`, default `"pa"`), and the planner may set `"team"` on a test-plan, a design review or a spike; for a `team` step the supervisor assigns it to a member whose seat the step's skills admit and publishes `step.claimed{by:<member>}`, runs it as a normal unit on that seat (the unit's `assigned_cli` is the member; the member's session for that step is a **writable** unit session, not its read-only monitor session), and publishes `step.completed{by:<member>}`.

**[OPERATOR DECISION 3] — when a member takes a step, the PA OWNS the outcome (default) vs. peer handoff.**

- **Default — PA owns.** A member's `step.completed` does not count until the PA reviews it. At the PA's next step boundary the injector renders the member's output as prior context `[team step — <step_id> by <seat>]` (the `PriorUnitOutput` mechanism, `src/actor.rs:6601-6620`), and the PA must answer with one line `STEP <step_id>: ACCEPT|REJECT — <reason>`, parsed into `step.reviewed`. `accepted` ⇒ the step's output is the unit's work output and the run advances; `rejected` ⇒ the step is re-planned (the PA may take it itself, or the supervisor reassigns it once) and the rejection reason rides as the member's amendment. The engine gate still judges the member's step as a unit whose creator is the **member** (`work_author = assigned_cli`, `src/cli_runner.rs:919`, `:937-942`, `:1020-1025`), with the ledger's authors excluded (DES-001 §6.2); the PA's review is **team evidence**, never the gate. The same `owner` field carries member work under the alternative below; only what `step.completed` counts for differs.
- **Alternative — peer handoff.** `step.completed{by:<member>}` counts on its own; the gate judges it with judge ≠ member; the PA sees it as ordinary prior context with no `STEP` line and no `step.reviewed` event. Simpler, and it removes the one place the PA's "ownership" of the path is visible on the record.

### 8.6 Advice reaches the PA: uniform at the step boundary, mid-turn on Claude ACP

- **Every carrier, at the next step boundary.** Before `run_unit_streaming` (`src/cli_runner.rs:761`) the worker thread reads the run's stream from `path.started` and renders one `[team advice]` prior-context block: every `finding.raised` with no `advice.delivered{outcome:"injected"}` on any channel yet (HIGH and MEDIUM; the block is capped at 8 KB, DES-001 §5.3's text), every `help.answered` for the PA's open asks, every `council.ruled`, and any member `step.completed` awaiting review. It publishes `advice.delivered{channel:"boundary", outcome:"injected"}` for what it rendered (a step boundary always lands: the block is part of the prompt the carrier builds). A finding that did not fit the 8 KB cap is not rendered and gets no row, so it is picked up at the following boundary. This replaces the operator-inject queue's shape for team content (`drain_operator_messages`, `src/acp_runner.rs:6750-6781`) and works for wrapped CLIs with null stdin (`src/execute_wrapped.rs:3468`), PTY and every ACP adapter alike, because a step boundary is a prompt every carrier builds.
- **Claude ACP, additionally mid-turn (S3, unchanged mechanism).** `steer_at_boundary` (`src/acp_runner.rs:4903`) keeps its delivery point and its `_session/steering` request with `idleBehavior:"promptRequired"`; its **source** becomes a poll of `finding.raised{severity:"high"}` for the attempt after the attempt's `step.claimed` id, minus the per-attempt delivered set. It publishes `advice.delivered{channel:"acp_steering", outcome}`. The `SteerMailbox` queue and its `records` are deleted (§10).
- **Authority is unchanged (DES-001 §5.3).** The advice block says ADVISORY; the PA answers `ADVICE <id>: ACCEPT|DECLINE — <evidence>` and may decline; the gate decides.

### 8.7 The council (operator step 8)

- **Trigger:** DES-001 §6.3 as the operator ruled it — every HIGH that is not `accepted`/`withdrawn`/`superseded` (declined, unanswered, or never delivered) and that the authoring member **held** in the hold round (`finding.settled{status:"held"}`, silence counts as held). Nothing else convenes a council; there is no routing council.
- **The call is a team event.** The supervisor publishes `council.called` carrying the DES-001 §6.3 input verbatim (question, the two positions, evidence, excluded seats) **plus** `transcript`: the `event_id`s of the finding's `raised → delivered → answered → settled` events, so the council reads the actual exchange, not a summary. It then calls `Core::convene_decision(DecisionRequest{session_id, ord, question, options:[YES, NO], evidence})` (`src/lib.rs:1283`) with the non-party roster; the ballots' `CouncilConvened/Deliberated/Voted` CoreEvents stay engine telemetry (§4.6).
- **The ruling is a team event.** The council thread publishes `council.ruled` from the `DecisionVerdict` (`src/decision.rs:292-310`): `winner` → `yes`/`no`; `None` → `no_verdict` with `reason` per DES-001 §6.3.
- **Three-way, on the record.** PA → `advice.answered` (its position, with evidence); team → `finding.settled{status:"held", reason}`; council → `council.ruled{dissent[]}`; PA again → at its next boundary the injector renders the ruling (§8.6), so a rework attempt argues against the ruling, not against a monitor. The whole exchange is the finding's transcript, readable by `re` chains.
- **Continue or pause (DES-001 §6.7, unchanged):** YES → autonomous continue; NO or no verdict → `team_pause` → `awaitingHuman{gate_kind:"team_dispute"}`; the human answers through `POST /api/v1/runs/:id/gate` (`routes.ts:2943`), and crew publishes `gate.decided{by:"human", decision:"human_approved"|"human_amended"|"human_rejected"}`.

### 8.8 The gate consumes the stream (operator step 9)

- **Final pass and fold, on the supervisor.** `step.completed{by: PA}` triggers DES-001 §4.7 steps 1–6 (settled review batch, re-confirmation, `ADVICE` dispositions are already on the stream as `advice.answered`, hold round → `finding.settled`, councils → `council.called/ruled`). Then `team::fold(&events_of_attempt) -> TeamLedger`: a pure function over the attempt's `wicked.team.*` rows that produces the DES-001 §7 ledger (finding status from `answered`/`settled`, `delivery` from `advice.delivered`, `dispute` from `council.ruled`, `team_pause` from the §6.7 condition). The supervisor publishes `gate.opened{ledger, transcript}`.
- **The worker thread waits, bounded.** After `run_unit_streaming` returns Ok it publishes `step.completed`, then polls for `gate.opened` for its `(run, ord, attempt)` from that `event_id`, exactly the `bus_request_agent_verdict` loop (`src/cli_runner.rs:417-445`), for at most `FINAL_PASS_BUDGET`. On timeout it synthesizes DES-001 §4.7's fail-closed ledger (`final_pass:"timed_out"`, every still-unaccepted HIGH held with `dispute:{no_verdict, timeout}` ⇒ `team_pause`) and publishes `gate.opened{final_pass:"timed_out"}` itself, keyed on the same parts, so the supervisor's late one dedups. The snapshot goes into `UnitEvidence.team` (`src/workflow.rs:265`, additive) and from there to the judge's WORK fence, the evaluator's prior context and the rework amendment — DES-001 §6.2 unchanged, with `render_for_gate` now also rendering a compact transcript (one line per event, ≤16 KB) so **reviewers have the outcomes and the comms**.
- **A failed step** (`status != ok`) skips the final pass (`final_pass:"skipped"`), as the guard's first look does (`src/cli_runner.rs:876`).
- **The decision is a team event.** The fold's outcome (`src/pipeline.rs:1519-1556`, plus the §6.7 `team_pause` arm) is published as `gate.decided` through the `TeamPublisher`.
- **Human gate.** `GET /api/v1/runs/:id/team` returns `{run_id, units:[{ord, attempt, ledger, transcript}]}` folded from the bus (`pollResolve`, `lib/query.js:41`) and, when the bus no longer has the rows, from the persisted snapshots on the unit records; `POST /api/v1/runs/:id/gate` is unchanged.

### 8.9 Studio

- **Run feed.** `NarratorFeed` (`src/components/NarratorFeed.tsx`) renders `teamEvent` frames as narrator lines: joined/left, finding (severity, `path:line`, claim), delivered (channel/outcome), answered (disposition), help asked/answered, step claimed/completed/reviewed, council called/ruled, gate opened/decided. The fold is the `interactiveEvent` pattern (`src/store/runtime.ts:219-230`): read `event_type`/`payload` defensively, drop what does not parse.
- **Plan card.** `plan.proposed`/`plan.accepted` → a card with steps, monitors target, asks; Edit (Decision 2 default) and Stop.
- **Gate panel.** `SteeringGate` (`src/components/SteeringGate.tsx:85`, buttons at `:302`) gains *Team findings* **and** *Team comms*: the ledger rows of DES-001 §6.5 plus the attempt's transcript, grouped by finding (`re` chains), from `GET /runs/:id/team`. `VerdictDetail` (`src/components/VerdictDetail.tsx:90`) shows `final_pass`, `rejected` counters and `transport` beside `gateEvaluated`, so a silent team reads as silent, and an un-teamed run (`transport:"none"`) reads as un-teamed.
- **Late join.** `api.runEvents` (`src/api/client.ts:181-185`) stays the engine trail; a new `api.runTeam(id)` backfills the team stream.

## 9. Authority model

- Advice is never binding: `combine_verdict` (`src/validator.rs:2263`) reads no team event; the fold's inputs are byte-identical with and without a team (DES-001 §6.6).
- The PA is the run's creator seat; evaluator ≠ creator is preserved per step by the existing fence (`src/distribute.rs:387`) and the judge's exclusions (`src/cli_runner.rs:937-942`, `:1020-1025`; ledger authors per DES-001 §6.2). A member that raised a finding never judges it; a member that took a step never judges that step; the PA never judges a member's step at the gate (its `step.reviewed` is evidence).
- The council rules on one question and its ruling chooses only between autonomous continue and a human pause (DES-001 §6.7). It never approves or denies a unit.
- Humans decide through the existing gate. A human's plan edit, help answer and gate decision are team events with `by:"human"`, so the record shows who said what.

## 10. Mapping: existing CoreEvents and direct channels → the stream

| Today | Where | Becomes | Action |
|---|---|---|---|
| `adviceDelivered` CoreEvent | `src/event.rs:1180`, `:2409`; api-types `index.d.ts:1933` | `wicked.team.advice.delivered` | **delete** the variant, its `to_json` arm, the api-types alias (a 0.41.0 removal), the core-ts `.d.ts` entries |
| `workerAdviceResponse` CoreEvent | `src/event.rs:1194`, `:2426`; api-types `:1953` | `wicked.team.advice.answered` | delete, as above |
| `unitCheckpoint`, `monitorAttached`, `monitorFinding` (S2) | `src/event.rs:1210`, `:1225`, `:1241`, `:2446-2494` | `checkpoint.reached`, `member.joined/left`, `finding.raised` | **delete** (landed in #609; no consumer outside core yet — api-types has no alias for them) |
| `teamLedger` (DES-001 §7, S6, not built) | — | `gate.opened` | never built |
| `TeamCmd::{Attach, Finish, RunComplete}`, `TeamHandle`, `runner.install_team`, `StepRunner::team_finish`, the supervisor's `Command::Subscribe` + `Command::EmitEvent` emit closure | `src/team.rs:1437-1470`, `:1498-1510`; `src/lib.rs:573`; `src/workflow.rs:418`; `src/cli_runner.rs:766` | `step.claimed`, `step.completed` + the gate wait, `path.ended`; the supervisor's bus cursor and its own `BusDb` | **delete**; the supervisor becomes `team::spawn_supervisor(host, bus_db_path, limits)` |
| S2's hold-checkpoints-before-attach buffer | `src/team.rs:1150-1200` | bus order (§4.3) | delete |
| `SteerMailbox` (queue + records), `TeamTurn.mailbox` | `src/team.rs:1730-1832`; `src/acp_runner.rs:5552`, `:8128` | a per-attempt poll of `finding.raised` + a delivered set; `advice.delivered` rows | delete the type; keep `steer_at_boundary`, `advice_block`, `steer_params`, `classify_steer_answer`, `parse_advice_lines` (`src/team.rs:1898-2046`) |
| `unitDistributed{routing_method:"teamed"}` | `src/event.rs:128`; api-types `:147`, `:2077` | stays (engine routing telemetry); `path.started.cli` names the PA | keep |
| `CouncilConvened/Deliberated/SeatFailed/Voted` from `convene_decision` | `src/event.rs:172-223`; `src/actor.rs:3432` | stay (ballot telemetry); `council.called`/`ruled` are the team record | keep |
| `awaitingHuman{gate_kind:"team_dispute"}`, `gateEvaluated`, `gateDecided`, `unitDone/Denied` | `src/event.rs:488`, `:299`, `:261`; `src/pipeline.rs:1519-1556` | stay; mirrored to the team record by `gate.decided` | keep |
| Operator inject (`InjectWorkerMessage`, `pending_injects`) | `src/actor.rs:2557`; `src/acp_runner.rs:5548`, `:6750` | unchanged in this document (it is an operator concern, not team comms); candidate for `help.answered{by:"human"}` later (§15) | keep |
| `GET /runs/:id/team` (DES-001 §6.4, not built) | — | built here, reading the bus + snapshots | build |

## 11. Risks → mechanism

| Risk | Mechanism | Where |
|---|---|---|
| **Stream volume** | Only `checkpoint.reached` is frequent, only for teamed units, ≤ ~600 B; member turns are batched at settled checkpoints (≥60 s apart, changed tree only, `MAX_BATCHES`) exactly as DES-001 §4.3/§4.8; the relay is `cursor_init:"latest"`, no retries; the read route pages by 100. | §6 #8, §8.5, §4.5 |
| **Noise** | The severity bar (`low` dropped), mechanical confirmation at `path:line`, dedup by `finding_id` with `corroborated_by`, answered ids never re-raised, `rejected` counters at the gate. | DES-001 §4.6, §6 #9, #19 |
| **Authority** | `combine_verdict` never reads a team event; the council's only power is continue-vs-pause; the PA may decline with evidence; judges exclude every party. | §9 |
| **Duplicates** (at-least-once) | Deterministic keys on every publish; every consumer keyed by entity id; the gate wait dedups its own late `gate.opened`. | §4.1, §4.2, §8.8 |
| **Bus rows expire** before a reviewer looks | The `gate.opened` snapshot (ledger + transcript) is persisted on the unit and in the evidence bundle; the read route falls back to it. | §4.4, §8.8 |
| **No bus at boot** | Crew always hands the engine its sidecar bus; absent → un-teamed and disclosed (`transport:"none"`), never an in-process fallback. | §4.1 |
| **Actor stall on a busy bus** | The actor never opens the bus; four events go through the `TeamPublisher` thread. | §4.1 |
| **Restart mid-attempt** | The supervisor starts at the tail and re-attaches from `step.claimed` of live runs (§4.2); findings already on the bus survive the restart and are folded at the gate (DES-001 §13's lost-ledger note is closed). | §4.2, §8.8 |
| **A member writes the worktree while monitoring** | Read-only monitor sessions (DES-001 §4.1); a member's *step* runs as a separate unit session with the unit's own write posture and guard. | §8.5 |
| **The plan escapes the floors** | Decision 1 default: composition from the library; pins/gates/floors are data the registry validates. | §8.3 |
| **The gate stalls on a slow team** | `FINAL_PASS_BUDGET` on the wait; a timeout synthesizes the fail-closed ledger and pauses. | §8.8 |

## 12. Where each piece lives

| Piece | Repo | Location |
|---|---|---|
| `TeamPublisher` thread; actor-side `path.started`, `plan.accepted`, `gate.decided`, `path.ended` | core | new `src/team/bus.rs`; hooks at launch admission, `RegisterWorkflow` (`src/actor.rs:2545`), the fold call site (`:4269` → `pipeline.rs:806`), `finalize_run`/`fail_run`/cancel |
| Event types, payload structs, keys, `fold(events) -> TeamLedger` | core | `src/team/events.rs` (pure; fixture-tested) |
| Supervisor on a bus cursor; `member.*`, `finding.raised`, `help.answered`, `finding.settled`, `council.called`, `gate.opened` | core | `src/team.rs` (S2's `TeamCore`/`UnitTeam`/batching/confirmation kept; `TeamCmd`/`TeamHandle` deleted) |
| Worker-thread seam: `step.claimed` before, `step.completed` + the boundary injector + the gate wait after | core | `src/cli_runner.rs:748-761`, prior-context assembly `src/actor.rs:6601-6650` (a `[team advice]` `PriorUnitOutput`) |
| ACP carrier: `checkpoint.reached`, steer source = bus poll, `advice.delivered{channel:"acp_steering"}`, `help.requested` | core | `src/acp_runner.rs:4440`, `:4903`, `:8128` |
| PA plan step: `signals_from_paths`, `path.scored`, `plan.proposed` parse; `LaunchSpec.primary`; PA-pinned `teamed_distribution` | core | `src/review_scale.rs` (S4), `src/plan.rs`, `src/lib.rs:190`, `src/distribute.rs:174` |
| Council: `council.ruled` from `DecisionVerdict` | core | `src/actor.rs:2525-2544` (the spawned thread publishes after `convene_decision` returns) |
| Delete the six team CoreEvents, `SteerMailbox`, `TeamCmd`, `team_finish` | core | `src/event.rs:1180-1201`, `:2400-2435`; `src/team.rs:1730-1832`; the #609 sites per §10 |
| Catalog annotations for 21 types | core | `crates/wicked-governance/seed/event-catalog-annotations.json` |
| Bus handoff at boot (`WICKED_BUS_DB` = the sidecar always); `teamEvent` relay; `GET /runs/:id/team`; `POST /runs/:id/plan`; `POST /runs/:id/help/:id`; human `gate.decided` publish; api-types 0.41.0 | crew | `packages/crew/src/cli/index.ts:99-137`, `core/adapter.ts:1183-1191`; new `packages/crew/src/team/ws-relay.ts` (copy of `interactive/ws-relay.ts:148-162`); `api/routes.ts` beside `:3206`; `packages/crew-api-types/index.d.ts` |
| Feed lines, plan card, gate panel comms, verdict detail, launch picker | studio | `NarratorFeed.tsx`, `SteeringGate.tsx:85`, `VerdictDetail.tsx:90`, `store/runtime.ts:219`, `api/client.ts` |

## 13. Build order — disjoint seams, each buildable by one agent

Seams are ordered by dependency; T2–T5 are parallel once T1 has merged. Each seam names its acceptance.

**T0 — Bus handoff (crew, prerequisite).** Crew sets `WICKED_BUS_DB` to the resolved crew bus on every boot, exec on or off; the exec seam keeps its switch.
*Accept:* (a) `wicked-crew serve` with no flags → the engine's `WICKED_BUS_DB` equals `resolveCrewBus(...).dbPath`; (b) `--engine-exec` mediates over the same file (one `bus.db` on disk, `task.dispatched` rows in it); (c) `--bus-db X` wins for both; (d) a bus that cannot open logs one line and the daemon still serves (un-teamed).

**T1 — Wire contract (core).** `src/team/events.rs`: the 21 payload types, `event_type` consts, key builders, `to_payload`/`from_payload`, `fold(events) -> TeamLedger`; catalog annotations; api-types 0.41.0 with the `TeamEvent` union and the two aliases removed. No publishers.
*Accept:* (a) every type matches `/^wicked\.team\.[a-z_]+\.[a-z_]+$/` and has four segments; (b) round-trip fixtures compare JSON values (not order) for all 21; (c) `fold` on the DES-001 acceptance fixtures (#8, #11, #15, #16 a–k) yields the same ledger DES-001 specified, including `team_pause` and the timeout synthesis; (d) `fold` is idempotent under duplicated rows and insensitive to cross-thread interleaving of `checkpoint.reached` vs `finding.raised`; (e) `gen_event_catalog.py --check` green.

**T2 — Publisher + actor events (core).** `TeamPublisher`; `LaunchSpec.primary` + random selection; PA-pinned `teamed_distribution`; `path.started`, `plan.accepted`, `gate.decided`, `path.ended`; `plan.proposed` consumption → `register_workflow`.
*Accept:* (a) a launch with `primary:"claude#1"` publishes `path.started{cli:"claude#1",selection:"chosen"}` with the deterministic key, and every creator-role unit's `assigned_cli` is `claude#1`; `primary:None` publishes `selection:"random"` with `cli ∈ roster`; (b) evaluator units never land on the PA (fence fixture), and a one-seat roster refuses the plan; (c) re-launching the same run id publishes no second `path.started` row (`event_id` equal); (d) a valid `plan.proposed` yields a registered `WorkflowDef` `"<run>:plan-1"` and `plan.accepted{refused:null}`; an invalid one (unknown field, unknown pin) yields `plan.accepted{refused:{reason}}` and the run continues on its launch workflow; (e) the actor thread never calls `BusDb::open` (a test hook counts opens per thread); (f) `gate.decided` and `path.ended` appear for every terminal run, including cancel.

**T3 — Worker-thread seam (core).** `step.claimed` / `step.completed`; the boundary injector; the gate wait with fail-closed timeout; `UnitEvidence.team` snapshot; `render_for_gate` with transcript; judge exclusion of ledger authors (DES-001 §6.2, all three paths).
*Accept:* (a) `step.claimed` has a lower `event_id` than every `checkpoint.reached` of the attempt (in-process **and** bus-worker path, `src/cli_runner.rs:1976-2018`); (b) a wrapped unit with an undelivered HIGH on the stream receives a `[team advice]` prior-context block on its next step and one `advice.delivered{channel:"boundary", outcome:"injected"}` row per finding; a second step does **not** render it again (an `injected` row exists), whether it was answered or not, and the same holds for a finding already `injected` over `acp_steering`; a finding whose only row is `outcome:"turn_ended"`, `"refused"` or `"not_delivered"` **is** rendered at the next boundary; (c) with no `gate.opened` within a shortened `FINAL_PASS_BUDGET`, the worker publishes `gate.opened{final_pass:"timed_out"}` with the synthesized holds and the unit pauses `team_dispute`; a late supervisor `gate.opened` dedups to the same row; (d) the judge prompt contains the ledger and the transcript inside the WORK fence; DES-001 acceptance #14 (a)–(e) hold with `excluded_seats` from the folded ledger; (e) under `spawn_with_engine` with no bus, the unit produces no team rows and `UnitEvidence.team` holds the local snapshot with `transport:"none"` and an empty ledger.

**T4 — Supervisor on the bus (core; re-homes what #609 landed).** S2's `TeamCore` re-homed onto a cursor; `member.*`, `finding.raised`, `finding.settled`, `council.called` + `convene_decision` + `council.ruled`, `help.answered`, member steps (Decision 3), `gate.opened`; `TeamCmd`/`TeamHandle`/`team_finish`/hold-buffer/S2 CoreEvents deleted.
*Accept:* (a) DES-001 S2 acceptance #1–#6 and #8 re-expressed on bus rows (one `finding.raised` per confirmed finding; zero for an unchanged tree; `member.joined{status:"failed"}` for the creator instance or an unadmitted seat); (b) a restart between two batches loses no finding: the fold after restart contains the rows published before it; (c) the hold round publishes exactly one `finding.settled` per unaccepted finding; silence ⇒ `held`; (d) DES-001 #15/#16 (a)–(k) with `council.called` and `council.ruled` rows instead of ledger fields, and the `transcript` ids of `council.called` resolving to that finding's rows; (e) a `HELP:` line yields one `help.requested` and, with a stub member, one `help.answered` that the next boundary renders; (f) a plan step with `"owner":"team"` parses into `PhaseDef.owner = Team` (a misspelled key such as `"ownr"` is still refused by `deny_unknown_fields`), reaches `WorkUnit.owner` through `plan_from_def`, is skipped by the PA pin, and runs on a member seat as a unit whose `assigned_cli` is the member, and (Decision 3 default) counts only after the PA's `STEP … ACCEPT` produces `step.reviewed{verdict:"accepted"}`; `REJECT` re-plans it once; (g) `grep -rn "TeamCmd\|TeamHandle\|team_finish\|SteerMailbox" src` is empty.

**T5 — ACP carrier (core).** `checkpoint.reached` publish; steer source = bus poll after the attempt's `step.claimed`; `advice.delivered{channel:"acp_steering"}`; mid-turn `help.requested`.
*Accept:* DES-001 S3 acceptance #7–#12 with rows instead of mailbox state: (a) a HIGH row published before a terminal `tool_call_update` produces exactly one `_session/steering` with `idleBehavior:"promptRequired"` and one `advice.delivered{outcome:"injected"}`; (b) `promptRequired` ⇒ `turn_ended`, and the boundary injector delivers it on the next step; (c) a non-advertising bridge receives no steer and the finding is `channel:"none"`; (d) a MEDIUM row is never steered; (e) a row for attempt 1 never reaches attempt 2; (f) a finding delivered mid-turn is not re-rendered at the boundary.

**T6 — Crew surface.** `teamEvent` relay; `GET /runs/:id/team`; `POST /runs/:id/plan`; `POST /runs/:id/help/:id`; human `gate.decided`.
*Accept:* (a) every `wicked.team.*` row on the bus arrives on `/ws` as `{type:"teamEvent", event}` within the poll interval, tagged `project_id` when filed; (b) the read route returns the attempt's rows ordered by `event_id` with the folded ledger, `units: []` for an un-teamed run, and the persisted snapshot when the bus has no rows (fixture: rows deleted); (c) a plan edit publishes `plan.proposed{by:"human"}` with the deterministic key and a repeat POST publishes no second row; (d) approving a `team_dispute` gate publishes `gate.decided{by:"human"}` and `resumed` follows as DES-001 §6.7 (approve never re-dispatches).

**T7 — Studio.** Launch picker (+Random); feed lines; plan card with Edit/Stop; gate panel *Team findings* + *Team comms*; verdict detail.
*Accept:* (a) a run with one HIGH finding, a decline, a hold and a council NO shows, in order, the finding, the delivery, the answer, the hold, the call and the ruling in the feed, and the gate panel groups them under the finding with the human's approve/reject still going through `POST /runs/:id/gate`; (b) a late-joining tab renders the same list from the read route; (c) an un-teamed run shows `transport: none`, not an empty "clean" team; (d) Playwright at 1440×700, via the studio UI, not the API.

**T8 — Rig proof (after T0–T7).** Re-run the #590 B18 shape: a retire-flow unit that writes a cancellation-free coverage fetch. Pass requires `finding.raised{severity:"high"}` at that handler's `path:line` before the PA's turn ends, `advice.delivered{outcome:"injected"}`, `advice.answered`, a `gate.opened` the gate panel renders with comms, a terminal run state, and one `bus.db` on the rig host with no `TeamCmd` in the binary.

## 14. Superseded clauses of DES-TEAMING-001

| DES-001 clause | Status under DES-002 | Replacement |
|---|---|---|
| §3 "A per-daemon TeamSupervisor subscribes to the engine's event fan-out … TeamLedger … emitted as `teamLedger`" | superseded | §3; the supervisor holds a bus cursor; `gate.opened` |
| §4.2 "Source: engine CoreEvents in-process, not crew `/ws`"; "The one event it consumes: `unitCheckpoint`"; "Attach, finish and context … travel on a direct `TeamCmd` channel" | superseded | §4.2 (cursor), §6 #7/#8/#14, §10 |
| §4.7 "Owner: the shared worker-thread seam … `team::finish` … process-wide handle … `TeamCmd::Finish` and blocks" | superseded | §8.8: `step.completed` + a bounded wait for `gate.opened`; timeout synthesis kept |
| §4.7 step 4 "(S3) Parse the worker's `ADVICE` lines → `workerAdviceResponse`" | superseded in transport only | `advice.answered`; the parser stays |
| §5.2 "Mailbox … written by the supervisor … Delivery point … drain all of it" | superseded | §8.6: source = a bus poll; delivery point and `_session/steering` request unchanged |
| §5.1 row "ACP, adapter does not advertise it … Findings reach the gate only" and row "Wrapped non-claude … Gate only" | superseded | §8.6: every carrier gets advice at its next step boundary from the stream |
| §5.3 "Teaming does not route a worker's question to a monitor" | narrowed | §8.5: a `HELP:` line goes to the team (`help.requested`); `AskUserQuestion` still goes to a human |
| §6.1 "emitted once as `teamLedger` … before `GateEvaluated`" | superseded | `gate.opened` precedes the worker's fold input; the CoreEvent order `GateEvaluated → GateDecided → UnitDone` is unchanged |
| §6.4 route shape `{runId, units:[{ord, attempt, ledger}]}` | extended | adds `transcript` and the bus/snapshot fallback |
| §6.5 "Live feed … renders `monitorAttached`, `monitorFinding`, `adviceDelivered`, `workerAdviceResponse`" | superseded | `teamEvent` frames |
| §7 (six `CoreEvent` variants; "All six go to the per-run event log") | superseded | §6 (21 bus types); nothing team-related goes to the per-run JSONL log |
| §8 "S5 … callable off the actor … must emit its own council events" | kept, extended | the council thread also publishes `council.ruled` |
| §9 rows: `TeamSupervisor, TeamCmd`, `steer mailbox`, `team::attach/finish … process-wide supervisor handle`, `Six to_json arms` | superseded | §12 |
| §10 build order (step 1 "six CoreEvent variants", S2/S3 seam "the mailbox type") | superseded | §13 |
| §11 rows "Advice for one attempt reaching another: the mailbox is keyed" | superseded | the attempt's floor is its `step.claimed` id |
| §13 "The per-attempt ledger is lost on a daemon restart mid-unit" | closed | §11 (restart) |
| §4.1, §4.3–§4.6, §4.8, §5.1 (Claude ACP row), §5.2 (request shape, `promptRequired`, re-confirmation before sending), §5.3 (text, parsing, authority), §6.2, §6.3, §6.6, §6.7, §8 (S4/S5 reads), §8.1 (impact model), §12 acceptance semantics | **kept** | re-expressed on bus rows where §13 says so |

## 15. Open questions (not blocking the build)

- **Q1. The operator inject bar** (`InjectWorkerMessage`, `src/actor.rs:2557`) is the one remaining prompt-side channel that is not a team event. Folding it into `help.answered{by:"human"}` (unsolicited, `re:null`) would make the human a team member on the record. Deferred: it is an operator concern with its own event (`workerMessageInjected`, `src/event.rs:1045`) and consumers today.
- **Q2. Bus retention for evidence.** The snapshot covers the gate; a `retention: forever` per-event flag exists only in wicked-bus DESIGN-v2 (`:97`). If operators want the raw transcript beyond the TTL, that is a wicked-bus feature, not a second store here.
- **Q3. SPEC filter text vs code.** `reqs/SPEC.md:700-727` documents single-level `.*` only; the code and three crew consumers use `prefix.**`. This design uses `**` (the code); SPEC should be corrected to match (`gen_event_catalog.py` regenerates only the catalog block, not that section).
- **Q4. Member seat diversity.** Unchanged from DES-001 §13 Q2: only claude is ACP-admitted, so members are `claude#N` today.
