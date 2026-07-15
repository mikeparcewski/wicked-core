# DES-OUTGOV-007 — end-to-end governed domain-extraction run (core#28)

**Milestone 6 / outcome #2 join point.** Prove ONE governed `domain-extraction` run end-to-end
(annotated estate store → coverage 1.0 → `requirements_graph.json`, with the input+output gates
enforcing) **and** wire the currently-inert per-unit conformance-**rule** recall into the run loop.
Scope: integration + one bounded seam edit — **no new engine primitives**. Recon: workflow
`wf_3e49462d` (source-verified). Supersedes the recon's Stop-hook mechanism (see §3).

## 1. What already composes (verified, do NOT rebuild)

- **Def-driven run loop.** `run --workflow --repo` → `run_interactive` → `Core::launch_run`
  (`src/bin/wicked-core.rs:462`) → `launch_run_inner` (`actor.rs:865`) → `plan_and_distribute`
  (`pipeline.rs:238`): `resolve_workflow_def` → `plan_from_def` (one WorkUnit/phase) →
  `attach_pinned_validators` (fail-closed) → `dispatch_unit` → `run_unit_and_judge` →
  `apply_and_finish_unit` (`pipeline.rs:1187`) → deny-dominant fold → `fail_run`/`finalize_run`.
- **INPUT gate — wired, fails runs.** `arm_input_governance` (`execute_wrapped.rs:412`) writes a
  per-unit `--settings` PreToolUse hook (`'<exe>' gate-hook`), sets `WICKED_GATE_SCOPE/PHASE`,
  `WICKED_DECISIONS_PATH`, `WICKED_ESTATE_DB` on the child; `run_gate_hook` appends a claim per
  tool-call; `fold_input_denial` (`gate_hook.rs:416`) folds this phase's claims → Deny →
  `validator_denial` (deny-dominates, `pipeline.rs:456`) → phase Rejected, **no `work_output`** →
  `fail_run`. Proven in `tests/governance_in_run.rs`.
- **POLICY-over-OUTPUT deny — wired.** `execute::apply_unit` (`execute.rs:64`) builds the gate
  context with `"work": output` (the CLI's produced text, `:87-94`), runs `select` + `decide`
  (`:97-105`) at `phase = unit_phase(ord)`; a policy matching the output denies → phase Rejected.
  Proven by `deny_policy`/`register_policy` in `tests/seam_findings.rs:135-170`.
- **Coverage deterministic gate** (pin `c4cc487a030d57b7`, `provision_and_approve_coverage_validator`
  / `seed-domain-validators`), fail-closed with no workdir (`pipeline.rs:562`).
- **Store→artifact emitters** `wicked-core coverage` / `domain-graph` recompute coverage from the
  store and write `coverage-report.json` / `requirements_graph.json`, fail-closed `< 1.0`
  (`wicked-core.rs:569-724`). Standalone subcommands — **not auto-invoked by the run loop**.
- **`workflows/domain-extraction.json`** — load-valid, 5 phases, `skill_ref`s match the real
  `wicked-garden/skills/modernize*` (core#27), coverage pin matches the seeder.

## 2. The gap

The per-unit conformance-**rule** RECALL→obligation (M6/M7) is **inert in every run**:
`attach_recalled_rules` (`gate_hook.rs:794`) is called **only** by the standalone
`output-gate-hook` subcommand + unit tests, never in the run loop. `apply_unit` runs `select` +
`decide` (policy) over the output but does **not** recall rules. So the applicable conformance
ruleset is never surfaced as obligations on a governed run's claims. `DES-OUTGOV-006:42-47` assigns
this wiring to core#28.

## 3. Resolved mechanism — recall in `apply_unit`, NOT a Stop hook

`attach_recalled_rules` only **pushes obligations** (`conform:<Severity>:<id>:<statement>`,
`gate_hook.rs:801`); it never sets `Decision::Deny`. So the enforcement split is:

| Artifact | Effect | Source |
|---|---|---|
| governance **Policy** | deterministic **DENY** | `decide` (already wired in `apply_unit`) |
| **ConformanceRule** | **recall → obligation** (evidence for a downstream evaluator) | `attach_recalled_rules` (this wiring) |

**Wire it into `apply_unit`, after `decide`, before the gate fires** (`execute.rs:105`). Rationale
for rejecting the recon's Stop-hook idea and the runner-post-step alternative:

- **Stop hook is WRONG for output.** A claude `Stop`/`SubagentStop` event delivers metadata
  (`transcript_path`, `session_id`) on stdin, **not the produced output text**, so
  `claude_output_context` would fall back to governing the event JSON — the gate runs but sees the
  wrong bytes (a silent fail-open). Rejected.
- **Runner post-step is redundant.** `apply_unit` **already** governs the real `work_output`
  in-process, at `phase = unit_phase(ord)`, appending to the run's durable conformance. Adding the
  recall there reuses that seam.
- **`apply_unit` avoids the `gate_hook.rs:482` fail-open trap entirely.** That trap
  (`if claim.phase != phase { continue }` — a mismatched-phase claim's Deny is silently dropped by
  `fold_input_denial`) exists only for the *separate-process, decisions-file* hook path. In
  `apply_unit` the claim's phase **is** `phase_name` by construction — no phase channel, no trap.
- **Zero deny-semantics change.** Recall only appends obligations; the gate still fires on the
  unchanged `decide` decision. It cannot make an Allow into a Deny or vice-versa — it makes the
  applicable ruleset **visible + durable** on the claim.

Edit (bounded): make `attach_recalled_rules` `pub(crate)` in `gate_hook.rs` (same root crate) and call
from `apply_unit` on the `claim` after `decide`, **fail-closed** on a recall error (a recall failure is
a governance failure — never silently drop the ruleset, mirroring `gate_hook.rs:735`). Query: a
**wildcard `RuleQuery::default()`** (recall EVERY applicable rule) — NOT the env-faceted
`output_rule_query()`. The in-process actor has no per-unit facet source, and reading process-global
`WICKED_OUTPUT_*` here would let a stray global export silently NARROW (fail-open) a unit's surfaced
ruleset; the over-broad wildcard is the fail-CLOSED direction (surface more, never fewer). Env-based
facet narrowing stays only in the subprocess `output-gate-hook`, where the launcher scopes it per-run.

## 4. Honest scope of "output gate enforced"

- **Policy violation → denies the phase.** Live today; TEST 2 proves it end-to-end in a run.
- **Rule → recalled as an obligation** on the unit's output claim (evidenced, durable). A pure
  `ConformanceRule` does NOT deny — that is the model (recall→obligation; violation-detection is a
  downstream/semantic check). A rule authored as a governance **Policy** (the `policies/*.json`
  ingest path, core#26) denies via `decide`. TEST 3 proves the **recall wiring** (obligation lands
  on the in-run claim).
- **Per-UNIT, not per-turn.** `apply_unit` governs the unit's produced output at the unit boundary
  — sufficient for the proof-of-done. True per-turn streaming governance (gate each intermediate
  turn during a session) needs live-output streaming / a StageKind the engine does not have yet →
  explicit follow-up, not core#28.

## 5. Driving test — `tests/domain_extraction_e2e.rs` (`#![cfg(unix)]`)

The coverage validator is a POSIX grep script (`domain_extraction.rs:43`), so the suite is unix-gated.
Only genuinely new code: a ~30-line `DomainExtractionRunner: StepRunner` keyed on `input.unit.ord`
(units 1-3 write stub deliverables into `input.workdir`; unit 4 shells `BIN coverage --db <db> --out
<workdir>/coverage-report.json`; unit 5 shells `BIN domain-graph --db <db> --out
<workdir>/requirements_graph.json`). Everything else is copy-reuse:
`StubDispatcher`/`cli`/`wait_status`/`deny_policy` (`tests/seam_findings.rs`), `seed(db,accounted)`
(`tests/coverage_cli.rs:22-47`), `make_git_repo` (`tests/p3_repo.rs:76`),
`const BIN = env!("CARGO_BIN_EXE_wicked-core")`; call `provision_and_approve_coverage_validator`,
`Core::{spawn_with_engine,register_repo,launch_run,sessions_detail,confirm_gate,work_output}`,
`wicked_governance::{register_policy,register_rule}`.

Fixture invariants (from recon): `set_var("WICKED_WORKFLOWS_DIR", <manifest>/workflows)` (domain-extraction
is a drop-in, not built-in); `provision_and_approve_coverage_validator(&mut store)` on the same db
BEFORE spawn (else `attach_pinned_validators` bails the run); `register_repo` + `repo_ref: Some(..)`
(the coverage validator fail-closes without a workdir); file-backed SQLite (in-process governance is
`Some` only for a file store, `actor.rs:100`).

- **TEST 1 — happy path.** Seed fully-accounted store → approve validator → drop store →
  `make_git_repo` → launch `domain-extraction`. Domain-graph carries `HumanConfirm{unconditional:false}`
  so the run parks `AwaitingHuman`; assert the worktree has `coverage-report.json` (`coverage:1.0`) +
  `requirements_graph.json`; `confirm_gate(Approve)` → `wait_status(Completed)`.
- **TEST 2 — a policy violation denies a phase.** Same setup + `register_policy(deny_policy("unit-3",
  "LEAKTOKEN"))`; runner emits `LEAKTOKEN` in unit-3 output → assert `Failed`, `units[2].status ==
  Rejected`, **no** `requirements_graph.json` (halted before domain-graph). Exercises the wired
  policy-over-output path.
- **TEST 3 — a conformance rule is recalled into the run.** `register_rule` a `ConformanceRule` → run →
  query the persisted `conformance_claim` nodes (`NodeKind::Other("conformance_claim")`) and assert one
  carries an obligation `conform:...:<rule-id>:...` in its `metadata.obligations`. Proves the recall→gate
  wiring is live in a run (impossible before this milestone).
- **TEST 4 — a coverage HOLE is DENIED by the pinned validator in a run** (the enforcement proof vs
  TEST 1's happy path). Seed the store BELOW full coverage (`seed(db, false)` — a bare function, no
  requirement) → the coverage phase's recomputed report is < 1.0 → the pinned coverage validator's grep
  DENIES it → assert the coverage unit (ord 4) is `Rejected` and NO `requirements_graph.json` is produced.
  This is the only test that proves the pinned coverage validator gates IN A RUN (a regression
  disconnecting it would pass TEST 1), and that the agent-judge PASS shim does NOT rescue a hole — the
  DETERMINISTIC validator's deny dominates the agent's PASS. (Folded from the adversarial review, which
  flagged TEST 1 as vacuous w.r.t. its own "enforced" claim.)

## 6. Risks / decisions

1. **Fail-closed recall.** `apply_unit` must treat a recall error as a governance failure (deny/err),
   never a silent skip — mirror `gate_hook.rs:735`.
2. **Per-unit not per-turn** — documented above; the finer-grained streaming case is a follow-up.
3. **SQLite-only governance** — `in_process_governance()` returns `Some` only for a file store
   (`actor.rs:100`); `:memory:`/postgres runs are silently ungoverned (core#30). Proof runs on file
   SQLite.
4. **Concurrent SQLite in the test** — the runner shells `BIN coverage --db <db>` while the actor
   holds the db; WAL tolerates readers + one writer and the emitters only read. If flaky, fall back to
   the hand-mocked full-coverage report + a direct `recompute_front_half_coverage(&store)==1.0` assert.
5. **`decide` newline-anchor limitation** (`gate_hook.rs:761`) — TEST 3's rule statement + TEST 2's
   policy trigger must be plain substrings, not line anchors.
6. **`required_deliverables` has zero consumers** (`plan_from_def` drops it, `plan.rs:76`) — the engine
   will not enforce `requirements_graph.json` presence; the TEST asserts it. (Wiring deliverable
   enforcement is out of scope — a separate hardening.)
