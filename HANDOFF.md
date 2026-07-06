# wicked-core — session handoff

## ⭐ Orchestrator build (current thrust) — see `ORCHESTRATOR.md`
COE is being grown into the **agentic-CLI orchestrator engine**: register a repo, chat with a CLI,
and run **interactive, resumable, multi-stage workflows** (the methodology spine = **recon →
adversarial-review (evaluator≠creator) → functional-test**) across one-or-many CLIs, with the egui
dashboard as the first surface. Full design (adversarially critiqued): **`ORCHESTRATOR.md`** (§1–11),
phased P0→P7.

**Progress:**
- **P0 — single-writer gate-hook reconciliation: DONE + GREEN.** The out-of-process gate-hook
  (`src/gate_hook.rs`, `wicked-core gate-hook` subcommand) no longer writes the store — it appends a
  `ConformanceClaim` to an absolute `WICKED_DECISIONS_PATH` ndjson; the actor drains it via
  `Command::ApplyHookDecisions` / `Core::apply_hook_decisions` (the sole writer; idempotent;
  Deny→veto). Phase-ownership decision locked in `src/workflow.rs`. Proof: `tests/p0_single_writer.rs`
  (external hook process runs concurrently with 200 actor reads → no `SQLITE_BUSY`; re-drain yields
  the claim once; Deny→phase `Rejected`). `Core` methods are **sync** (not async as the draft sketched
  — matches the egui/no-tokio locked decision).
- **P1 — re-entrant off-thread engine: DONE + GREEN.** `src/workflow.rs` (`StepInput`/`StepOutput`/
  injectable `StepRunner` + `StubStepRunner`); `src/actor.rs` rewritten to dispatch each unit's slow
  work to a worker thread (no store handle) that posts `ApplyStepResult` back to the actor (the sole
  writer) — actor stays responsive; `in_flight` HashSet + `RunBusy` guard prevents double-dispatch;
  `Core::launch_run`/`resume_run` + cursor fields (`unit_ix`/`attempt`) on `AgentSession`.
  `pipeline.rs` extracted `plan_and_distribute` + `apply_and_finish_unit` shared by the sync
  `run_session` and the actor (one composition surface). Proof: `tests/p1_reentrant.rs` (actor serves
  reads while a step blocks; concurrent mutating cmd → `RunBusy`; fresh Core resumes from the
  persisted cursor to completion). **DEFERRED from the design's P1** (revisit at P4a): `advance()` as
  sole phase opener / delete `tick_workflow` / strip `Phase::open` from execute (kept the existing
  model — works on the stub); explicit event `seq` (actor is already the single emit point).
- **P1.5 — hardening: DONE + GREEN.** The P0+P1 adversarial-review workflow (`REASSESS-P0-P1.md`)
  caught a **critical bug**: the actor held its own `self_tx`, so the command channel never closed →
  the actor never terminated (`drop(core)` didn't stop it; two writable actors could share one db).
  Fixed: `Command::Shutdown` + a handle-count `ShutdownGuard` (lib.rs) fires when the last `Core`
  drops; proven by `actor_shuts_down_when_last_core_drops`. Also: `apply_step_result` idempotency/
  cursor guard (`StepApplied::{Continuing,Finished,Stale}`); `finalize_run` surfaces errors; gate-hook
  doc honesty (RW-open + no `busy_timeout` = **hard P4b precondition**); off-thread claim scoped to
  EXECUTE (distribute still sync → P5); strengthened P0/P1 proofs.
- **4 open product decisions LOCKED** (ORCHESTRATOR.md §9): worktree shared-per-run; functional via
  host-CLI subprocess; non-claude = loud UNGOVERNED opt-in; default flow Recon→Build→Review→Test.
- **Revised critical path** (post-reassessment): `P1.5 → P2 → P4a → {P5 ∥ P6}`, with **P3 ∥ P2**,
  **P4b ∥ P4a**, **P7 last**. **Hard preconditions:** P4a does the `advance()`-sole-opener unification
  first; P4b makes the hook read-only/`busy_timeout`; P2 decides the run-level deny contract +
  `StepStatus`.
- **P2 (interactive gates) — DONE+green**, hardened after its review: pause-before-unit gates
  (`AwaitingHuman`), `confirm_gate(Approve{amend}|Reject)`, `cancel_run`; COE-level (not the published
  orchestration reducer). **RUN-LEVEL DENY CONTRACT decided:** a governance Deny / worker Failed →
  `SessionStatus::Failed` (never silent Completed); operator/reject/worker-cancel → `Cancelled`.
  **Retry descoped** (operator relaunch/resume). Tests `p2_gates.rs` + `p2_contract.rs`.
- **P3 (repo registry) — DONE+green**: `register_repo`/`list_repos` (git-validated); run → isolated
  worktree `<repo>/.wicked/worktrees/<run_id>` as `StepInput.workdir`; Completed keeps it, Cancelled
  discards; startup orphan reaper. `tests/p3_repo.rs`.
- **P4a (real wrapped-CLI execution) — DONE+green**: `WrappedCliStepRunner` runs the assigned CLI as a
  no-shell subprocess in the worktree (concurrent drain + timeout); `Core::spawn` uses it; operator
  CLI exposes `repos`/`register-repo`/`run`/`resume`/`cancel`. **FUNCTIONAL END-TO-END from the CLI.**
  `tests/p4a_wrapped.rs`.
- **P7 (egui dashboard on Core) — IN PROGRESS.** `wicked-agent-ui` repointed off the retired
  `wicked-agent` crate onto `wicked-core = { path }` (compiles). `App` now holds a `wicked_core::Core`;
  **launch** → `core.register_repo` (if a repo is chosen) + `core.launch_run`, **resume** →
  `core.resume_run` — the dashboard drives the engine instead of shelling a binary / polling. Still
  reads the project list from the store directly (works; live `subscribe()` wiring is a refinement).
  **Remaining P7:** gate-prompt UI (`confirm_gate` Approve/Reject/amend on `AwaitingHuman`), repo-
  registry view, `CoreEvent` subscription for the live stream, chat-through-Core; and remove the now-
  dead launch/worktree helpers in `data.rs` (`spawn_launch`/`spawn_resume`/`create_worktree`/
  `repo_scoped_problem`/`split_pieces` + their tests — 5 dead-code warnings, UI is warnings-allowed).
- **NEXT:** finish **P7** (dashboard interactivity) + **P5/P6** (council adversarial-review +
  wicked-testing functional-test stages) + **P4b** (gate-hook re-home for per-tool-call governance:
  read-only/`busy_timeout` + the `advance()`-sole-opener unification). See `ORCHESTRATOR.md` §8.

> Test counts as of P4a: `cargo test` → **37 passed**, clippy `-D warnings` clean, fmt clean.
> Re-verify: `cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --all -- --check`.

---

**State (HEAD `b6f6d44`, re-derived this session):** the COE composition runtime is built and green.
`cargo test` → **14 passed, 0 failed** (deterministic, ~0.3s); `cargo clippy --all-targets -D warnings`
→ clean; `cargo fmt --all --check` → clean; `wicked-core status --db <demo>` reads real data.

> Re-verify before trusting this: `cd wicked-core && cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --all -- --check`.
> (A "done/green" claim made earlier in the session was STALE — a test had started hanging. Always re-derive.)

## What COE is
The in-process composition runtime that replaces `wicked-agent`'s role: one thread (the actor) owns
the `SqliteStore`; everything else holds a clonable `Core` handle and talks to it via commands + a
live `CoreEvent` stream. Separates system-of-record (SQLite) from the orchestration seam (commands +
events) so consumers stop racing on the shared file. Full rationale: `DESIGN.md`.

## Done (7 commits: `f16f438` → `b6f6d44`)
- **Actor + command/event runtime** (`actor.rs`, `command.rs`, `event.rs`, `lib.rs::Core`). Single
  writer; `subscribe()` for live events. Concurrency is **capability-driven** (`shared_writers`):
  actor for SQLite, pool for Postgres — not hard-coded.
- **Domain** (`domain.rs`, `scope.rs`): `AgentSession`/`WorkUnit`/`SessionStatus`/`UnitStatus`/
  `HumanConfirm`/`EntityMode`/`resolve_scope` — ported out of wicked-agent, serde round-trip through
  estate `Node.metadata`. No `core→agent` dependency.
- **Pipeline** (`plan.rs`, `distribute.rs`, `execute.rs`, `pipeline.rs`): `plan → distribute
  (council) → execute (governance + orchestration gates) → evidence`, against the engine crates
  directly. `Core::launch` streams `CoreEvent`s; failures → `CoreEvent::Error`. **Stub execute path**
  (deterministic; real CLIs are P2b).
- **Read API**: `Core::sessions_detail()` (project list), `Core::work_output()` (transcripts).
- **Consumer / CLI**: `src/bin/wicked-core.rs` — `status` + `launch`. The agent binary's replacement.
- **Tests** (14): plan, distribute synthesis, scope, domain round-trip + store persist, actor
  read+event, and `pipeline_composes_and_streams_events_deterministically` (full composition +
  event sequence via a STUB dispatcher — see gotcha below).

## Next — in order
1. **P3 — migrate `wicked-agent-ui` onto `Core` (task #31, highest value).** This makes COE *used*
   and fixes the original "UI feels broken." Steps:
   - Add `wicked-core = { path = "../wicked-core" }` to `wicked-agent-ui/Cargo.toml`.
   - In `data.rs`: replace `load_projects` (store-direct) with `core.sessions_detail()` → map
     `SessionView` → the UI's `Project` (the `SessionStatus→ProjState` / `UnitStatus→UnitState`
     mapping stays in the UI). Replace `spawn_launch` (shells the binary) with
     `core.launch(LaunchSpec { clis: wicked_core::registry_roster(), .. })`. Replace
     `unit_transcript`/transcript reads with `core.work_output(unit_id)`.
   - `App` holds a `wicked_core::Core` (spawned on the store path) + a `Receiver<CoreEvent>` from
     `core.subscribe()`; the terminal "agent" tab renders the live stream instead of polling.
   - Drop the kill/`runs` pid-registry for now (kill returns with P2b subprocesses).
   - Keep the UI green (`cargo build/clippy/fmt`).
2. **P2b — wrapped-CLI execute backend (task #30).** Port `execute_unit_wrapped` + `inject.rs`
   (PreToolUse `gate-hook` generation, sandbox, MCP toolbox injection) from `wicked-agent` into COE.
   The `gate-hook` becomes a `wicked-core gate-hook` subcommand (claude invokes it as a subprocess).
   Until then `launch` uses the stub execute (composition identical; only the unit's *work* differs).
   Reference: `wicked-agent/src/execute.rs` (`execute_unit_wrapped`, ~line 255+) and
   `wicked-agent/src/inject.rs`.
3. **P4 — delete the `wicked-agent` crate (task #32)**, once P3 + P2b land. `wicked-core` (lib + bin)
   is the replacement.

## Locked decisions (don't relitigate)
- **Kill the agent.** COE owns composition; wicked-agent is retired to nothing (its CLI + gate-hook
  live in the `wicked-core` bin).
- **No `core→agent` dependency.** The pipeline was ported, not wrapped.
- **Concurrency = capability-driven**, not a hard-coded actor (estate ADR-003 §3/§5: `shared_writers`).
  The single-writer actor is the SQLite arm; Postgres → pool + MVCC + (bonus) LISTEN/NOTIFY for
  cross-process events, which makes the daemon variant unnecessary.
- **Council dispatcher is injectable** (`Arc<dyn Dispatcher>`) so composition is tested without a
  flaky subprocess.

## Gotchas / context
- **Why the stub dispatcher:** `wicked-council`'s `RealDispatcher` spawns CLI subprocesses to collect
  votes; under load it hung the launch test indefinitely (passed at 0.9s early, later hung past 90s).
  That's a council/dispatch concern, NOT COE composition. The injectable dispatcher (`distribute::
  real_dispatcher()` in prod, a `Stub` in the test) makes the composition test deterministic. If you
  test `Core::launch` end-to-end with real CLIs, expect subprocess timing flakiness.
- **The actor blocks during `launch`** (the pipeline runs on the actor thread — the single writer).
  Fine for the stub path (fast). P2b's real CLIs are slow → may want a worker that sends writes back
  to the actor, or accept the block. Documented in `DESIGN.md`.
- **The UI today** still polls the file + shells `wicked-agent` (pre-P3). The user's real store is
  `wicked-agent-ui/wicked-estate.db` (via `WICKED_ESTATE_DB`). The demo store is
  `…/scratchpad/ui-demo/demo.db` — **never run `setup_demo.py` against a store with the user's real
  work; it wipes + reseeds with fakes** (it cost real data this session).
- **Postgres is designed-not-built** in estate (ADR-003). The actor is correct for today's SQLite; do
  not build the pool arm speculatively.

## Pointers
- `DESIGN.md` (this crate) — the why + phasing.
- Brain memories (wicked-agent-ui project brain): `egui-interactive-pty-terminal`,
  `repo-targeting-via-worktree`, `never-reset-active-user-store`,
  `ui-kill-run-session-pid-registry`, `wicked-council-registry-merge-user-toml`.
- P2b reference: `wicked-agent/src/{execute.rs,inject.rs}` + `docs/adr/0003-*`.
- Tasks #30 (P2b), #31 (P3), #32 (P4).
