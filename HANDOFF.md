# wicked-core — session handoff

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
