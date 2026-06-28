# wicked-core — in-process composition runtime for the core services

## Why

Today the core services (estate store + governance + orchestration + council + memory/knowledge)
integrate through **one shared SQLite file**, which is asked to do three different jobs:

1. **System of record** (durable graph) — SQLite is good at this. Keep it.
2. **Integration bus** between engines — engines read each other's JSON metadata fields off nodes
   (`phase.gate_decision`, `ConformanceClaim` shape, `EVAL_AT_BASE` magic constant). The contract is
   "everyone agrees on the bag," enforced nowhere. The apps-core event catalog declares ~18 events;
   the agent emits 5. There is no real event backbone.
3. **Multi-process IPC** — the bare `SqliteStore` sets WAL but **no `busy_timeout`**
   (`wicked-estate-store/.../sqlite.rs`), so concurrent writers get an immediate `SQLITE_BUSY`.
   wicked-knowledge already carved out its own `.db` (DEC-1) to escape multi-writer FTS corruption,
   and wicked-overlay/xedge exists to re-join the fragmented stores. Fragment-then-overlay is the
   smell.

Consumers re-assemble the wiring independently: the UI **polls the file directly AND shells the
`wicked-agent` binary** to write; the agent re-opens the store per subcommand and hand-wires the
pipeline twice (`run_session` vs `run_session_wrapped`). The "capstone" (wicked-agent) is really
just one hand-wired consumer.

## What

`wicked-core` separates **system-of-record** (SQLite, single writer) from the **orchestration seam**
(a command API + a live event stream). One thread owns the store; everything else talks to a handle.

```
            ┌──────────────── Core (clonable handle) ────────────────┐
 agent ───▶ │  tx: Sender<Command>                                    │
 UI ──────▶ │                                                         │
 mcp ─────▶ └───────────────────────┬─────────────────────────────────┘
                                     │ Command (+ oneshot reply)
                          ┌──────────▼───────────┐
                          │   StoreActor (1 thread)│  ← the ONLY writer
                          │   owns SqliteStore     │
                          │   composes engines     │  governance + orchestration + council
                          │   emits CoreEvent ──────┼─▶ subscribers (live, no polling)
                          └────────────────────────┘
```

### Guarantees (backend-independent)
- **Live events, not polling.** Consumers `subscribe()` to a `CoreEvent` stream. The UI stops
  scraping the file on a timer and stops shelling a binary; it calls `launch()` and watches events.
- **One composition surface.** The plan → distribute → execute → evidence pipeline lives here ONCE
  (deduping the agent's `run_session`/`run_session_wrapped`), with events emitted at each step.

### Concurrency is capability-driven, NOT a hard-coded actor
COE programs against the `GraphStore` trait and chooses its write strategy from
`store.capabilities().shared_writers` (estate ADR-003 §3/§5) — so the "clean" layer never hard-codes
a SQLite assumption:

| `shared_writers` | backend | COE write strategy | events delivery |
|---|---|---|---|
| `false` | SQLite (local file, `!Sync`) | **single-writer actor** — one owning thread serializes writes (= ADR-003's `Arc<Mutex<_>>`); no in-process `SQLITE_BUSY`, no read-vs-write races | in-process broadcast (single process); `changes_since` poll for other processes |
| `true` | Postgres / server (pooled, `Sync`) | **connection pool** — hand each reader/writer its own connection; Postgres MVCC does concurrency; no serialization | Postgres `LISTEN/NOTIFY` → real cross-process events |

The `Core` command + event **API is identical** across backends; only the internal driver changes.
The actor is the `shared_writers=false` arm, not the architecture. Sync traits today; ADR-003 §4's
async flip is the escape hatch if a high-concurrency server ever needs it (mechanical, contained).

With Postgres, the **daemon variant (option B) is largely unnecessary** — Postgres is itself the
shared, concurrent, notify-capable server, so COE-in-process + Postgres beats building a daemon.

### Out-of-process writers (the gate-hook)
The wrapped-CLI **gate-hook** (claude's PreToolUse invokes a subprocess to run the governance gate)
writes the store directly, outside COE's process. On SQLite that's serialized via `busy_timeout` +
the WAL writer lock (brief writes retry instead of failing); on Postgres it's just another pooled
writer (no issue). Killing `wicked-agent` (below) does NOT remove this subprocess — it relocates to
a thin bin over COE.

## API (target)
- reads: `sessions() -> Vec<ProjectView>`, `session(id) -> SessionDetail` (units + outputs)
- lifecycle: `launch(spec) -> SessionId`, `resume(id)`, `kill(id)`
- governance/roster: `register_policy(..)`, `list_clis()`, `add_cli(..)`
- events: `subscribe() -> Receiver<CoreEvent>`

## Phasing — COE absorbs the harness; wicked-agent is retired
Decision (2026-06-28): **kill wicked-agent.** COE becomes the composition owner; the agent crate's
orchestration logic moves here, and what's left of "agent" is a thin subprocess bin over COE.

- **P1 (done):** StoreActor + command/reply + event fan-out + a read path (`sessions`), proven by a
  test. Builds 0 warnings, 1 test green. ← skeleton.
- **P2 (DONE):** ported `plan → distribute (council) → execute (governance + orchestration) →
  evidence` into COE, depending on the engine crates **directly** (no wicked-agent dep). Domain +
  scope + node read/write live here; `Core::launch` streams real `CoreEvent`s; `Core::sessions_detail`
  + `Core::work_output` are the read API; the `wicked-core` CLI is the first real consumer. 14 tests
  green (incl. a fake-CLI launch asserting the event sequence). Stub execute path.
- **P2b:** the wrapped-CLI execute backend — real subprocess + the per-tool-call `gate-hook`
  (ADR-0003); the `gate-hook` becomes a subcommand of the `wicked-core` bin. Until then `launch` uses
  the deterministic stub execute (the composition is identical; only the unit's WORK differs).
- **P3:** migrate `wicked-agent-ui` onto `Core` — drop direct store polling + binary shelling; the
  terminal "agent" tab renders the live event stream.
- **P4:** delete the `wicked-agent` crate. What survives is a **thin bin over COE** providing (a) the
  `gate-hook` subprocess (wrapped-CLI governance, ADR-0003 — physically must be a subprocess claude
  can invoke) and (b) the operator CLI (`run`/`status`/`resume`), both as thin wrappers over the COE
  library. Could keep the `wicked-agent` name as a gutted shim or rename to `wicked-core` bin.

## Decisions resolved
1. **Domain ownership → COE.** The session/unit/phase domain + readers move into COE (ported from
   wicked-agent, which is then deleted). No `core → agent` dependency; the layering points the right
   way (agent was a consumer, now it's gone).
2. **Async vs sync → sync now.** Matches the UI (egui, no tokio) + the engines. ADR-003 §4's
   async-trait flip is the documented, contained escape hatch if a hosted Postgres server demands it.
3. **Gate-hook → thin bin over COE.** Governance gate logic lives in the COE library; the subprocess
   entrypoint is the surviving thin bin. SQLite: `busy_timeout`. Postgres: just a pooled writer.
