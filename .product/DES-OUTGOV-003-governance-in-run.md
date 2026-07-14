# DES-OUTGOV-003 — Governance-in-run wiring (milestone #1 / core#24)

The keystone of the outcome-#2 roadmap. Today governance is INERT in a real wrapped-CLI run: the
launcher (`execute_wrapped.rs::exec`) runs the CLI with `current_dir(cwd)` but sets NO governance env
and writes NO hook config, so a wrapped Claude never invokes `gate-hook`; and while the engine's
per-unit gate DOES fire in-process, nothing feeds it the agent's tool-call decisions. This design wires
input governance in and folds both input and output governance into the ONE gate the engine already
resolves.

> **REVISION 2 — corrected after adversarial design review (workflow `wf_860d6262-093`: 20 confirmed
> findings, 10 rejected).** The v1 mechanism — a *separate* `Command::ApplyHookDecisions` drain that
> resolves its own phase node — was WRONG on six convergent counts (findings #1/#2/#7/#11/#16/#17). It
> collides with the engine's existing per-unit gate on the same node `{workflow_id}:unit-{ord}` and
> never fails the run. The corrected design **folds governance into the engine's single per-unit
> deny-dominant gate via the existing `validator_denial` seam** — reusing machinery already built in
> core#20/#21, not racing it. Several factual premises in v1 were also false and are corrected below.

## Recon facts (verified against code)

### The engine already owns exactly ONE per-unit gate — fold into it, don't compete
- `execute::apply_unit` (execute.rs:64) is the SOLE opener + resolver of the unit's phase. It opens
  `phase_id = "{workflow_id}:unit-{ord}"` (execute.rs:77-78), scope
  `resolve_scope(entity_mode, session_id, unit.id)` (execute.rs:79), `Phase::open` → `put_node` →
  `advance_to_gate_running` → `apply_gate` (execute.rs:82-84,124). workflow.rs:9-20 locks the
  "exactly ONE phase opener" invariant. A second resolver (a drain) on the same node is refused by the
  reducer's `from_mismatch` OR clobbered by the raw `put_node` upsert OR duplicates the `advance-{step}`
  event id → `bail!`. **Do not add one.**
- `apply_unit` already takes a **`validator_denial: Option<String>`** deny-dominance seam (execute.rs:71;
  doc: "`None` ⇒ governance decides alone"). pipeline.rs:431 folds
  `validator_denial = det_denial.or(agent_denial).or(evaluator_denial)`. **Input/output-hook denials fold
  in here too** → `.or(hook_denial).or(output_denial)`. One `apply_gate`, deny-dominant across
  governance + dual-validator + evaluator + input-hook + output.
- The run's terminal Failed/Completed status is decided SOLELY in `actor.rs::apply_step_result`
  (the `if !outcome.approved → fail_run` block, actor.rs:1093-1102), keyed on `outcome.approved` from
  `apply_and_finish_unit`. A governance deny MUST flow through `outcome.approved`, not a side channel —
  a Rejected phase node alone does NOT fail the run (finding #16). The one exception
  (`HumanConfirmIf(VerdictNotPass)`) is preserved unchanged.
- `apply_and_finish_unit` (pipeline.rs:358) is the single chokepoint for BOTH delivery modes
  (in-process actor.rs:1080; off-thread pipeline.rs:77) — so folding here covers both.

### Input vs output split
- **INPUT (tool calls):** Claude-only, via a real `PreToolUse` hook. Exit 2 blocks the call at runtime
  AND the hook appends a claim to the decisions ndjson. Non-Claude CLIs get no input hook (documented
  limitation; their input governance stays the existing in-process `execute.rs` path).
- **OUTPUT (generated text):** all CLIs, IN-PROCESS from the launcher's already-captured stdout —
  CLI-agnostic, no hook dependency. NOTE (finding #4, corrected): a Claude-native Stop-hook path DOES
  exist — Claude ≥ 2.1.196 (installed: 2.1.209) sends `last_assistant_message` on Stop/SubagentStop. We
  still govern in-process because it is CLI-agnostic and not version-gated, NOT because the text is
  unavailable to a hook.

### Injection must be OPT-IN, not `is_claude` (finding #14)
- The engine's OWN internal claude invocations (agent-judge, deterministic-validator authoring;
  `run_id="validator"`) flow through the very same `WrappedCliStepRunner::exec`. Internal StepInput
  literals live at validator.rs:125/843 and cli_runner.rs:534/718/822. If governance were injected
  whenever the binary is claude, these self-govern against empty scope/phase/db and fail closed.

### Settings placement — corrected justification (findings #5/#8/#20)
- v1 claimed "the sandbox validator BLOCKS `.claude` (validator.rs:428)". FALSE as applied: that is a
  READ-block of `$HOME/.claude` inside the *deterministic-validator's* OS sandbox
  (`macos_sandbox_profile`, reached only from validator.rs:686). **The wrapped-CLI launcher applies NO
  sandbox at all** (execute_wrapped.rs:290-291 runs the CLI with the orchestrator's full inherited env).
  The real reason to use `--settings <file>` OUTSIDE the worktree: writing `<worktree>/.claude/settings.json`
  pollutes the user's repo tree (spurious `git status` diffs, collision with their own settings). Nothing
  blocks writing `.claude/`, reading `--settings`, or spawning `wicked-core gate-hook`.
- **Security note (finding #8):** the wrapped CLI AND the `gate-hook` subprocess it spawns both inherit
  the orchestrator's full, UNSANDBOXED environment, including API keys/tokens. Documented here as a known
  property of the current launcher; hardening (env minimization for the wrapped CLI) is out of scope for
  this milestone but noted for a follow-up.

### Store-open premise — corrected (findings #3/#9/#19)
- v1 justified a read-only opener by "`open_store` has no `busy_timeout` → a hook racing the actor
  fail-closes into a spurious DENY". FALSE: `SqliteStore::open` sets `PRAGMA busy_timeout=5000`
  (wicked-estate `sqlite.rs:468`, added PR#47 2026-07-10), so a racing open waits up to 5s, it does not
  fail immediately. The TRUE hazard: `SqliteStore::open` runs `execute_batch(SCHEMA)` + `migrate_schema`
  DDL on every on-disk open (sqlite.rs:474/477) — a hook subprocess performing schema WRITES violates
  the single-writer invariant. That is the real reason for a pure-reader opener.

### Cross-repo + backend facts (findings #12/#13/#18)
- `open_store_ro` CANNOT be built in wicked-apps-core — `SqliteStore`'s fields are private and it lives
  in the separate wicked-estate repo (`crates/wicked-estate-store/src/sqlite.rs`, path dep). The read-only
  PRIMITIVE must be added THERE; apps-core `open_store_ro` is a thin delegate. **Milestone #1 is
  cross-repo (wicked-core + wicked-estate).**
- The hook opens the store SQLite-only (`open_store`, gate_hook.rs:88/338) while the actor is
  backend-agnostic (`open_store_any`, actor.rs:98, supports `postgres://`). Handing the hook a
  `postgres://` db bricks governance. **Scope for #1: governance-in-run is SQLite-only, guarded loudly.**

## Design

### §1 — Fold governance into the ONE per-unit gate (replaces the v1 drain)
- **Do NOT dispatch `Command::ApplyHookDecisions` during a governed run.** Instead, in
  `apply_and_finish_unit` (before it resolves the unit): read the unit's claims from the decisions
  ndjson, `conform` each for durable evidence (reuse the conform half of `apply_hook_decisions`), compute
  a deny-dominant `hook_denial: Option<String>` (Deny ≻ AllowWithConditions ≻ Allow), and fold it into
  the existing seam: `validator_denial = det_denial.or(agent_denial).or(evaluator_denial).or(hook_denial)
  .or(output_denial)`. The ONE `apply_gate` on `unit-{ord}` then deny-dominates across all layers,
  resolves the phase Rejected, suppresses the `work_output` write, and — via the unchanged
  `!outcome.approved → fail_run` path — actually FAILS the run.
- The standalone `apply_hook_decisions` drain + its `open-if-absent` phase shim stay ONLY for the P0
  standalone-observability path; they are never invoked during an engine-driven run.

### §2 — INPUT governance: Claude PreToolUse hook, pinned to the unit's real scope/phase
- The launcher writes a per-run settings JSON and passes `--settings <path>` when the CLI is Claude AND
  `input.governance.is_some()`. It declares a `PreToolUse` hook whose command is (finding #6):
  `"<current_exe>" gate-hook --scope <resolve_scope(entity_mode,session_id,unit.id)> --phase unit-<ord>`
  — the exe path QUOTED (Windows spaces), `--db` DROPPED (the hook reads `WICKED_ESTATE_DB` env, which it
  already falls back to). Scope/phase are pinned to the unit's real values (execute.rs:77-79) so recorded
  claims land on the SAME node the gate reads and policy `select` is consistent (findings #1/#7).
- Claude invokes it per proposed tool-call; exit 2 blocks the call AND the claim is appended to the
  ndjson.

### §3 — OUTPUT governance: already rides `apply_unit` (AS-BUILT correction)
- Recon during build showed `execute::apply_unit` ALREADY governs the unit's OUTPUT: its `select`/`decide`
  context carries `"work": output` (execute.rs:88), so an output-violating POLICY already denies through
  the existing per-unit gate — output-policy deny is NOT inert. The genuinely-inert gap is INPUT
  (per-tool-call) governance. So this milestone wires INPUT governance only; no separate `output_denial`
  seam / no `StepOutput` change is needed, and finding #16's "output deny never fails the run" concern is
  moot (output-policy deny rides `apply_unit`, whose `outcome.approved` DOES fail the run). Surfacing the
  applicable conformance RULES as obligations on the output (the recall→obligation wiring) is deferred to
  milestone #3 (core#26, where rules are populated into a run).

### §4 — Opt-in governance context (finding #14) — AS-BUILT
- `StepInput` gains a single OPTIONAL field `governance: Option<GovernanceContext>` where
  `GovernanceContext { db_path: String }` — the ONE value the worker cannot derive (the store path).
  `scope` (`resolve_scope`), `phase` (`unit-{ord}`), and the `decisions_path` are DERIVED in the launcher
  from the `StepInput`'s own fields + `decisions_path_for(run_id)` (a pure function shared with the
  actor-side fold, no threaded state to diverge). The internal StepInput literals (validator.rs:125/843,
  cli_runner.rs test sites) set `None`.
- The actor arms the context in `dispatch_unit` via `in_process_governance()` (a per-actor-thread store
  path armed at startup, mirroring `cli_runner`'s `EXEC_PUBLISHER`) — `Some(abs db_path)` for a
  file-backed store, `None` for `:memory:`/`postgres://` (cross-process-unopenable).
- `WrappedCliStepRunner::exec` gates the input-hook injection + env on `input.governance.is_some() &&
  is_claude`. `None` ⇒ exec behaves exactly as today, so the engine's own internal `claude` calls are
  ungoverned by construction.

### §5 — Delivery-mode coverage: exec-mediation (bus) path (finding #15)
- Under `WICKED_BUS_EXEC` the CLI is launched off-actor by the cli-runner from a StepInput reconstructed
  from `DispatchedTask` (cli_runner.rs:534) — which carries none of the governance context, so input
  governance would be silently OFF for a whole delivery mode. Fix: add `governance` to `DispatchedTask`
  (serialize `decisions_path` as String), populate it in `try_publish_dispatched`, and reconstruct it
  into the off-actor StepInput so `exec` governs identically.
- The evidence-conform + `hook_denial`/`output_denial` fold lives in `apply_and_finish_unit`, which the
  actor drives for BOTH modes (via `ApplyStepResult`) — so the fail path is delivery-mode-agnostic for
  free. No per-mode drain dispatch.

### §6 — P4b: pure-reader store opener (cross-repo)
- Add a read-only PRIMITIVE in **wicked-estate** (`SqliteStore::open_readonly(path)`): open with
  `OpenFlags::SQLITE_OPEN_READ_ONLY` (no `SQLITE_OPEN_CREATE`), set `busy_timeout` (read-safe), SKIP
  `execute_batch(SCHEMA)`/`migrate_schema`. Keep `SQLITE_OPEN_READ_ONLY` (verified to read the live WAL
  db race-free while the actor writes) — NOT `immutable=1` (stale snapshot). Subject to estate's
  GraphStore-conformance kit + its "single `open_store` factory" decision.
- wicked-apps-core `open_store_ro` is a thin delegate to `SqliteStore::open_readonly`, mirroring
  `open_store`. The hooks (`run_gate_hook`/`run_output_gate_hook`, which only select/decide/recall) use it.
- **Precondition (finding #9):** the single-writer actor creates + migrates `WICKED_ESTATE_DB` at run
  start, before any wrapped-CLI (hence any hook) is spawned. If the RO opener ever hits a missing
  db/schema, surface a launcher-ordering error DISTINCT from a governance DENY, never a silent fail-closed.
- **Postgres (findings #13/#18):** governance-in-run is SQLite-only for this milestone. Add a LOUD
  fail-closed guard in `run_gate_hook`/`run_output_gate_hook` that errors when `db_path` is a
  `postgres://`/`postgresql://` spec (never silently create a garbage SQLite file). Making `open_store_ro`
  spec-dispatch (read-only Postgres arm) is added to milestone #8 (core#30, Postgres runtime parity)
  BEFORE Postgres governed runs are enabled.

### §7 — Decisions ndjson integrity (finding #10)
- **Write side:** in `append_decision`, build the full `json + '\n'` line once and issue a single
  `write_all` (not the two-syscall `writeln!`), under an exclusive advisory file lock (cross-platform
  `fs4`/`fs2` `lock_exclusive`, or `libc::flock`), so parallel per-tool-call `gate-hook` subprocesses +
  the in-process output write cannot interleave even for large claims.
- **Drain/read side:** a `{`-prefixed line that fails to parse is un-evaluable governance evidence — do
  NOT `continue`-skip it (that turns a corrupted Deny into a silent allow). Fail closed: treat it as a
  hard veto / un-resolvable, matching the module's stated "un-evaluable ⇒ never silently allowed"
  invariant.

### §8 — Decisions path + env
- The pipeline mints an ABSOLUTE `decisions_path` per run in an allowed dir OUTSIDE the worktree (e.g.
  under the sandbox/work root, not `.claude/`), threaded via `GovernanceContext`. `db_path` is
  canonicalized to ABSOLUTE before threading (finding #6), with carve-outs for `:memory:` and
  `postgres://` (left verbatim).
- The launcher sets on the wrapped CLI's `Command`: `WICKED_DECISIONS_PATH=<decisions_path>` and
  `WICKED_ESTATE_DB=<db_path>` (absolute) so the hook subprocess reads the right store + log.

## Test strategy (authored before build)
- **Integration (the headline):** a STUB CLI that (a) emits a tool-call tripping a seeded deny policy →
  claim in ndjson → the fold drives `unit-{ord}` Rejected → **the SESSION reaches `Failed`** (assert
  session status, NOT merely a phase node — finding #16); and (b) emits a violating OUTPUT → `output_denial`
  → session `Failed`. Assert `--settings` is written OUTSIDE the worktree.
- **Opt-in (finding #14):** an invocation with `governance: None` (agent-judge / validator-authoring
  shape) gets NO `--settings`, NO env, NO output governance, and completes ungoverned.
- **ndjson integrity (finding #10):** concurrent appenders never corrupt a line under the lock; a
  hand-planted malformed `{`-line makes the drain fail closed (veto), not skip.
- **P4b:** `open_store_ro` refuses writes AND successfully opens a WAL-mode db while the actor holds it
  open and is actively writing (no `SQLITE_READONLY_CANTLOCK`).
- **Postgres guard:** a `postgres://` `db_path` handed to the hook errors loudly (no garbage SQLite file).
- **Unit:** settings-JSON shape; env set; `db_path` absolute (or `:memory:`/`postgres`) before it reaches
  the hook command; exe path quoted.

## Scope / cross-repo
- **wicked-core:** the fold (§1), input-hook injection (§2), in-process output governance (§3), opt-in
  context (§4), exec-mediation threading (§5), apps-core `open_store_ro` delegate + hook use (§6), ndjson
  locking + fail-closed drain (§7), path/env (§8).
- **wicked-estate:** `SqliteStore::open_readonly` primitive (§6) — a coordinated PR in the estate repo.
- **Deferred to later milestones:** Postgres RO spec-dispatch → core#30 (#8); wrapped-CLI env
  minimization (security hardening) → follow-up; non-Claude input governance → documented gap.

## Resolved open decisions (were open in v1)
1. Decisions/settings location → OUTSIDE the worktree in an allowed dir; not a sandbox concern (the
   launcher is unsandboxed) but a repo-pollution one.
2. Output-govern every unit vs gated phases → every governed unit (gated on `governance.is_some()`),
   folded in-process; cost is one recall per unit, acceptable.
3. Non-Claude input governance → documented gap (their in-process `execute.rs` path stands); not a
   blocker for #1.
4. Separate drain vs fold → **FOLD** (the review's decisive correction).

## REVISION 3 — implementation-review fixes (workflow `wf_b9c2e411-5d4`: 10 confirmed, 10 rejected)

Four defects in the first implementation, all fixed on-branch:

- **CRITICAL — shell injection / governance fail-open.** The hook command interpolated the
  caller-controlled `session_id`/`unit.id` (via `scope`/`phase`) into the shell-executed `PreToolUse`
  command with only double-quote wrapping — which does NOT escape `$`, backtick, or a literal `"`. A
  hostile id → RCE per tool-call; a merely-malformed id → the command fails to spawn → Claude treats the
  non-2 exit as non-blocking → **governance silently OFF** (no claim appended → the fold finds nothing →
  the run Completes). **Fix:** scope/phase now travel via `WICKED_GATE_SCOPE`/`WICKED_GATE_PHASE` ENV
  (like `db_path` already did); the hook command is a CONSTANT `"<exe>" gate-hook` (only the trusted
  `current_exe` interpolated). Defense-in-depth: `session_id` is rejected at launch if it carries
  shell-hostile / control chars.
- **MAJOR — stale decisions poison every retry.** The decisions log was keyed only on `run_id`, never
  cleared, and the fold matched by phase only — so an attempt-0 Deny re-folded on every retry, defeating
  the `HumanConfirmIf(VerdictNotPass)` human override, resume, and redrive. **Fix:** the log is
  attempt-scoped (`decisions_path_for(run_id, attempt)`), so a bumped-attempt retry reads a clean slate;
  and a FRESH (re-)launch clears the run's whole gov dir (resume/redrive do not).
- **MAJOR — run_id path collision.** The lossy `run_id`→dir sanitization mapped distinct ids (`a:b`,
  `a_b`, `a/b`) to one gov dir → cross-run veto contamination / evidence bleed (an attacker could aim a
  collision). **Fix:** injective `_<hex>` byte-escaping (escapes `_` too).
- **MAJOR — corrupted line wedges the run.** `fold_input_denial` returned `Err` on a corrupt claim line,
  which propagated out and left the session non-terminal (`Executing`) — re-executed on every restart.
  **Fix:** a corrupt line is a deny-dominant DENIAL routed through the normal terminal path (clean
  `Failed`); and the actor's `ApplyStepResult` error arm now drives a terminal `Failed` for ANY apply
  error (never leaves a run non-terminal).
