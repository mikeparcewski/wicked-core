# Purpose

One-page orientation for the ACP elicitation Rust implementation — context, goals, invariants, risks, observability, and exit criteria.

---

## TL;DR

Add `elicitation/create` handling to wicked-core's ACP turn loop for **native/ecosystem adapters only** (`claude-agent-acp`, `codex-acp`).

When an MCP server inside a native ACP adapter calls `elicitation/create`, wicked-core:

1. Mints an `elicitationId`, stores an `mpsc::Sender<ElicitationResult>` in `ElicitationMaps` under a **single atomic lock** (prevents registration/cancel races).
2. Emits `CoreEvent::ElicitationCreated` to the crew daemon (via NAPI).
3. Suspends the pending ACP request using a **dual-poll loop** — `try_recv` on the resolution channel + `recv_timeout(50 ms)` on `line_rx` — keeping stdout drained and `session/update` chunks flowing.
4. On `resolveElicitation` (NAPI call from crew): responds directly to the `elicitation/create` JSON-RPC request via `rpc_respond`, echoing the request id verbatim as a `Value`.
5. On terminal — three distinct cases: `Ok{action:"cancel"}` (human dismiss) → `StepStatus::Cancelled`; `Err(Disconnected)` or deadline expiry → `StepStatus::ElicitationFailed` (bypasses triage, preventing silent redispatch); `Ok{action:"decline"}` → re-enter outer loop.
6. On `on_run_complete`: calls `cancel_epoch` before spawning the background teardown thread, releasing any suspended worker.

agy-acp is **explicitly out of scope** — see Appendix A in `DES-002-tests.md`.

---

## Context

`DES-002-acp-session-elicitation` describes the full feature; the TypeScript half (crew `ElicitationCache`, REST routes, Studio `ElicitationPrompt`) is in wicked-crew. This document set covers only the Rust half.

### What is NOT changing

- The agy-acp bridge (`bridge.mjs`) — agy-acp is out of scope.
- The crew daemon's `ElicitationCache`, REST routes, or Studio components.
- The existing ACP turn loop for non-elicitation methods.
- The `StepRunner` trait — not widened; actor access uses a shared `Arc` clone.

### Why agy-acp is out of scope

Antigravity is spawned with `stdio: ['ignore', 'pipe', 'ignore']` — its stdin is closed. MCP servers running inside Antigravity have no transport to send `elicitation/create` back through the bridge's stdio channel. See `DES-002-tests.md` Appendix A.

### Why a dual-poll loop (not `select!`)

`exec_turn_acp` uses `std::sync::mpsc` — no async runtime. `std::sync::mpsc` has no `select!`. The turn loop blocks on `line_rx.recv_timeout(remaining)`. When the native adapter goes quiet waiting for human input, no new lines arrive.

The **dual-poll loop** resolves this:
- `rx_res.try_recv()` — non-blocking check of the resolution channel.
- `line_rx.recv_timeout(ELICITATION_POLL_MS=50)` — yield up to 50 ms.

This keeps the stdout pipe drained (prevents the adapter's reader-thread full-pipe deadlock), processes `session/update` chunks while the human types, and checks for resolution at ≤50 ms intervals.

### Elicitation deadline: turn's residual budget is the human's window

`exec_turn_acp` computes `deadline = Instant::now() + timeout` at turn start (default `WICKED_UNIT_TIMEOUT_SECS` = 7200 s). The elicitation arm reuses this deadline — if the human does not respond before the remaining turn budget expires, the dual-poll loop cancels the elicitation.

**Floor case**: an elicitation raised late in a long turn may give the human only seconds before `elicitation_timed_out` fires → `StepStatus::ElicitationFailed`. Acceptable for v1; a future `MIN_ELICITATION_SECS` clamp is a named extension point.

A dedicated per-elicitation timeout is a Non-goal; the shared deadline is the human's window.

### Registration / cancel atomicity

Both `pending_elicitations` and the `run_id → [(elicitation_id, epoch)]` reverse index live inside a single `ElicitationMaps` struct behind one `Mutex`. All inserts and removes acquire the mutex once. This ensures teardown (`cancel_epoch`) can never observe a partially-registered elicitation — if it finds an `elicitation_id` in the reverse index, the sender is guaranteed to exist in the primary map, and vice versa.

### Concurrent inbound requests during suspension

During `elicitation/create` suspension, the `'elicit` loop handles `session/update` notifications, a second `elicitation/create` (cancels it immediately — first-held/first-wins semantics), and the `session/prompt` response. All other frames (`fs/*` requests etc.) hit `continue 'elicit` and are dropped with `tracing::warn`.

**OQ-R-6 (OPEN — verify before PR):** Do the initial native adapters (`claude-agent-acp`, `codex-acp`) serialize tool execution such that at most one MCP tool is blocked waiting at any point? If yes, concurrent `fs/*` during suspend is structurally impossible. If no, the shared frame handler must be implemented before enabling elicitation for that adapter. The `tracing::warn` on dropped frames provides field observability until verified.

If OQ-R-6 resolves to "no serialization," then `fs/*` dropped during suspend is a live 7200s-hang bug that requires the shared frame handler. Both cases must resolve together.

### Adapter-supplied message/options: trust boundary

MCP server-supplied `message` and `options` are relayed verbatim to the Studio UI, crossing the adapter→human trust boundary. For v1:
- **Message**: capped at 8 KB at intake; excess truncated with `[truncated]`.
- **Options**: capped at 100 entries; empty-string options dropped; options exceeding 512 bytes are **dropped** (with `tracing::warn`), not truncated — option strings are semantic values echoed verbatim on accept; a truncated value would not match the MCP server's enum and the tool call would fail.
- **No Rust-side sanitization**: the message text is forwarded as an opaque string. Studio must treat the string as untrusted display text — escape before render; do not evaluate markdown/HTML from MCP server-supplied content. This obligation is tracked in DES-002-acp-session-elicitation §Security and in EC-5 below.

### Daemon restart: accepted gap

`ElicitationMaps` is in-process state. On restart, the ACP adapter (child process) dies with wicked-core; the crew `ElicitationCache` entry persists until `reconcile()` prunes it. Consistent with DES-002 crew §Risks "Daemon restart while elicitation pending."

### Operator injects during suspension

`pending_injects` queued via `InjectWorkerMessage` while the human is answering are drained at the start of `AcpStepRunner::exec_turn`, BEFORE `exec_turn_acp` is called. Injects queued during elicitation are NOT delivered at the next `recv_timeout` iteration after elicitation resolves — the drain already ran before the turn started. In practice, injects received during elicitation are held until the NEXT unit's `exec_turn` call. If the current unit terminates (e.g., user cancels via elicitation dismiss), queued injects may be lost if the run does not proceed to another unit. Accepted gap for v1.

---

## Goals

- **G-1.** `elicitation/create` from native adapters reaches crew as `elicitationCreated` CoreEvents.
- **G-2.** `resolveElicitation` (NAPI) delivers the human response without blocking the stdout pipe.
- **G-3.** `on_run_complete` (fired by `cancel_run`/`fail_run`, and also by actor `Shutdown`) releases all pending elicitations for the run so no worker thread hangs past the turn deadline. The actor's `Command::Shutdown` arm must call `cancel_run`/`fail_run` (or the equivalent tombstone+drain sequence) for every active run before exiting — otherwise ACP workers suspended in `exec_turn_acp` retain their `Arc<AcpStepRunner>` and pending senders, blocking until the 7,200-second residual budget expires.
- **G-4.** `ElicitationMaps` is mutually consistent at all times: an `elicitation_id` appears in `pending` and `run_index` atomically.

## Non-goals

- agy-acp elicitation — structurally unreachable; deferred to Appendix A in DES-002-tests.md.
- Multi-property `requestedSchema` elicitations — immediately cancelled (case d in schema parser); the tool call receives `action:"cancel"` and can handle it gracefully. Multi-field forms are a v2 extension point.
- Array-typed and object-typed single-property schemas — also immediately cancelled (case b); a free-text string response would not be schema-conformant.
- Dedicated per-elicitation timeout — human's window is the turn's residual budget (documented above).
- End-to-end automated test — requires both Rust and TS halves deployed.
- `session/new` clearing CoreEvent — agy-acp concern.
- Integer, number, and boolean property types in v1 — cancelled before registration; crew submits free-text strings and cannot produce schema-valid integer/boolean JSON. Lift in concert with crew/Studio changes.

---

## Key invariants

| # | Invariant |
|---|-----------|
| I-1 | `elicitationId` in `ElicitationCreated` is wicked-core-minted — distinct from the inbound `elicitation/create` JSON-RPC `id`. The guard checks `id != null`, not `as_u64()`, so string IDs are handled correctly. |
| I-2 | `exec_turn_acp` removes the elicitation from `ElicitationMaps` before returning. `register`, `remove`, `cancel_epoch`, and `deliver` are individually atomic (each acquires the single mutex); they are NOT a single transaction — the invariant is per-operation atomicity, not distributed atomicity across calls. |
| I-3 | Between `deliver` (removes from `pending`) and `remove` (prunes `run_index`), `run_index` may contain a stale entry. This window is benign: a concurrent `cancel_epoch` calling `pending.remove` on an absent key is a no-op; no path reads `run_index` to assert liveness of `pending`. |
| I-4 | The Rust teardown obligation covers abnormal terminals only (`cancel_run`→`on_run_complete`, `fail_run`→`on_run_complete`). Normal turn completion cannot have a pending elicitation — the outer loop does not advance past the elicitation arm until it resolves or cancels. `cancel_run` teardown arrives at the worker as `Disconnected` → `elicitation_timed_out=true` → `StepStatus::ElicitationFailed` at the **unit level**; the run-level status is `Cancelled` (set by the actor before emitting `RunCancelled`), so the unit-level Failed is overridden and does not surface to the user. |
| I-5 | `options: Some(v)` is never emitted with `v.is_empty()`. A selection constraint (enum/oneOf) present but with all non-representable choices (numeric, over-cap, empty string) cancels before registration — no `ElicitationCreated` emitted, no entry in `ElicitationMaps`. `options: None` means no selection constraint, not an unrepresentable one. |
| I-6 | `prop_key` in the response `content` matches the property name from `requestedSchema.properties` — not hardcoded to `"response"`. |
| I-7 | Three distinct cancel signals from the `'elicit` loop — `Ok{action:"cancel"}` (human explicitly dismissed the dialog) → `cancelled=true` → `StepStatus::Cancelled`; `Err(Disconnected)` (teardown: cancel_epoch drops sender) or deadline expiry → `elicitation_timed_out=true` → `StepStatus::ElicitationFailed` (no fallback); `action:"decline"` → re-enters outer loop to await `session/prompt`. **`elicitation_timed_out` and `cancelled` must not be collapsed.** `StepStatus::ElicitationFailed` is required (not `::Failed`) because `Failed` enters the unrecognized-failure triage path which can produce `Retry`, silently redispatching the unit after a deadline or adapter disconnect. |
| I-8 | `session/prompt` usage captured during elicitation replaces (not sums) any prior `handle_update`-derived token counts — matching the existing outer-loop merge. |
| I-9 | `exec_turn_acp` must never execute on the actor thread. The dual-poll suspend blocks the calling thread; if this ran on the actor thread, `Command::ResolveElicitation` could never be processed (deadlock). `exec_turn_acp` is called inside `std::thread::spawn` in actor.rs; the actor thread is always free to service `ResolveElicitation` while a worker thread is suspended. |
| I-10 | `proc.stdin` is written only by the turn loop thread. `InjectWorkerMessage` delivers its payload via `pending_injects` read by the same loop thread; no other thread ever writes `proc.stdin` directly. Interleaved writes corrupt JSON-RPC framing. |
| I-11 | Post-deadline late-answer drain runs unconditionally (all terminal exits). When `elicitation_timed_out=true` and a human answer arrives, the answer is forwarded to the adapter (so the MCP server is not left with a dangling request), but `elicitation_timed_out` stays `true` — the unit still reports Failed/ElicitationFailed. This is I-7/F7 fix: the lock-protected drain closes the deliver-race window for all terminal paths. |

---

## Risks and mitigations

| Risk | Severity | Mitigation |
|------|----------|------------|
| Turn deadline expires while human is typing | MED | Accepted: crew-side route returns 409 after cancel is posted; human window = turn's residual budget; `MIN_ELICITATION_SECS` clamp is a named v2 extension point |
| Adapter sends `elicitation/create` with string id (e.g. UUID) | MED | `rpc_respond` echoes `&Value` verbatim; guard checks `id != null`, not `as_u64()` |
| `elicitation` capability not declared; adapter silently never sends requests | MED | `"elicitation": {"form": {}}` added to `clientCapabilities` (→ DES-002-actor-teardown.md §initialize); OQ-R-4 resolved |
| `StepStatus::ElicitationFailed` triage bypass — `Failed` enters triage, can produce `Retry`, silently redispatching after deadline/disconnect | HIGH | Dedicated `ElicitationFailed` variant routes directly to run-terminal path, bypassing triage entirely. All exhaustive match sites must be updated — see DES-002-tests.md §exhaustive match sites. |
| Second concurrent elicitation during suspension | LOW | Immediately answered with `action:"cancel"` inside `'elicit`; first elicitation continues |
| `on_run_complete` fires after `exec_turn_acp` already removed elicitation | LOW | `cancel_epoch` on already-empty `run_index` is a no-op (safe). `cleanup_run` is exclusively the RAII guard's responsibility; `on_run_complete` only calls `cancel_epoch`. Double-calling `cleanup_run` would decrement `active_workers` twice — never call it from `on_run_complete`. |
| Adapter sends `session/prompt` result mid-elicitation | LOW | Inner loop consumes it, captures usage, sets `found`/`timed_out`, breaks `'elicit`; outer loop terminates |
| Native adapter doesn't implement `elicitation/create` | LOW | Method never appears in `line_rx`; feature is a no-op |
| Adapter child dies mid-elicitation suspend | LOW | `line_rx` Disconnected → `elicitation_timed_out=true` → `StepStatus::ElicitationFailed` (no fallback); `tracing::warn` emitted; distinct from human-dismiss (I-7) |
| `resolveElicitation` lands between deadline break and `maps.remove()` | LOW | Closed by unconditional drain (I-11): drain always runs before `maps.remove()`; if an answer arrived, it is forwarded to the adapter |
| Concurrent inbound `fs/*` requests during suspension | MED | Pending OQ-R-6 verification; dropped with `tracing::warn`; enable per-adapter only after EC-3 passes |
| CancelRun between `maps.remove()` and the adapter write | MED | Pre-write tombstone check, Phase 1/Phase 2 re-checks, post-write tombstone check — three gates close this window. → DES-002-exec-turn-acp.md §Phase 1/2 write |
| Bus task with `launch_seq=0` or mismatched `process_gen` executing against a cancelled run | HIGH | `try_next_epoch_bus` unconditionally rejects `launch_seq==0`; process-generation token check discards cross-restart tasks. → DES-002-elicitation-maps.md §try_next_epoch_bus |

---

## Observability

All tracing events are keyed by `elicitation_id`. Lock-poison recovery uses `unwrap_or_else(|p| p.into_inner())` uniformly throughout — documented once here, applied without repetition.

**Subscriber wiring required**: adding the `tracing` crate alone does NOT make events observable — the default dispatcher silently discards them. A subscriber must be initialized (e.g. `tracing_subscriber::fmt::init()`) before any event will appear. Options: `[dev-dependencies]` for tests; or replace elicitation `tracing::warn!` calls with the existing event/logging mechanism already used in this codebase.

| Event | Level | Fields |
|-------|-------|--------|
| `elicitation.created` | INFO | `run_id`, `elicitation_id`, `option_count` |
| `elicitation.resolved` | INFO | `run_id`, `elicitation_id`, `action`, `reason` |
| `elicitation.cancelled` (teardown) | INFO | `run_id`, `elicitation_id`, `reason="teardown"` |
| `elicitation.cancelled` (superseded) | INFO | `run_id`, `reason="superseded"`, `second_req_id` |
| `elicitation.timed_out` | WARN | `run_id`, `elicitation_id`, `overrun_ms` (time past deadline ≤50 ms; not total turn elapsed) |
| `elicitation.deliver_failed` | WARN | `elicitation_id`, `error` |
| Adapter stdout disconnect | WARN | `run_id`, `elicitation_id`, "adapter stdout disconnected mid-suspend" |

`ElicitationResolved` reason mapping:
- `"human"` — `deliver(action)` resolved it (human accept/decline/cancel via `resolveElicitation`)
- `"session_prompt"` — session/prompt arrived mid-elicitation
- `"timeout"` — elicitation deadline expired (`elicitation_timed_out=true`, `elicitation_teardown=false`)
- `"teardown"` — run cancelled or adapter died: channel Disconnected / post-deliver cancellation race / pre-registration cancel (`elicitation_teardown=true`)
- `"adapter_write_failure"` — adapter pipe write failed after a human accept/decline; adapter may not have received the response; unit falls back with `StepStatus::ElicitationFailed`

Emitted **after** the `rpc_respond` adapter write attempt so the durable log records only actions the adapter actually received.

**Crew-side `ElicitationCache` MUST consume `elicitationResolved`** — a v1 blocking requirement. When `session/prompt` resolves an elicitation before a non-final unit completes, Rust removes the `ElicitationMaps` entry while Studio retains a stale prompt. Crew listeners must use wire field names `session` and `elicitationId` (not `run_id` / `elicitation_id`).

---

## Exit criteria (pre-merge checklist)

| # | Item |
|---|------|
| EC-1 | OQ-R-4 resolved: `clientCapabilities.elicitation.form` shape confirmed — RESOLVED; `{"form":{}}` confirmed; P-6 already correct |
| EC-2 | OQ-R-5 resolved: `params.message` and `params.requestedSchema.properties` paths confirmed against SDK v1.3.0 `types.gen.ts`; extraction updated if paths differ |
| EC-3 | OQ-R-6 resolved: `claude-agent-acp` / `codex-acp` adapter tool-execution serialization confirmed or shared frame handler implemented before enabling those adapters |
| EC-4 | All tests in the test plan pass — `ElicitationMaps` unit tests (1–11, 10a), arm-level turn tests (12–20), `rpc_expect` frame-routing tests (21–24), and gate tests (25–36) |
| EC-5 | DES-002 TS-side confirmed: Studio `ElicitationPrompt` escapes `message` before render (tracks injection obligation delegated from §Adapter-supplied message/options) |
| EC-6 | OQ-R-7 resolved or `chat_turn` elicitation guard kept in place (`elicitation_enabled=false` for `chat_turn` until the routing contract is verified end-to-end) |

---

## File index

| File | Content |
|------|---------|
| `DES-002-overview.md` | TL;DR, context, design constraints, goals/non-goals, key invariants, risks, observability, exit criteria (this file) |
| `DES-002-elicitation-maps.md` | `CoreEvent::ElicitationCreated` and `ElicitationResolved`, `Command::ResolveElicitation`, `ElicitationMaps` struct and all methods, `EpochCleanup` RAII guard, session generation / `drop_session_gen`, `TurnResult` and `default_at` constructor, NAPI binding |
| `DES-002-exec-turn-acp.md` | `rpc_respond` helper, `exec_turn_acp` signature and params, `elicitation_enabled` / `form_enabled` relationship, five new loop variables, outer-loop bounded poll (500 ms when run_epoch > 0), full elicitation/create arm (disabled → mode → schema → register → dual-poll → drain → Phase 1/2 write → resolve), `exec_turn` match block arm order |
| `DES-002-actor-teardown.md` | `actor::run` signature and three call sites, `dispatch_unit` epoch allocation (bus vs local), `Command::EmitEvent` epoch-tombstone suppression, `Command::ResolveElicitation` handler, `shared_run_terminal` ordered teardown, `CancelRun`, `ReassignUnit`, `Command::Shutdown` drain with pre/post-spawn checks, write-lock registry, `initialize` capability, handshake-phase elicitation guard in `rpc_expect`, `StepStatus::ElicitationFailed` bus serialization |
| `DES-002-tests.md` | File map table (src file → changes), `StepStatus::ElicitationFailed` exhaustive match sites, test plan matrix (tests 1–36), design decisions and guards, Appendix A (agy-acp deferred items) |
