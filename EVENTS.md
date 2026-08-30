# wicked-core event catalog (generated)

> **GENERATED FILE — do not hand-edit.** Regenerate with
> `python3 crates/wicked-governance/seed/gen_event_catalog.py --workspace <dir containing the wicked-* checkouts>`.
> Rows come from the machine-readable seams listed under "Seams scanned";
> trigger/payload prose comes from `crates/wicked-governance/seed/event-catalog-annotations.json`.
> `--check` exits non-zero when this file or the wicked-bus SPEC catalog block is stale;
> `--drift [--json]` prints the drift report below as a standalone query.

## Core-declared event constants

| Constant | Event type | Declared at | In apps-core `EVENT_CATALOG` | Emit seam (non-test) |
|---|---|---|---|---|
| `EV_AGENT_PLAN_CREATED` | `wicked.crew.agent_plan.created` | `crates/wicked-apps-core/src/lib.rs:193` | yes | — |
| `EV_AGENT_SESSION_COMPLETED` | `wicked.crew.agent_session.completed` | `crates/wicked-apps-core/src/lib.rs:196` | yes | — |
| `EV_AGENT_SESSION_STARTED` | `wicked.crew.agent_session.started` | `crates/wicked-apps-core/src/lib.rs:192` | yes | — |
| `EV_AGENT_TASK_COMPLETED` | `wicked.crew.agent_task.completed` | `crates/wicked-apps-core/src/lib.rs:195` | yes | — |
| `EV_AGENT_WORK_DISTRIBUTED` | `wicked.crew.agent_work.distributed` | `crates/wicked-apps-core/src/lib.rs:194` | yes | — |
| `EV_CLI_RANKED` | `wicked.crew.cli.ranked` | `crates/wicked-apps-core/src/lib.rs:188` | yes | `crates/wicked-council/src/worker.rs` |
| `EV_CONFORMANCE_RECORDED` | `wicked.crew.conformance.recorded` | `crates/wicked-apps-core/src/lib.rs:166` | yes | — |
| `EV_CONFORMANCE_RECORDED_LITERAL` | `wicked.crew.governance.conformance_recorded` | `crates/wicked-governance/src/engine.rs:42` | no | `crates/wicked-governance/src/engine.rs` |
| `EV_COUNCIL_DELIBERATED` | `wicked.crew.council.deliberated` | `crates/wicked-apps-core/src/lib.rs:181` | yes | `crates/wicked-council/src/worker.rs` |
| `EV_COUNCIL_REQUESTED` | `wicked.crew.council.requested` | `crates/wicked-apps-core/src/lib.rs:178` | yes | `crates/wicked-council/src/worker.rs` |
| `EV_COUNCIL_SEAT_FAILED` | `wicked.crew.council_seat.failed` | `crates/wicked-apps-core/src/lib.rs:187` | yes | `crates/wicked-council/src/worker.rs` |
| `EV_COUNCIL_VOTED` | `wicked.crew.council.voted` | `crates/wicked-apps-core/src/lib.rs:179` | yes | `crates/wicked-council/src/worker.rs` |
| `EV_DOC_DRIFTED` | `wicked.estate.doc.drifted` | `crates/wicked-governance/src/events.rs:56` | no | `crates/wicked-governance/src/events.rs` |
| `EV_PHASE_APPROVED` | `wicked.crew.phase.approved` | `crates/wicked-apps-core/src/lib.rs:174` | yes | — |
| `EV_PHASE_READY_FOR_GATE` | `wicked.crew.phase.ready-for-gate` | `crates/wicked-apps-core/src/lib.rs:173` | yes | — |
| `EV_PHASE_REJECTED` | `wicked.crew.phase.rejected` | `crates/wicked-apps-core/src/lib.rs:175` | yes | — |
| `EV_PHASE_STARTED` | `wicked.crew.phase.started` | `crates/wicked-apps-core/src/lib.rs:172` | yes | — |
| `EV_PHASE_TRANSITIONED` | `wicked.crew.phase.transitioned` | `crates/wicked-orchestration/src/gate.rs:30` | no | `crates/wicked-orchestration/src/gate.rs` |
| `EV_POLICY_EVALUATED` | `wicked.crew.policy.evaluated` | `crates/wicked-apps-core/src/lib.rs:165` | yes | — |
| `EV_POLICY_REGISTERED` | `wicked.crew.policy.registered` | `crates/wicked-apps-core/src/lib.rs:164` | yes | — |
| `EV_POLICY_VIOLATED` | `wicked.crew.policy.violated` | `crates/wicked-apps-core/src/lib.rs:167` | yes | — |
| `EV_RULE_INGESTED` | `wicked.estate.rule.ingested` | `crates/wicked-governance/src/events.rs:51` | no | `crates/wicked-governance/src/events.rs` |
| `EV_RULE_RETIRED` | `wicked.estate.rule.retired` | `crates/wicked-governance/src/events.rs:53` | no | `crates/wicked-governance/src/events.rs` |
| `EV_WORKFLOW_COMPLETED` | `wicked.crew.workflow.completed` | `crates/wicked-apps-core/src/lib.rs:171` | yes | — |
| `EV_WORKFLOW_STARTED` | `wicked.crew.workflow.started` | `crates/wicked-apps-core/src/lib.rs:170` | yes | — |
| `GATE_EVAL_REQUESTED` | `wicked.gate.eval.requested` | `src/cli_runner.rs:88` | no | `src/cli_runner.rs` |
| `GATE_EVAL_RESPONDED` | `wicked.gate.eval.responded` | `src/cli_runner.rs:89` | no | — |
| `RUN_LAUNCHED` | `wicked.crew.run.launched` | `src/bus.rs:600` | no | `src/bus.rs` |
| `RUN_REQUESTED` | `wicked.crew.run.requested` | `src/bus.rs:598` | no | — |
| `TASK_COMPLETED` | `wicked.crew.task.completed` | `src/cli_runner.rs:82` | no | `src/cli_runner.rs` |
| `TASK_DISPATCHED` | `wicked.crew.task.dispatched` | `src/cli_runner.rs:80` | no | `src/cli_runner.rs` |

An emit seam is a non-test line referencing the constant (or its literal type)
within 3 lines of `EmitEvent::new(` / `BusEmit::new(` / `.emit(` — the same
window `--drift` queries. "—" means no emit seam found in the scanned
wicked-core sources: declared, not emit-wired HERE (an emitter in another repo,
e.g. the requester side of `wicked.crew.run.requested`, is outside this scan).

## Declared-vs-emitted drift (computed from the seams — a query, not a guess)

### Declared in apps-core `EVENT_CATALOG` but no emit seam found (non-test)

- `wicked.crew.policy.registered` (`EV_POLICY_REGISTERED`)
- `wicked.crew.policy.evaluated` (`EV_POLICY_EVALUATED`)
- `wicked.crew.conformance.recorded` (`EV_CONFORMANCE_RECORDED`)
- `wicked.crew.policy.violated` (`EV_POLICY_VIOLATED`)
- `wicked.crew.workflow.started` (`EV_WORKFLOW_STARTED`)
- `wicked.crew.workflow.completed` (`EV_WORKFLOW_COMPLETED`)
- `wicked.crew.phase.started` (`EV_PHASE_STARTED`)
- `wicked.crew.phase.ready-for-gate` (`EV_PHASE_READY_FOR_GATE`)
- `wicked.crew.phase.approved` (`EV_PHASE_APPROVED`)
- `wicked.crew.phase.rejected` (`EV_PHASE_REJECTED`)
- `wicked.crew.agent_session.started` (`EV_AGENT_SESSION_STARTED`)
- `wicked.crew.agent_plan.created` (`EV_AGENT_PLAN_CREATED`)
- `wicked.crew.agent_work.distributed` (`EV_AGENT_WORK_DISTRIBUTED`)
- `wicked.crew.agent_task.completed` (`EV_AGENT_TASK_COMPLETED`)
- `wicked.crew.agent_session.completed` (`EV_AGENT_SESSION_COMPLETED`)

### Emit-wired core constants declared OUTSIDE apps-core `EVENT_CATALOG`

- `wicked.crew.governance.conformance_recorded` (`EV_CONFORMANCE_RECORDED_LITERAL`) — emitted at `crates/wicked-governance/src/engine.rs`
- `wicked.estate.doc.drifted` (`EV_DOC_DRIFTED`) — emitted at `crates/wicked-governance/src/events.rs`
- `wicked.crew.phase.transitioned` (`EV_PHASE_TRANSITIONED`) — emitted at `crates/wicked-orchestration/src/gate.rs`
- `wicked.estate.rule.ingested` (`EV_RULE_INGESTED`) — emitted at `crates/wicked-governance/src/events.rs`
- `wicked.estate.rule.retired` (`EV_RULE_RETIRED`) — emitted at `crates/wicked-governance/src/events.rs`
- `wicked.gate.eval.requested` (`GATE_EVAL_REQUESTED`) — emitted at `src/cli_runner.rs`
- `wicked.crew.run.launched` (`RUN_LAUNCHED`) — emitted at `src/bus.rs`
- `wicked.crew.task.completed` (`TASK_COMPLETED`) — emitted at `src/cli_runner.rs`
- `wicked.crew.task.dispatched` (`TASK_DISPATCHED`) — emitted at `src/cli_runner.rs`

### Emit-seam-only types (no registry or const declaration anywhere)

- `wicked.interactive.artifact.created` — emitted at `wicked-interactive:src/artifact/create.js`
- `wicked.interactive.artifact.published` — emitted at `wicked-interactive:src/artifact/publish.js`
- `wicked.interactive.artifact.validation_failed` — emitted at `wicked-interactive:src/artifact/validate.js`
- `wicked.qe.deploy.completed` — emitted at `wicked-garden:scripts/qe/lib/gate.mjs`
- `wicked.qe.gate.conditional` — emitted at `wicked-garden:scripts/qe/lib/gate.mjs`
- `wicked.qe.gate.failed` — emitted at `wicked-garden:scripts/qe/lib/gate.mjs`
- `wicked.qe.gate.passed` — emitted at `wicked-garden:scripts/qe/lib/gate.mjs`
- `wicked.test.evidence.captured` — emitted at `wicked-ledger:lib/bus-emit.mjs`
- `wicked.test.run.completed` — emitted at `wicked-ledger:lib/bus-emit.mjs`
- `wicked.test.run.started` — emitted at `wicked-ledger:lib/bus-emit.mjs`
- `wicked.test.scenario.authored` — emitted at `wicked-ledger:lib/bus-emit.mjs`
- `wicked.test.strategy.generated` — emitted at `wicked-ledger:lib/bus-emit.mjs`

### Grammar conformance (seed corpus `event-grammar.md`, POL-1801/POL-1802)

- `wicked.crew.phase.ready-for-gate` — segment(s) outside WB-001 charset [a-z0-9_]: `ready-for-gate`
- `wicked.gate.eval.requested` — producer domain `gate` not in the POL-1802 whitelist (crew, estate, garden, interactive, qe; legacy: test)
- `wicked.gate.eval.responded` — producer domain `gate` not in the POL-1802 whitelist (crew, estate, garden, interactive, qe; legacy: test)

### Types with seams in more than one repo (shared contract — informational)

- `wicked.crew.phase.transitioned` — wicked-core, wicked-garden
- `wicked.crew.run.launched` — wicked-core, wicked-garden
- `wicked.crew.run.requested` — wicked-core, wicked-garden
- `wicked.crew.task.completed` — wicked-core, wicked-garden
- `wicked.crew.task.dispatched` — wicked-core, wicked-garden
- `wicked.gate.eval.requested` — wicked-core, wicked-garden
- `wicked.gate.eval.responded` — wicked-core, wicked-garden
- `wicked.interactive.chat.posted` — wicked-crew, wicked-interactive
- `wicked.interactive.demo.requested` — wicked-crew, wicked-interactive
- `wicked.interactive.doc.created` — wicked-crew, wicked-interactive
- `wicked.interactive.draft.completed` — wicked-crew, wicked-interactive
- `wicked.interactive.edit.completed` — wicked-crew, wicked-interactive
- `wicked.interactive.feedback.processed` — wicked-crew, wicked-interactive
- `wicked.interactive.status.posted` — wicked-crew, wicked-interactive
- `wicked.test.verdict.created` — wicked-garden, wicked-ledger

## Ecosystem catalog (all domains)

The same generated tables are published into `wicked-bus/reqs/SPEC.md` §Event
Catalog (between the AW-21 markers) — the bus SPEC remains the reader-facing
home of the catalog; this file is the engine-side view plus the drift report.

#### `crew` events — engine (wicked-core), crew product lifecycle, and garden's shared phase lifecycle

| Event type | Declared / emitted at | Trigger / description | Key payload fields |
|---|---|---|---|
| `wicked.crew.agent_plan.created` | `core:crates/wicked-apps-core/src/lib.rs` (const — **no emit seam**) | — | — |
| `wicked.crew.agent_session.completed` | `core:crates/wicked-apps-core/src/lib.rs` (const — **no emit seam**) | — | — |
| `wicked.crew.agent_session.started` | `core:crates/wicked-apps-core/src/lib.rs` (const — **no emit seam**) | — | — |
| `wicked.crew.agent_task.completed` | `core:crates/wicked-apps-core/src/lib.rs` (const — **no emit seam**) | — | — |
| `wicked.crew.agent_work.distributed` | `core:crates/wicked-apps-core/src/lib.rs` (const — **no emit seam**) | — | — |
| `wicked.crew.cli.ranked` | `core:crates/wicked-apps-core/src/lib.rs` (const) | — | — |
| `wicked.crew.conformance.recorded` | `core:crates/wicked-apps-core/src/lib.rs` (const — **no emit seam**) | — | — |
| `wicked.crew.council.deliberated` | `core:crates/wicked-apps-core/src/lib.rs` (const) | — | — |
| `wicked.crew.council.requested` | `core:crates/wicked-apps-core/src/lib.rs` (const) | — | — |
| `wicked.crew.council.voted` | `core:crates/wicked-apps-core/src/lib.rs` (const) | — | — |
| `wicked.crew.council_seat.failed` | `core:crates/wicked-apps-core/src/lib.rs` (const) | — | — |
| `wicked.crew.governance.conformance_recorded` | `core:crates/wicked-governance/src/engine.rs` (const) | ConformanceClaim recorded by the governance engine (`wicked-governance src/engine.rs`) | claim ids + `obligation_count` (no rule text) |
| `wicked.crew.membership.attached` | `crew:packages/crew/src/projects/events.ts` (const) | Run attached to a crew project; `domain` stamp `wicked-crew` | project id, run id |
| `wicked.crew.membership.detached` | `crew:packages/crew/src/projects/events.ts` (const) | Run detached from a crew project; `domain` stamp `wicked-crew` | project id, run id |
| `wicked.crew.phase.approved` | `core:crates/wicked-apps-core/src/lib.rs` (const — **no emit seam**) | — | — |
| `wicked.crew.phase.ready-for-gate` | `core:crates/wicked-apps-core/src/lib.rs` (const — **no emit seam**) | — | — |
| `wicked.crew.phase.rejected` | `core:crates/wicked-apps-core/src/lib.rs` (const — **no emit seam**) | — | — |
| `wicked.crew.phase.started` | `core:crates/wicked-apps-core/src/lib.rs` (const — **no emit seam**) | — | — |
| `wicked.crew.phase.transitioned` | `core:crates/wicked-orchestration/src/gate.rs` (const)<br>`garden:scripts/_bus.py` (registry) | Phase approved and advanced to next | — |
| `wicked.crew.policy.evaluated` | `core:crates/wicked-apps-core/src/lib.rs` (const — **no emit seam**) | — | — |
| `wicked.crew.policy.registered` | `core:crates/wicked-apps-core/src/lib.rs` (const — **no emit seam**) | — | — |
| `wicked.crew.policy.violated` | `core:crates/wicked-apps-core/src/lib.rs` (const — **no emit seam**) | — | — |
| `wicked.crew.project.archived` | `crew:packages/crew/src/projects/events.ts` (const) | Crew project archived; `domain` stamp `wicked-crew` | project id |
| `wicked.crew.project.created` | `crew:packages/crew/src/projects/events.ts` (const) | Crew project created; `domain` stamp `wicked-crew` | project id + descriptor |
| `wicked.crew.project.updated` | `crew:packages/crew/src/projects/events.ts` (const) | Crew project updated; `domain` stamp `wicked-crew` | project id + changed fields |
| `wicked.crew.run.launched` | `core:src/bus.rs` (const)<br>`garden:scripts/_bus.py` (registry) | Launch confirmed by the engine (bus-as-truth handoff); `domain` stamp `wicked-core`; idempotency-keyed on the run id | `run_id`, `workflow`, `problem` |
| `wicked.crew.run.requested` | `core:src/bus.rs` (const — **no emit seam**)<br>`garden:scripts/_bus.py` (registry) | Governed run intent (human CLI / scheduler / campaign); `domain` stamp = the requester's own — the engine's launch poller matches by event type, not domain | `workflow?`, `problem`, `args?` (payload contract in `wicked-core src/bus.rs`) |
| `wicked.crew.task.completed` | `core:src/cli_runner.rs` (const)<br>`garden:scripts/_bus.py` (registry) | Workflow unit completion (verdict in payload); `domain` stamp `wicked-core` | run/unit ids + verdict |
| `wicked.crew.task.dispatched` | `core:src/cli_runner.rs` (const)<br>`garden:scripts/_bus.py` (registry) | Workflow unit handoff to a governed worker; `domain` stamp `wicked-core` | run/unit ids + dispatch descriptor |
| `wicked.crew.workflow.completed` | `core:crates/wicked-apps-core/src/lib.rs` (const — **no emit seam**) | — | — |
| `wicked.crew.workflow.started` | `core:crates/wicked-apps-core/src/lib.rs` (const — **no emit seam**) | — | — |

#### `estate` events — governance-corpus lifecycle (wicked-core `wicked-governance`, AW-22)

| Event type | Declared / emitted at | Trigger / description | Key payload fields |
|---|---|---|---|
| `wicked.estate.doc.drifted` | `core:crates/wicked-governance/src/events.rs` (const) | Drift tooling (`wicked-core rules drift`) — once per drifted governed doc | `doc_path`, `doc_id`, `reason`, `rule_ids`, `rule_count` |
| `wicked.estate.rule.ingested` | `core:crates/wicked-governance/src/events.rs` (const) | Every rule `wicked-core rules ingest` registers (after its store commit; JSON-bundle and markdown-doc lanes) | `rule_id`, `rule_type`, `severity`, `retired`, `source`, `ref`, `confidence` — classifications only, never `rule.statement` |
| `wicked.estate.rule.retired` | `core:crates/wicked-governance/src/events.rs` (const) | `retire_rule` after its store commit — only on an actual state change (re-retiring emits nothing) | `rule_id`, `rule_type`, `severity`, `source`, `ref` |

#### `garden` events — wicked-garden registry (`scripts/_bus.py` BUS_EVENT_MAP is the contract)

| Event type | Declared / emitted at | Trigger / description | Key payload fields |
|---|---|---|---|
| `wicked.garden.amendment.appended` | `garden:scripts/_bus.py` (registry) | Phase amendment appended to amendments.jsonl (Site W6 cutover) | — |
| `wicked.garden.archetype.advanced` | `garden:scripts/_bus.py` (registry) | v11 archetype phase approved + (when present) next phase named | — |
| `wicked.garden.archetype.classified` | `garden:scripts/_bus.py` (registry) | v11 prompt classified into work-shape archetype set (LLM or regex tier) | — |
| `wicked.garden.archetype.completed` | `garden:scripts/_bus.py` (registry) | v11 archetype final phase approved (project is_complete) | — |
| `wicked.garden.archetype.created` | `garden:scripts/_bus.py` (registry) | v11 archetype-mode project created (carries v11_archetype + initial phase_plan) | — |
| `wicked.garden.archetype.hard_gate_passed` | `garden:scripts/_bus.py` (registry) | v11 archetype hard gate (cutover/mitigate/etc.) passed with confirmed_by + evidence | — |
| `wicked.garden.compliance.failed` | `garden:scripts/_bus.py` (registry) | Compliance check failed for a framework | — |
| `wicked.garden.compliance.passed` | `garden:scripts/_bus.py` (registry) | Compliance check passed for a framework | — |
| `wicked.garden.condition.marked_cleared` | `garden:scripts/_bus.py` (registry) | Condition verification flipped to verified=True via mark_cleared() (Site 5 cutover) | — |
| `wicked.garden.condition.resolved` | `garden:scripts/_bus.py` (registry) | Mechanical CONDITIONAL finding resolved via crew:resolve skill (verdict unchanged) | — |
| `wicked.garden.consensus.evidence_recorded` | `garden:scripts/_bus.py` (registry) | Consensus rejection evidence written to consensus-evidence.json (audit trail) | — |
| `wicked.garden.consensus.gate_completed` | `garden:scripts/_bus.py` (registry) | Consensus gate verdict written to reviewer-report.md (append or create) | — |
| `wicked.garden.consensus.gate_pending` | `garden:scripts/_bus.py` (registry) | Pending consensus gate placeholder written to reviewer-report.md (evaluation failed) | — |
| `wicked.garden.consensus.report_created` | `garden:scripts/_bus.py` (registry) | Consensus gate report written to consensus-report.json | — |
| `wicked.garden.convergence.transition_recorded` | `garden:scripts/_bus.py` (registry) | Convergence-log transition recorded for an artifact (Site W8 cutover) | — |
| `wicked.garden.council.voted` | `garden:scripts/_bus.py` (registry) | Council evaluation completed with model votes | — |
| `wicked.garden.coverage.changed` | `garden:scripts/_bus.py` (registry) | Test coverage metrics changed | — |
| `wicked.garden.crew.inline_review_context_recorded` | `garden:scripts/_bus.py` (registry) | Inline-HITL gate review evidence recorded by solo_mode (Site W1 cutover) | — |
| `wicked.garden.crew.legacy_adopted` | `garden:scripts/_bus.py` (registry) | Legacy beta.3 → v6.0 project migration applied via adopt_legacy.py (audit marker) | — |
| `wicked.garden.crew.qe_evaluator_migrated` | `garden:scripts/_bus.py` (registry) | qe-evaluator → gate-adjudicator rename applied via migrate_qe_evaluator_name.py (audit marker) | — |
| `wicked.garden.crew.yolo_revoked` | `garden:scripts/_bus.py` (registry) | Yolo auto-approval revoked due to scope-increase mutation (audit + observability) | — |
| `wicked.garden.dispatch.log_entry_appended` | `garden:scripts/_bus.py` (registry) | HMAC-signed dispatch-log.jsonl entry appended (orphan-check sentinel) | — |
| `wicked.garden.fact.extracted` | `garden:scripts/_bus.py` (registry) | Structured fact extracted from conversation (consumed by the garden-run auto-memorize drain -> estate memory) | — |
| `wicked.garden.gate.blocked` | `garden:scripts/_bus.py` (registry) | Gate returned REJECT — phase advancement blocked | — |
| `wicked.garden.gate.decided` | `garden:scripts/_bus.py` (registry) | Gate returned APPROVE, CONDITIONAL, or REJECT | — |
| `wicked.garden.guard.surfaced` | `garden:scripts/_bus.py` (registry) | Autonomous session-close guard pipeline surfaced findings (Issue #448) | — |
| `wicked.garden.hitl.decision_recorded` | `garden:scripts/_bus.py` (registry) | HITL pause-decision evidence recorded by hitl_judge.write_hitl_decision_evidence (Site W5 cutover) | — |
| `wicked.garden.log.rotated` | `garden:scripts/_bus.py` (registry) | Log file rotated by log_retention.rotate_if_needed (audit marker) | — |
| `wicked.garden.loom.parity_mismatched` | `garden:scripts/_bus.py` (registry) | Loom hard-gate verdict differs from the in-process gate result (diagnostic: parity mirror) | — |
| `wicked.garden.modernize.gap_emitted` | `garden:scripts/_bus.py` (registry) | Legacy stack class is planned/none/unknown — capability-gap task emitted instead of a fabricated migration | — |
| `wicked.garden.outgov.pattern_drift_detected` | `garden:scripts/_bus.py` (registry) | Pattern-conformance check found drift between session output and a  | — |
| `wicked.garden.outgov.policy_violation_found` | `garden:scripts/_bus.py` (registry) | Per-turn policy compliance check surfaced a violation (garden#984);  | — |
| `wicked.garden.persona.contributed` | `garden:scripts/_bus.py` (registry) | Persona contributed a perspective in a brainstorm round | — |
| `wicked.garden.phase.auto_advanced` | `garden:scripts/_bus.py` (registry) | Phase auto-advanced for low-complexity project (audit trail) | — |
| `wicked.garden.project.completed` | `garden:scripts/_bus.py` (registry) | Crew project completed (final phase approved) | — |
| `wicked.garden.project.complexity_scored` | `garden:scripts/_bus.py` (registry) | Complexity score computed for a project | — |
| `wicked.garden.project.created` | `garden:scripts/_bus.py` (registry) | New crew project created with complexity scoring | — |
| `wicked.garden.quality.drift_detected` | `garden:scripts/_bus.py` (registry) | Cross-session quality metric drifted past baseline threshold (special-cause or >=15% drop) | — |
| `wicked.garden.reeval.addendum_appended` | `garden:scripts/_bus.py` (registry) | Re-eval addendum appended to per-phase + project-root JSONL logs (Site W7 cutover; dual-file projection) | — |
| `wicked.garden.review.semantic_gap_recorded` | `garden:scripts/_bus.py` (registry) | Semantic-gap report persisted at review phase (Site W10a cutover) | — |
| `wicked.garden.rework.triggered` | `garden:scripts/_bus.py` (registry) | Rework initiated after gate REJECT or CONDITIONAL | — |
| `wicked.garden.scenario.run` | `garden:scripts/_bus.py` (registry) | Test scenario executed with pass/fail result | — |
| `wicked.garden.security.finding_raised` | `garden:scripts/_bus.py` (registry) | Security review raised a finding | — |
| `wicked.garden.sentinel.claim_unverified` | `garden:scripts/_bus.py` (registry) | Sentinel found a claim that could not be independently verified (skip-is-evidence signal) | — |
| `wicked.garden.sentinel.prepush_blocked` | `garden:scripts/_bus.py` (registry) | Pre-push hook blocked a commit due to a failed sentinel invariant check | — |
| `wicked.garden.sentinel.unverified_task_done` | `garden:scripts/_bus.py` (registry) | TaskCompleted hook found a done-claim that could not be independently verified (skip-is-evidence signal) | — |
| `wicked.garden.session.started` | `garden:scripts/_bus.py` (registry) | Brainstorm or council session started | — |
| `wicked.garden.session.synthesis_ready` | `garden:scripts/_bus.py` (registry) | All expected Round 1 personas contributed or timeout elapsed — facilitator may synthesize | — |
| `wicked.garden.session.synthesized` | `garden:scripts/_bus.py` (registry) | Session synthesis completed | — |
| `wicked.garden.subagent.engaged` | `garden:scripts/_bus.py` (registry) | Specialist subagent engagement recorded by subagent_lifecycle (Site W9b cutover) | — |

#### `gate` events — governed evaluator round-trip (wicked-core engine)

| Event type | Declared / emitted at | Trigger / description | Key payload fields |
|---|---|---|---|
| `wicked.gate.eval.requested` | `core:src/cli_runner.rs` (const)<br>`garden:scripts/_bus.py` (registry) | Governed evaluator bus round-trip — request (evaluator≠creator); `domain` stamp `wicked-core` | evaluation request (run/gate ids + material refs) |
| `wicked.gate.eval.responded` | `core:src/cli_runner.rs` (const — **no emit seam**)<br>`garden:scripts/_bus.py` (registry) | Governed evaluator bus round-trip — response; `domain` stamp `wicked-core` | evaluation verdict for the paired request |

#### `interactive` events — wicked-interactive registry (`src/service/events.js` EVENT_TYPES is the contract)

| Event type | Declared / emitted at | Trigger / description | Key payload fields |
|---|---|---|---|
| `wicked.interactive.artifact.created` | `interactive:src/artifact/create.js` (emit) | Artifact scaffolded via the wicked-interactive CLI (`src/artifact/create.js`) | artifact/document ids |
| `wicked.interactive.artifact.published` | `interactive:src/artifact/publish.js` (emit) | Artifact published (`src/artifact/publish.js`) | artifact/document ids |
| `wicked.interactive.artifact.validation_failed` | `interactive:src/artifact/validate.js` (emit) | Artifact failed validation (`src/artifact/validate.js`) | artifact id + failure detail |
| `wicked.interactive.chat.posted` | `crew:packages/crew/src/interactive/chat-events.ts` (mirror)<br>`interactive:src/service/events.js` (registry) | subdomain `chat`; owners: ui, agent; UI-emittable | — |
| `wicked.interactive.demo.requested` | `crew:packages/crew/src/interactive/demo-events.ts` (mirror)<br>`interactive:src/service/events.js` (registry) | subdomain `demo`; owners: ui, agent; UI-emittable | — |
| `wicked.interactive.doc.created` | `crew:packages/crew/src/interactive/draft-events.ts` (mirror)<br>`interactive:src/artifact/create.js` (emit)<br>`interactive:src/service/events.js` (registry) | subdomain `docs`; owners: service | — |
| `wicked.interactive.draft.completed` | `crew:packages/crew/src/interactive/draft-events.ts` (mirror)<br>`interactive:src/service/events.js` (registry) | subdomain `generation`; owners: agent, crew | — |
| `wicked.interactive.edit.completed` | `crew:packages/crew/src/interactive/edit-events.ts` (mirror)<br>`interactive:src/service/events.js` (registry) | subdomain `feedback`; owners: agent, crew | — |
| `wicked.interactive.error.raised` | `interactive:src/service/events.js` (registry) | subdomain `error`; owners: service | — |
| `wicked.interactive.export.generated` | `interactive:src/service/events.js` (registry) | subdomain `export`; owners: service | — |
| `wicked.interactive.export.requested` | `interactive:src/service/events.js` (registry) | subdomain `export`; owners: service | — |
| `wicked.interactive.export.reviewed` | `interactive:src/service/events.js` (registry) | subdomain `export`; owners: agent | — |
| `wicked.interactive.feedback.processed` | `crew:packages/crew/src/interactive/edit-events.ts` (mirror)<br>`interactive:src/service/events.js` (registry) | subdomain `feedback`; owners: service | — |
| `wicked.interactive.feedback.submitted` | `interactive:src/service/events.js` (registry) | subdomain `feedback`; owners: ui; UI-emittable | — |
| `wicked.interactive.question.answered` | `interactive:src/service/events.js` (registry) | subdomain `chat`; owners: ui; UI-emittable | — |
| `wicked.interactive.review.completed` | `interactive:src/service/events.js` (registry) | subdomain `review`; owners: agent | — |
| `wicked.interactive.review.requested` | `interactive:src/service/events.js` (registry) | subdomain `review`; owners: ui, agent; UI-emittable | — |
| `wicked.interactive.source.attached` | `interactive:src/service/events.js` (registry) | subdomain `sources`; owners: ui; UI-emittable | — |
| `wicked.interactive.source.removed` | `interactive:src/service/events.js` (registry) | subdomain `sources`; owners: ui; UI-emittable | — |
| `wicked.interactive.source.updated` | `interactive:src/service/events.js` (registry) | subdomain `sources`; owners: agent | — |
| `wicked.interactive.status.posted` | `crew:packages/crew/src/interactive/draft-events.ts` (mirror)<br>`interactive:src/service/events.js` (registry) | subdomain `status`; owners: agent, service, crew | — |
| `wicked.interactive.status.requested` | `interactive:src/service/events.js` (registry) | subdomain `status`; owners: ui; UI-emittable | — |
| `wicked.interactive.theme.learned` | `interactive:src/service/events.js` (registry) | subdomain `theme`; owners: service | — |
| `wicked.interactive.theme.requested` | `interactive:src/service/events.js` (registry) | subdomain `theme`; owners: ui, agent; UI-emittable | — |
| `wicked.interactive.version.created` | `interactive:src/service/events.js` (registry) | subdomain `versions`; owners: service | — |

#### `qe` events — QE acceptance gate + lifecycle (garden gate CLI, wicked-ledger)

| Event type | Declared / emitted at | Trigger / description | Key payload fields |
|---|---|---|---|
| `wicked.qe.deploy.completed` | `garden:scripts/qe/lib/gate.mjs` (emit) | Deploy signal alongside a PASS | `run_id`, `project_id` |
| `wicked.qe.gate.conditional` | `garden:scripts/qe/lib/gate.mjs` (emit) | CONDITIONAL or SYSTEM_ERROR | same 8 fields as `wicked.qe.gate.passed` |
| `wicked.qe.gate.failed` | `garden:scripts/qe/lib/gate.mjs` (emit) | Acceptance gate FAIL | same 8 fields as `wicked.qe.gate.passed` |
| `wicked.qe.gate.passed` | `garden:scripts/qe/lib/gate.mjs` (emit) | Acceptance gate PASS (garden qe gate CLI) | `run_id`, `context`, `gate_verdict`, `exit_code`, `verdict_summary`, `mode`, `completed_at`, `scenario_count` |
| `wicked.qe.release.assessed` | `garden:scripts/_bus.py` (registry) | Release readiness verdict assessed against ledger evidence window | — |
| `wicked.qe.scenario.authored` | `garden:scripts/_bus.py` (registry) | Scenario file authored from a production incident; queues a human review task | — |

#### `test` events — legacy-stable QE-lifecycle spelling kept at the wicked-testing retirement — the `domain` stamp is `qe`

| Event type | Declared / emitted at | Trigger / description | Key payload fields |
|---|---|---|---|
| `wicked.test.evidence.captured` | `ledger:lib/bus-emit.mjs` (emit) | Evidence written for a run (ledger `verdicts.create` with `vault_payload_sha`; union payload) | `project_id`, `run_id`, `evidence_path`, `qe_version` |
| `wicked.test.run.completed` | `ledger:lib/bus-emit.mjs` (emit) | Test run finishes (ledger `runs.update`, `finished_at` set) | `run_id`, `project_id`, `scenario_id` |
| `wicked.test.run.started` | `ledger:lib/bus-emit.mjs` (emit) | Test run begins (ledger `runs.create`) | `run_id`, `project_id`, `scenario_id`, `started_at` |
| `wicked.test.scenario.authored` | `ledger:lib/bus-emit.mjs` (emit) | Scenario file created/updated (ledger `scenarios.create`) | `scenario_id`, `project_id`, `format_version` |
| `wicked.test.strategy.generated` | `ledger:lib/bus-emit.mjs` (emit) | Test strategy produced (ledger `strategies.create`) | `strategy_id`, `project_id`, `qe_version` |
| `wicked.test.verdict.created` | `garden:scripts/_bus.py` (registry)<br>`ledger:lib/bus-emit.mjs` (emit) | Verdict recorded (ledger `verdicts.create`; garden qe reviewer) | `verdict_id`, `run_id`, `verdict`, `reviewer` |

## Seams scanned

| Repo | File | Role |
|---|---|---|
| wicked-core | `crates/wicked-apps-core/src/lib.rs` | const |
| wicked-core | `src/bus.rs` | const |
| wicked-core | `src/cli_runner.rs` | const |
| wicked-core | `crates/wicked-orchestration/src/gate.rs` | const |
| wicked-core | `crates/wicked-governance/src/events.rs` | const |
| wicked-core | `crates/wicked-governance/src/engine.rs` | const |
| wicked-garden | `scripts/_bus.py` | registry |
| wicked-garden | `scripts/qe/lib/gate.mjs` | emit |
| wicked-ledger | `lib/bus-emit.mjs` | emit |
| wicked-crew | `packages/crew/src/projects/events.ts` | const |
| wicked-crew | `packages/crew/src/interactive/chat-events.ts` | mirror |
| wicked-crew | `packages/crew/src/interactive/edit-events.ts` | mirror |
| wicked-crew | `packages/crew/src/interactive/draft-events.ts` | mirror |
| wicked-crew | `packages/crew/src/interactive/demo-events.ts` | mirror |
| wicked-interactive | `src/service/events.js` | registry |
| wicked-interactive | `src/artifact/create.js` | emit |
| wicked-interactive | `src/artifact/publish.js` | emit |
| wicked-interactive | `src/artifact/validate.js` | emit |
