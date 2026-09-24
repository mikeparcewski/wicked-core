# DES-TEAMING-002 — The team model on the bus: one transport, one grammar, one phase catalog

- **Status:** rev 11, ready for review. **No open operator items.** The operator decided all three design questions (2026-09-23, §8.3–§8.8) and on 2026-09-24 confirmed the two remaining recommendations: the floor-override rule (§8.5) and "team runs never use `creator_seat`" (§8.1). Every decision is written into the text it governs.
- **Date:** 2026-09-23
- **Rev 11 (2026-09-24): T0 amended.** The T0 build found three facts about the code that rev 10's T0 did not account for. Handing every engine `WICKED_BUS_DB` as written would have broken governed runs, so T0 now also changes the engine (wicked-core#613, wicked-crew#671):
  1. **`WICKED_BUS_DB` alone switched the gate judge to the bus.** `run_unit_and_judge_with_roster` sent every approved-validator judge to `bus_request_agent_verdict` whenever the variable was set (`src/cli_runner.rs:912-927` at `e8ae661`). That path publishes `wicked.gate.eval.requested` and denies fail-closed after 180 s (`:93`, `:446-462`). The only responder is `scripts/gate_eval_daemon.py`, which is started by hand (`tests/gate_eval_daemon_verdict.rs:4-9`). The built-in feature/bug/migration Evaluator phases all carry the evidence-floor pin (`workflows/feature.json:58`, `bug.json:55`, `migration.json:39`). So every default daemon would have stalled and denied them. **Now:** the bus judge follows exec mediation, never the bus alone. It uses the bus only when exec mediation is ARMED for the unit, and the caller passes that bus: the cli-runner's own bus, or the actor's armed publisher's. This covers the env-free `Core::spawn_with_engine_exec` entry. It never re-reads process env, so `WICKED_BUS_EXEC` with a bus that exec cannot initialise keeps the in-process judge inline instead of denying on the missing bus.
  2. **The actor opened the bus at startup.** The launch bridge's start-point snapshot (`src/bus.rs:707-709`) and `init_exec_consumers` (`src/cli_runner.rs:1596`, `:1608`) ran on the actor thread (`actor::run`, spawned at `src/lib.rs:520-521`), so a locked bus stalled the first command for the 5 s busy timeout. **Now:** the actor never opens the bus, at startup or after. The launch bridge is armed by `Core::spawn*` on the caller's thread with a synchronous, bounded handshake. The bridge thread opens the bus and reads `MAX(event_id)`; the caller waits at most `BUS_ARM_TIMEOUT` (2 s) for that floor and returns only once the bridge polls strictly after it. The start point is therefore an **event id**, never a wall-clock time: polling is by `event_id > floor` (`src/bus.rs:495`) while `emitted_at` is millisecond wall clock (`:288-290`), so a time floor cannot tell a same-millisecond request from history. A request emitted after `spawn` returns is delivered, and a row already on the bus is never launched, whatever its `emitted_at`. The bridge has one owner: dropping it stops and joins its thread, whether the actor exits normally or returns early (for example, when it cannot open its store). On a timeout or an open error the bridge is **not armed**: its thread exits without polling rather than guess a start point, and `Core::bus_bridge_state` / core-ts `busBridgeState()` report `not-armed` with the reason, which crew discloses on the `bus.unavailable` notice. Exec mode's actor still waits for its bus thread's answer, which keeps its arm-before-first-command and redrive-before-orphan-report contract unchanged.
  3. **One bus file, two SQLite libraries in one process.** This is the F-E2E-021 lock-loss class: closing any descriptor drops the process's POSIX locks, and one library cannot see the other's. **Rule now:** each library holds one long-lived connection to the bus file per process and never opens and closes one. The engine uses `BusDb::shared` (a process-wide handle, never closed); a source-guard test fails the build on any production `BusDb::open`. The handle is reused only while the path still names the file it opened: the registry stores the file's identity at open (`(st_dev, st_ino)` on unix; the creation time on Windows, where SQLite's open without `FILE_SHARE_DELETE` makes the OS refuse to delete a held bus file) and compares it on every lookup. On a mismatch (the file was deleted and recreated) the old handle is leaked, per the rule, and a fresh one is opened, registered and counted. Crew uses one long-lived crew bus handle, opened at boot before the engine spawns and reused only while the path names the file it opened (`dev:ino`). The project activity feed's per-request open/close now goes through it. Shutdown order: the engine's connection closes only at process exit. Crew hands the bus only to an engine that carries the rule (`Core.busConnectionStats`); an older engine gets the pre-T0 handoff. Before spawning, crew makes `process.env` match its handoff decision exactly: an inherited `WICKED_BUS_DB` or `WICKED_BUS_EXEC` that the decision did not hand the engine is removed. An unavailable bus dominates every branch: whichever file the engine would get (the pre-T0 exec bus included) is probed, and if it cannot open, the engine gets no bus and exec mediation is off.
  4. **T0's no-bus case is a daemon-level notice** (`/health.warnings` `bus.unavailable` plus one log line). The per-run `transport:"none"` (§4.8 row 6) lands with P1, where `AgentSession.team` exists.
- **Rev 10 (2026-09-24):** review on #612 at `548d273` (no HIGH; 2 MEDIUM, both undecided choices, now decided by the coordinator).
  1. The `path.started` fallback is disclosed only by persisted `transport:"none"` on the run and attempt snapshots, served by `GET /api/v1/runs/:id/team` and studio's run-header banner. The proposed `teamTransportDisabled` CoreEvent is removed, keeping §4.6's one-mechanism rule (§4.1, P1 (b), §8.12, T8 (b)).
  2. migration/cleanup maps to `build`: evidence pin, `executes_code`, creator role. This is a known behaviour change recorded in §11.3 and M2 and pinned by the C1 fixture; Q5 is closed.
- **Rev 9 (2026-09-24):** review on #612 at `522a804` (1 HIGH, 1 MEDIUM, both verified in the rev-7 reliability section).
  1. A failed `path.started` left its outbox line behind, so a later replay could arm team state for a run on `transport:"none"`. The **supersede rule** fixes it: an irreversible fallback writes an outbox tombstone before it is acknowledged, and replay and drain skip superseded lines. No event type is added.
  2. `gate.opened.ledger_ref` dangled for a synthesized or no-bus snapshot. It is now `null` with `ledger_source`, and consumers read `UnitEvidence.team`. The "before the fold" wording now names `apply_step_result` → `GateEvaluated`.

  A sweep of the class adds §4.8, twelve failure and fallback paths each tabulated with (a) persisted, (b) on the bus, (c) references and (d) replay. It found three more gaps, now fixed:
  - one owner's facts could be reordered on replay (`gate.decided` before `gate.opened`): the fix is a per-publisher, per-run FIFO;
  - a published-then-retention-deleted fact could re-publish from an old line: the fix is outbox compaction;
  - a late S fold could publish a ledger the gate never used: the fix is a deadline tombstone plus the "`ledger_ref` is authoritative" rule.

  P1 gains acceptance (e)–(i).
- **Rev 8 (2026-09-24):** review on #612 at `369fc50` (2 MEDIUM, both leftovers, verified).
  1. The `advice.delivered` example still named the supervisor's sweep and `delivery_id "sweep"`. It is now R-only with `"end:<attempt>"`.
  2. `step.completed.status` now lists all five spellings `status_to_str` emits (`src/cli_runner.rs:285-296`; `StepStatus`, `src/workflow.rs:175-196`).

  A final sweep checked every JSON example and enum comment against the §6/§7 owner and key tables and against the code spellings, and fixed nine more:
  - payload numbering now matches the event table (two blocks were both "20");
  - the `path.scored`, `step.claimed` and `step.completed` headers named a second producer (only one owns each);
  - `path.scored` carried a dropped `score_seq`;
  - `member.*` lacked their `ord`/`attempt` key fields, and `member.joined.status` said `"joined"` where S2 spells `"attached"` (`src/team.rs:1070`);
  - `council.ruled` showed a `seats` field `DecisionVerdict` does not have (now `task_id`, `consensus`, `src/decision.rs:292-310`);
  - `gate.decided.kind` lacked `team_transport`, and `PauseReason` now gains `TeamTransport`;
  - the `finding.raised` example presented DES-001 §4.6's `anchor`/`line_key` as existing fields, but on main `finding_id` is `sha256(path ‖ evidence)` (`src/team.rs:386-392`), so they are marked "T6 builds them";
  - every remaining enum comment now cites the code that spells it (S2 monitor status, S3 `SteerOutcome` and `Disposition`, checkpoint status, severity, terminal `SessionStatus`);
  - the envelope's `by` is stated as "who the row is about", not the owner.
- **Rev 7 (2026-09-24):** review on #612 at `6a81b1d` (3 HIGH, each verified at the code), resolved under one rule added as §4.0: **commands go through the API, facts go on the bus, each fact has exactly one owner, and no consumer turns a bus row into a command on someone else's state.**
  1. The `TeamPublisher` is **reliable**. It reuses the engine's durable emit-outbox pattern (NDJSON spool, `DEADLETTER_MARKER`, idempotent replay; `crates/wicked-apps-core/src/emit.rs:453-470`, `:614`) with a bounded retry. A required fact that cannot be published pauses the run (`team_transport`), or marks it `transport:"none"` before any team work begins (§4.1).
  2. **The actor reads no bus row.** User plans, preset launches, plan edits and approval edits are API commands. PA plans arrive on `Command::ApplyStepResult` (`src/command.rs:218`). The supervisor's re-scores and member-step rulings arrive as its own commands. The engine publishes `plan.proposed` after accepting any of them.
  3. **Crew publishes no fact.** It only POSTs; `gate.decided` and every plan fact are engine-only.

  To give every type one owner: `ledger.folded` (S) splits from `gate.opened` (E), `change.requested` (S) splits from `help.requested` (R), the attempt-end not-delivered row moves to R, and the human `help.answered` path is removed (human-directed questions stay S1 elicitation). §7 is now the producer/consumer matrix. Both confirmed recommendations are folded in, and their markers removed.
- **Rev 6 (2026-09-23):** review on #612 at `ea4a2cb` (1 HIGH, the same class as rev 4's). `help_id` hashed the question text, so a second, distinct request with the same question deduped onto the first (`src/bus.rs:316-342`). It is now derived from the producer's per-attempt `help_seq`. The class is closed by **one rule** in §6.1: no key is derived from content a later distinct request can repeat; keys come from a producer-assigned sequence or id. Walking every key against it changed nine more:
  - `path.scored` → `score_source`;
  - `member.*` + `open_seq` (monitors re-open after a failed turn, `src/team.rs:667`);
  - `finding.raised`, `finding.settled` → the supervisor's `raise_seq`, with `finding_id` kept as payload identity only;
  - `advice.delivered` → `delivery_id`;
  - `advice.answered` + `answered_in`;
  - `help.answered` → `answer_id`;
  - `council.*` → `subject` built from `raise_seq` or step:attempt;
  - `gate.decided` engine-published only.

  T1 gains the table-driven rule test.
- **Rev 5 (2026-09-23):** review on #612 at `c2443f1` (3 HIGH, each verified at the code), plus a re-sweep of every "as today"/"unchanged" claim against the code.
  1. A gate-originated `plan.proposed` takes its `source` from the gate's `gate_id`. The interaction id is `deterministic_id(session, "gate", ord)` (`src/interaction.rs:141-144`), which every gate at that ord reuses (§6.1, T3 (i)).
  2. Supervisor restart is **one mechanism, a startup replay of live runs**, with run-level team state persisted on the session. A restart never resumes an in-flight attempt (the attempt is reported orphaned or redriven at attempt+1, `src/actor.rs:1058-1065`, `:3993-4030`), so the replay's job is plan and gate state plus the prior attempt's findings (§4.7, T6 (k)).
  3. **Evaluator ≠ creator for team runs** (then a coordinator recommendation; confirmed by the operator in rev 7). Never `creator_seat`: a distinct CLI, else a second usable instance of an eligible CLI (`same_cli_instance`), else refuse `NoEligibleSeat`. Today a bench-free roster keeps review on the creator seat (`src/distribute.rs:286-290`, `:417-426`; refusal only when a bench caused it, `:388-409`). Legacy consumers keep today's behaviour until their migration seam (§8.1, §11.3, seam D1).

  The sweep corrected four overclaims (§16.1).
- **Rev 4 (2026-09-23):** review on #612 at `5ba3a2c` (2 HIGH, 1 MEDIUM, each verified at the code), plus a sweep of every idempotency key against its payload. (1) A plan with a creator step and no declared `touch[]` now scores `no_graph_score` (100, "no declared scope"), S4's fail-closed rule (`src/review_scale.rs:264-265`, `:281`). Only a plan with no creator step may score 0 (§8.2, §8.4, T2 (g)). (2) Gates are keyed by a `gate_id` minted from a per-run gate sequence, so a re-opened `plan_approval` gate is a new row, not a dedup onto the old one (`src/bus.rs:316-342`) (§6, §8.6, T3 (i)). (3) `advice.delivered` is one row per finding (§6, §8.9, T5). The sweep fixed seven more keys (§6.1). The T5–T9 acceptance text that rev 3 cited as "rev 2's" is now inlined.
- **Rev 3 (2026-09-23):** folds in the operator decisions above and the consumer constraint: every workflow consumer is inventoried (§11.1), mapped onto the catalog (§11.2) and given a before/after behaviour table with named contract tests (§11.3). Migrating them is in scope, one seam per consumer, with no dual path (§14). New: the phase catalog (§8.3), plans and presets (§8.4), the floor and high-risk table (§8.5), the approval matrix and the `plan_approval` gate (§8.6), `plan.revised` with the re-score trigger (§8.7). The event table gains `plan.revised`, and `gate.opened`/`gate.decided` gain `kind` (§6). Citations re-verified at core `main` `fe94ffc`, which now includes S4 (#600).
- **Rev 2 (2026-09-23):** review on #612 at `06db4cd`: the exact idempotency-key algorithm with test vectors; the explicit `owner` field; the step-boundary dedup keyed on `outcome:"injected"`; an enum sweep.
- **Supersedes:** the transport and orchestration parts of DES-TEAMING-001 (§15 lists every clause), and the per-surface workflow files (§10, §11). DES-001's landed seams stay: S1 elicitation (#599), S3's steer over `_session/steering` (#607), S5 `RoutingInfo::Teamed` + `Core::convene_decision` (#608), S4 impact scoring (#600), S2 monitors (#609), the batching/confirmation/dedup rules (DES-001 §4.3–§4.6), the gate render and judge exclusion (§6.2), and the unresolved-HIGH ruling (§6.3, §6.7).
- **Scope:** wicked-core (bus publisher, supervisor, carrier, fold, catalog, presets, floor, plan gate, re-plan), wicked-crew (bus handoff, relay, routes, migration of its workflow consumers), wicked-studio (feed, phase picker, plan and unit gates), wicked-bus (no change; read as the transport contract).
- **Related:** #590 and its operator decisions, #604/#610 (DES-001), #609 (S2, `7f07428`), #600 (S4, `fe94ffc`).
- **Evidence base:** wicked-core `main` at `fe94ffc`; wicked-crew `main` at `fc079ec`; wicked-bus `main` at `d309d56`; wicked-studio `main` at `340e92e`. Every `file:line` was opened at that revision; citations in studio and crew files name the repo.

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
| A direct `TeamCmd` channel from the worker-thread seam to the supervisor (`Attach`, `Finish`, `RunComplete`) plus a `TeamHandle` installed on the runner | attach context, the final-pass trigger, the run-complete sweep | `src/team.rs:1437-1443`, `:1453-1470`, `:1498-1510`; `src/lib.rs:576` (`runner.install_team(...)`); `src/workflow.rs:418` (`StepRunner::team_finish` default), called at `src/cli_runner.rs:766` |
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
| The engine is handed a bus db only through `WICKED_BUS_DB`, and crew sets it **only** when `--engine-exec` is on. Crew resolves a **different** default bus for its own seams: the `<core db>.bus/bus.db` sidecar. **`WICKED_BUS_DB` is not only a handoff:** set alone, it also switches every approved-validator gate judge to the bus round-trip, which has no in-product responder. The bus is opened **on the actor thread** at startup (the bridge's snapshot, and exec init). Engine and crew seams sharing one file means two SQLite libraries in one process (the F-E2E-021 class). All three are resolved by T0 (rev 11). | `src/actor.rs:985-995`, `:1001-1015`; `src/cli_runner.rs:912-927`, `:93`, `:446-462`; `src/bus.rs:707-709`; `src/cli_runner.rs:1596`, `:1608`; `tests/gate_eval_daemon_verdict.rs:4-9`; `wicked-crew packages/crew/src/core/adapter.ts:1183-1191`; `packages/crew/src/cli/index.ts:99-113`, `:125-137`; `packages/crew/src/projects/activity.ts:158-193` (all at core `e8ae661`, crew `fc079ec`) |
| Crew's `/ws` is fed by the **one** in-process napi subscription, not by the bus; `broadcast` fans a frame to every socket. | `adapter.ts:1213-1216`, `:1297`; `packages/crew/src/api/server.ts:31`, `:1202`, `:1235`, `:1458`; `packages/crew/src/events/bus.ts:10-35`; `crates/wicked-core-ts/src/lib.rs:804` |
| Crew already relays a bus family onto `/ws` as an envelope frame: `bus.subscribe({plugin, filter:"wicked.interactive.**", cursor_init:"latest", maxRetries:0})` → `broadcast({type:"interactiveEvent", event})`. Studio folds that envelope defensively. | `packages/crew/src/interactive/ws-relay.ts:47-55`, `:148-162`; `wicked-studio src/store/runtime.ts:219-230` |
| Crew's durable consumers are idempotent by construction and keyed by a plugin name; emits carry a deterministic `idempotency_key` and treat `WB-002` as "already happened". | `packages/crew/src/qe/gate-events.ts:41-47`, `:77-79`; `packages/crew/src/projects/events.ts:93-108`, `:114-135` |
| `GET /api/v1/runs/:id/events` serves the engine's per-run JSONL log (`run_events`), which records **CoreEvents only**. High-volume variants are excluded from the log. | `packages/crew/src/api/routes.ts:3415-3431`; `adapter.ts:1825`; `src/lib.rs:1243`; `src/event_log.rs:149`, `:370-380`, `:448` |
| The grammar: `wicked.<domain>.<noun>.<past-tense-verb>`, four segments, lowercase; validated by `/^wicked\.[a-z0-9_]+(\.[a-z0-9_]+)*$/`. The `<domain>` gloss says "the producing product's short name", and core already publishes under a functional domain (`wicked.gate.eval.requested`). | `wicked-bus reqs/SPEC.md:369`, `:379-397`; `lib/validate.js:9`; `src/cli_runner.rs:88` |
| Filters: SPEC v1 text documents exact and single-level `.*` and defers `**`; the shipped code matches `prefix.**` (one-or-more segments) and crew relies on it. Core mirrors the code. | `reqs/SPEC.md:700-727`; `lib/poll.js:50-59`; `packages/crew/src/qe/gate-events.ts:42`; `src/bus.rs:555-562` |
| Retention: `expires_at` (72 h) hides a row from polls; `dedup_expires_at` (24 h) is the **row deletion** trigger for the v1 sweep. The v2 sweep moves TTL'd rows to warm monthly buckets and `pollResolve` reads across tiers. The `retention: forever` column exists in DESIGN-v2 only, not in the shipped schema. Neither crew nor core runs a sweep. | `reqs/SPEC.md:804-818`; `lib/sweep.js:53`; `lib/sweep-v2.js:2-3`; `lib/query.js:41`; `DESIGN-v2.md:97` vs `lib/schema.sql` (no `retention` column) |
| At-least-once is the contract; idempotency is the consumer's responsibility. | `reqs/SPEC.md:751-771`, `:780-798`; `DESIGN-v2.md:64` |
| `wicked_apps_core::emit_event_to` writes an EVENT **node on the estate graph**, not a bus row. It is not a transport. | `crates/wicked-apps-core/src/emit.rs:172-186`, `:199` |
| The shared worker-thread seam every carrier's unit passes through: run, guard's first look, `work_for_agent`, judge; callers in-process and bus. | `src/cli_runner.rs:748-761`, `:876`, `:881-901`; `src/actor.rs:7034` (via `run_unit_and_judge`, `src/cli_runner.rs:682`), `src/cli_runner.rs:1976-2018` |
| The judge excludes the work author on all three paths; monitor exclusion is DES-001 §6.2. | `src/cli_runner.rs:912-930`, `:937-942`, `:1020-1025`; `src/validator.rs:1506`, `:1585` |
| The fold's emission order: `GateEvaluated` → `GateDecided` → `UnitDone`/`UnitDenied`; the gate's logic is `combine_verdict`. | `src/pipeline.rs:806`, `:1519-1556`; `src/validator.rs:2263` |
| The human pause is durable state plus one `awaitingHuman{gate_kind}` event; resumption is `confirm_gate`; a rework carries `rework_amendment` as prior context. | `src/actor.rs:6022-6060`, `:7736`, `:8052`, `:6624-6650` |
| The council entry point: `DecisionRequest {session_id, ord, question, options, evidence}` → `DecisionVerdict {winner, consensus, agreement_pct, returned, seated, dissent, no_ruling_reason}`; ballots run off-actor and relay `CouncilConvened`/`CouncilDeliberated`/`CouncilSeatFailed`/`CouncilVoted` through the emit point. | `src/decision.rs:276-310`; `src/lib.rs:1286-1296`; `src/actor.rs:2525-2544`, `:3432`; `src/event.rs:172`, `:180`, `:194`, `:223` |
| Routing is deterministic per unit (`RoutingInfo::Teamed { winner }`), then the evaluator≠creator fence moves review/test units off builder seats. | `src/distribute.rs:174`, `:387`, `:547` |
| A workflow is data: `WorkflowDef { id, phases: Vec<PhaseDef>, base_skill_ref }`; a `PhaseDef` carries `kind` (Recon/Build/Review/Test), `gate_type`, `gate`, `role`, `skill_ref`, `validator_pin`, `depends_on`, `executes_code`, with `deny_unknown_fields`. `Core::register_workflow` validates and registers at runtime; `plan_from_def` yields one unit per phase. | `src/workflow.rs:640-647`, `:686-708`, `:800-803`; `src/domain.rs:630-640`; `src/lib.rs:1299`; `src/actor.rs:2545-2556`; `src/plan.rs:94` |
| Launch input: `LaunchSpec { problem, clis, entity_mode, session_id, human_confirm, auto_deliver, repo_ref, base_ref, … }`. Crew's `POST /runs` takes `clis`. | `src/lib.rs:193-216`; `routes.ts:222` |
| Operator inject on ACP: queued per `(run, cli)`, delivered as a `[operator message]` prior-context block on the next matching unit's prompt. Wrapped CLIs run with `stdin` null. | `src/actor.rs:2557-2600`; `src/acp_runner.rs:5548`, `:6750-6781`, `:7017-7021`; `src/execute_wrapped.rs:3468` |
| The only mid-turn channel is the ACP adapter's `_session/steering`, read once per process, delivered at a terminal `tool_call_update` in the `session/update` arm. | `src/acp_runner.rs:2835`, `:3910`, `:4440`, `:4903`; `src/team.rs:1550` |
| S4 (on main since `fe94ffc`): `signals_from_diff`, `graph_age`, `assess(&ChangeSignals, Graph, Option<&dyn ModelAssessment>) -> Assessment { deterministic, score, reasons, model, signals, plan }`; `plan_for(score)`; bands `monitors` 0/1/2/3. | `src/review_scale.rs:640`, `:375`, `:563`, `:167-176`, `:552`, `:222-245`, `:134-139` |
| S2 (on main since `7f07428`): `TeamPlan { monitors, candidates }`, `AttachCtx`, `MonitorScope`, `MonitorHost` trait, `Emit`, the supervisor consuming **only** `UnitCheckpoint` from the fan-out, and a hold-checkpoints-before-attach buffer that exists because the fan-out and the direct channel have no shared order. | `src/team.rs:122-128`, `:625-645`, `:651`, `:659-670`, `:675`, `:1498-1510`, `:1150-1200` |
| `serde_json` has no `preserve_order`; fixtures compare values. | `Cargo.toml:59` |

## 3. Shape in one paragraph

A run starts a **path**. The launch names the PA seat (chosen or random), and either a phase selection (a user plan or a named preset) or nothing, and publishes `wicked.team.path.started`. With nothing named, the PA's first step (`understand`) predicts the touch set and proposes a plan. The engine scores the path with S4 (`path.scored`). The band sets the **floor**: the minimum phases, drawn from one **phase catalog**, whose entries are `PhaseDef`s, so gates, pins and floors keep working. Missing floor phases are added and marked. The plan then passes the **approval matrix**: manual mode, or high risk in any mode, opens a `plan_approval` gate (`gate.opened{kind:"plan_approval"}` → `gate.decided`); otherwise it is accepted at once (`plan.accepted`). Every step is bracketed by `step.claimed` and `step.completed` on the bus, with `checkpoint.reached` at each terminal tool call. At checkpoints the diff is re-scored; a higher band raises the floor through `plan.revised`, which only ever adds phases and never re-runs finished ones. The PA and accepted member requests can revise the plan too, under the same matrix. Members hold a durable cursor on the run's `wicked.team.**` stream. They raise findings, answer help, and take steps the PA must accept. Advice reaches the PA at its next step boundary on every carrier, and mid-turn on Claude ACP. Disputes go to a one-off council (`council.called`/`council.ruled`). The final pass folds the attempt's stream into one ledger (`ledger.folded`, S), and the engine opens the unit gate on it (`gate.opened{kind:"unit_review"}`, E), and the fold's decision is published (`gate.decided`). Crew relays `wicked.team.**` onto `/ws`, and studio renders the feed, the plan card, the phase picker and both gates.

## 4. Transport: the bus is the one fabric

### 4.0 The rule: commands go through the API, facts go on the bus

**COMMANDS go through the API; FACTS go on the bus.**

A **command** is a request to change state. Examples: a launch, a user-composed plan, a plan edit, a gate answer, a preset save. Crew submits a command to the engine through the existing API: `POST /api/v1/runs` → `Core::launch_run` (`src/lib.rs:715`); `POST /api/v1/runs/:id/gate` → `Core::confirm_gate` (`src/lib.rs:742`); plus the new `Core::propose_plan` and `Core::put_preset`. The engine validates the command, and **the owner of the resulting state publishes the fact**.

The bus carries only **facts**, each published by **exactly one owner** (§7). No consumer turns a bus row into a command on someone else's state.

When an in-process core component must feed the actor, it uses the actor's existing command channel, the `Sender<Command>` that every off-actor thread already uses (`src/bus.rs:27-33`):
- the worker thread's step result, `Command::ApplyStepResult` (`src/command.rs:218`), which carries the step output with its `PLAN+`, `STEP` and `PLAN <id>` lines and the ledger snapshot;
- the supervisor's own measurements: `Command::TeamRescored` (a re-score of the settled tree it took) and `Command::TeamStepRuling` (a council verdict it convened);
- the publisher's acknowledgements, `Command::TeamPublished` / `Command::TeamTransportFailed` (§4.1).

A command carries the sender's own result, never a relayed bus row. **The actor reads no bus row.** Every rule below that says "cites §4.0" follows this.


### 4.1 Publishing

- **Where:** `BusDb::emit(&BusEmit)` (`src/bus.rs:288`) on a connection owned by the publishing thread (`:239-241`). `domain` = `CORE_DOMAIN` (`wicked-core`, `src/cli_runner.rs:73`); `subdomain` = `core.team`.
- **Owners and their threads (§4.0, §7).** Each event type has one owner:
  - **Engine (E):** the actor, publishing through the `TeamPublisher`.
  - **Supervisor (S):** its own thread and connection; the council calls it makes (`Core::convene_decision`, `src/lib.rs:1286`, ballots off-actor at `src/actor.rs:2525-2544`) return to it, and it publishes the ruling.
  - **Attempt runner (R):** the worker thread of one attempt (`run_unit_and_judge_with_roster`, `src/cli_runner.rs:748`) together with that attempt's ACP carrier turn loop (`src/acp_runner.rs:3910`).

  The actor thread **never opens the bus**: E's events go through the `TeamPublisher` (one thread, one connection, one `mpsc::Sender` the actor holds), so a busy bus can never stall the single writer (`src/bus.rs:27-33`).
- **Publishing is reliable, reusing the engine's durable emit-outbox pattern.** Every publisher (E through the `TeamPublisher`, S, R) calls one wrapper, `TeamBus::publish`.
  - **The pattern it reuses** is `wicked_apps_core::emit`'s: a failed write is spooled as one NDJSON line to an outbox with a loud `DEADLETTER_MARKER` on stderr (`crates/wicked-apps-core/src/emit.rs:12-16`, `:453-470`). The record is the envelope plus `deadletter_reason`, `ts` and `pid` (`:90-105`). It can be replayed idempotently (`replay_outbox`, `:614`; napi `replay_emit_outbox`, `crates/wicked-core-ts/src/lib.rs:774-790`; crew CLI `wicked-crew governance replay`, `packages/crew/src/cli/governance.ts:2`).
  - **Outbox and retry:** the team outbox is `<state home>/team-outbox.ndjson`. `TeamBus::publish` retries a failed emit from the outbox on a **bounded** schedule: 5 attempts at 1, 2, 4, 8 and 16 s, about 31 s in all. A retry can never double-publish, because the key is deterministic (§6.1) and `BusDb::emit` resolves a duplicate to the existing row (`src/bus.rs:316-342`). Past the bound, the line stays in the outbox for `Core::replay_team_outbox`, the same shape as `replay_emit_outbox`.
  - **Order: one FIFO per publisher and run.** Each owner drains its own outbox lines for a run **in emit order**, and a later fact of that run waits behind an earlier unpublished one. So E's `gate.decided` can never land before E's `gate.opened`, and no replay reorders one owner's facts. Order across owners is the bus's `event_id`, as always (§4.3).
  - **Compaction:** a line is removed from the outbox once it is published, or once it is superseded (next rule). An old line therefore cannot re-publish a fact whose bus row was already written and later deleted by retention.
  - **The supersede rule (tombstone).** When a publish failure triggers an **irreversible fallback**, the publisher appends a **tombstone** to the outbox in the same step, *before* it acknowledges the fallback to the actor. Irreversible fallbacks are: the run on `transport:"none"`, an attempt on `transport:"none"`, a `team_transport` pause resolved as continue-without-team or cancel, and a fold past its deadline. Two tombstone forms:
    - `{"superseded":"<idempotency key>","reason":…}` for one fact;
    - `{"superseded_run":"<run>","from_event":"<type>","reason":…}` for every later line of that run from that owner.

    `replay_team_outbox` and the live drain skip every superseded line and compact it. No event type is added: the tombstone lives only in the outbox, the bus never sees it, and the fallback itself is disclosed through the persisted state and the engine fan-out.
  - **Crash between tombstone and store write:** the tombstone is written first, then the actor persists the fallback. On boot, a team run with no acknowledged `path.started` (`AgentSession.team.stream_floor` unset) is set to `transport:"none"`, so the store and the tombstoned outbox agree. The reverse order cannot happen.
  - **Acknowledgements:** the `TeamPublisher` acknowledges each engine event to the actor through `Command::TeamPublished { token }` or `Command::TeamTransportFailed { token, reason }`. The actor defers the dependent step until then and never blocks.
- **Required transitions never continue silently.** Four engine facts are *required*:

  | Fact | The engine waits for its acknowledgement before | If it cannot be published within the bound |
  |---|---|---|
  | `path.started` | dispatching the run's first unit | the run proceeds **un-teamed** with `AgentSession.team.transport = "none"`, set **before any team work begins**; it is disclosed by that **persisted state** alone: `transport:"none"` on the run and in every unit's snapshot, read through the existing read path (`GET /api/v1/runs/:id/team` returns `{transport:"none", reason}`, and studio's run header shows an "un-teamed: team transport unavailable" banner, §8.12). No CoreEvent is added: §4.6 keeps each concern on one mechanism |
  | `plan.accepted` | dispatching the plan's first unit | the run **pauses** with `gate_kind:"team_transport"` and the reason in the prompt |
  | `gate.opened` | — (the pause itself is already durable in the store, `src/actor.rs:6036-6046`) | the pause stands, answerable as usual; its `awaitingHuman` prompt names the missing fact |
  | `gate.decided` | emitting `Resumed` and dispatching | the run stays paused with `gate_kind:"team_transport"` |

  S and R facts past the bound are spooled and disclosed:
  - **A missing `ledger.folded`** reaches the gate wait's fail-closed timeout, which pauses (§8.11). S's spooled line is tombstoned when the fold's deadline passes, so a late replay never publishes a ledger the gate did not use.
  - **A missing `step.claimed`** makes that attempt un-teamed, with `transport:"none"` in its snapshot, before its turn starts. The attempt's pending R lines are tombstoned from `step.claimed` on.

  **A `team_transport` pause has three answers**, through `confirm_gate` (§4.0):
  - **approve:** retry the pending fact, one more bounded round; still failing means still paused;
  - **approve with amend `"continue without team"`:** `transport:"none"` from this point, and the run's pending lines are tombstoned;
  - **reject:** cancel. Pending lines are tombstoned, and `path.ended` is spooled normally so a later drain lets consumers forget the run.

  Every failure and fallback path, with what is persisted, what is on the bus, what references point at and what a replay does, is tabulated in §4.8.
- **Idempotency key.** Every team event pins `deterministic_key(&["team", <event_type>, <run_id>, <entity ids…>])` (`src/bus.rs:545`), so a re-publish after a retry, a restart or a duplicate delivery resolves to the existing row (`:319-345`). The key parts per event are in §6. **The algorithm, exactly as `deterministic_key` computes it (`src/bus.rs:545-553`):** SHA-256 over the concatenation, for **every** part in order, of the part's UTF-8 bytes followed by one `0x00` byte (so the last part is NUL-terminated too; this is not a `\0`-join); keep the first **16** bytes of the digest; encode each as two **lowercase** hex digits (`{:02x}`), giving a 32-character key. Crew's JS publishers (§7) must reproduce it byte for byte: `const h = createHash('sha256'); for (const p of parts) { h.update(Buffer.from(p, 'utf8')); h.update(Buffer.from([0])); } key = h.digest().subarray(0, 16).toString('hex');`.
  - **Test vector.** parts `["team", "wicked.team.finding.raised", "run-1", "f-3fa9c2e1d0b4a7e6"]` → `f8289d402fc42823fd875fcf4456bd8f`. parts `["team", "wicked.team.path.started", "run-1"]` → `25ea4932f42b6e22f1aacd16bc3dcdd9`. A `\0`-join **without** the trailing NUL gives `fed61d5909428bb7aec506ad09dc86d1` for the first vector, which is wrong, and that mismatch is the failure the vector exists to catch. The values were computed by a byte-for-byte transcription of `deterministic_key` (no build was run for this draft). T1 pins them in a Rust unit test against `crate::bus::deterministic_key`, and in a crew test against the JS helper, so the two implementations cannot drift.
- **One db.** The engine needs a bus path whether or not exec mediation is on. Crew hands the engine **its own cross-product bus** (the sidecar `resolveCrewBus` already resolves, `cli/index.ts:129-137`) as `WICKED_BUS_DB` on every boot, not only under `--engine-exec` (`adapter.ts:1183-1191`). Exec mediation keeps its `WICKED_BUS_EXEC` switch; it now mediates over the same file. **The gate judge's bus round-trip belongs to exec mediation** and switches on `WICKED_BUS_EXEC`, never on the presence of `WICKED_BUS_DB` (rev 11). **The actor never opens the bus**, at startup or after: bus threads open it, and `Core::spawn*` arms the launch bridge with a bounded event-id handshake (not a wall-clock floor). **One connection per SQLite library per bus file per process**, never closed (the engine's `BusDb::shared`, crew's bus handle), because the engine's rusqlite and crew's better-sqlite3 share the file in one process. Crew hands the bus only to an engine that carries this rule (`Core.busConnectionStats`). A daemon with no usable bus runs **un-teamed and says so**: nothing can be published, so the worker thread builds the unit's snapshot locally (not published) with `transport:"none"`, an empty ledger and an empty transcript, and the gate renders it as un-teamed (§8.11). It never silently falls back to an in-process channel.

### 4.2 Subscribing

Every consumer is a named cursor with a filter, and every consumer is idempotent on the event's identity (the payload's ids, never the row id) because delivery is at-least-once (`reqs/SPEC.md:751-798`).

| Consumer | Where | Cursor name | Filter | Floor on start | Idempotency |
|---|---|---|---|---|---|
| Team supervisor (monitors' host) | core thread (S2's, re-homed) | `team-supervisor-<actor_process_gen>` (`consumer_name` pattern, `src/cli_runner.rs:1407`); no durable floor is needed because start-up replays (§4.7) | `wicked.team.**` | **Replay, then tail** (§4.7): at spawn it snapshots `tail_event_id()` = T (`src/bus.rs:348`), replays `wicked.team.**` for every live run from that run's `stream_floor` up to T, then polls live from T | per `(run, ord, attempt)` state keyed by `finding_id`, `step_id`, `checkpoint.seq` |
| Steer point (S3) | ACP carrier, per attempt | none persisted — the attempt's floor is the `event_id` of its own `step.claimed` | `wicked.team.finding.raised` | the attempt's `step.claimed` id | a per-attempt `delivered: BTreeSet<finding_id>` (replaces the mailbox `records`) |
| Step-boundary injector | worker thread, before `run_unit_streaming` (`src/cli_runner.rs:761`) | none — one bounded read per step | `wicked.team.**` for `(run)` | the run's `path.started` id | renders only items for which no `advice.delivered` row with `outcome:"injected"` exists yet, on **any** `channel` (`acp_steering` or `boundary`) |
| Gate wait (worker thread) | `src/cli_runner.rs:761` after the run returns | none — the `bus_request_agent_verdict` pattern (`:417-445`) | `wicked.team.ledger.folded` | the attempt's `step.completed` id | matches `run_id, ord, attempt` |
| Crew `/ws` relay | daemon | plugin `wicked-crew-team-relay` (the `ws-relay.ts:47-55` pattern) | `wicked.team.**` | `cursor_init: "latest"`, `maxRetries: 0` | none needed: it broadcasts, never re-emits |
| Crew read route | `GET /api/v1/runs/:id/team` | none — `pollResolve(liveDb, archDir, {lastEventId: 0, filter})` (`lib/query.js:41`), paged, filtered by `payload.run_id` | `wicked.team.**` | 0 | dedup by `event_id` across tiers (`lib/query.js:184`) |
| Studio | `/ws` `teamEvent` frames + the read route on late join | — | — | — | folds by the payload ids |

### 4.3 Ordering

The bus `event_id` is the order (`ORDER BY event_id ASC`, `src/bus.rs:497-499`). No per-run team sequence is minted. Causal references are carried **in the payload** (`re: <event_type>#<entity id>`, e.g. `re: "finding.raised#f-3fa9…"`), not in the bus causality columns: core's `BusEmit` writes only the base columns (`src/bus.rs:22-24`, `:299-303`) and `withContext` is JS-only (`lib/index.js:39`). A consumer that needs "was X before Y" compares `event_id`s it has read; a consumer that needs "X happened" checks the entity id. Because publishers write in program order on one connection each, the events one thread publishes for one attempt are ordered; cross-thread order (a checkpoint from the carrier vs. a finding from the supervisor) is whatever the bus assigned, which is fine for every consumer in §4.2 — none of them keys on cross-thread order, which is why S2's hold-checkpoints-before-attach buffer (`src/team.rs:1150-1200`) is deleted: `step.claimed` is published by the worker thread **before** `run_unit_streaming` starts the turn, so it always has a lower `event_id` than any checkpoint of that attempt.

### 4.4 Retention, and what the record is

The bus is the live record and the transcript's source for at least the TTL window (72 h visible; rows deleted at 24 h by the v1 sweep, moved to warm buckets by the v2 sweep, kept forever when no sweep runs — `reqs/SPEC.md:804-818`, `lib/sweep.js:53`, `lib/sweep-v2.js:2-3`). A per-event TTL cannot extend a row's life (`dedup_expires_at` is config-level, `src/bus.rs:294`; the `retention` column is design-only). So the **durable evidence** of what the gate saw is the fold's snapshot, persisted exactly as DES-001 §6.1 (`WorkUnit.team_ledger`, `UnitEvidence.team`, the evidence bundle at `routes.ts:2879`), with the difference that the snapshot now carries the attempt's **transcript** (every team event of the attempt, capped, §6 `ledger.folded`). The ledger IS the stream: the snapshot is a deterministic fold of the stream (`team::fold(events) -> TeamLedger`, a pure function tested on fixtures), and the studio transcript view reads the bus while it can and the snapshot afterwards.

### 4.5 The relay to `/ws`

One relay, the `interactiveEvent` pattern verbatim (`ws-relay.ts:148-162`): `bus.subscribe({plugin:"wicked-crew-team-relay", filter:"wicked.team.**", cursor_init:"latest", maxRetries:0, handler: e => broadcast({type:"teamEvent", event: e})})`. Frames carry `project_id` when the run's membership files it, as every other frame does (`server.ts:1233-1235`). No `CoreEvent` variant is added for any team event.

### 4.6 What stays on the engine fan-out, and why that is not "two mechanisms"

The `CoreEvent` fan-out (`src/event_log.rs:492`) remains the engine's telemetry: `unitDispatched`, `unitDistributed`, `gateEvaluated`, `gateDecided`, `awaitingHuman`, the council's ballot-level events (`CouncilConvened`/`Deliberated`/`SeatFailed`/`Voted`), chat, terminals. The rule is **one mechanism per concern**: engine state changes are CoreEvents; team communication is `wicked.team.*`. No concern rides both. The six DES-001 team CoreEvents are deleted, not mirrored (§10).

### 4.7 Restart: replay live runs, then tail (one mechanism)

**What a restart loses and what it cannot lose.** No in-flight attempt survives a daemon restart. Anything still `Executing` without a worker in the new process is reported as orphaned and resumed by `POST /api/v1/runs/:id/resume`, or redriven by armed exec mode with the attempt bumped (`src/actor.rs:1058-1065`, `:3993-4030`). Either way, the unit continues as **attempt n+1**, whose `step.claimed` is published after the restart and reaches the supervisor live. So nothing waits on a gate timeout for the dead attempt. What the supervisor must recover is:
- (a) each live run's **team state**: the accepted plan, the floor band, the gate sequence, the member targets;
- (b) the **findings of the attempt that died**, so attempt n+1's advice block and rework context carry them instead of silently dropping them.

**Team state lives in the core store, not the bus.** `AgentSession.team: { plan_rev, plan: steps[], floor_band, high_risk, gate_seq, stream_floor }` is written by the actor in the same batch as each `plan.accepted`, `plan.revised` or pause. It is additive, `#[serde(default)]`. `stream_floor` is the `event_id` of the run's `path.started`: the `TeamPublisher` reports it back with `Command::TeamStreamFloor { run_id, event_id }`, the `self_tx` write-back pattern the bus pollers already use (`src/bus.rs:27-33`). The per-run `WorkflowDef` `"<run>:plan-<rev>"` is re-registered from `team.plan` at boot, because the workflow registry is in-memory (`src/workflow.rs:980-997`).

**Replay procedure (supervisor spawn):**
1. Snapshot `T = tail_event_id()` on the spawning thread (`src/bus.rs:344-353`, the same "cannot miss an event emitted right after" rule the launch poller uses).
2. Ask the actor for live runs (`Command::LiveTeamRuns`: status `Executing` or `AwaitingHuman`), with their `team` state.
3. For each, `poll("wicked.team.**", stream_floor - 1, …)` up to `T`, keep that run's rows, and fold them into the supervisor's state. The previous attempt's unresolved findings become the redriven attempt's carried findings, marked `carried_from_attempt`.
4. Start the live cursor at `T`.

Because consumers are keyed by entity id (§6.1), a row seen both in replay and live is harmless.

**Why replay and not a stable cursor, against retention.** A stable cursor, resuming from the predecessor's last acknowledged row (the exec seam's `reclaim_predecessor_cursors`, `src/cli_runner.rs:1480`), re-delivers only rows **after** the acknowledgement. The supervisor's in-memory attempt state was built from rows **before** it, so a stable cursor would still need a replay to rebuild that state. Replay alone is sufficient, so it is the one mechanism.

Retention bounds the replay:
- The bus deletes rows 24 h after emission (`dedup_expires_at`; `reqs/SPEC.md:804-818`).
- An attempt lasts at most `WICKED_UNIT_TIMEOUT_SECS` (default 7200 s; `src/acp_runner.rs:5911-5914`, `src/execute_wrapped.rs:1185-1189`), so every row of the attempt that died is inside the window.
- A run that has been live longer than 24 h (e.g. paused at a gate for days) can have lost older rows. What those rows fed is already durable: the plan in `AgentSession.team`, each finished attempt's ledger in its unit's `ledger.folded` snapshot (`UnitEvidence.team`, §4.4).
- When `path.started` itself is gone, the supervisor logs one `stream_gap` line per run and carries nothing from before the gap. For any unit whose attempt rows are missing, S publishes `ledger.folded{final_pass:"stream_gap", team_pause:true}` from what it could rebuild, and the gate opens on that row: an incomplete team record goes to a human, never auto-approved.

**Cost:** one live-tier scan at spawn, bounded by 24 h of rows.

### 4.8 Failure and fallback paths: what is persisted, on the bus, referenced and replayed

The rule every row obeys: **nothing on the bus references a fact that is not there, and no replay publishes a fact the run has already moved past.** A missing referent is only legitimate when retention deleted it, and consumers treat that as "aged out", not as an error.

| # | Path | (a) Persisted (core store) | (b) On the bus | (c) References | (d) A later replay / drain |
|---|---|---|---|---|---|
| 1 | `path.started` fails past the bound | `AgentSession.team.transport = "none"` + reason, written after the tombstone; every unit's `UnitEvidence.team` snapshot says `transport:"none"` | nothing for the run, ever (no other team fact is generated for a `transport:"none"` run) | none: no team rows exist to reference | `superseded_run` tombstone ⇒ **publishes nothing** for the run (P1 (e)) |
| 2 | `plan.accepted` fails past the bound | the plan rev in `AgentSession.team.plan`; the `team_transport` pause (session + open interaction row, `src/actor.rs:6036-6046`) | the run's earlier facts; not `plan.accepted`, nor anything E queued after it (FIFO) | `plan.proposed` exists and is referenced by nothing unpublished | approve: the queued lines publish in order. Continue without team: tombstone ⇒ nothing more, plus row 1's state. Reject: tombstone, then only `path.ended` |
| 3 | `gate.opened` fails past the bound | the pause, durable as every pause | not `gate.opened`, nor E's later facts (FIFO) | `gate.decided` cannot land first (FIFO), so it never dangles | lands in order when the bus returns; a gate answered meanwhile makes `gate.decided` required (row 4) |
| 4 | `gate.decided` fails past the bound | the decision in the resolved interaction row; the run stays paused `team_transport` | `gate.opened` but not its decision | nothing references the missing decision (`Resumed` and the dispatch wait for it) | as row 2 |
| 5 | `step.claimed` fails (R) | the attempt's snapshot with `transport:"none"` | nothing from R for that attempt | none | attempt tombstone ⇒ nothing for that attempt |
| 6 | Bus absent at boot (T0: daemon-level notice only, `/health.warnings` `bus.unavailable`; the per-run state below lands with P1) | `transport:"none"` on every run launched without a bus; snapshots built locally, `ledger_source:"no_bus"` | nothing | `gate.opened` is not published (no bus); the unit's gate reads `UnitEvidence.team` | nothing: no lines are generated or spooled without a bus |
| 7 | Final-pass timeout (worker synthesizes) | `UnitEvidence.team` = the synthesized fail-closed ledger | `gate.opened{ledger_ref:null, ledger_source:"synthesized"}`; no `ledger.folded` for the attempt, or an `unused` one if S published just before its deadline | `ledger_ref` null ⇒ consumers read `UnitEvidence.team` | S's spooled fold line is tombstoned at its deadline ⇒ never published late |
| 8 | Rows deleted by the 24 h retention | `AgentSession.team`; every `UnitEvidence.team` snapshot | recent rows only | `re`/`ledger_ref` to a deleted row = aged out: the read route falls back to the persisted snapshot (§6.4 of DES-001; T8 (b)) | compaction removes published lines, so an old line cannot re-publish a deleted row's fact |
| 9 | Supervisor / daemon restart mid-step | `AgentSession.team`; the dead attempt's snapshot is never written (the attempt never folded) | the dead attempt's rows (within retention) | the redriven attempt n+1 references attempt n's findings as `carried_from_attempt` (rows that exist, or their persisted copy) | S's replay rebuilds state from `stream_floor`; the outbox drains in order; nothing is re-published twice (keys) |
| 10 | Restart with the stream gone (`path.started` aged out) | as 9 | nothing for the gap | `ledger.folded{final_pass:"stream_gap"}` references only rows it could read | nothing to replay for the gap; the gate pauses (team_pause) |
| 11 | Council with no verdict | `dispute:{verdict:"no_verdict", reason}` in the ledger; the `team_dispute` pause | `council.called`, `council.ruled{verdict:"no_verdict"}`, `ledger.folded`, `gate.opened{kind:"team_dispute"}` | all present | nothing special: normal FIFO drain if any failed |
| 12 | Crash between a tombstone and the store write | the store is updated on boot (row 1's rule) | nothing for the superseded facts | none | the tombstone is already written ⇒ nothing (P1 (f)) |



- **Type:** `wicked.team.<noun>.<past-tense-verb>` — four segments, lowercase (`reqs/SPEC.md:379-393`). The domain segment `team` is a functional domain like `gate` in `wicked.gate.eval.requested` (`src/cli_runner.rs:88`); the `domain` column (publisher identity) is `wicked-core` for engine-published events and `wicked-crew` for the human-originated ones (§7).
- **Envelope (every payload):**

```jsonc
{
  "run_id": "<run>",           // always
  "ord": 3,                    // the unit; null on run-level events
  "attempt": 1,                // null on run-level events
  "by": "claude#1",            // who the row is about or authored it: seat instance | "engine" | "human" | "council:<task_id>". NOT the owner: the owner is fixed per type (§7)
  "at": 1758600000000,         // publisher's epoch ms (the bus stamps emitted_at too)
  "re": null                   // causal reference: "<noun>.<verb>#<entity id>" or null
  // …event fields
}
```

- **Caps:** strings capped as stated per event, at a UTF-8 boundary (`cap_utf8`, `src/team.rs:131`). Payloads stay far under the 1 MB bus cap (`reqs/SPEC.md:371`); the one large event (`gate.opened`) caps its transcript at 256 KB and says so.
- **Catalog:** every type below is added to `crates/wicked-governance/seed/event-catalog-annotations.json` (the generator fails on an unknown key, `seed/README.md:23-24`), and `gen_event_catalog.py --check` stays green.

## 6. The events

Twenty-five types. `Key` is the idempotency key's parts after `["team", <type>, run_id]`. `Owner` is the one component that publishes it; `Consumers` read it.

Owners: **E** engine (actor, via the `TeamPublisher`), **S** supervisor, **R** attempt runner (worker thread + its carrier). Consumers additionally: **X** crew (relay + read routes), **U** studio. The actor consumes **no** bus row (§4.0); §7 is the full matrix.

| # | Type | Owner (sole publisher) | Consumers | Key parts | When |
|---|---|---|---|---|---|
| 1 | `wicked.team.path.started` | E | S, X, U | — | launch admitted; PA seat known (required, §4.1) |
| 2 | `wicked.team.path.scored` | E (a diff score arrives as the supervisor's `Command::TeamRescored`, §4.0) | X, U | `score_source` | intent score at plan time; diff re-score at checkpoints and `step.completed` (§8.7) |
| 3 | `wicked.team.plan.proposed` | E (after accepting the command or step result that carries it, §4.0) | S, X, U | `proposal_id` | a plan or plan change was submitted (§8.4) |
| 4 | `wicked.team.plan.revised` | E | S, X, U | `plan_rev` | the plan grew (§8.7) |
| 5 | `wicked.team.plan.accepted` | E | S, X, U | `plan_rev` | composed, floor-filled, approved or auto-released (required, §4.1) |
| 6 | `wicked.team.plan.refused` | E | X, U | `proposal_id` | compose refused a proposal; the run keeps its accepted rev |
| 7 | `wicked.team.member.joined` | S | X, U | `ord`, `attempt`, `member_id`, `open_seq` | a monitor session opened (or failed) |
| 8 | `wicked.team.member.left` | S | X, U | `ord`, `attempt`, `member_id`, `open_seq` | budget exhausted, failed, closed |
| 9 | `wicked.team.step.claimed` | R | S, X, U | `step_id`, `attempt`, `by` | before the step's turn starts |
| 10 | `wicked.team.checkpoint.reached` | R (carrier) | S, X, U | `ord`, `attempt`, `seq` | terminal `tool_call_update` of a team unit |
| 11 | `wicked.team.finding.raised` | S | R, X, U | `ord`, `attempt`, `raise_seq` | confirmed, above-bar, first-seen finding |
| 12 | `wicked.team.advice.delivered` | R (carrier mid-turn; worker thread at the boundary and at attempt end) | S, R, X, U | `ord`, `attempt`, `raise_seq`, `delivery_id` | one row per finding per delivery |
| 13 | `wicked.team.advice.answered` | R (worker thread, `ADVICE` lines) | S, X, U | `ord`, `attempt`, `raise_seq`, `answered_in` | end of the PA's turn |
| 14 | `wicked.team.help.requested` | R (worker thread / carrier, `HELP:` lines) | S, X, U | `help_id` | the PA asks the team |
| 15 | `wicked.team.help.answered` | S (a member's turn) | R, X, U | `help_id`, `answer_id` | a member answers |
| 16 | `wicked.team.change.requested` | S (a member's turn) | R, X, U | `change_id` | a member asks for plan steps (§8.7) |
| 17 | `wicked.team.step.completed` | R | S, X, U | `step_id`, `attempt`, `by` | the step's turn returned |
| 18 | `wicked.team.step.reviewed` | R (worker thread, `STEP` line) | S, X, U | `step_id`, `attempt` | the PA accepted or rejected a member's step (§8.8) |
| 19 | `wicked.team.finding.settled` | S (hold round) | X, U | `ord`, `attempt`, `raise_seq` | held / withdrawn / superseded |
| 20 | `wicked.team.council.called` | S | X, U | `ord`, `attempt`, `subject` | an unresolved HIGH, or a member-step dispute |
| 21 | `wicked.team.council.ruled` | S (the verdict of the council it convened) | R, X, U | `ord`, `attempt`, `subject` | `convene_decision` returned |
| 22 | `wicked.team.ledger.folded` | S | R, X, U | `ord`, `attempt` | the final pass folded the attempt's stream into its `TeamLedger` (§8.11) |
| 23 | `wicked.team.gate.opened` | E | X, U | `gate_id` | a gate opened (`unit_review`, `plan_approval`, `team_dispute`, `team_transport`) (required) |
| 24 | `wicked.team.gate.decided` | E (human decisions included, after `confirm_gate` accepts them) | X, U | `gate_id` | a gate decided (required) |
| 25 | `wicked.team.path.ended` | E | S, X, U | — | the run reached a terminal state |

### 6.1 Identity rule for every key (rev 6; closes the class)

**The rule.** No idempotency key is derived from content that a later, distinct request can repeat. Keys come from a **producer-assigned sequence or id**: a counter the one producer of that row increments, an id the producer mints, or the id of the upstream row or request that caused it. The one exception is a hash of the **full** payload, where resending an identical payload must dedup, and no row in this table uses it.

**Why it matters.** `BusDb::emit` resolves a duplicate key to the existing row and returns its id (`src/bus.rs:316-342`). A key built from content therefore makes a second, distinct request **vanish without an error**. That is what happened to the rev 4 and rev 5 `help_id`, which hashed the question text.

**Content hashes that stay (as payload, not key).** `finding_id` and `line_key` remain what DES-001 §4.6 made them: the finding's *identity*, used for dedup and correlation **inside** the supervisor, which is the single producer and emits each finding once per attempt. Neither is ever a bus key.

**Every key, with its source of uniqueness** (all keys are prefixed `["team", <event_type>, <run_id>]`, §4.1):

| # | Event | Key parts | Source of uniqueness (who assigns it) | Changed |
|---|---|---|---|---|
| 1 | `path.started` | — | the run id: one path per run, minted at launch (E) |  |
| 2 | `path.scored` | `score_source` | `"intent:" + proposal_id` (E, one per proposal) or `"diff:" + ord + ":" + attempt + ":" + rescore_seq`: `rescore_seq` is assigned by the supervisor and carried in `Command::TeamRescored`, so E publishes it unchanged |  |
| 3 | `plan.proposed` | `proposal_id` | `"p-" + deterministic_key([run, by, source])`. Every `source` is an id the engine received with the command (§4.0): the launch's session id (user plan, preset); the `understand` step's `ord:attempt` from its `ApplyStepResult` (PA plan); `ord:attempt:plan_block_seq` (a `PLAN+` block, numbered by the step-output parser); the accepted `change_id` (member request); the `gate_id` (approval edit); the `Core::propose_plan` call's request id (a mid-run human plan edit, minted by crew per POST) | rev 7: every source arrives on a command, not the bus |
| 4 | `plan.revised` | `plan_rev` | the engine's per-run plan counter |  |
| 5 | `plan.accepted` | `plan_rev` | the same counter |  |
| 6 | `plan.refused` | `proposal_id` | one refusal per proposal |  |
| 7 | `member.joined` | `ord`, `attempt`, `member_id`, `open_seq` | S's per-attempt member id and its opening counter (a monitor is re-opened after a failed turn, `src/team.rs:667`, `:885`, `:909`) |  |
| 8 | `member.left` | `ord`, `attempt`, `member_id`, `open_seq` | the opening it closes |  |
| 9 | `step.claimed` | `step_id`, `attempt`, `by` | the engine's attempt counter: every (re-)dispatch of a step is a new attempt |  |
| 10 | `checkpoint.reached` | `ord`, `attempt`, `seq` | the carrier's per-attempt checkpoint counter |  |
| 11 | `finding.raised` | `ord`, `attempt`, `raise_seq` | S's per-attempt emission counter; `finding_id` rides in the payload |  |
| 12 | `advice.delivered` | `ord`, `attempt`, `raise_seq`, `delivery_id` | `delivery_id` = the carrier's `steer_id`, or `"boundary:" + step_id + ":" + attempt` of the step whose prompt carried it, or `"end:" + attempt` (the attempt-end not-delivered row, now R's instead of an S sweep) | rev 7: the not-delivered row moved from S to R, so the type has one owner |
| 13 | `advice.answered` | `ord`, `attempt`, `raise_seq`, `answered_in` | the `step_id:attempt` whose output carried the line |  |
| 14 | `help.requested` | `help_id` | `"h-" + deterministic_key([run, ord, attempt, by, help_seq])`; `help_seq` is R's per-attempt counter over `HELP:` lines (the step-output parser and the carrier share it) | rev 7: member plan-change requests moved to `change.requested` |
| 15 | `help.answered` | `help_id`, `answer_id` | `answer_id` = S's member-turn id | rev 7: S only; no crew or PA answers (a human-directed question is S1 elicitation; a PA verdict on a member request is part of its step output) |
| 16 | `change.requested` | `change_id` | `"c-" + deterministic_key([run, ord, attempt, member_id, change_seq])`; `change_seq` is S's per-attempt counter | new in rev 7 |
| 17 | `step.completed` | `step_id`, `attempt`, `by` | the attempt counter |  |
| 18 | `step.reviewed` | `step_id`, `attempt` | the reviewed attempt; the first `STEP` line for it wins |  |
| 19 | `finding.settled` | `ord`, `attempt`, `raise_seq` | the finding's emission id |  |
| 20 | `council.called` | `ord`, `attempt`, `subject` | `"finding:" + raise_seq` or `"step:" + step_id + ":" + attempt` |  |
| 21 | `council.ruled` | `ord`, `attempt`, `subject` | the call it answers |  |
| 22 | `ledger.folded` | `ord`, `attempt` | one final pass per attempt | new in rev 7 (was `gate.opened{kind:"unit_review"}` published by S, which gave `gate.opened` two owners) |
| 23 | `gate.opened` | `gate_id` | `AgentSession.gate_seq` (every engine gate, including `unit_review` and `team_transport`) | rev 7: E only |
| 24 | `gate.decided` | `gate_id` | one decision per gate; E publishes human decisions after `confirm_gate` accepts them |  |
| 25 | `path.ended` | — | one terminal state per run |  |

**How the rule is enforced.** T1 adds a table-driven test over all 25 types. For every event type, two distinct logical requests that share all content except the producer-assigned part produce two rows; a re-publish of the same logical request produces one. The `help.requested` fixture is two `HELP:` lines with the same question and different context: two rows, two `help_id`s. The `change.requested` fixture is two member requests with the same text and different steps: two rows.

Exact payloads (envelope fields omitted after the first):

```jsonc
// 1 — path.started (E), at launch admission (LaunchSpec.primary resolved, §8.1)
{"run_id":"r1","ord":null,"attempt":null,"by":"engine","at":…,"re":null,
 "cli":"claude#1",                 // the PA seat instance
 "selection":"chosen",             // "chosen" | "random"
 "roster":["claude#1","claude#2","codex"],
 "request":"…",                    // the problem text, ≤8 KB
 "workflow":"feature",             // the preset name the launch named, or null (§8.4)
 "plan":false}                     // true when the launch carried a user-composed plan

// 2 — path.scored (E): S4 assess() on the declared touch set; a diff score arrives from S as Command::TeamRescored (§4.0)
{"score_source":"intent:p-…",     // "intent:<proposal_id>" | "diff:<ord>:<attempt>:<rescore_seq>" (§6.1)
 "basis":"intent",                 // "intent" | "diff"
 "score":70,"deterministic":70,"reasons":["destructive ⇒ floor 70","reach 21-100 dependents: +60"],
 "model":null,                     // {"add":10,"rationale":"…"} or null
 "signals":{"changed_symbols":4,"dependents":37,"products":1,"contract_change":false,"test_gap":0.5,
            "critical":false,"destructive":true,"truncated":false},   // null when the graph was unusable
 "plan":{"monitors":3,"depth":"deep","post_hoc_reviewer":true,"post_hoc_other_cli":true},
 "tree":null}                      // the T_k the diff was taken at; null for "intent"

// 3 — plan.proposed (E, after accepting the command or step result that carries it, §4.0); steps name catalog entries plus the step fields §8.3 allows
{"proposal_id":"p-…",             // derived by the engine from the command's source id (§6.1); plan_rev is assigned on acceptance
 "base_rev":null,                  // the accepted rev this changes; null for the first plan
 "kind":"initial",                 // "initial" | "change" (a PA PLAN+, incl. an accepted member request) | "edit" (a human edit: an approval edit or Core::propose_plan)
 "by":"claude#1",                  // PA seat | "human"
 "preset":null,                    // the preset name when launched from one
 "steps":[{"catalog":"understand","id":"understand"},
          {"catalog":"design","id":"design","instructions":"…"},
          {"catalog":"test_plan","id":"test-plan","owner":"team"},          // owner: "pa" (default) | "team"
          {"catalog":"build","id":"build","depends_on":["design"]},        // role, pin and gate come from the entry
          {"catalog":"review","id":"review","gate":{"human_confirm_if":"verdict_not_pass"}},   // gate raised; GateSpec is externally tagged (workflows/feature.json:8-11)
          {"catalog":"deliver","id":"deliver"}],
 "monitors":{"asked":1},           // the PA's own ask; target = min(max(band monitors, asked), TeamLimits.max_monitors)
 "asks":["a codex seat to cross-check the migration"],   // ≤2 KB each, ≤8
 "touch":["src/retire.ts","src/coverage.ts"],            // predicted touch set, ≤64 paths; the intent score's input
 "override":null,                  // manual mode only (§8.5): {"remove":["review"],"reason":"…"}
 "rationale":"…"}                  // ≤4 KB

// 4 — plan.revised (E): the plan grew (§8.7)
{"plan_rev":2,"by":"engine",       // always "engine" (it assigns plan_rev)
 "proposal_id":null,               // the plan.proposed{kind:"change"} this composes; null for a floor raise
 "reason":"floor_raised",          // "floor_raised" | "pa_added" | "member_request"
 "from_band":"20-39","to_band":"40-69","high_risk":false,
 "added":[{"catalog":"test_plan","id":"test-plan","added_by":"floor","floor_reason":"band 40-69 requires test_plan","late":false},
          {"catalog":"design","id":"design","added_by":"floor","floor_reason":"band 40-69 requires design","late":true}]}   // late: its catalog position precedes a done step

// 5 — plan.accepted (E): composed, floor-filled, and approved or auto-released
{"plan_rev":1,"workflow_id":"r1:plan-1",
 "by":"engine",                    // "engine" (auto release) | "human" (approved at the plan_approval gate)
 "band":"40-69","high_risk":false,"mode":"manual",       // mode: "auto" | "manual" (§8.6)
 "steps":[ /* the composed steps, each with "added_by":"plan"|"floor" and "floor_reason" when added by floor */ ],
 "override":null,                  // as recorded, manual mode only
 "proposal_id":"p-…"}              // the proposal this accepts

// 6 — plan.refused (E): compose refused a proposal; the run keeps its accepted rev
{"proposal_id":"p-…","base_rev":1,"by":"engine","reason":"step `review` lowers its entry's gate"}   // reason: free text naming the refusing rule (floor, pin, schema, "override in auto mode")

// 7 / 8 — member.joined / member.left (S)
{"ord":3,"attempt":1,"member_id":"m1","open_seq":1,   // open_seq: this member's session openings in the attempt (re-opened after a failed turn)
 "seat":"claude#2","role":"monitor","status":"attached",   // role: "monitor" (the only value today); status: "attached" | "failed", as S2 spells it (src/team.rs:1070)
 "reason":"path.scored#2 monitors=3","error":null}
{"ord":3,"attempt":1,"member_id":"m1","open_seq":1,"seat":"claude#2","status":"budget_exhausted",   // "completed" | "budget_exhausted" | "failed" | "timed_out", as S2's LedgerMonitor spells it (src/team.rs:966-973)
 "batches":10,"error":null}

// 9 — step.claimed (R): the attempt's worker thread, for a PA step or a member's step alike (a member step runs as a normal unit, §8.8)
{"ord":3,"attempt":1,"by":"claude#1",
 "step_id":"build","role":"creator","kind":"build","phase":"build",
 "criterion":"…",                  // ≤2 KB
 "baseline_tree":"<tree id>",      // null when unbound (then: not monitored, disclosed)
 "repo":{"workdir":"…","git_dir":"…"},
 "code_graph_db":"…"}              // what AttachCtx carries today (src/team.rs:625-645)

// 10 — checkpoint.reached (R: the attempt's ACP carrier), only for a unit with a team context (the fields of today's unitCheckpoint, src/event.rs:1210)
{"ord":3,"attempt":1,"by":"claude#1","seq":17,"tool_call_id":"toolu_…","kind":"edit",
 "title":"Edit src/retire.ts","status":"completed","paths":["src/retire.ts"]}   // kind: ACP ToolKind verbatim; status: "completed" | "failed" (src/team.rs:266)

// 11 — finding.raised (S; `by` is the authoring member). DES-001 §4.6's parse → bar → confirm → dedup
{"ord":3,"attempt":1,"by":"claude#2","re":"checkpoint.reached#17",
 "raise_seq":4,                     // the supervisor's per-attempt emission counter: the key (§6.1)
 "finding_id":"f-3fa9c2e1d0b4a7e6","member_id":"m1",   // identity, never a key. On main it is sha256(path ‖ normalized evidence) (src/team.rs:386-392)
 "line_key":"l-9c0e4b7a1d2f3e58","anchor":"retire","anchor_source":"graph",   // DES-001 §4.6 fields NOT built on main (no anchor or line_key in src/team.rs); T6 builds them. anchor_source: "graph" | "hunk" | "none"
 "severity":"high","path":"src/retire.ts","line":41,   // severity: "high" | "medium" ("low" is dropped at the bar, src/team.rs:345, :1585-1594)
 "evidence":"fetchCoverage(scope).then(setCount)","claim":"…","suggestion":null,
 "tree":"<T_k>","in_diff":true,"corroborated_by":[]}

// 12 — advice.delivered (R only): the carrier mid-turn, the worker thread at the next step boundary, or the worker thread's attempt-end row
{"ord":3,"attempt":1,"by":"engine","re":"finding.raised#4",
 "raise_seq":4,"finding_id":"f-3fa9c2e1d0b4a7e6",   // one row per finding
 "delivery_id":"s-…",                // steer_id | "boundary:<step_id>:<attempt>" | "end:<attempt>" (§6.1)
 "steer_id":"s-…",                   // acp_steering only: groups the rows one steer request carried; null otherwise
 "channel":"acp_steering",         // "acp_steering" | "boundary" | "none"
 "outcome":"injected",             // "injected" | "turn_ended" | "refused" | "not_delivered", as S3's SteerOutcome spells them (src/team.rs:1704-1709); an "end:" row is always channel "none", outcome "not_delivered"
 "detail":null}

// 13 — advice.answered (R): the worker thread, from the PA's `ADVICE <id>: ACCEPT|DECLINE — <reason>` lines (S3 parser, src/team.rs:1989)
{"ord":3,"attempt":1,"by":"claude#1","re":"finding.raised#4","raise_seq":4,"answered_in":"build:1",   // the step:attempt whose output carried the line
 "finding_id":"f-3fa9c2e1d0b4a7e6","disposition":"declined","reason":"campaign.rs:325 documents the exclusion"}   // "accepted" | "declined" (src/team.rs:1676-1679)

// 14 / 15 — help.requested (R: the PA asks) and help.answered (S: a member answers)
{"ord":3,"attempt":1,"by":"claude#1","help_id":"h-1a2b…",
 "help_seq":2,                      // R's per-attempt counter; help_id is derived from it, never from the question
 "question":"…","context":"…"}     // ≤4 KB each
{"ord":3,"attempt":1,"by":"claude#2","re":"help.requested#h-1a2b…","help_id":"h-1a2b…",
 "answer_id":"t-…",                 // S's member-turn id
 "answer":"…","evidence":["src/x.rs:41"]}

// 16 — change.requested (S): a member asks for plan steps (§8.7). The PA's verdict comes back in its step output, not here.
{"ord":3,"attempt":1,"by":"claude#2","change_id":"c-…","change_seq":1,
 "steps":[{"catalog":"test_plan","id":"test-plan-2"}],      // the plan.proposed step shape
 "reason":"…"}

// 17 — step.completed (R): the attempt's worker thread after run_unit_streaming returns, for a PA step or a member's step alike
{"ord":3,"attempt":1,"by":"claude#1","step_id":"build","status":"ok",   // "ok" | "failed" | "cancelled" | "elicitation_failed" | "timed_out": StepStatus (src/workflow.rs:175-196) exactly as status_to_str spells it (src/cli_runner.rs:285-296)
 "tree":"<T_final>","output_bytes":12345,"output_ref":"unit:r1:3:1"}    // where get_work_output finds it

// 18 — step.reviewed (R): the PA's `STEP <id>: ACCEPT|REJECT <to:member|pa> — <reason>` line at its next turn (§8.8)
{"ord":4,"attempt":1,"by":"claude#1","re":"step.completed#test-plan","step_id":"test-plan",
 "verdict":"accepted",             // "accepted" | "rejected"
 "to":null,                        // on "rejected": "member" | "pa"
 "reason":"…"}

// 19 — finding.settled (S): the hold round (DES-001 §4.7 step 5); also "superseded" from re-confirmation
{"ord":3,"attempt":1,"by":"claude#2","re":"advice.answered#4","raise_seq":4,"finding_id":"f-3fa9…",
 "status":"held",                  // "held" | "withdrawn" | "superseded"
 "reason":"no reply (counted as hold)","final_line":43}

// 20 — council.called (S): one per unresolved HIGH or member-step dispute (DES-001 §6.3 input, verbatim)
{"ord":3,"attempt":1,"by":"engine","re":"finding.settled#4",
 "subject":"finding:4",            // "finding:<raise_seq>" | "step:<step_id>:<attempt>"
 "finding_id":"f-3fa9…",           // identity, for a finding subject
 "trigger":"unresolved_high",      // "unresolved_high" | "member_step" (§8.8)
 "question":"…","positions":[{"by":"worker claude#1","position":"YES — the refusal stands","reason":"…"},
                             {"by":"monitor claude#2","position":"NO — the finding stands","reason":"…"}],
 "evidence":"…",                   // ≤16 KB: finding, T_final hunk, criterion, tree id
 "excluded_seats":["claude#1","claude#2"],
 "transcript":[1201,1207,1215,1220]}   // event_ids of raised/delivered/answered/settled for this finding

// 21 — council.ruled (S): the DecisionVerdict its convene_decision call returned (src/decision.rs:292-310); `by` names the council
{"ord":3,"attempt":1,"by":"council:<task_id>","re":"council.called#finding:4","subject":"finding:4",
 "verdict":"no",                   // "yes" | "no" | "no_verdict"
 "reason":null,                    // no_verdict: "no_quorum" | "seats_benched" | "error" | "timeout" | "cap"
 "task_id":"…","consensus":true,"agreement_pct":67,"dissent":["…"],"returned":3,"seated":3}   // DecisionVerdict's fields; it has no seat list (src/decision.rs:292-310)

// 22 — ledger.folded (S): once per attempt, the fold of the attempt's stream (DES-001 §7 teamLedger + transcript)
{"ord":3,"attempt":1,"by":"engine","final_pass":"completed",   // "completed" | "timed_out" | "skipped" | "stream_gap"
 "ledger":{ /* TeamLedger exactly as DES-001 §7: monitors[], findings[] (status, delivery, monitor_reply, dispute), rejected{}, team_pause */ },
 "transport":"bus",               // "bus" | "none" (the local no-bus snapshot, §4.1)
 "transcript":{"from_event_id":1180,"to_event_id":1290,"count":31,
               "events":[ /* the attempt's wicked.team.* rows, event_id-ordered, ≤256 KB; "truncated":true past the cap */ ]}}

// 23 — gate.opened (E only). kind:"unit_review": the actor, when it applies the step result (apply_step_result, src/actor.rs:4269), before the fold emits GateEvaluated (src/pipeline.rs:1519)
{"gate_id":"g-r1-3",              // "g-<run>-<gate_seq>" for every kind (AgentSession.gate_seq)
 "kind":"unit_review",             // "unit_review" | "plan_approval" | "team_dispute" | "team_transport"
 "ord":3,"attempt":1,"by":"engine",
 "ledger_ref":"ledger.folded#3:1", // the S row the gate used; null when the snapshot was synthesized by the worker (final-pass timeout) or built with no bus
 "ledger_source":"folded"}         // "folded" (ledger_ref set) | "synthesized" | "no_bus"; with null ledger_ref, consumers read UnitEvidence.team

// 23 (cont.) — gate.opened, kind:"plan_approval" (E), when the approval matrix requires it (§8.6)
{"gate_id":"g-r1-4",              // gate_seq 4: a re-opened gate for a newer plan_rev gets a new one
 "kind":"plan_approval","ord":2,"attempt":1,"by":"engine",   // ord = the first not-yet-dispatched unit
 "reviewing_ord":1,"plan_rev":1,"band":"70-100","high_risk":true,"mode":"auto",
 "reason":"high_risk",             // "manual_mode" | "high_risk" | "into_high_risk" | "override"
 "diff":{"from_rev":null,"added":["architecture","security_review"]}}

// 24 — gate.decided (E only): the fold decides unit_review; a human decision is published by E after confirm_gate accepts it (§4.0)
{"gate_id":"g-r1-3",              // the gate this decides
 "kind":"unit_review",            // "unit_review" | "plan_approval" | "team_dispute" | "team_transport"
 "ord":3,"attempt":1,"by":"engine","re":"gate.opened#g-r1-3",
 "decision":"paused",              // unit_review: "allow" | "deny" | "paused"; human (any kind): "human_approved" | "human_amended" | "human_rejected"
 "combined":true,"team_pause":true,"unresolved":["f-3fa9…"]}

// 25 — path.ended (E), at finalize/fail/cancel
{"run_id":"r1","ord":null,"attempt":null,"by":"engine","status":"completed"}   // "completed" | "failed" | "cancelled": the terminal SessionStatus values (src/domain.rs:17-31)
```

## 7. Producer / consumer matrix (§4.0)

Every event has **one owner**, the only component that publishes it. **E** is the engine (the actor, through the `TeamPublisher`), **S** the supervisor, **R** the attempt runner (the attempt's worker thread plus its carrier). **X** is crew (relay and read routes), **U** studio. The actor consumes no row.

| # | Event | Owner (sole publisher) | Consumers | Does a consumer change state from it? |
|---|---|---|---|---|
| 1 | `path.started` | E | S (arms the run's state), X, U | S: own state only |
| 2 | `path.scored` | E | X, U | no |
| 3 | `plan.proposed` | E | S (member targets), X, U | S: own state only |
| 4 | `plan.revised` | E | S, X, U | S: own state only |
| 5 | `plan.accepted` | E | S, X, U | S: own state only |
| 6 | `plan.refused` | E | X, U | no |
| 7 | `member.joined` | S | X, U | no |
| 8 | `member.left` | S | X, U | no |
| 9 | `step.claimed` | R | S (attaches the attempt), X, U | S: own state only |
| 10 | `checkpoint.reached` | R | S (batch and re-score trigger), X, U | S: own state only; its re-score reaches the actor as S's own measurement (`TeamRescored`), not as the row |
| 11 | `finding.raised` | S | R (steer mid-turn; render at the boundary), X, U | R: its own prompt only (advisory) |
| 12 | `advice.delivered` | R | S (ledger), R (dedup), X, U | S, R: own state only |
| 13 | `advice.answered` | R | S (dispositions, hold round), X, U | S: own state only |
| 14 | `help.requested` | R | S (a member turn), X, U | S: own state only |
| 15 | `help.answered` | S | R (renders it to the PA), X, U | R: its own prompt only |
| 16 | `change.requested` | S | R (renders it to the PA), X, U | R: its own prompt only; the engine gets the steps only from the PA's `PLAN+` in `ApplyStepResult` |
| 17 | `step.completed` | R | S (final pass), X, U | S: own state only |
| 18 | `step.reviewed` | R | S (ledger; a `HOLD` may trigger a council), X, U | S: own state only; a council verdict reaches the actor as S's own `TeamStepRuling` |
| 19 | `finding.settled` | S | X, U | no |
| 20 | `council.called` | S | X, U | no |
| 21 | `council.ruled` | S | R (renders it to the PA), X, U | R: its own prompt only |
| 22 | `ledger.folded` | S | R (gate wait: the snapshot becomes its `UnitEvidence.team`), X, U | R: its own step result only; the actor receives it on `ApplyStepResult` |
| 23 | `gate.opened` | E | X, U | no |
| 24 | `gate.decided` | E | X, U | no |
| 25 | `path.ended` | E | S (forgets the run), X, U | S: own state only |

**Assertions (checked by T1, T4 (e), T8 (g)):**
1. **No fact has two owners.** Each of the 25 types appears once in the Owner column, with a single letter. Rev 7 split the three types that had two owners: `gate.opened` (the S snapshot is now `ledger.folded`), `help.requested` (member plan changes are now `change.requested`), and `help.answered` (the human and PA answer paths are gone). The attempt-end not-delivered row moved from S to R.
2. **No consumer changes state it does not own.** Every consumer's effect is on its own state: the supervisor's attempt state, the runner's own prompt and evidence, the relay's broadcast, studio's view.
3. **The engine is changed only by commands.** These are the API (`launch_run`, `confirm_gate`, `propose_plan`, `put_preset`) and the actor's existing command channel: `ApplyStepResult` from the worker, `TeamRescored`/`TeamStepRuling` carrying the supervisor's own measurements, and `TeamPublished`/`TeamTransportFailed` from the publisher.
4. **Crew publishes nothing.** It relays, reads and POSTs.

## 8. Lifecycle, step by step

### 8.1 Start a path; choose the CLI (operator steps 1–3)

- `LaunchSpec` gains `primary: Option<String>` (`src/lib.rs:193-216`), a seat-instance key from `clis`. `None` means the engine picks uniformly at random among the eligible seats (the `seat_candidates` set S5 computes, `src/distribute.rs:11`) and records `selection:"random"`. Crew's `POST /runs` body gains `primary` (`LaunchSchema`, `packages/crew/src/api/routes.ts:476-500`); studio's composer gains a seat picker with a **Random** option.
- The PA seat is the run's **creator seat**. `teamed_distribution` (`src/distribute.rs:166`, result at `:174`) pins `winner` to the PA for every creator-role unit whose `owner` is `pa` (§8.8). `enforce_evaluator_distinct` (`src/distribute.rs:387`, fn `:547`) moves every evaluator unit off the creator seats.
- **What it does today when it cannot.** A **bench-free** roster with no distinct seat keeps the review/test unit on its creator seat, disclosed as `distinctness_fallback:"creator_seat"` (`src/distribute.rs:286-290`, set at `:417-426`). The plan is refused with `NoEligibleSeat` **only** when a bench caused the shortfall (`:388-409`). A distinct instance of the creator's own CLI (`claude#2` grading `claude#1`) is already accepted and disclosed as `"same_cli_instance"` (`:291-294`).
- **Team runs never use `creator_seat`** (operator decision, 2026-09-24). For a team run, `distribute_units_against_benched` refuses whenever `same_seat` is non-empty, with or without a bench. In order:
  1. a distinct CLI;
  2. else a distinct **instance** of an eligible CLI (`same_cli_instance`, core#595/#605 instance seats);
  3. else `NoEligibleSeat`, naming the unit and the missing instance.
- **"Can be provisioned" means configured and usable.** The instance must be in the launch roster (`LaunchSpec.clis`) and signed in in its own configuration home. Each instance has its own home (`seat_instance_root_name`, `crates/wicked-apps-core/src/spawn.rs:1295`, tested injective at `:3173`; the ACP carrier looks up instance keys at `src/acp_runner.rs:8461-8468`), and therefore its own login. The engine never mints or signs in an instance itself; crew adds the instances it has configured for the eligible CLIs to a team run's roster.
- **Non-team runs** (every consumer not yet migrated) keep today's `creator_seat` behaviour until their migration seam, which records the change (§11.3).
- Built in seam D1.
- The launch names **either** a phase selection (§8.4: a user-composed plan, or a preset) **or** nothing (the PA composes). The actor publishes `path.started` through the `TeamPublisher` when the launch is admitted.

### 8.2 The PA scores the path (operator step 4)

- A PA-composed run's first unit is the catalog's `understand` step on the PA seat (§8.3). A user or preset plan is scored from its declared touch set instead (§8.4). **One rule for every author:** a plan with a creator step and no declared `touch[]` (the PA's block omitted it, or the user's or preset's plan left it out) scores `no_graph_score` = 100, with the reason `"no declared scope"`. That puts it in band 70–100: the full floor, high risk, and so plan approval even in auto mode. A plan with no creator step and no `touch[]` scores 0. Its required deliverable is the **plan block** (§8.4): the predicted touch set, the phases the PA wants beyond the floor, and its asks.
- Scoring is S4, not the model. The engine builds `ChangeSignals` from the predicted touch set with a new `signals_from_paths(&[&str]) -> ChangeSignals`, beside `signals_from_diff` (`src/review_scale.rs:640`), and calls `assess(&signals, Graph::Ready{store, base_commit}, hook)` (`src/review_scale.rs:563`). The result is `path.scored{basis:"intent"}`. No graph, or a stale graph, scores `no_graph_score` (S4's fail-closed rule, `src/review_scale.rs:26-27`). The optional model hook may add 0/10/20 and never subtracts (`src/review_scale.rs:22-24`). The PA's own view of risk enters only through that hook and through phases it adds.
- The score picks a **band** (`THRESHOLDS.bands`, `src/review_scale.rs:285-290`; `plan_for`, `:552`), and the band sets the **floor** (§8.5).

### 8.3 The phase catalog (operator decision 1, 2026-09-23: no pre-canned workflows)

**One catalog of phase types, in one place: `src/catalog.rs`, a single `pub fn catalog() -> &'static [PhaseDef]`.** It is data and nothing else: no workflow file, no per-surface copy. Each entry **is** a `PhaseDef` (`src/workflow.rs:641`). That keeps every existing control working through the mechanism that already enforces it, with no second one:

| Control | Why a `PhaseDef` entry keeps it working |
|---|---|
| Gates | `plan_from_def` (`src/plan.rs:94`) copies `phase.gate` into `WorkUnit.gate` (`src/domain.rs:446`), and `should_pause` (`src/actor.rs:6333`) reads `prev.gate`. |
| Validator pins | `attach_pinned_validators` (`src/pipeline.rs:231`) zips the units with `def.phases` and attaches each `validator_pin`, refusing an unapproved one. |
| Repo-checks floor | Applies to every bound agent unit that changed the tree (`default_floor_applies`, `src/cli_runner.rs:868`), with or without a pin. |
| Evaluator ≠ creator | Comes from `phase.role`: `plan_from_def` copies it (`src/plan.rs:155` → `src/domain.rs:451`), and the fence enforces it (`src/distribute.rs:387`). |

A run's accepted plan is therefore composed into a **per-run `WorkflowDef`** (`src/workflow.rs:801`) whose phases are catalog entries with the plan's step fields applied (§8.4). It is registered in memory as `"<run>:plan-<rev>"` and planned by the existing `pre_distribute` path (`src/pipeline.rs:348`, def match at `:406`). The per-run def is an internal value, never a file.

**The entries**: twelve, the minimum set that covers every shipped workflow and every crew-registered def. §11.2 maps every current phase onto them.

| Catalog id | `kind` | `role` | `gate` | `gate_type` | `validator_pin` | `executes_code` | `executor` | Subsumes today (by workflow/phase) |
|---|---|---|---|---|---|---|---|---|
| `understand` | recon | neutral | auto | value | — | false | agent | feature/clarify, bug/triage, migration/— , chat/explore, survey-repo/{structure, stack, conventions, synthesize}, memories/gather, domain-graph-slice/identify, domain-extraction/{survey, analyze}, capture-learnings/{churn, hotspots}, steering-author/analyze, interactive-chat/understand, interactive-draft/outline, interactive-demo/scenes, qe-author-tests/recon |
| `test_plan` | test | neutral | auto | value | — | false | agent | bug/reproduce; formal test planning (new use) |
| `design` | recon | neutral | auto | strategy | — | false | agent | feature/design, migration/plan |
| `architecture` | recon | neutral | auto | strategy | — | false | agent | none today (new) |
| `build` | build | creator | auto | execution | `EVIDENCE_FLOOR_PIN` | true | agent | feature/build, bug/fix, migration/{execute, cutover, cleanup}, qe-author-tests/author |
| `produce` | build | creator | auto | value | — | false | agent | memories/store, domain-graph-slice/extract, domain-extraction/extract, capture-learnings/capture, steering-author/propose, collab/{propose, revise}, interactive-draft/draft, interactive-edit/edit, interactive-chat/revise, interactive-demo/spec, interactive-demo-reauthor/respec |
| `test` | test | evaluator | `{"human_confirm_if":"verdict_not_pass"}` | execution | `EVIDENCE_FLOOR_PIN` | false | agent | feature/test, bug/verify, migration/verify, domain-extraction/coverage |
| `review` | review | evaluator | auto | execution | `EVIDENCE_FLOOR_PIN` | false | agent | feature/adversarial-review, qe-author-tests/review |
| `critique` | review | evaluator | auto | execution | — | false | agent | feature/review, domain-graph-slice/validate, collab/{critique, verdict} (review of a non-code artifact, or a final read-through with no evidence floor) |
| `security_review` | review | evaluator | auto | execution | `EVIDENCE_FLOOR_PIN` | false | agent, `skill_ref` = the garden QE security specialist (id fixed in seam C1) | none today (new) |
| `run` | (per step) | neutral | auto | value | — | (per step) | tool | onboarding/{index, annotate}, domain-extraction/domain-graph, qe-author-tests/verify |
| `deliver` | build | neutral | auto | execution | — | true | tool | crew's composed deliver phase (`packages/crew/src/core/deliver.ts:789`, appended by `composeDeliverWorkflow` `:859`) |

`EVIDENCE_FLOOR_PIN` is `e2e7af1db9e48454` (`src/builtin_floors.rs:109`). `deliver` keeps the id `deliver` because the engine recognises the deliver unit by it (`src/deliver_lift.rs:54-60`). Its Tool command stays crew-authored: crew supplies `deliverPrScript(intent, opts)` (`packages/crew/src/core/deliver.ts:791`) as the `deliver` step's `executor` at launch, exactly the command its composed phase carries today; the deliver gate stays the engine's (`should_pause` `DeliverGate`, `src/actor.rs:6348-6355`, driven by `auto_deliver`, `src/lib.rs:201-205`).

**What a plan step may change on its entry (and nothing else).** The catalog entry is the floor of that step's controls; a step may only strengthen it.
- `instructions`: free text, as today (`src/workflow.rs:657`).
- `gate`: may be **raised** along `auto` < `human_confirm_if` < `human_confirm{unconditional:false}` < `human_confirm{unconditional:true}`, never lowered. That is how migration/cutover keeps its unconditional gate.
- `validator_pin`: may be **added** to an unpinned entry, or **swapped** for another pin that resolves to an approved validator (the existing refusal, `src/pipeline.rs:246-258`). It is never removed from an entry that carries one. That is how domain-extraction/coverage keeps `COVERAGE_VALIDATOR_PIN` (`src/domain_extraction.rs:101`) and qe verify keeps its evidence floor.
- `executes_code`: may be raised to `true` on any step (it provisions a worktree, and the repo-checks floor then applies), never lowered on an entry that sets it.
- `gate_type`: free. It has no production reader; only `PhaseDef`'s builder sets it (`src/workflow.rs:738`), and only tests read it.
- `skill_ref`, `allowed_skills`, `required_deliverables`, `depends_on`: as today (`src/workflow.rs:683-708`).
- `executor`: only on `run` and `deliver` (the Tool entries).
- `owner`: `"pa"` (default) or `"team"` (§8.8).
- `kind`: only on `run`.
- `role`: never. That is what keeps evaluator ≠ creator a property of the catalog, not of a plan.

Validation is `deny_unknown_fields` (`src/workflow.rs:640`) plus these rules. It runs in `plan::compose(catalog, &PlanSteps) -> Result<WorkflowDef, PlanRefusal>`, the only constructor of a per-run def.

### 8.4 Plans: PA-composed, user-composed, or a preset. One mechanism.

A plan is a `plan.proposed` event whatever its author. The payload's `steps[]` is an ordered list of `{catalog, id, …step fields}`; `by` is the author (`"claude#1"` for the PA, `"human"` for a user).
- **PA-composed:** the launch names no phases. The run's first unit is `understand` on the PA seat, and its plan block is parsed at the step boundary.
- **User-composed:** the launch carries `plan: {steps:[…], touch?:[…]}` (crew `POST /runs` gains `plan`; studio's phase picker produces it). The plan is part of the **launch command** (`POST /api/v1/runs` → `Core::launch_run`, §4.0). The engine validates it and then publishes `plan.proposed{by:"human", kind:"initial"}`, before any unit runs. Crew publishes nothing. The plan runs its own steps; `understand` runs only if the user included it. The intent score comes from the plan's declared `touch[]`. **A plan that has any creator step (`build` or `produce`) and a missing or empty `touch[]` scores `no_graph_score` (100, reason `"no declared scope"`)**. That is S4's fail-closed rule (`src/review_scale.rs:264-265`, `no_graph_score: 100` at `:281`) applied to a plan that will change something but has not said what. Only a plan with **no** creator step may score 0 on an empty `touch[]`. The diff re-score (§8.7) ratchets the floor from there, and the PA may add phases at any of its step boundaries as a `plan.revised`, so a user plan is never silently replaced.
- **Preset:** the launch carries `preset: "<name>"`. The engine expands the preset into `steps[]` and publishes `plan.proposed{by:"human", preset:"<name>"}`. A preset is **only** a saved phase selection, a named `steps[]`.

Every `plan.proposed`, from any author, goes through the same pipeline: **floor fill** (§8.5), then **compose** (§8.3), then the **approval matrix** (§8.6), then `plan.accepted`.

**Presets: where they live.** Presets are rows in the **core estate store**, as `Node(Other("plan_preset"))` with `{name, scope: "global" | "project:<id>", steps[], created_by, updated_at}`. They are written through a new `Core::put_preset` / `delete_preset` / `list_presets` (napi mirrors, crew routes `GET/PUT/DELETE /api/v1/presets[/:name]`). The evidence for choosing core and not crew settings:
- Campaign nodes launch by `workflow_id` resolved in core (`src/campaign.rs:62`, `:77`).
- The bus launch poller turns `wicked.crew.run.requested {workflow}` into a core launch without crew (`src/bus.rs:15`).
- `resolve_workflow_def` resolves ids in core (`src/pipeline.rs:174`).
- A preset kept in crew's settings (`packages/crew/src/api/routes.ts:4257`; per-project `ProjectSettingsStore`, `packages/crew/src/projects/settings.ts`) would be invisible to all three.
- Today's workflow registry is in-memory plus a file overlay (`src/workflow.rs:980-997`; `src/pipeline.rs:331-337`), and crew re-registers its runtime defs at every boot (`packages/crew/src/interactive/demo-events.ts:715-716`, `edit-events.ts:453`). That is not a durable home either.

**Built-in presets** are the migrated consumers (§11), seeded as code data in `src/catalog.rs` beside the catalog and written to the store at boot (idempotent by name, `created_by:"builtin"`). A user preset with the same name in a project scope shadows a built-in one for that project; a user cannot delete a built-in. **That is how today's workflow files become user data instead of code: each migration seam (§14) deletes a workflow file and adds one preset row.** `LaunchSpec.workflow: Option<String>` (`src/lib.rs:224`), crew's `LaunchSchema.workflow` (`packages/crew/src/api/routes.ts:483`) and the bus payload's `workflow` all **keep their names** and now name a preset. Every current id string keeps launching, because each built-in preset is named after the workflow it replaces. The alternative, a rename plus an alias, would be two spellings for one field.

### 8.5 The floor: the score sets the minimum phases

One table, `THRESHOLDS.floors`, sits beside `THRESHOLDS.bands` in `src/review_scale.rs` (`:272-290`), so the monitor count, the floor and the high-risk rule are tuned in **one** place. Starting values (tunable data):

| Band (S4 score) | Monitors (today's `bands`) | Floor: minimum phase types, in order | High risk |
|---|---|---|---|
| 0–19 (`PLAN_NONE`) | 0 | `build` → `deliver` | no |
| 20–39 (`PLAN_STANDARD`) | 1 | `build` → `review` → `deliver` | no |
| 40–69 (`PLAN_DEEP`) | 2 | `test_plan` → `design` → `build` → `review` → `deliver` | no |
| 70–100 (`PLAN_MOST`) | 3 | `test_plan` → `design` → `architecture` → `build` → `review` → `security_review` → `deliver` | **yes** |
| any band, `destructive` signal (`ChangeSignals.destructive`, `src/review_scale.rs:60`; `ImpactSignals.destructive`, `:112`) | per band | per band | **yes** |

**High risk** = band ≥ 70 **or** the destructive signal. It is stated once, here. Today `destructive` already floors the score at 70 (`destructive_floor: 70`, `src/review_scale.rs:280`), so the second row is redundant until someone tunes that floor down. It is kept so that tuning cannot silently drop the approval requirement.

Rules:
- `understand` is not a floor item. For a PA-composed plan it is ord 1, because it is how the score exists.
- **A plan with no creator step** (no `build` or `produce`: read-only runs such as chat and survey-repo, and tool-only runs such as onboarding) has an **empty floor** and is never high risk. It cannot change the tree without failing the worktree guard, so there is nothing for the floor to protect.
- `deliver` is in the floor only for a run that delivers (`deliver: "pr"`, crew's rule, `packages/crew/src/api/routes.ts:486-494`). Otherwise the floor ends at its last non-deliver phase.
- **Non-code runs:** when no step of the plan sets `executes_code`, the `build` slot is satisfied by `produce` and the `review` slot by `critique`. A documents or memories run is floored by an artifact creator and an artifact review, not by a code build.
- **Floor fill:** every floor phase type missing from `steps[]` is **inserted** at its catalog-order position and marked `added_by:"floor"` with `floor_reason` (for example `"band 40-69 requires design"`). The PA and the user may **add** phases and steps anywhere at any time. They may not remove or reorder a floor phase before its catalog-order predecessors. Duplicates are allowed (two `review`s).
- **Ratchet:** the run's floor band is the maximum band any score in the run has reached. It only goes up.

**Floor override (operator decision, 2026-09-24).** An **auto-mode** run can never go below the floor: a plan carrying an override is **refused** in auto mode (`plan.refused`, reason `"override in auto mode"`). A **manual-mode** run (§8.6) may carry an explicit override, `plan.override: {remove: ["review"], reason: "…"}`. It is recorded on the plan (`plan.accepted.override`), shown at the `plan_approval` gate and at the unit gate, and requires that gate like every manual-mode plan. It never removes a phase that carries a validator pin in a high-risk band.

### 8.6 Plan approval (operator decision 2, 2026-09-23)

**Auto mode is the existing run-level autonomy setting; there is no second knob.** Auto mode ⇔ `session.human_confirm == HumanConfirm::None`. Manual mode ⇔ `All` or `Before(_)` (`src/domain.rs:37-45`; the one parser, `:58-60`; read by `should_pause`, `src/actor.rs:6356-6360`).
- **Studio:** today's default posture is manual. The composer defaults to "human_confirm before the first gate-bearing unit" (`COMPOSER_DEFAULT_GATE_POSTURE`, `studio src/components/composerDefaults.ts:14`; applied at `ChatInput.tsx:219-222`, sent at `:576-584`); only the **Autonomous** pill sends no `humanConfirm`.
- **API, bus and campaign callers:** an absent `humanConfirm` parses to `None` (`src/domain.rs:52-60`), so these callers are auto by default. That is the platform's existing "absent = unattended" contract, and the high-risk rule below still binds them.

**The approval matrix:**

| Plan event | Manual mode | Auto mode, not high risk | Auto mode, high risk |
|---|---|---|---|
| Initial plan (`plan.proposed`, rev 1) | **approval** | proceeds | **approval** |
| Revision that crosses **into** high risk (previous accepted rev not high risk) | **approval** | — | **approval** |
| Revision that stays high risk (an earlier rev was already approved as high risk) | **approval** | — | proceeds |
| Any other revision (PA addition, accepted member request, floor raise below high risk) | **approval** | proceeds | — |
| A plan carrying a floor `override` (manual only, §8.5) | **approval** | refused | refused |

**The gate is a real gate.** When approval is required, at the step boundary the actor:
1. advances the cursor past the finished unit (the `advance_or_pause` bookkeeping, `src/actor.rs:6071`), then
2. calls `pause_for_human(…, gate_kind: "plan_approval", prompt)` (`src/actor.rs:6022`) with `ord` = the first **not-yet-dispatched** unit of the new plan and `reviewing_ord` = the unit whose output produced the plan, and
3. the team publisher emits `gate.opened{kind:"plan_approval", gate_id}`, with `gate_id` from `AgentSession.gate_seq`, incremented in the same batch as the pause (§6.1).

`PauseReason` (`src/actor.rs:6298-6309`) gains `PlanApproval` and `TeamTransport` (§4.1), mapped to the tokens `plan_approval` and `team_transport` at `:6124-6128`. The pause is durable exactly as every pause is: the session state and the open `interaction_request` commit in one batch (`src/actor.rs:6036-6046`).

**Resume through `confirm_gate`** (`src/actor.rs:7736`). The rule is DES-001 §6.7's: read the open row's `gate_kind` (`InteractionRequest.gate_kind`, `src/interaction.rs:76`, via `list_interactions`, `:161`) **before** `resolve_open_for_session` (`src/actor.rs:7815`; `src/interaction.rs:184`). For `plan_approval`:
- **Approve** releases the plan. The answer is the `confirm_gate` command (`POST /api/v1/runs/:id/gate`, §4.0), and the **engine** publishes `gate.decided{kind:"plan_approval", decision:"human_approved"}` and `plan.accepted{by:"human"}`. After both acknowledgements (§4.1) it emits `Resumed` and then calls `dispatch_unit` at the cursor. The existing arm bumps `attempt` when the cursor unit is `Done`/`Rejected` (`src/actor.rs:7976-7984`); the `plan_approval` arm skips that bump. It asserts the cursor unit is `Pending`, so a finished unit is never re-dispatched.
- **Approve with amend:** the amend body of the `confirm_gate` command is an edited `steps[]`. It becomes `plan.proposed{by:"human", kind:"edit"}`, goes through floor fill and compose, and is accepted directly as rev n+1: the human who edited it approved it. A refusal publishes `plan.refused` and **re-opens** the gate with the reason. The re-opened gate gets a new `gate_seq`, and so a new `gate_id` (§6.1). The first gate's `gate.decided{decision:"human_amended"}` stays the record of the first answer.
- **Reject:** cancel, as today.

The event sequence is `plan.proposed` → `gate.opened{kind:"plan_approval"}` → `gate.decided{kind:"plan_approval"}` → `plan.accepted`. When no approval is needed it is `plan.proposed` → `plan.accepted{by:"engine"}`.

### 8.7 Re-deciding mid-run: `plan.revised`

The plan only **grows**. The engine publishes `plan.revised{plan_rev, reason, added[], from_band, to_band}` whenever it does. It is the only publisher, because only the actor assigns `plan_rev` (§6.1). There are three triggers:
1. **The PA adds phases or steps:** a `PLAN+` block in the PA's output at any step boundary (the `ADVICE` parser family, `src/team.rs:1989`). It reaches the actor inside the step's output on `Command::ApplyStepResult` (`src/command.rs:218`, §4.0). The engine publishes `plan.proposed{kind:"change"}` and composes it into `plan.revised{reason:"pa_added"}`.
2. **A member asks, and the PA accepts:** the supervisor publishes `change.requested{change_id, steps}` (S). The injector renders it to the PA at its next boundary, and the PA answers `PLAN <change_id>: ACCEPT|DECLINE — <reason>`. An `ACCEPT` restates the accepted steps as a `PLAN+` block, so the engine receives them in the step output (`ApplyStepResult`) and **never reads the member's row**. The engine publishes `plan.proposed{kind:"change"}` citing the `change_id`, then `plan.revised{reason:"member_request"}`.
3. **Automatic floor raise.** The supervisor re-scores the settled diff and sends its own measurement to the actor as `Command::TeamRescored { run_id, ord, attempt, rescore_seq, assessment }` (§4.0). When the band rises, the engine publishes `path.scored{basis:"diff"}` and then `plan.revised{reason:"floor_raised"}` with the newly-required floor phases.

**Re-score trigger and cost bound.** The re-score is `assess` (`src/review_scale.rs:563`) on the settled diff `baseline..T_k`: the same snapshot and `git diff-tree` the monitor batch already takes (DES-001 §4.3–§4.4), so it adds graph reads only.
- It runs at a tree-changing `checkpoint.reached`, when **all** of these hold: the tree id differs from the last re-score's; at least `RESCORE_MIN_INTERVAL` = 60 s has passed since the attempt's previous re-score; and the attempt's re-scores are under `RESCORE_MAX` = 10.
- It always runs once more at `step.completed`.
- Worst case per attempt: 11 `assess` calls. Each is one `diff-tree` over the object DB plus one bounded traversal (`hops`, `THRESHOLDS`, `src/review_scale.rs:182`).
- A carrier with no checkpoints (wrapped, PTY) re-scores at `step.completed` only.
- The band only ratchets up (§8.5), so a lower re-score publishes nothing.

**Applying a revision.**
- **When:** at the next step boundary, never mid-unit. The actor's `apply_step_result` (`src/actor.rs:4269`) applies it after the finished unit folds and before `advance_or_pause`.
- **Where new steps go:** they are inserted after the cursor, in catalog order relative to each other, before `deliver`. Units not yet dispatched are renumbered from the cursor; units already dispatched or done are never touched and **never re-run**.
- **When the ideal position has passed:** a floor phase whose catalog position is before an already-done step (e.g. `architecture` arriving while `build` is done) is inserted at the cursor and says so: `added[].late: true`, reviewing the work as built.
- **Mechanism:** this is new engine surface, because no path inserts units into a live run today. It is a `Command::RevisePlan { run_id, def }`: compose the per-run def rev n+1, plan the new tail with `plan_from_def`, distribute only the new units (`distribute_units_against`), attach their pins, and persist in one batch.
- **Approval:** the matrix in §8.6 decides whether the revision pauses.

### 8.8 Members work, advise, support; the PA owns a member's step (operator decision 3, 2026-09-23)

- **Members** are the monitors S2 built: read-only ACP sessions on distinct seat instances (`MonitorHost`, `src/team.rs:659-670`), opened lazily at the first due batch (DES-001 §4.1). Their **input** is the stream: the supervisor's cursor (§4.2) feeds each member settled diffs at checkpoints, exactly as DES-001 §4.3–§4.5. It also passes every `advice.answered`, `help.requested` and `council.ruled` since the member's last turn.
- **Guidance** is `finding.raised`. The DES-001 contract is unchanged: parse, bar, mechanical confirmation at `path:line`, dedup by `finding_id`.
- **Support** is `help.answered` (S). The PA asks with one line, `HELP: <question>` (`help.requested`, R). Only members answer on the bus. A human-directed question stays with S1 (`AskUserQuestion` → `ElicitationCreated`, `src/event.rs:1145`), so there is no human `help.answered` path.
- **Work.** A plan step with `owner:"team"` (§8.3; `PhaseDef.owner: StepOwner { Pa, Team }`, `#[serde(default, skip_serializing_if = "StepOwner::is_pa")]`) is carried by `plan_from_def` into `WorkUnit.owner`, the way `role` is carried (`src/plan.rs:155` → `src/domain.rs:451`). The PA pin skips it. The supervisor assigns a member whose seat the step's skills admit, publishes `step.claimed{by:<member>}`, and runs it as a normal unit on that seat: `assigned_cli` is the member, with a **writable** unit session, not the member's read-only monitor session. It then publishes `step.completed{by:<member>}`.
- **The PA owns it.** A member's step counts only once the PA accepts it:
  - At the PA's next step boundary the member's output is rendered as `[team step — <step_id> by <seat>]` prior context (`src/actor.rs:6601-6620`). The PA answers `STEP <step_id>: ACCEPT — <reason>` or `STEP <step_id>: REJECT <to:member|pa> — <reason>`, which becomes `step.reviewed{verdict, to, reason}`.
  - `accepted`: the step's output is the unit's work output, and the run advances.
  - `rejected`: the step goes back to the member (`to:member`, a rework attempt with the reason as its amendment, the `rework_amendment` path, `src/actor.rs:6627-6650`) or to the PA (`to:pa`, re-planned onto the PA seat). A member step is rejected at most `MAX_STEP_REWORK` = 2 times before the third rejection goes to the PA.
- **Evaluator ≠ creator still applies per step.** The engine gate judges the member's step as a unit whose creator is the member (`work_author = assigned_cli`, `src/cli_runner.rs:919`, `:937-942`, `:1020-1025`), with the ledger's authors excluded (DES-001 §6.2). The PA's `step.reviewed` is team evidence, never the gate.
- **A dispute over a member step uses the council path.** A member may answer a `REJECT` with `HOLD <step_id> — <reason>` in its next turn. That is a concrete dispute, so the supervisor publishes `council.called{trigger:"member_step"}` with the question *"The PA rejected this step; the member holds its output. Should the member's output count as submitted?"* and the two positions.
  - **YES:** the output counts, and the PA's rejection is recorded as dissent. The supervisor, having convened the council, sends the verdict to the actor as `Command::TeamStepRuling { run_id, step_id, attempt, verdict }` (§4.0); the engine applies it to the unit.
  - **NO:** the rejection stands.
  - **No verdict:** a human pause, gate kind `team_dispute`, exactly DES-001 §6.7's fail-closed rule.

### 8.9 Advice reaches the PA: uniform at the step boundary, mid-turn on Claude ACP

- **Every carrier, at the next step boundary.**
  - **The read:** before `run_unit_streaming` (`src/cli_runner.rs:761`) the worker thread reads the run's stream from `path.started` and renders one `[team advice]` prior-context block.
  - **What goes in the block:** every `finding.raised` that has no `advice.delivered{outcome:"injected"}` yet on any channel (HIGH and MEDIUM; capped at 8 KB with DES-001 §5.3's text); every `help.answered`; every `council.ruled`; and any member `step.completed` awaiting review.
  - **What it publishes:** one `advice.delivered{channel:"boundary", outcome:"injected"}` row **per finding** it rendered. A finding that did not fit the cap gets no row and is picked up at the following boundary.
  - **Why this covers every carrier:** it works for wrapped CLIs with null stdin (`src/execute_wrapped.rs:3468`), for PTY, and for every ACP adapter, because a step boundary is a prompt every carrier builds.
- **Claude ACP, additionally mid-turn (S3's mechanism, unchanged).** `steer_at_boundary` (`src/acp_runner.rs:4903`) keeps its delivery point and its `_session/steering` request with `idleBehavior:"promptRequired"`. Its **source** becomes a poll of `finding.raised{severity:"high"}` for the attempt, after the attempt's `step.claimed` id and minus the attempt's delivered set. It publishes one `advice.delivered{channel:"acp_steering", outcome}` row per finding the steer carried, all sharing that steer's `steer_id`.
- **Authority is unchanged (DES-001 §5.3).** The advice block says ADVISORY. The PA answers `ADVICE <id>: ACCEPT|DECLINE — <evidence>` and may decline; the gate decides.

### 8.10 The council (operator step 8)

- **Triggers:** DES-001 §6.3's unresolved HIGH (not `accepted`/`withdrawn`/`superseded`, and held by its member), plus §8.8's member-step dispute. Nothing else convenes a council.
- **The call is a team event.** `council.called` carries the DES-001 §6.3 input verbatim plus `transcript`: the `event_id`s of the exchange, so the council reads what was said. The supervisor then calls `Core::convene_decision` (`src/lib.rs:1286`) with the non-party roster. The ballots' `CouncilConvened`/`CouncilDeliberated`/`CouncilVoted` CoreEvents stay engine telemetry (§4.6).
- **The ruling is a team event.** The supervisor publishes `council.ruled` from the `DecisionVerdict` its `convene_decision` call returned (`src/decision.rs:292`). It is S's own fact: S convened the council.
- **Three-way, on the record:** the PA's position is `advice.answered` (or `step.reviewed`), the team's is `finding.settled{status:"held"}` (or a member `HOLD`), and the council's is `council.ruled{dissent[]}`. The injector renders the ruling back to the PA at its next boundary.
- **Continue or pause (as DES-001 §6.7 specifies; not yet built on main: `team_dispute` appears nowhere in `src/`, so T6 builds it):** YES continues autonomously; NO or no verdict produces `team_pause`, which becomes `awaitingHuman{gate_kind:"team_dispute"}` and `gate.opened{kind:"team_dispute"}`.

### 8.11 The gate consumes the stream (operator step 9)

- **Final pass and fold, on the supervisor.** `step.completed` triggers DES-001 §4.7 steps 1–6. Then `team::fold(&events_of_attempt) -> TeamLedger` (a pure function over the attempt's rows) runs, and the supervisor publishes `ledger.folded{ledger, transcript}`, an S fact.
- **The worker thread waits, bounded**, with the `bus_request_agent_verdict` loop (`src/cli_runner.rs:417-445`), for at most `FINAL_PASS_BUDGET`. On timeout it synthesizes DES-001 §4.7's fail-closed ledger itself (`final_pass:"timed_out"`) **without publishing it**, because `ledger.folded` has one owner (S). The snapshot rides `Command::ApplyStepResult` to the actor, which publishes `gate.opened{kind:"unit_review", ledger_ref:null, ledger_source:"synthesized"}` (E) and folds. Consumers then read the gate's ledger from `UnitEvidence.team` (read route, evidence bundle). A `ledger.folded` S publishes afterwards is never the gate's ledger. S drops a fold past its deadline and tombstones any spooled line for it (§4.1), and any row that did land before the tombstone is labelled `unused` by the read route, because `gate.opened.ledger_ref` is the only authority on what the gate read.
- **Where the snapshot goes:** into `UnitEvidence.team` (`src/workflow.rs:265`). From there it reaches the judge's WORK fence, the evaluator's prior context and the rework amendment, as in DES-001 §6.2. `render_for_gate` also renders a compact transcript (≤16 KB), so reviewers have the outcomes **and** the comms.
- **A failed step** skips the final pass (`src/cli_runner.rs:876`).
- **No bus:** the worker builds the snapshot locally with `transport:"none"` (§4.1).
- **The decision** is published as `gate.decided{kind:"unit_review"}`, and for a human-resolved `team_dispute` as `gate.decided{kind:"team_dispute", by:"human"}`.

### 8.12 Studio

- **Run feed.** `NarratorFeed` renders `teamEvent` frames defensively (the `interactiveEvent` fold pattern, `studio src/store/runtime.ts:219-230`).
- **Launch form: the phase picker** (seam S-UI below; the build is a later seam). It is a new `PhasePicker` in the composer (`studio src/components/ChatInput.tsx`, beside the mode pills at `:576-584`) and shows:
  - the catalog list (`GET /api/v1/catalog`);
  - the user's ordered selection, with optional per-step instructions;
  - a **presets** dropdown (`GET /api/v1/presets`) and **Save as preset**;
  - the **floor-added** phases, rendered from the engine's floor fill of the draft (`POST /api/v1/plans/preview` → `{steps[], added_by_floor[], band, high_risk}`), marked "added by floor — band 40–69 requires design", and not removable in auto mode;
  - in manual mode, a disclosed override control (§8.5); in auto mode none.
- **Plan card** on the run page: `plan.proposed` / `plan.revised` / `plan.accepted`, the band, floor-added and late phases, **Edit** and **Stop**. **Edit** sends the command `POST /api/v1/runs/:id/plan` → `Core::propose_plan` (§4.0).
- **Plan approval gate:** `SteeringGate` (`studio src/components/SteeringGate.tsx:85`) renders `gate_kind:"plan_approval"` with the plan diff (previous rev → this rev). Approve, approve-with-edit and reject go through `POST /api/v1/runs/:id/gate` (`packages/crew/src/api/routes.ts:2943`).
- **Run header banner:** when `GET /api/v1/runs/:id/team` reports `transport:"none"` (the run, or an attempt), the run header shows "un-teamed: team transport unavailable" with the reason. This is the only disclosure of a `path.started` or `step.claimed` fallback (§4.1); it is read from persisted state, not a live event.
- **Unit gate panel:** *Team findings* and *Team comms* from `GET /api/v1/runs/:id/team`. `VerdictDetail` (`studio src/components/VerdictDetail.tsx:90`) shows `final_pass`, the `rejected` counters and `transport`.

## 9. Authority model

- **Advice never decides.** It is never binding: `combine_verdict` (`src/validator.rs:2263`) reads no team event, and the fold's inputs are byte-identical with and without a team (DES-001 §6.6).
- **The floor binds everyone.** The floor is engine data: the PA cannot remove it, and neither can a user in auto mode. A manual-mode override is explicit, recorded and approved at the `plan_approval` gate (§8.5).
- **Approval is the existing gate.** Plan approval is a `pause_for_human` gate resolved through `confirm_gate`, and nothing but a human (or auto mode below high risk) releases a plan.
- **Evaluator ≠ creator is per step.** The PA is the run's creator seat, and evaluator ≠ creator is preserved per step by `role` on the catalog entry (never plan-editable) and the fence (`src/distribute.rs:387`), which for team runs never falls back to the creator seat (§8.1). **Today** the judge excludes only the work author (`src/cli_runner.rs:919`, `:937-942`, `:1020-1025`); excluding every ledger author (DES-001 §6.2) is not built on main (no `excluded_seats` in `src/cli_runner.rs`) and is built in T5.
- **The PA owns a member's step.** It counts only after `step.reviewed{verdict:"accepted"}`, and the gate still judges it with judge ≠ member.
- **The council's power is narrow.** It rules on one question, and its ruling chooses only between autonomous continue and a human pause (DES-001 §6.7).

## 10. Mapping: existing CoreEvents, direct channels and workflow files → the new mechanism

| Today | Where | Becomes | Action |
|---|---|---|---|
| `adviceDelivered` CoreEvent | `src/event.rs:1180`, `:2409`; api-types `index.d.ts:1933` | `wicked.team.advice.delivered` | **delete** the variant, its `to_json` arm and the api-types alias |
| `workerAdviceResponse` CoreEvent | `src/event.rs:1194`, `:2426`; api-types `index.d.ts:1953` | `wicked.team.advice.answered` | delete |
| `unitCheckpoint`, `monitorAttached`, `monitorFinding` | `src/event.rs:1210`, `:1225`, `:1241`, `:2446-2494` | `checkpoint.reached`, `member.joined`/`member.left`, `finding.raised` | delete |
| `TeamCmd`, `TeamHandle`, `runner.install_team`, `StepRunner::team_finish`, the supervisor's `Command::Subscribe` + `EmitEvent` closure, and the hold-before-attach buffer | `src/team.rs:1437-1470`, `:1498-1510`, `:1150-1200`; `src/lib.rs:576`; `src/workflow.rs:418`; `src/cli_runner.rs:766` | `step.claimed`, `step.completed` plus the gate wait, `path.ended`; a bus cursor | delete |
| `SteerMailbox` | `src/team.rs:1730-1832`; `src/acp_runner.rs:5552`, `:8128` | a per-attempt poll of `finding.raised` plus a delivered set | delete the type; keep `steer_at_boundary`, `advice_block`, `steer_params`, `parse_advice_lines` |
| **Workflow files and their mirrors:** core built-ins, the core JSON mirror files, crew's mirrors and runtime-registered defs, and crew's seeding into `$WICKED_WORKFLOWS_DIR` | core `WorkflowRegistry::with_defaults` (`src/workflow.rs:984-997`: `feature_def` `:1206`, `bug_def` `:1262`, `migration_def` `:1297`, `onboarding_def` `:1367`, `collab_def` `:1177`); `workflows/*.json`; crew `packages/crew/src/core/adapter.ts:559-780`; crew runtime-registered defs (§11); crew seeding (`packages/crew/src/projects/state-home-preflight.ts:15`, `:75`) | the catalog (`src/catalog.rs`) plus preset rows (§8.4) | **delete per migration seam** (§14 M1–M11); each seam deletes its consumer's def, mirror, JSON file, seeding and mirror-guard entry when it lands |
| The per-def `verified_evidence` arming with `EVIDENCE_FLOOR_PIN` | `src/pipeline.rs:236-244`; `src/builtin_floors.rs:109` | the pin on the `test` / `build` / `review` catalog entries | move (seam C1) |
| Crew's `composeDeliverWorkflow` (appends a deliver phase to a per-run copy) | `packages/crew/src/core/deliver.ts:789`, `:859` | the catalog `deliver` entry, added by floor fill when `deliver:"pr"` | delete (seam C2) |
| The operator overlay directory `$WICKED_WORKFLOWS_DIR` | `src/pipeline.rs:331-337`; crew `packages/crew/src/core/adapter.ts:78-82` | user presets (`PUT /api/v1/presets/:name`) | delete once M11 lands (no drop-in file mechanism survives) |
| Kept: `unitDistributed{routingMethod:"teamed"}`; the council ballot events; `awaitingHuman`; `gateEvaluated`; `gateDecided`; `unitDone`/`unitDenied`; operator inject | as cited in §2 | unchanged (engine telemetry, not team comms) | keep |

## 11. Consumers of workflows today, and their migration onto the catalog

### 11.1 Inventory (crew `main` `fc079ec`, core `main` `fe94ffc`, studio `main` `340e92e`)

| Consumer | Workflow id(s) | Where the def lives | How it launches |
|---|---|---|---|
| Build surface (coder runs) | `feature`, `bug`, `migration` | core `src/workflow.rs:1206`, `:1262`, `:1297`; crew mirror `packages/crew/src/core/adapter.ts:585`, `:596`, `:608`; `workflows/{feature,bug,migration}.json` | `POST /api/v1/runs {workflow}` (`LaunchSchema`, `packages/crew/src/api/routes.ts:476-483`); studio composer (`ChatInput.tsx:576-584`) |
| Deliver | (appended phase `deliver`) | crew `packages/crew/src/core/deliver.ts:789` | `composeDeliverWorkflow` on `deliver:"pr"` (`deliver.ts:859`; `routes.ts:486-494`) |
| Chat | `chat` | crew `adapter.ts:559`; `workflows/chat.json` | studio `ChatPanel.tsx:1211` (`workflowOverride:'chat'`) |
| Onboarding | `onboarding` | core `src/workflow.rs:1367`; crew `adapter.ts:566` | crew `adapter.ts:2284` |
| Repo survey | `survey-repo` | crew `adapter.ts:624`; `workflows/survey-repo.json` | no launcher in crew or studio `src` (grep); reachable by `POST /runs` and the bus |
| Capture-learnings / repo-learn | `capture-learnings` | crew `adapter.ts:670` (skill_ref `wicked-garden-repo-learn`) | studio `RepositoriesPanel.tsx:244` |
| Memories | `memories` | crew `adapter.ts:688`; `workflows/memories.json` | no launcher in `src` (grep) |
| Domain graph slice | `domain-graph-slice` | crew `adapter.ts:679`; `workflows/domain-graph-slice.json` | no launcher in `src` (grep) |
| Domain extraction | `domain-extraction` | crew `adapter.ts:755`; core `src/domain_extraction.rs` (`COVERAGE_VALIDATOR_PIN` `:101`); `workflows/domain-extraction.json` | studio `RequirementsModal.tsx:230` |
| Steering author | `steering-author` | crew `adapter.ts:713` | crew `packages/crew/src/api/governance-steering.ts:351`, `:362` |
| Collab | `collab` | core `src/workflow.rs:1177`; crew `adapter.ts:721` | no launcher in crew or studio `src` (grep); listed in studio's system set (`runMode.ts:36-41`) |
| Documents / Vibe | `interactive-chat`, `interactive-draft`, `interactive-edit`, `interactive-demo`, `interactive-demo-reauthor` | crew `interactive/chat-events.ts:116`, `draft-events.ts:175`, `edit-events.ts:100`, `demo-events.ts:232`, `:289`; registered at boot (`demo-events.ts:715-716`, `edit-events.ts:453`) and seeded into `$WICKED_WORKFLOWS_DIR` (`state-home-preflight.ts:15`, `:75`) | crew bus seams `chat-events.ts:910`, `draft-events.ts:1256`, `edit-events.ts:847`, `demo-events.ts:1368`, `:1479` |
| Test / QE | `qe-author-tests` | crew `packages/crew/src/qe/author-workflow.ts:322` | crew `packages/crew/src/api/testing.ts:723-770` |
| QE acceptance gate | none (reads ledger evidence, launches no workflow) | — | `GET /api/v1/runs/:id/acceptance` (`routes.ts:2905`) |
| Evals | none (eval store/compare launch no run; grep of `api/eval-store.ts`, `api/eval-compare.ts`) | — | — |
| Campaigns | any id per node | — | crew `packages/crew/src/campaigns/plan.ts:473`, `:495` → core `src/campaign.rs:62`, `:77` |
| Bus launch | any id | — | `wicked.crew.run.requested {workflow}` → core `src/bus.rs:15` |
| Operator drop-ins | any file | `$WICKED_WORKFLOWS_DIR` (`src/pipeline.rs:331-337`) | `resolve_workflow_def` (`src/pipeline.rs:174`) |
| Studio lookups | id strings | studio `runMode.ts:36-41` (system set), `store/workflowCache.ts` | display and delivery classification only |

### 11.2 Mapping current phases onto catalog types

The hypothesis was that **no consumer needs more than the core catalog**. It is **confirmed**: every phase of every consumer maps onto one of the twelve core types below, with the step-field strengthenings §8.3 allows. Beyond the obvious, the core catalog needs `produce` and `critique` (creating and reviewing a **non-code artifact**) and `run` (a Tool command). None of those is surface-specific: documents, domain extraction, memories, steering and collab all use them. Adding `critique` to §8.3's table makes twelve entries: `review`/evaluator/`auto` gate/`execution` type, **no pin**, no code. Without it, today's unpinned reviews (feature/review, domain-graph-slice/validate, collab/critique, collab/verdict) would each gain the evidence-floor pin, whose criterion judges code evidence.

| Consumer | Current phases → catalog (bold = a behaviour change, detailed in §11.3) |
|---|---|
| feature | clarify → `understand` (gate raised to `human_confirm`); design → `design`; build → `build`; adversarial-review → `review` (gate raised); test → `test` (**role neutral → evaluator**); review → `critique` (**role neutral → evaluator**) |
| bug | triage → `understand`; reproduce → `test_plan`; fix → `build` (instructions kept); verify → `test` |
| migration | plan → `design` (gate raised); execute → `build`; cutover → `build` (gate raised to unconditional; **gains pin, role neutral → creator**); verify → `test`; cleanup → `build` (**gains pin and `executes_code`, role neutral → creator**) |
| deliver | deliver → `deliver` |
| chat | explore → `understand` |
| onboarding | index, annotate → `run` (tool cmd kept) |
| survey-repo | structure, stack, conventions, synthesize → 4× `understand` (instructions and `depends_on` kept) |
| capture-learnings | churn, hotspots → `understand`; capture → `produce` (skill_ref kept) |
| memories | gather → `understand`; store → `produce` |
| domain-graph-slice | identify → `understand`; extract → `produce`; validate → `critique` |
| domain-extraction | survey, analyze → `understand`; extract → `produce` (**kind recon → build**); coverage → `test` (pin swapped to `COVERAGE_VALIDATOR_PIN`, `executes_code` raised, skill_ref kept); domain-graph → `run` (gate raised to `human_confirm`) |
| steering-author | analyze → `understand`; propose → `produce` (**kind recon → build**; gate raised to `human_confirm`) |
| collab | propose, revise → `produce` (**kind recon → build**); critique → `critique`; verdict → `critique` (gate raised to `human_confirm`) |
| interactive-chat | understand → `understand`; revise → `produce` |
| interactive-draft | outline → `understand`; draft → `produce` (the draft skill kept via `skill_ref`, `withDraftSkill`) |
| interactive-edit | edit → `produce` |
| interactive-demo | scenes → `understand`; spec → `produce` |
| interactive-demo-reauthor | respec → `produce` |
| qe-author-tests | recon → `understand` (gate_type strategy kept; informational only, `src/workflow.rs:738`); author → `build` (skill_ref `wicked-garden-qe` kept); verify → `run` (tool script kept; pin **added**, the `EVIDENCE_FLOOR_PIN` it carries today); review → `review` (gate raised to `human_confirm_if`) |

§8.3's step rules are relaxed in exactly two directions to make this mapping exact, both strengthenings: a step may **add** a pin to an unpinned entry (qe verify), and may **raise** `executes_code` to `true` (domain-extraction coverage). The §8.3 text is updated accordingly.

### 11.3 Per-consumer before → after

Every migration is **one seam** (§14 M-seams). It adds the preset, switches the launcher to it, deletes the old def, mirror, JSON and seeding, and keeps the named contract tests green, updated only where the table says the behaviour changes.

Common to every consumer (not repeated per row):
- The run now publishes `wicked.team.*` events (path, plan, gate) on the bus, and studio renders them.
- The per-run def id becomes `"<run>:plan-<rev>"`; the launch's `workflow` field on the wire, in `LaunchSpec` (`src/lib.rs:224`) and in the bus payload keeps its name and now names a **preset**.
- Delivery classification reads the preset's `system` flag (the `is_system` it replaces, `adapter.ts:560`) instead of the def.
- **Evaluator seat (§8.1):** after migration, a consumer launched on a roster with no distinct seat and no usable second instance is **refused** (`NoEligibleSeat`) where today it runs its review/test unit on the creator seat with `creator_seat` disclosed. Where an instance is configured, the unit moves to it (`same_cli_instance`). Each M-seam's contract tests assert this change for its consumer.
- **Plan approval** applies per §8.6. A launcher that omits `humanConfirm` is auto and pauses only at high risk. A read-only or tool-only plan has no creator step, so its floor is empty and it never scores high risk.

| Consumer | What changes (gates, floors, seat routing, human_confirm, outputs, events, studio) | What must stay identical (named contract tests) |
|---|---|---|
| feature | **Floor fill** may add phases by band (e.g. band 70–100 adds `test_plan`, `architecture`, `security_review`). **`test` and the final review move to an evaluator seat** (role neutral → evaluator), so the fence places them off the creator. The final review loses no pin (it maps to the unpinned `critique`). **Manual mode:** the clarify boundary's pause becomes `gate_kind:"plan_approval"` instead of `"def"`, one pause when the two coincide. The PA's `understand` step emits the plan block. | Gate ladder and pins on build / adversarial-review / test; deliver gate; evidence bundle shape. core `tests/p10_methodology.rs`, `tests/governed_floor_and_fence.rs`, `tests/p2_gates.rs`, `tests/p14_gate_phase.rs`, `tests/dead_seat_gate.rs`; crew `tests/deliver-launch.test.ts`, `tests/deliver-phase.test.ts`, `tests/deliverable-floor-launch.test.ts` |
| bug | **Floor fill** adds `review` at band ≥ 20 (bug has none today). **Manual mode:** a new `plan_approval` pause after triage. | reproduce → fix → verify order, the fix sweep instructions, verify's `human_confirm_if` gate and pin. The same core tests; crew `tests/deliver-launch.test.ts` |
| migration | **cutover and cleanup gain the evidence-floor pin and the creator role.** **cleanup → `build` (decision, 2026-09-24):** it removes the old path, so it is code work. It gains a worktree (`executes_code`), the evidence pin and the creator role. That is the stricter and correct classification, and a **known behaviour change**: a cleanup that passed ungated today can now be denied by the evidence floor, and it runs on a creator seat, so its judge is a distinct seat. A new `understand` ord 1 is added. Floor fill applies. | cutover's **unconditional** human gate; verify's gate and pin. core `tests/p14_gate_phase.rs`, `tests/governed_floor_and_fence.rs` |
| deliver | Composed by floor fill instead of crew; the deliver unit id stays `deliver`, so `is_deliver_unit` is unchanged. | Deliver gate and `auto_deliver` behaviour; PR text. crew `tests/deliver-*.test.ts` (all 15), core `tests/deliver_refusal_gate.rs` |
| chat | None: a read-only plan, empty floor, never high risk. | Chat surface launch and output. crew chat promote tests; studio `ChatPanel` tests |
| onboarding | None: a tool-only plan, empty floor. | Index then annotate, placeholders `{repo_root}` / `{code_graph_db}`. crew `tests/onboarding-phases.test.ts`, `tests/onboarding-run-seats.test.ts` |
| survey-repo | None: read-only. | Phase instructions and dependencies. crew `tests/builtin-overlay-shadow.test.ts` (its `MIRRORED_IDS` entry is **deleted**, and the equivalent assertion moves to a preset fixture) |
| capture-learnings | None beyond the common changes (produce ≡ its current build/creator/value phase). | skill_ref on every phase; `delivery:'none'`. crew `tests/capture-learnings-skill-ref.test.ts` |
| memories | None (produce ≡ store). | — (no dedicated test today; M-seam adds a preset launch test) |
| domain-graph-slice | validate maps to `critique`: identical controls. | — (M-seam adds a preset launch test) |
| domain-extraction | **extract's kind recon → build** (studio's stage badge changes; `StageKind`, `src/domain.rs:630`). coverage keeps `COVERAGE_VALIDATOR_PIN`, skill_ref and a worktree. | core `tests/domain_extraction_e2e.rs`, `src/domain_extraction.rs:379` (drop-in test, rewritten as a preset test) |
| steering-author | **propose's kind recon → build** (badge). Gate kept (raised to `human_confirm`). | crew `tests/governance-steering-routes.test.ts` |
| collab | **propose and revise kind recon → build** (badge). | core `collab_def` unit tests (moved to a preset fixture) |
| interactive-* (5) | None beyond the common changes: auto-gated, unpinned, no code, the same kinds. Crew stops registering them at boot and stops seeding `$WICKED_WORKFLOWS_DIR`. | crew `tests/interactive-chat-events.test.ts`, `tests/interactive-draft-events.test.ts`, `tests/interactive-draft-skill.test.ts`, `tests/interactive-edit-events.test.ts`, `tests/interactive-demo-events.test.ts` |
| qe-author-tests | None beyond the common changes. | crew `tests/qe-author-workflow.test.ts`, `tests/testing-author-route.test.ts`, `tests/testing-routes.test.ts` |
| Campaigns | Node `workflow_id` names a preset (same strings). | core `tests/p13_campaign.rs`, `tests/campaign_denial_gate.rs`; crew `tests/campaign-plan.test.ts`, `tests/campaign-routes.test.ts`, `tests/campaign-seam-engine-roster.test.ts` |
| Bus launch | The payload's `workflow` names a preset. | core `tests/bus_bridge.rs`, `tests/exec_seam.rs` |
| Operator drop-ins | **Removed** after M11: an operator saves a preset instead. A drop-in file left in the directory is ignored with one boot warning naming it. | — |
| QE acceptance gate, evals | Not consumers: no change. | — |

## 12. Risks → mechanism

| Risk | Mechanism | Where |
|---|---|---|
| **Stream volume** | Only `checkpoint.reached` is frequent, only for team units, and ≤ ~600 B. Monitor batches and re-scores ride the same settled checkpoints (60 s apart, changed tree only, capped). The relay starts at the latest row and does not retry. | §6, §8.7, §4.5 |
| **Noise** | The severity bar, mechanical confirmation, dedup, never re-raising an answered finding, and the `rejected` counters. | DES-001 §4.6 |
| **Authority** | `combine_verdict` never reads a team event; the council only chooses continue vs pause; the floor is engine data. | §9 |
| **A plan escaping the floors** | Catalog entries fix `role`, pins and gates; steps can only strengthen them; floor fill is engine-side; the only override is manual, recorded and approved. | §8.3, §8.5 |
| **Re-score cost** | At most `RESCORE_MAX` + 1 = 11 `assess` calls per attempt, reusing the batch diff. | §8.7 |
| **Migration regressions** | One seam per consumer, a before/after table, and named contract tests. No dual path: the def is deleted in the same seam. | §11.3, §14 |
| **An approval gate re-running work** | The `plan_approval` arm dispatches only a `Pending` cursor unit and never bumps `attempt`. | §8.6 |
| **Duplicates** (at-least-once) | Deterministic keys; consumers keyed by entity id. | §4.1, §4.2 |
| **Bus rows expire** | The `ledger.folded` snapshot is persisted on the unit (`UnitEvidence.team`). | §4.4 |
| **No bus** | A local snapshot with `transport:"none"`, never an in-process fallback. | §4.1 |
| **Actor stall** | The actor never opens the bus. | §4.1 |
| **A dropped fact fails open** | Reliable publishing with a bounded retry; a required fact that cannot land pauses the run or disables teaming before team work begins. | §4.1, P1 |
| **Two components writing one fact / a consumer mutating someone else's state** | §4.0: one owner per fact, commands through the API, and the actor reads no bus row; asserted by the §7 matrix and grep tests (T4 (e), T8 (g)). | §4.0, §7 |
| **Restart mid-step** | The attempt never survives (orphaned or redriven at attempt+1); team state is persisted on the session; the supervisor replays live runs from `stream_floor` before tailing; a gap past retention pauses for a human (`stream_gap`). | §4.7 |
| **Evaluator on the creator seat** | Team runs refuse rather than fall back; a second signed-in instance is the only fallback. | §8.1, D1 |

## 13. Where each piece lives

| Piece | Repo | Location |
|---|---|---|
| `TeamBus::publish` (team outbox, bounded retry, reusing `wicked_apps_core::emit`'s spool pattern), `TeamPublisher` acknowledgements, `Core::replay_team_outbox` | core | new `src/team/bus.rs`; the pattern at `crates/wicked-apps-core/src/emit.rs:453-470`, `:614` |
| `TeamPublisher` thread; actor-side `path.started`, `plan.accepted`, `plan.revised`, `gate.opened`/`gate.decided` (plan approval), `path.ended` | core | new `src/team/bus.rs`; hooks at launch admission, `apply_step_result` (`src/actor.rs:4269`), `pause_for_human` (`:6022`), `confirm_gate` (`:7736`), terminal paths |
| Event types, payloads, keys, `fold(events) -> TeamLedger` | core | `src/team/events.rs` (pure) |
| Catalog (12 `PhaseDef` entries), built-in presets, `plan::compose`, floor fill | core | new `src/catalog.rs`; `src/plan.rs` (beside `plan_from_def`, `:94`) |
| `PhaseDef.owner` / `WorkUnit.owner` | core | `src/workflow.rs:641` (struct), `src/domain.rs:451` (beside `role`), `src/plan.rs:155` (copy) |
| Floor table and high-risk rule | core | `src/review_scale.rs` `THRESHOLDS` (`:272-290`) |
| `signals_from_paths`, re-score at checkpoints | core | `src/review_scale.rs` (beside `:640`); supervisor |
| `PauseReason::PlanApproval`, the `plan_approval` arm in `confirm_gate`, `Command::RevisePlan` | core | `src/actor.rs:6298-6309`, `:6124-6128`, `:7736`, `:7815`, `:7976-7984`; `src/command.rs` |
| Preset rows and `Core::{put,delete,list}_preset` | core | new `src/preset.rs`; `src/lib.rs` + napi |
| Supervisor on the bus; member steps; council calls | core | `src/team.rs` |
| Worker-thread seam: `step.claimed`/`step.completed`, boundary injector, gate wait | core | `src/cli_runner.rs:748-766` |
| ACP carrier: `checkpoint.reached`, steer from the bus | core | `src/acp_runner.rs:4440`, `:4903`, `:8128` |
| Bus handoff at boot; `teamEvent` relay; `GET /runs/:id/team`; `GET /catalog`; `GET/PUT/DELETE /presets`; `POST /plans/preview`; the commands `POST /runs {plan, workflow}` and `POST /runs/:id/plan` (→ `Core::propose_plan`); api-types. Crew publishes no team fact (§4.0) | crew | `packages/crew/src/cli/index.ts:99-137`, `core/adapter.ts:1183-1191`; new `packages/crew/src/team/ws-relay.ts`; `api/routes.ts`; `packages/crew-api-types/index.d.ts` |
| Phase picker, preset dropdown, plan card, plan approval gate, team panels | studio | `ChatInput.tsx` (new `PhasePicker`), `SteeringGate.tsx:85`, `VerdictDetail.tsx:90`, `NarratorFeed.tsx`, `store/runtime.ts:219` |

## 14. Build order: disjoint seams, each buildable by one agent

The team-run core comes first (T0–T9); then **one migration seam per consumer** (M1–M11), each of which deletes that consumer's workflow def, mirror, JSON and seeding in the same PR. T0, T1 and C1 are prerequisites; after them, T2–T9 are parallel except where a line says otherwise. The M-seams need T0–T5 plus C1–C2, and are then parallel with each other.

**T0 — Bus handoff (core + crew; amended in rev 11).** Crew sets `WICKED_BUS_DB` to the resolved crew bus on every boot. The engine opens it only on bus threads, holds one connection for the life of the process, and routes the gate judge to the bus only under exec.
*Accept:*
- (a) with no flags, the engine's `WICKED_BUS_DB` equals `resolveCrewBus(...).dbPath`;
- (b) `--engine-exec` mediates over the same file, behaviour otherwise unchanged;
- (c) `--bus-db X` wins for both;
- (d) a bus that cannot open logs one line, and the daemon serves un-teamed with a daemon-level notice (`/health.warnings` `bus.unavailable`); per-run `transport:"none"` is P1's;
- (e) `WICKED_BUS_DB` without `WICKED_BUS_EXEC` keeps the inline judge for an evidence-floor-pinned unit: no `gate.eval.requested`, no 180 s wait;
- (f) the actor starts and serves commands while the bus file is locked (no 5 s stall); `spawn`'s arming handshake is bounded and reports the bridge not armed;
- (f2) the bridge's start point is an event id taken by the arming handshake: a request emitted immediately after `spawn` returns is delivered, and a row already on the bus (even one whose `emitted_at` is not earlier) is never launched;
- (f3) a bus file deleted and recreated under the same path gets a fresh handle that sees the new rows (file identity), and the reopen is counted;
- (g) no production path opens a private bus connection (a source guard);
- (h) engine churn plus external emits keep every row visible to crew's long-lived handle, with `quick_check` ok and `-wal` intact. The F-E2E-021 symptom does not reproduce.

**T1 — Wire contract (core).** `src/team/events.rs`: all event types, payloads, key builders (the §4.1 algorithm and vectors), `fold`; catalog annotations; api-types.
*Accept:* (a) every type is four segments; (b) value-compared round-trip fixtures; (c) `fold` reproduces DES-001's acceptance fixtures (#8, #11, #15, #16 a–k); (d) `fold` is idempotent under duplicates; (e) the §4.1 test vectors match `crate::bus::deterministic_key` and the crew JS helper; (f) `gen_event_catalog.py --check` is green; (g) **the §6.1 identity rule, table-driven over all 25 types:**
  - for every type, two distinct logical requests identical in every content field and differing only in the producer-assigned part (`help_seq`, `raise_seq`, `open_seq`, `delivery_id`, `answer_id`, `plan_block_seq`, `rescore_seq`, `gate_seq`, attempt) produce **two** rows with distinct `event_id`s;
  - a re-publish of the same logical request produces **one**;
  - named fixture: two `HELP:` lines in one output with the same question and different context yield two `help.requested` rows; two member `change.requested` rows with the same text and different steps yield two;
  - a grep test fails the build if a key builder in `src/team/events.rs` takes a payload text field (question, claim, evidence, reason, context).

**C1 — Phase catalog + compose (core).** `src/catalog.rs` (12 entries, §8.3/§11.2), `PhaseDef.owner`/`WorkUnit.owner`, `plan::compose` with the step rules, and the evidence-floor pin moved onto the entries.
*Accept:* (a) `compose` of every §11.2 mapping yields a def whose per-phase `(kind, role, gate, validator_pin, executes_code, executor, skill_ref, instructions, depends_on)` equals today's def **except** exactly the bold cells of §11.2 (one fixture per consumer, generated from today's defs). The migration fixture pins cleanup as `build`: `kind:build`, `role:creator`, `validator_pin:EVIDENCE_FLOOR_PIN`, `executes_code:true`; (b) a step that lowers a gate, removes a pin, changes `role`, or sets `executor` on a non-Tool entry is refused with a named reason; (c) a misspelled step key is refused (`deny_unknown_fields`); (d) an owner-omitted def serializes byte-identically; (e) `attach_pinned_validators` attaches every catalog pin (an unapproved swap is refused, `src/pipeline.rs:246-258`).

**C2 — Presets (core + crew).** `src/preset.rs` rows, the built-in seeding, `Core::{put,delete,list}_preset`, crew routes, and `launch_run` resolving `workflow` as a preset name.
*Accept:* (a) a built-in preset named `feature` launches the same unit list C1(a) fixed; (b) `PUT /presets/my-flow` then `POST /runs {workflow:"my-flow"}` launches that selection (**preset launch**); (c) a project-scoped preset shadows a built-in only for that project; (d) a built-in cannot be deleted; (e) a bus `wicked.crew.run.requested {workflow:"my-flow"}` and a campaign node naming it both launch it with no crew involvement in resolution; (f) presets survive a daemon restart.

**P1 — Reliable publishing (core).** `TeamBus::publish` with the team outbox (the `wicked_apps_core::emit` spool pattern), bounded retry, the `TeamPublisher` acknowledgements, the required-transition gates, and `Core::replay_team_outbox`.
*Accept:*
- (a) with the bus db made unwritable for 10 s, every team event lands in `team-outbox.ndjson` with a `DEADLETTER_MARKER` line on stderr, and is published once the bus returns (one row each: replay dedups);
- (b) unwritable past the 31 s bound: a failing `path.started` makes the run proceed with `transport:"none"` **before** its first unit dispatches, with `transport:"none"` persisted on the run and in every snapshot, served by `GET /api/v1/runs/:id/team` (`{transport:"none", reason}`) and shown as studio's run-header banner, with **no** new CoreEvent (a grep test asserts no `teamTransportDisabled` exists); a failing `plan.accepted` pauses the run `team_transport` **before** the plan's first unit dispatches; a failing `gate.decided` keeps the run paused `team_transport` (no `Resumed`, no dispatch);
- (c) the actor thread never blocks on a publish (a test holds the bus lock for 60 s while the actor keeps answering `subscribe`);
- (d) `Core::replay_team_outbox` replays the leftover lines idempotently: a line replayed twice lands once;
- (e) **replay after a `transport:"none"` fallback publishes nothing for that run.** A run whose `path.started` failed past the bound has a `superseded_run` tombstone before its `transport:"none"` is persisted; after the bus returns, `replay_team_outbox` and the live drain publish **zero** rows for that run, and the supervisor never arms it. The same holds for an attempt whose `step.claimed` failed, and for a `team_transport` pause answered "continue without team" or rejected (only `path.ended` is published for the rejected run);
- (f) crash between the tombstone and the store write: on boot the run is `transport:"none"` and replay publishes nothing for it;
- (g) per-run FIFO: with `gate.opened` failing and `gate.decided` queued behind it, the bus never holds a `gate.decided` whose `gate.opened` is absent, in live draining or after replay;
- (h) a final-pass timeout gives `gate.opened{ledger_ref:null, ledger_source:"synthesized"}`; S's late fold publishes nothing, and its spooled line, if any, is tombstoned; the read route serves the gate's ledger from `UnitEvidence.team`;
- (i) every path in the §4.8 table has a named fixture asserting its four columns.

**T2 — Floor table + floor fill (core).** `THRESHOLDS.floors` and the high-risk rule; `signals_from_paths`; floor fill with `added_by:"floor"`; the empty floor for a plan with no creator step.
*Accept:* (a) for scores 10/30/50/80 and for a destructive signal at a score of 10 (with the destructive floor tuned to 0 in the fixture), the floor and `high_risk` equal §8.5's table; (b) **a user plan below the floor gets the floor phases added**, each marked `added_by:"floor"` with its `floor_reason`, and `plan.accepted.steps` shows them; (c) a user plan that already contains the floor is unchanged; (d) a read-only plan (`understand` only) and a tool-only plan have an empty floor; (e) no graph means score 100, so the band 70–100 floor applies; **(g) an auto-mode `POST /runs {plan:{steps:[{catalog:"build"}]}}` with `touch` omitted, and again with `touch:[]`, scores 100 with reason `"no declared scope"`, gets the 70–100 floor, and pauses `plan_approval` (high risk) before `build` dispatches. The same launch with `touch:["src/x.rs"]` scores from the graph. A read-only plan (`understand` only) with `touch` omitted scores 0 and does not pause;** (f) the table is the only place the values live (a grep test for literal band numbers outside `THRESHOLDS`).

**D1 — Team-run evaluator distinctness (core; operator decision, §8.1).** For a team run, `creator_seat` is never used.
*Accept:*
- (a) a team run on a one-seat roster `[claude]` whose launch also lists a usable `claude#2` puts its review/test units on `claude#2` with `distinctnessFallback:"same_cli_instance"`;
- (b) the same roster without `claude#2`, bench-free, is refused `NoEligibleSeat` naming the units;
- (c) the same roster with `claude#2` listed but not signed in (health not usable) is refused too;
- (d) an all-builder roster behaves like (a) or (b);
- (e) a **non-team** launch of the same shape still returns `creator_seat` exactly as `src/distribute.rs:417-426` does today (the existing `dead_seat_gate.rs` and `p10_methodology.rs` assertions stay green);
- (f) a bench-caused shortfall is refused as today (`:388-409`).

**T3 — Plan approval gate (core + crew).** `PauseReason::PlanApproval`, the `plan_approval` arm in `confirm_gate`, the approval matrix, and the `gate.opened`/`gate.decided{kind:"plan_approval"}` events.
*Accept:* **the approval matrix, row by row, on a PA plan and again on a user-composed plan:*
- (a) manual mode (`before:1` and `all`): the initial plan pauses `plan_approval` before the first execution unit;
- (b) auto mode (`none`), band < 70, no destructive signal: no pause, `plan.accepted{by:"engine"}`;
- (c) **auto mode, high risk:** a pause (**high-risk-in-auto**);
- (d) approve dispatches exactly the `Pending` cursor unit once, emits no second `unitDispatched` for a finished unit, and leaves `session.attempt` unchanged (fixture: the cursor unit's predecessor is `Done`);
- (e) approve-with-edit publishes `plan.proposed{by:"human", kind:"edit"}` then `plan.accepted{plan_rev:n+1}`; an edit below the floor gets floor phases added, not refused;
- (f) reject cancels;
- (g) a restart while paused keeps the gate open and resumable;
- (i) **re-open:** approve-with-edit whose edit is refused publishes `plan.refused` and a second `gate.opened{kind:"plan_approval"}` with a **different** `gate_id` (its `event_id` differs from the first). **A second human edit on the re-opened gate publishes a new `plan.proposed` with a `proposal_id` distinct from the first edit's (it is derived from the second `gate_id`); both rows exist on the bus (distinct `event_id`s), and neither resolves to the other.** Approving it publishes a `gate.decided` referencing the second `gate_id` and dispatches once. The same holds for a second revision that needs approval at the same ord before the next unit dispatches, and after a restart between the two openings. (h) in manual mode with the §8.5 override: the override is recorded on `plan.accepted.override` and shown in the gate prompt; the same override in auto mode is refused.

**T4 — Re-plan (core).** `plan.revised`, `Command::RevisePlan`, the three triggers, re-score at checkpoints (`RESCORE_MIN_INTERVAL`, `RESCORE_MAX`), the ratchet, and no re-run.
*Accept:*
- (a) a checkpoint whose settled diff re-scores from band 20–39 into 40–69 publishes `path.scored{basis:"diff"}` then `plan.revised{reason:"floor_raised", added:[test_plan, design]}`, inserted after the cursor; a later lower score publishes nothing;
- (b) **revision into high risk:** in auto mode, a re-score into band 70–100 (or a destructive signal) pauses `plan_approval`; a revision in auto mode that stays below high risk does not pause; in manual mode every revision pauses;
- (c) a floor phase whose catalog position precedes a done step is inserted at the cursor with `late:true`, and no done unit is re-dispatched (no `unitDispatched` for any done ord after the revision);
- (d) **a PA revision on a user plan:** a user-composed plan plus a PA `PLAN+` block produces `plan.proposed{kind:"change", by:<PA>}` and then the engine's `plan.revised{reason:"pa_added"}`. The user's steps are all still present, in order. Two concurrent proposals against the same `base_rev` (a PA `PLAN+` and a human edit) produce two distinct `plan.proposed` rows (distinct `proposal_id`) and two successive revisions; neither is dropped;
- (e) a member `change.requested` produces a revision only after the PA's `PLAN <change_id>: ACCEPT` plus its `PLAN+` block arrive in `ApplyStepResult`. A test asserts the actor holds no bus connection and never calls `BusDb::poll`;
- (f) a 30-checkpoint burst in 60 s with one tree change produces at most one re-score; the attempt's re-scores never exceed 11.

**T5 — Worker-thread seam (core).** `step.claimed`/`step.completed`; the boundary injector (`outcome:"injected"` dedup on any channel); the gate wait with its fail-closed timeout; the `UnitEvidence.team` snapshot; the transcript render; judge exclusion of ledger authors.
*Accept:* (a) `step.claimed` has a lower `event_id` than every `checkpoint.reached` of the attempt (in-process **and** bus-worker path, `src/cli_runner.rs:1976-2018`); (b) a wrapped unit with an undelivered HIGH on the stream receives a `[team advice]` prior-context block on its next step and one `advice.delivered{channel:"boundary", outcome:"injected"}` row **per rendered finding** (a block that renders three findings publishes three rows with distinct keys); a second step does **not** render it again (an `injected` row exists), whether it was answered or not, and the same holds for a finding already `injected` over `acp_steering`; a finding whose only row is `outcome:"turn_ended"`, `"refused"` or `"not_delivered"` **is** rendered at the next boundary; (c) with no `ledger.folded` within a shortened `FINAL_PASS_BUDGET`, the worker synthesizes the `final_pass:"timed_out"` snapshot itself **without publishing it**; the engine publishes `gate.opened{kind:"unit_review"}` and the unit pauses `team_dispute`; a late supervisor `ledger.folded` is published by S alone and changes nothing already decided; (d) the judge prompt contains the ledger and the transcript inside the WORK fence; DES-001 acceptance #14 (a)–(e) hold with `excluded_seats` from the folded ledger; (e) under `spawn_with_engine` with no bus, the unit produces no team rows and `UnitEvidence.team` holds the local snapshot with `transport:"none"` and an empty ledger.

**T6 — Supervisor on the bus + member steps (core).** Re-homes #609 onto a cursor; deletes `TeamCmd`/`TeamHandle`/`team_finish`/the hold buffer/the S2 CoreEvents; member steps; council calls.
*Accept:*
- (a) DES-001 S2 acceptance #1–#6 and #8 re-expressed on bus rows (one `finding.raised` per confirmed finding; zero for an unchanged tree; `member.joined{status:"failed"}` for the creator instance or an unadmitted seat); (b) a restart between two batches loses no finding: the fold after restart contains the rows published before it; (c) the hold round publishes exactly one `finding.settled` per unaccepted finding; silence ⇒ `held`; (d) DES-001 #15/#16 (a)–(k) with `council.called` and `council.ruled` rows instead of ledger fields, and the `transcript` ids of `council.called` resolving to that finding's rows; (e) a `HELP:` line yields one `help.requested` and, with a stub member, one `help.answered` that the next boundary renders;;
- (f) **member step accepted:** an `owner:"team"` step runs on a member seat (`assigned_cli` = member), does not count until `step.reviewed{verdict:"accepted"}`, and then advances;
- (g) **member step rejected:** `REJECT to:member` produces a rework attempt with the reason as its amendment, and `REJECT to:pa` re-plans it onto the PA seat; the third rejection goes to the PA;
- (h) a member `HOLD` on a rejection convenes one council (`trigger:"member_step"`): YES counts it, NO keeps the rejection, no verdict produces a `team_dispute` pause;
- (i) evaluator ≠ creator: the judge of a member step is neither the member nor any ledger author;
- (j) `grep -rn "TeamCmd\|TeamHandle\|team_finish\|SteerMailbox" src` is empty;
- (k) **restart mid-step:** a team run whose attempt 1 has raised one unanswered HIGH is killed after `step.claimed` and before `step.completed`, then the daemon restarts (both paths: orphan plus `POST /runs/:id/resume`, and armed exec redrive). Then:
  - the supervisor's replay rebuilds the run's state from `stream_floor`;
  - attempt 2's `step.claimed` attaches live, with no gate timeout;
  - attempt 2's boundary advice block carries the HIGH with `carried_from_attempt:1`;
  - the accepted plan and `gate_seq` are those persisted before the kill;
  - a replayed row also delivered live changes nothing;
  - with the bus rows deleted (fixture: `path.started` and attempt 1's rows removed), the gate records `final_pass:"stream_gap"` and pauses for a human instead of passing.

**T7 — ACP carrier (core).** `checkpoint.reached`; the steer sourced from the bus; `advice.delivered{acp_steering}`.
*Accept:* DES-001 S3 acceptance #7–#12 with rows instead of mailbox state: (a) a HIGH row published before a terminal `tool_call_update` produces exactly one `_session/steering` with `idleBehavior:"promptRequired"` and one `advice.delivered{outcome:"injected"}` row per carried finding, sharing one `steer_id`; (b) `promptRequired` ⇒ `turn_ended`, and the boundary injector delivers it on the next step; (c) a non-advertising bridge receives no steer and the finding is `channel:"none"`; (d) a MEDIUM row is never steered; (e) a row for attempt 1 never reaches attempt 2; (f) a finding delivered mid-turn is not re-rendered at the boundary.

**T8 — Crew surface (commands only, §4.0).** `teamEvent` relay; `GET /runs/:id/team`; `GET /catalog`; `POST /plans/preview`; the commands `POST /runs {plan, workflow}`, `POST /runs/:id/plan` and `POST /runs/:id/gate`. Crew publishes **no** `wicked.team.*` row.
*Accept:*
- (a) every `wicked.team.*` row on the bus arrives on `/ws` as `{type:"teamEvent", event}` within the poll interval, tagged `project_id` when filed;
- (b) the read route returns the attempt's rows ordered by `event_id` with the folded ledger, and the run's `transport` (`"bus"` | `"none"`, with its reason); `units: []` for an un-teamed run, and the persisted snapshot when the bus has no rows (fixture: rows deleted);
- (c) `POST /runs/:id/plan` calls `Core::propose_plan`. The **engine** then publishes `plan.proposed{by:"human"}`, and a repeat POST with the same request id publishes no second row;
- (d) approving a `team_dispute` gate calls `confirm_gate`; the **engine** publishes `gate.decided{by:"human"}` and `resumed` follows as DES-001 §6.7 (approve never re-dispatches);
- (e) `POST /plans/preview` returns the same floor fill T2 computes;
- (f) `POST /runs {plan:{steps}}` makes the **engine** publish `plan.proposed{by:"human"}` before the first unit dispatches;
- (g) a grep test fails the build if crew source calls `bus.emit` with a `wicked.team.` type.

**T9 — Studio (later seam, after T8).** Phase picker, presets dropdown and save, floor-added markers, plan card, plan approval gate, team panels, launch seat picker.
*Accept:* (a) picking `build` alone for a repo change that previews at band 40–69 shows `test_plan`, `design` and `review` added by floor, and in auto mode they cannot be removed; (b) saving a preset and relaunching it reproduces the selection; (c) the plan approval gate shows the plan diff and approves through `POST /runs/:id/gate`; (d) a run with one HIGH finding, a decline, a hold and a council NO shows, in order, the finding, the delivery, the answer, the hold, the call and the ruling in the feed, and the gate panel groups them under the finding with the human's approve/reject still going through `POST /runs/:id/gate`; (e) a late-joining tab renders the same list from the read route; (f) an un-teamed run shows `transport: none`, not an empty "clean" team; (g) Playwright at 1440×700, via the studio UI, not the API. Test with Playwright at 1440×700, through the UI.

**Migration seams (after T0–T5, C1, C2).** Each seam does three things. It adds the built-in preset (the §11.2 row). It switches the launcher to name the preset (usually no code change: the same string). It deletes the old def, mirror, `workflows/<id>.json`, any boot registration or seeding, and the `builtin-overlay-shadow` / `armed-workflow-served` entry.

**Acceptance for every M-seam:**
- (i) the §11.3 row's named contract tests pass, and each one changed only where §11.3's *What changes* column says;
- (ii) a launch test of that consumer's shape through the preset produces the same ordered unit list, gates and pins as C1(a)'s fixture;
- (iii) `rg -n '"<id>"' workflows/ packages/crew/src/core/adapter.ts` and the def constructor grep are empty, so no dual path remains.

| Seam | Consumer(s) | Extra acceptance / risk note |
|---|---|---|
| M1 | feature, bug (+ C3: deliver composition moves from `composeDeliverWorkflow` into floor fill) | All crew `tests/deliver-*.test.ts` green; the feature role changes (test and final review on evaluator seats) asserted |
| M2 | migration | cleanup is `build` (§11.2, §11.3). The contract tests assert the change: cleanup's unit carries `EVIDENCE_FLOOR_PIN`, `executes_code:true` and `role:creator`, and a cleanup that leaves no evidence is denied |
| M3 | chat | — |
| M4 | onboarding | `onboarding-phases`, `onboarding-run-seats` |
| M5 | survey-repo, memories, domain-graph-slice | a new preset launch test each (none exists today) |
| M6 | domain-extraction | `domain_extraction_e2e.rs`; the stage badge change is visible in studio |
| M7 | capture-learnings | `capture-learnings-skill-ref.test.ts` |
| M8 | steering-author, collab | `governance-steering-routes.test.ts` |
| M9 | interactive-* (5) | crew boot stops registering and seeding them; all five `interactive-*-events` tests |
| M10 | qe-author-tests | `qe-author-workflow`, `testing-author-route`, `testing-routes` |
| M11 | the operator overlay directory, `WorkflowRegistry::with_defaults`, core `workflows/`, and crew's mirror guards | The last one. `resolve_workflow_def` resolves presets only; a left-over drop-in file produces one boot warning; `WorkflowRegistry` keeps only the per-run defs |

## 15. Superseded clauses of DES-TEAMING-001

| DES-001 clause | Status under DES-002 | Replacement |
|---|---|---|
| §3 "A per-daemon TeamSupervisor subscribes to the engine's event fan-out … emitted as `teamLedger`" | superseded | §3; the supervisor holds a bus cursor; `ledger.folded` (S) and `gate.opened{kind:"unit_review"}` (E) |
| §4.2 "Source: engine CoreEvents in-process"; "The one event it consumes: `unitCheckpoint`"; "Attach, finish and context … travel on a direct `TeamCmd` channel" | superseded | §4.2, §6 #7/#8/#14, §10 |
| §4.7 "Owner: the shared worker-thread seam … `team::finish` … process-wide handle … `TeamCmd::Finish`" | superseded | §8.11: `step.completed` plus a bounded wait for `ledger.folded`; the timeout synthesis is kept |
| §4.7 step 4 "(S3) Parse the worker's `ADVICE` lines → `workerAdviceResponse`" | transport superseded | `advice.answered`; the parser stays |
| §5.2 "Mailbox … written by the supervisor … drain all of it" | superseded | §8.9: the source is a bus poll; the delivery point and the request are unchanged |
| §5.1 rows "ACP, adapter does not advertise it … gate only" and "Wrapped non-claude … Gate only" | superseded | §8.9: every carrier gets advice at its next step boundary |
| §5.3 "Teaming does not route a worker's question to a monitor" | narrowed | §8.8: `HELP:` goes to the team; `AskUserQuestion` still goes to a human |
| §6.1 "emitted once as `teamLedger` … before `GateEvaluated`" | superseded | `ledger.folded` (S) then `gate.opened{kind:"unit_review"}` (E); the CoreEvent order is unchanged |
| §6.4 route shape | extended | adds `transcript` and the snapshot fallback |
| §6.5 live-feed CoreEvents | superseded | `teamEvent` frames |
| §7 (six `CoreEvent` variants) | superseded | §6 |
| §8 S4 read of `plan.monitors` only | extended | the band also sets the floor and the high-risk rule (§8.5) |
| §9, §10 rows naming `TeamCmd`, the mailbox, the process-wide handle, the six `to_json` arms; the build order | superseded | §13, §14 |
| §11 "the mailbox is keyed" row | superseded | the attempt's floor is its `step.claimed` id |
| §13 "The per-attempt ledger is lost on a daemon restart mid-unit" | closed | §12 |
| §4.1, §4.3–§4.6, §4.8, §5.1 (Claude ACP row), §5.2 (request shape), §5.3 (text, parsing, authority), §6.2, §6.3, §6.6, §6.7, §8.1, §12 acceptance semantics | **kept** | re-expressed on bus rows where §14 says so |

## 16. Open questions (not blocking the build)

- **Q1. The operator inject bar** (`InjectWorkerMessage`, `src/actor.rs:2557`) stays what it is: an API **command** to the engine, delivered on the next matching unit's prompt, with its own telemetry (`workerMessageInjected`, `src/event.rs:1045`). Under §4.0 it could never become a team fact published by crew. If it is ever surfaced on the team stream, it will be as an engine-owned fact published after the engine accepts the command.
- **Q2. Bus retention for evidence.** The snapshot covers the gate. Keeping raw transcripts past the TTL would be a wicked-bus feature (`retention: forever` exists only in `DESIGN-v2.md:97`).
- **Q3. SPEC filter text vs code.** `reqs/SPEC.md:700-727` documents single-level `.*` only, but the code and three crew consumers use `prefix.**`. SPEC should match the code.
- **Q4. Member seat diversity.** Unchanged from DES-001 §13 Q2: only claude is ACP-admitted.
- **Q5. migration/cleanup: decided (2026-09-24)** as `build` (§11.2, §11.3, M2).
- **Q6. Security review skill id.** `security_review`'s `skill_ref` names the garden QE security specialist; the exact skill id is fixed in C1 against garden's catalog.

### 16.1 Rev 5 sweep: every "as today" / "unchanged" claim checked against the code

**Corrected (the claim was wrong):**

| Claim | Where | What the code does | Fix |
|---|---|---|---|
| The fence refuses `NoEligibleSeat` "when none exists, exactly as today" | §8.1 | A bench-free roster keeps review on the creator seat, disclosed as `creator_seat` (`src/distribute.rs:286-290`, `:417-426`); refusal only when a bench caused it (`:388-409`) | Rewritten; team-run rule (operator-confirmed); seam D1 |
| "The judge excludes every party" | §9 | Only the work author is excluded today (`src/cli_runner.rs:919`, `:937-942`, `:1020-1025`); no `excluded_seats` exists | Stated as today vs built in T5 |
| "Continue or pause (DES-001 §6.7, unchanged)" | §8.10 | `team_dispute` appears nowhere in `src/`; DES-001's S6 is not built | Stated as specified-not-built; T6 builds it |
| The team-dispute re-open argument cited `src/actor.rs:7736` | §6.1 | The refuse-before-resolve rule is at `src/actor.rs:7758` | Cite corrected |

**Verified true (the claim stands):**
- Plan step fields `instructions`, `skill_ref`, `allowed_skills`, `required_deliverables`, `depends_on` (`src/workflow.rs:657`, `:683-708`).
- The pause is durable in one batch (`src/actor.rs:6036-6046`).
- Reject cancels the run (`src/actor.rs:7831-7832`, `cancel_run`).
- S3's steer carries `idleBehavior:"promptRequired"` (`src/team.rs:1923-1928`) at the `session/update` delivery point (`src/acp_runner.rs:4903`).
- `plan_from_def` copies `gate` onto the unit (`src/plan.rs:179`) and `role` (`:155`).
- The repo-checks floor arms for every bound agent unit that changed the tree (`src/cli_runner.rs:868`).
- The CoreEvent order `GateEvaluated` → `GateDecided` → `UnitDone`/`UnitDenied` (`src/pipeline.rs:1519-1556`).
- `combine_verdict` reads only the deterministic pass and the agent verdict (`src/validator.rs:2263`).
- The deliver unit is recognised by its phase id (`src/deliver_lift.rs:54-60`); its command is crew's `deliverPrScript` (`packages/crew/src/core/deliver.ts:791`), now stated in §8.3.
- The operator inject path and the council ballot events are untouched by this design (§10 "Kept").

**Design statements, not code claims:**
- "unchanged" references to DES-001 sections (§4.3–§4.6 rules, §5.3 text) mean *this document does not change DES-001's rule*. Of those, S2 (#609) and S3 (#607) are on main; S6 is not.

