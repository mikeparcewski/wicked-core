# Plan: DES-002 ACP Elicitation Maps

- **Spec:** [`spec.md`](spec.md)
- **Status:** Done

> **Plan contract:** this is the implementation strategy. Unlike the spec, this
> document is allowed to change as you learn. When it changes substantially,
> note why in the changelog at the bottom.

## Approach

Implement in strict dependency order (nine source files; the fourth `StepStatus`
variant causes compile errors at every exhaustive match site until all are
updated together). The natural seam is: **wire types first** (event, command,
workflow variants) → **bus DB layer** (cursor metadata APIs) → **ElicitationMaps
data structure** (unit-testable in isolation) → **exec_turn_acp elicitation arm**
(arm-level tests with `rpc_expect<W:Write>`) → **actor.rs plumbing** (handlers,
dispatch, shutdown) → **cli_runner.rs** (serde defaults, bus consumer) →
**lib.rs + NAPI** (wiring). Tests are written alongside each layer (TDD for
data-structure tasks; arm-level stubs before exec_turn_acp). The riskiest part
is the bus consumer's ack-gated cursor advance and predecessor terminalization —
these are not testable in isolation without the bus DB layer, so bus.rs is
unblocked early.

## Constraints

- Change sequencing is fixed (DES-002-tests.md §Change sequencing): each step
  must compile before the next starts, because `StepStatus::ElicitationFailed`
  propagates exhaustive-match errors across all importers. Exact order:
  Cargo.toml → event.rs + command.rs + workflow.rs → bus.rs → acp_runner.rs →
  actor.rs → cli_runner.rs → lib.rs → crates/wicked-core-ts.
- No Tokio dependency in root crate; use `std::sync::mpsc::sync_channel` for all
  reply/ack channels (rendezvous, buffered=0 for ack, buffered=1 for reply).
- `exec_turn_acp` must never run on the actor thread (spec I-9).
- `cleanup_run` called exclusively by `EpochCleanup::drop` — no other call site.
- `bus_in_flight_workers` canonical type: `HashSet<(String, u64)>` — keys are
  `(run_id, launch_seq)` pairs; `HashMap<String, u64>` is incorrect (see Design
  Decisions for rationale).
- Ack reply sent by actor only AFTER `ApplyStepResult` is committed to the store
  (spec Never do: "Send the ack reply before the actor commits").
- Startup cursor reclamation ordering: migrate positions → delete old rows →
  set_stable (spec **Always do** + startup AC).

## Construction tests

**Cross-cutting (span multiple tasks):**

- Integration: all nine source files compile together with no `StepStatus` match
  errors (`cargo check --workspace`). Gate T8 before T9.
- `grep 'StepStatus::' src/**/*.rs` — confirm no wildcard arm silently swallows
  `ElicitationFailed` after T1 lands.

**Manual verification (EC-3, EC-6):**
- Confirm `claude-agent-acp` and `codex-acp` serialize tool execution (OQ-R-6)
  — either by reading adapter source or running a probe call. Required before
  removing the `ELICITATION_VERIFIED_ADAPTERS` guard.

## Design (LLD)

### Design decisions

- **Single `ElicitationMaps` mutex** (not per-run map + global map): eliminates
  register/cancel races at the cost of one contended lock per elicitation
  lifecycle event (registration, deliver, cancel_epoch). Elicitation events are
  low-frequency; lock hold time is O(1) HashMap ops. Traces to: AC G-4.
- **`HashSet<(String, u64)>` for `bus_in_flight_workers`** (not `HashMap<String,
  u64>`): two workers can be alive for the same `run_id` during reassignment;
  `HashMap` would overwrite the old entry, losing in-flight tracking. Traces to:
  AC (bus_in_flight race fix).
- **`sync_channel(0)` for ack** (rendezvous, not buffered): ensures cursor advance
  happens only AFTER the actor dequeues `ApplyStepResult`. Send blocks until
  dequeue; `recv().is_ok()` confirms the channel was not closed mid-way. No
  external dep. Traces to: AC (ack-gated cursor advance).
- **`bus_in_flight_deferred` flag on `EpochCleanup`**: the RAII guard fires when
  `exec_turn` returns (which is BEFORE `confirm_task_completed`). Setting this
  flag defers clearing the in-flight marker to after cursor/confirm; panic/cancel
  paths clear it immediately. Traces to: AC G-3.
- **`predecessor_gen` derived from `old_consumer` before `set_stable`**: ordering
  was a live bug (gate-83); fix is to read `get_stable` into `old_consumer_opt`
  FIRST, then migrate cursors, then call `set_stable`. Traces to: AC (startup
  reclamation ordering).
- **`find_completed` scans, not peeks**: interleaved completions from other runs
  sit before the target event in the completed stream; a peek only checks the
  next event and returns `None`, incorrectly terminalizing a task that succeeded.
  Scan by `(run_id, launch_seq)` without consuming non-matching events. Traces
  to: AC (predecessor terminalization).

### Data & schema

**New ElicitationMaps fields (src/acp_runner.rs):**
```rust
struct ElicitationMaps {
    pending:              HashMap<String, ElicitationEntry>,
    run_index:            HashMap<String, Vec<(String, u64)>>,
    active_workers:       u32,
    cancelled_epochs:     Vec<(String, u64)>,
    suppressed_creations: HashSet<String>,
    bus_dispatched_runs:  HashSet<String>,
    bus_in_flight_workers: HashSet<(String, u64)>,
    bus_activated_seqs:   HashMap<String, u64>,
    run_launch_seq:        HashMap<String, u64>,
    shutdown_flag:         bool,
}
```

**New bus.rs APIs:**
- `BusDb::delete_cursor(name: &str) -> Result<()>`
- `BusDb::get_stable(key: &str) -> Result<Option<String>>`
- `BusDb::set_stable(key: &str, value: &str) -> Result<()>`
- `BusDb::find_completed(consumer: &str, run_id: &str, launch_seq: u64) -> Result<Option<CompletedTask>>`

Requires `cursor_owners` (or `meta`) KV table in SQLite schema; add migration.

**New serde-defaulted fields on wire types (cli_runner.rs):**
```rust
// DispatchedTask:
#[serde(default)] process_gen: Option<uuid::Uuid>,
#[serde(default)] launch_seq: u64,
#[serde(default)] is_acp: bool,

// CompletedTask:
#[serde(default)] process_gen: Option<uuid::Uuid>,
#[serde(default)] launch_seq: u64,
```

### Interfaces & contracts

**NAPI surface (crates/wicked-core-ts/src/lib.rs):**
```typescript
// Crew caller:
await core.resolveElicitation(runId, elicitationId, action, result?.content?.response ?? null)
```

**CoreEvents emitted:**
- `ElicitationCreated { session, epoch, elicitation_id, message, options, prop_type }` — `options`/`propType` always explicit (never absent — use `null` not omit).
- `ElicitationResolved { session, elicitation_id, action, reason }`.

**StepStatus wire token:**
- `ElicitationFailed` ↔ `"elicitation_failed"`.

### Component / module decomposition

| Module | New or changed | What it holds |
|--------|---------------|---------------|
| `src/event.rs` | Changed | `ElicitationCreated`, `ElicitationResolved` variants |
| `src/command.rs` | Changed | `ResolveElicitation`, `ApplyStepResult` ack field |
| `src/workflow.rs` | Changed | `StepStatus::ElicitationFailed`, `StepInput` new fields |
| `src/bus.rs` | Changed | Cursor/metadata APIs; `find_completed` scanner |
| `src/acp_runner.rs` | Changed (major) | `ElicitationMaps`, `EpochCleanup`, `TurnResult`, elicitation arm, `rpc_expect<W>`, `rpc_respond<W>`, `WriteWatchdog` |
| `src/actor.rs` | Changed (major) | `dispatch_unit`, command handlers, `shared_run_terminal`, `Shutdown` drain, write-lock registry |
| `src/cli_runner.rs` | Changed | Serde defaults, status round-trip, bus consumer |
| `src/lib.rs` | Changed | Wiring: `ElicitationMaps` Arc, `resolve_elicitation` |
| `crates/wicked-core-ts/src/lib.rs` | Changed | NAPI binding |
| `crates/wicked-core-ts/Cargo.toml` | Changed | `"serde-json"` napi feature |
| `Cargo.toml` | Changed | `uuid`, `tracing` deps |

### State & control flow

**Elicitation lifecycle (happy path):**
1. ACP adapter sends `elicitation/create` JSON-RPC on stdout.
2. `exec_turn_acp` parses schema, mints `elicitation_id`, calls `maps.register`.
3. `Command::EmitEvent` sends `ElicitationCreated` to crew (unless `creation_announced` suppression guard fires).
4. `'elicit` dual-poll loop: `try_recv` on resolution channel + `line_rx.recv_timeout(50ms)`.
5. Crew calls `resolveElicitation` → NAPI → `Command::ResolveElicitation` → `maps.deliver`.
6. Loop breaks; `rpc_respond` writes JSON-RPC response to adapter stdin.
7. `maps.remove` prunes `run_index`. `ElicitationResolved` event emitted.
8. `EpochCleanup::drop` fires (on `exec_turn` return): calls `cleanup_run` (deferred clear if `bus_in_flight_deferred`).

**Terminal paths:**
- `action:"cancel"` → `cancelled=true` → `StepStatus::Cancelled`.
- `Err(Disconnected)` or deadline → `elicitation_timed_out=true` → `StepStatus::ElicitationFailed`.
- `action:"decline"` → re-enter outer loop.

**Bus consumer ack-gated cursor advance (predecessor path):**
```
find_completed(completed_stream, run_id, launch_seq)
  → Some(completion) → ApplyStepResult{ack:Some(tx)} → ack_rx.recv().is_ok() → advance both cursors
  → None             → synthetic failure → ApplyStepResult{ack:Some(tx)} → ack_rx.recv().is_ok() → advance dispatch cursor
```

### Failure, edge cases & resilience

- **Daemon restart with in-flight tasks**: startup reclamation migrates predecessor's
  cursor positions; `predecessor_gen` derived from old consumer name; tasks from
  predecessor processed via completed-stream reconciliation (real completion) or
  synthetic `ElicitationFailed` (no completion found).
- **Actor crash mid-ack**: `ack_rx.recv()` returns `Err` if sender dropped; cursor
  NOT advanced. Task re-delivered on next restart.
- **Replacement worker finishing before superseded worker**: `HashSet<(run_id, launch_seq)>`
  tracks each independently; empty map only when all workers exit.
- **Second `elicitation/create` during suspension**: immediately answered with
  `action:"cancel"` in `'elicit` handler; first elicitation continues (first-held wins).
- **`resolveElicitation` after deadline**: unconditional late-answer drain (I-11)
  forwards response to adapter; unit still reports `ElicitationFailed`.
- **SQLite error in `get_stable`**: returns `Result::Err`; bus consumer fails closed
  (does not mint new owner at tail).

### Quality attributes (NFRs)

- **No additional latency on non-elicitation turns**: the dual-poll loop's `recv_timeout(50ms)`
  only activates inside the `'elicit` loop arm; normal turns are unaffected.
- **Tracing subscriber wiring required**: `tracing` alone silently discards events;
  a subscriber must be initialized in tests (`tracing_subscriber::fmt::init()` under
  `[dev-dependencies]`) and at runtime. Noted in DES-002-overview.md §Observability.

## Tasks

### T1: Wire types — deps + event/command/workflow variants

**Depends on:** none

**Tests:**
- `cargo check --workspace` passes (no compile errors).
- `grep 'StepStatus::' src/**/*.rs` — confirm wildcard catch-all in `status_from_str`
  is replaced by explicit `"elicitation_failed"` arm.
- Goal-based: `cargo test` on `src/workflow.rs` — `StepStatus` round-trips cleanly
  for the new variant.

**Approach:**
1. `Cargo.toml` root: add `uuid = { version = "1", features = ["v4", "serde"] }` and
   `tracing = "0.1"` to `[dependencies]`.
2. `src/event.rs`: add `ElicitationCreated` and `ElicitationResolved` variants with
   `to_json` arms (options/propType always explicit — use `null` not absent).
3. `src/command.rs`: add `ResolveElicitation { run_id, elicitation_id, action, response, reply: mpsc::SyncSender<anyhow::Result<()>> }`.
   Update `ApplyStepResult`: add `process_gen: Option<uuid::Uuid>`, `launch_seq: u64`,
   `ack: Option<std::sync::mpsc::SyncSender<()>>`.
   Update `CliOutputDelta`: add `process_gen: Option<uuid::Uuid>`, `launch_seq: u64`.
4. `src/workflow.rs`: add `ElicitationFailed` to `StepStatus` enum; add
   `elicitation_epoch: u64`, `process_gen: Option<uuid::Uuid>`, `launch_seq: u64`
   to `StepInput`.
5. Fix every exhaustive `match step_status` site to add `ElicitationFailed` arm
   (actor.rs, cli_runner.rs status round-trips). Grep to find all sites.

**Done when:** `cargo check --workspace` exits 0.

---

### T2: Bus DB APIs — cursor metadata and completion scanner

**Depends on:** T1

**Tests:**
- Unit: `delete_cursor` removes the row; subsequent `read_cursor` returns `None`.
- Unit: `get_stable` returns `None` on first call, `Some(value)` after `set_stable`.
- Unit: `get_stable` on SQLite error returns `Err`, not `None`.
- Unit: `find_completed` returns the correct `CompletedTask` when the target event
  is preceded by unrelated events in the stream (other `run_id`s); does NOT advance
  cursor for non-matching events.
- Unit: startup reclamation deletes predecessor cursor rows (see DES-002-tests.md
  §startup reclamation unit test).

**Approach:**
1. Add `cursor_owners` (or `meta`) KV table to SQLite schema; write migration.
2. Implement `BusDb::delete_cursor(name: &str) -> Result<()>`.
3. Implement `BusDb::get_stable(key: &str) -> Result<Option<String>>`.
4. Implement `BusDb::set_stable(key: &str, value: &str) -> Result<()>`.
5. Implement `BusDb::find_completed(consumer, run_id, launch_seq) -> Result<Option<CompletedTask>>`:
   scan from current cursor position; return first matching `(run_id, launch_seq)` without
   consuming non-matching events.

**Done when:** `cargo test -p wicked-core -- bus` (or equivalent module filter) passes.

---

### T3: ElicitationMaps struct + all methods

**Depends on:** T1

**Tests:** (TDD — write stubs first, fill while implementing)
- Tests 1–2: `register` + `remove` round-trip.
- Test 3: `cancel_epoch` cross-run isolation.
- Test 4: `deliver` resolves receiver.
- Test 5: `deliver` with wrong `run_id` returns `Err`.
- Test 6: `deliver` then `cancel_epoch` is no-op.
- Test 7: 8 KB message truncation — byte-length cap, not character count. A
  4-byte-per-codepoint UTF-8 string of 2,049 codepoints (8,196 bytes) is truncated.
- Test 8: options entry exceeding 512 bytes (byte length) is dropped; entry under
  512 bytes is passed through. Verify with a valid multi-byte UTF-8 string.
- Test 9: empty-string options entry dropped; non-empty entry retained.
- Test 10: `prop_key` preserved from schema (not hardcoded `"response"`).
- Test 10a: null constraint fields treated as absent.
- Tests 11: (remaining ElicitationMaps unit tests per DES-002-tests.md §Test plan)

Full test stubs from DES-002-tests.md §Test skeleton code.

**Approach:**
1. Add `ElicitationResult`, `ElicitationEntry` types.
2. Implement `ElicitationMaps` struct with all fields (including `bus_in_flight_workers:
   HashSet<(String, u64)>`, `shutdown_flag: bool`, `bus_activated_seqs: HashMap<String, u64>`,
   `run_launch_seq: HashMap<String, u64>`, `suppressed_creations: HashSet<String>`,
   `bus_dispatched_runs: HashSet<String>`).
3. Implement all methods per DES-002-elicitation-maps.md §Methods:
   `register`, `remove`, `deliver`, `cancel_epoch`, `begin_launch`,
   `mark_bus_dispatch`, `mark_bus_in_flight`, `is_bus_worker_in_flight`,
   `clear_bus_in_flight`, `any_bus_worker_in_flight`, `take_suppressed_creation`,
   `advance_launch_seq`, `restore_launch_seq`, `has_active_run`, `has_activated_seq`,
   `set_shutdown_flag`, `is_shutdown`, `cleanup_run` (3-arg: run_id, epoch, launch_seq).
4. Key invariant for `begin_launch`: do NOT clear `bus_in_flight_workers` (each
   worker manages its own `(run_id, launch_seq)` entry independently).
5. `cleanup_run` checks `bus_in_flight_deferred` flag; clears only on panic/cancel
   paths (not after normal bus completion).

**Done when:** tests 1–11 and 10a pass.

---

### T4: EpochCleanup RAII guard + TurnResult + session gen + AcpStepRunner fields

**Depends on:** T3

**Tests:**
- Unit (test 35): `cleanup_run` reclaims state — pass `launch_seq=0` for local/non-bus case.
- Unit: `EpochCleanup::drop` fires `cleanup_run`; `bus_in_flight_deferred=false` clears
  in-flight immediately.
- Unit: `EpochCleanup::drop` with `bus_in_flight_deferred=true` does NOT clear in-flight
  on drop (bus consumer clears later).

**Approach:**
1. Implement `EpochCleanup` struct with all fields: `maps`, `run_id`, `epoch`, `launch_seq`,
   `bus_in_flight_deferred`, `tx`, `in_flight_id`, `in_flight_action`, `in_flight_reason`.
2. Implement `EpochCleanup::drop`: `EpochCleanup::drop` (not `cleanup_run`) inspects
   the `bus_in_flight_deferred` flag; if false (panic/cancel), calls
   `maps.clear_bus_in_flight(run_id, launch_seq)` immediately; if true (normal bus
   completion path), skips clear (bus consumer will clear after confirm_task_completed).
   Then emits `Command::EmitEvent` for `ElicitationResolved` if in-flight, then calls
   `maps.cleanup_run(run_id, epoch, launch_seq)`. `cleanup_run` itself does NOT receive
   or check the flag.
3. Implement `TurnResult` and `TurnResult::default_at` constructor.
4. Implement `drop_session_gen` on `AcpStepRunner`.
5. Add `AcpStepRunner` fields: `elicitation_maps`, `session_gen` map, `write_reg`.
6. Guard installation pattern: `let mut _epoch_guard: Option<EpochCleanup> = if
   input.elicitation_epoch > 0 { Some(EpochCleanup { ..., bus_in_flight_deferred: false, ... }) } else { None }`.

**Done when:** test 35 + EpochCleanup drop tests pass; `cargo check` clean.

---

### T5: exec_turn_acp elicitation arm + rpc_respond + rpc_expect

**Depends on:** T3, T4

**Tests:** (TDD — arm-level tests 12–20 and frame tests 21–24, plus new tests 37)

- Test 12: elicitation disabled → `continue 'exec`.
- Test 13: non-string single-property schema → immediate cancel.
- Test 14: multi-property schema → immediate cancel.
- Test 15: valid schema → `ElicitationCreated` emitted, `pending_elicitations` populated.
- Test 16: `deliver` resolves via channel → `rpc_respond` writes response to adapter.
- Test 17: timeout → `elicitation_timed_out=true` → `ElicitationFailed`.
- Test 18: human dismiss (`action:"cancel"`) → `StepStatus::Cancelled`.
- Test 19: adapter disconnect mid-suspend → `ElicitationFailed`.
- Test 20: second `elicitation/create` during suspension → immediately cancelled.
- Test 21: `rpc_respond` echoes string-typed request id verbatim (`id != null` guard,
  not `as_u64()`). Feed `elicitation/create` with `id: "uuid-format-string"` and
  verify response carries the same string id.
- Tests 22–24: `rpc_expect` frame routing with `W:Write` seam.
- Test 37: `session/prompt` usage captured mid-elicitation replaces (not sums) prior
  `handle_update`-derived token counts. Verify the token count after elicitation
  resolves equals the `session/prompt` usage value, not prior + prompt usage.

**Approach:**
1. Implement `rpc_respond<W: Write>` with generalized writer signature.
2. Implement `rpc_expect<W: Write>` with handshake-phase elicitation guard:
   - On `elicitation/create` during handshake: cancel immediately; skip other methods.
3. Add `const ELICITATION_POLL_MS: u64 = 50`, `const FRAME_BYTE_CAP: usize = MAX_OUT * 7`.
4. Add `ELICITATION_VERIFIED_ADAPTERS` allow-list; `elicitation_enabled` and
   `form_enabled` separation (`form_enabled` requires both the adapter allow-list and
   the schema capability).
5. Five new variables before outer loop: `elicitation_timed_out`, `prompt_done_path`,
   `prompt_error`, `write_failed_terminal`, `dead_session`.
6. Outer-loop bounded poll (500 ms when `run_epoch > 0`).
7. Full `elicitation/create` arm: method guard → schema parse → register → emit event
   → `'elicit` dual-poll → drain → Phase 1 tombstone gate → Phase 2 write → Phase 3
   post-write tombstone gate.
8. `exec_turn` match block arm order:
   `elicitation_timed_out → write_failed_terminal → dead_session → Ok → Cancelled → Ok(_) → Err(e)`.
9. `WriteWatchdog` 3-state atomic CAS protocol.
10. FRAME_BYTE_CAP enforcement in stdout reader thread.

**Done when:** arm-level tests 12–24 pass.

---

### T6: actor.rs — dispatch_unit + command handlers + shared_run_terminal + shutdown drain

**Depends on:** T3, T4, T5

**Tests:**
- Test 25: tombstone race (pre-write tombstone gate fires).
- Test 27: epoch separation.
- Test 29: `shared_run_terminal` teardown reason.
- Test 36: `Command::Shutdown` drain.

**Approach:**
1. Update `actor::run` signature: add `is_acp: bool`, `elicitation_maps: Option<Arc<Mutex<ElicitationMaps>>>`.
2. `Command::EmitEvent` handler: add `suppressed_creations` guard using `take_suppressed_creation`
   (NOT epoch tombstone).
3. `Command::ResolveElicitation` handler: look up maps, call `deliver`, send reply.
4. `dispatch_unit` updated signature: `process_gen: uuid::Uuid` (bare, NOT Option),
   `elicitation_maps`, `actor_maps`, `is_acp`. Bus path: epoch = sentinel 0 +
   `mark_bus_dispatch`; local path ACP: `next_epoch`.
5. `shared_run_terminal` — ordered teardown steps 1–6; lock ordering: write_reg BEFORE maps.
6. `Command::Shutdown` drain with pre/post-spawn checks.
7. `ReassignUnit` path: `advance_launch_seq` in same lock as `cancel_epoch`; second
   registry sweep bounded by `old_max_gen` snapshot.
8. `Command::ApplyStepResult` handler: send `ack.take()` AFTER committing result to store.
9. Write-lock registry `WriteReg` — `Arc<Mutex<()>>` shared by all sessions for a runner.
10. `KillHandle` reuse-safety (pidfd).
11. `Command::FailureTriageReady`: add `process_gen`, `launch_seq` fields.
12. Update all three `actor::run` call sites in `lib.rs` to pass `is_acp`, `elicitation_maps`.

**Done when:** tests 25, 27, 29, 36 pass; `cargo check` clean.

---

### T7: cli_runner.rs — serde defaults + status round-trip + bus consumer

**Depends on:** T2, T6

**Tests:**
- T7-a: pre-change `DispatchedTask` JSON (no `launch_seq`, `is_acp`, `process_gen`)
  deserializes; defaults are `0`, `false`, `None`.
- T7-b: `status_to_str(ElicitationFailed) == "elicitation_failed"`.
- T7-c: `status_from_str("elicitation_failed") == StepStatus::ElicitationFailed`.
- T7-d: `ElicitationFailed` routes to terminal (non-retry) path in `step_status`.
- T7-e: startup reclamation — cursor positions migrated BEFORE old rows deleted;
  old rows deleted BEFORE `set_stable` called.
- T7-f: `predecessor_gen` is `Some(_)` when old consumer exists; `None` otherwise.
- T7-g: predecessor path cursor NOT advanced if `ack_rx.recv()` returns `Err`
  (actor crash mid-processing simulation).
- T7-h: `find_completed` scan returns the correct completion when interleaved events
  from other run_ids precede the target event.
- Test 38: degraded-mode path — `has_activated_seq=true`, `is_bus_worker_in_flight=false`
  — ack is sent; cursor advanced only after ack success; actor commit ordering enforced.

**Approach:**
1. `DispatchedTask`: add `#[serde(default)]` fields `process_gen`, `launch_seq`, `is_acp`.
2. `CompletedTask`: add `#[serde(default)]` fields `process_gen`, `launch_seq`.
3. `status_to_str` / `status_from_str`: add explicit `ElicitationFailed` arms.
4. Bus consumer startup reclamation (from DES-002-actor-teardown.md §Startup):
   - `old_consumer_opt = bus.get_stable(&owner_key)?` FIRST.
   - Derive `predecessor_gen` from `old_consumer_opt`.
   - Migrate cursor positions from old consumer.
   - Delete old cursor rows (AFTER migration, BEFORE set_stable).
   - THEN call `bus.set_stable(&owner_key, &consumer_name)?`.
5. `try_next_epoch_bus`: reject `launch_seq==0`; `is_acp` param to `EpochCleanup`.
6. Predecessor processing: `find_completed` scan → real completion with ack → cursor
   advance; else synthetic `ElicitationFailed` with ack → cursor advance.
7. **Degraded-mode path**: when `has_activated_seq=true` and `is_bus_worker_in_flight=false`
   (task was activated but no worker is running — crash recovery scenario), emit
   synthetic `ElicitationFailed` with `ack: Some(tx)`; advance cursor only after
   `ack_rx.recv().is_ok()`. This is the third ack-gated site.
8. Bus consumer dedup key: `(run_id, unit_ix, attempt, process_gen, launch_seq)`.
9. `bus_in_flight_deferred = true` after normal completion; deferred clear by bus consumer
   after `confirm_task_completed`.
10. `core_stable_id` from fixed key `"wicked-core-instance-stable-id-{workspace_id}"` in
    bus DB (not process-generated UUID).

**Done when:** tests T7-a through T7-h and test 38 pass; `cargo check` clean.

---

### T8: lib.rs wiring completion + NAPI binding

**Depends on:** T1–T7

Note: T6 step 12 already updates the three `actor::run` call sites in `lib.rs`.
T8 completes the remaining lib.rs wiring (ElicitationMaps Arc creation, `resolve_elicitation`
method) and adds the NAPI binding. T6 and T8 do not conflict because T6's lib.rs
changes are to call sites, T8's are to new method declarations and wiring.

**Tests:**
- Goal-based: `cargo build --workspace` exits 0.
- Goal-based: `cargo test --workspace` exits 0.

**Approach:**
1. `src/lib.rs` (wiring): create `ElicitationMaps` Arc in `spawn_with_acp_sessions`;
   share between `AcpStepRunner` and `actor::run`; add `Core::resolve_elicitation` method.
   (T6's edits to the three `actor::run` call sites are pre-existing; do not re-edit them.)
2. `crates/wicked-core-ts/Cargo.toml`: add `"serde-json"` to napi feature list.
3. `crates/wicked-core-ts/src/lib.rs`: add `#[napi] async fn resolve_elicitation(run_id, elicitation_id, action, response: Option<serde_json::Value>)`.
4. `initialize` capability declaration: `{"elicitation":{"form":{}}}` in
   `clientCapabilities` when `form_enabled`, per `ELICITATION_VERIFIED_ADAPTERS`.

**Done when:** `cargo build --workspace` and `cargo test --workspace` both pass.

---

### T9: Test suite — implement all 39 test cases

**Depends on:** T3, T5, T6, T7 (stubs exist; fill implementations)

**Tests:** All tests from DES-002-tests.md baseline (37 cases) + new tests 37 and 38:
- ElicitationMaps unit tests 1–11, 10a (T3 stubs → now filled; includes tests 8 and 9).
- Arm-level turn tests 12–20 (T5 stubs → now filled).
- `rpc_respond` string-id echo test 21 (T5 stub → now filled).
- `rpc_expect` frame-routing tests 22–24 (`rpc_expect<W:Write>` seam).
- Gate tests 25–36 (tombstone race, epoch separation, cleanup_run, shutdown drain).
- Test 37: `session/prompt` usage replaces (not sums) prior token counts (T5 stub → filled).
- Test 38: degraded-mode ack-gated cursor advance (T7 stub → filled).

Note: `rpc_expect` harness must return `rx` from `make_rpc_expect_harness` —
otherwise sends fail with `Disconnected` (gate-83 correctness finding).

**Approach:**
1. Verify all stub tests are red.
2. Fill implementations; add `tracing-subscriber = { version = "0.3", features = ["fmt"] }`
   to `[dev-dependencies]` so tracing events are observable in tests.
3. Confirm test 35 uses 3-arg `cleanup_run("run-1", ep1, 0)`.
4. Confirm `make_rpc_expect_harness` returns `(tx, rx, ...)` tuple.
5. Test 38 setup — two sub-cases required:
   a. **Failure path**: create bus state `has_activated_seq=true, is_bus_worker_in_flight=false`;
      simulate actor crash between dequeue and commit (drop ack sender before `ack_tx.send()`);
      verify cursor NOT advanced (`ack_rx.recv()` returns `Err`).
   b. **Success path**: same bus state; actor commits and sends ack successfully;
      verify cursor IS advanced after `ack_rx.recv()` returns `Ok`. Without the positive
      sub-case, a test that never advances the cursor would still pass sub-case (a).

**Done when:** `cargo test` passes all 39 test cases (tests 1–36 + 10a + 37 + 38;
T7-a through T7-h pass as part of T7's "Done when").

---

### T10: Pre-merge exit criteria checks

**Depends on:** T8, T9

**Tests:**
- EC-1: `grep '"form"' src/acp_runner.rs` shows `{"form":{}}`.
- EC-2: `params.message` and `params.requestedSchema.properties` paths confirmed
  against SDK v1.3.0 `types.gen.ts` — manual verification; update extraction code if paths differ.
- EC-3: OQ-R-6 adapter serialization confirmed with an explicit verification artifact
  (e.g., adapter source audit or integration test run URL cited in PR description),
  OR `ELICITATION_VERIFIED_ADAPTERS` guard in place and the adapter NOT in the allowlist.
- EC-4: `cargo test --workspace` passes all 39 tests (37 baseline + tests 37 and 38).
- EC-5 **(blocking)**: wicked-crew PR with Studio `ElicitationPrompt` message-escape
  control reviewed and approved; link to that PR in the wicked-core PR description.
  This PR must not merge until EC-5 is confirmed.
- EC-6: `chat_turn` elicitation guard present (`elicitation_enabled=false`) in code;
  if OQ-R-7 is to be declared resolved, a verifiable artifact (link to integration
  test run or source-code audit) must be cited in the PR description — self-assertion
  is not sufficient.

**Approach:**
1. `cargo test --workspace` — verify all 39 tests pass.
2. `grep '"form"' src/acp_runner.rs` and `grep 'ELICITATION_VERIFIED_ADAPTERS' src/acp_runner.rs`.
3. Read SDK types.gen.ts for EC-2 (web fetch if not local).
4. Read adapter source or run probe for EC-3; add OQ-R-6 resolution artifact to PR.
5. Verify EC-5 wicked-crew PR link is in the PR description (blocking gate).
6. Verify EC-6 guard is in code; add OQ-R-7 resolution artifact to PR if applicable.

**Done when:** All six EC checks are green or have a documented explicit deferral
linked from the PR description. EC-5 is a hard block — no merge without it.

## Rollout

- **Delivery:** feature-flagged via `ELICITATION_VERIFIED_ADAPTERS` allowlist and
  `elicitation_enabled` / `form_enabled` per-adapter check. No adapter emits
  `elicitation/create` unless it has been verified (EC-3/EC-6). Safe to merge with
  no impact on unverified adapters.
- **Infrastructure:** none beyond SQLite schema migration (adds `cursor_owners` table;
  backward-compatible; existing rows unaffected).
- **External-system integration:** crew TS-side `ElicitationCache` must consume
  `elicitationResolved` using wire field names `session` and `elicitationId`. Must
  be deployed alongside this Rust change.
- **Deployment sequencing:** Rust half first (safe to deploy; no adapter will send
  `elicitation/create` until crew is updated), then crew TS half, then enable the
  adapter allowlist entry for `claude-agent-acp` / `codex-acp` after EC-3 is resolved.

## Risks

- **Exhaustive match breakage**: the fourth `StepStatus` variant fails `cargo check`
  across the workspace until ALL match sites are updated in T1. Must land T1 atomically.
  Mitigation: `grep 'StepStatus::' src/**/*.rs` before and after T1 to confirm
  no wildcard arm swallows the new variant.
- **SQLite migration in the bus DB**: `cursor_owners` table added by T2. If the daemon
  crashes mid-migration, a partial schema could corrupt cursor tracking. Mitigation:
  wrap the migration in a SQLite transaction.
- **ack-gated cursor advance deadlock**: if the actor crashes after dequeuing
  `ApplyStepResult` but before committing, `ack_rx.recv()` returns `Err` and cursor
  is NOT advanced. This is correct (re-deliver on restart), but the bus consumer
  thread will block for the full `recv()` timeout window. Mitigation: `sync_channel(0)`
  — the send from the actor thread is instant; no unbounded wait.
- **Predecessor task terminalization false-positive**: if `find_completed` has a bug
  (e.g., returns `None` when a completion exists due to scan-past logic), real
  completions are replaced by synthetic failures. Mitigation: unit test that places
  an interleaved event before the target completion (T2 unit test).

## Changelog

- 2026-08-06: initial plan — derived from DES-002 design docs (gates 1–83).
- 2026-08-06: rev 2 — second adversarial + security re-review findings applied:
  spec EC-4 count updated to 39; T9 step 5 adds positive-path assertion for test 38;
  plan Constraints cross-ref fixed (Always do not Never do); 8 KB cap byte-length
  qualifier added to spec AC and T3 test 7; message cap AC added to spec.
- 2026-08-06: rev 1 — adversarial + security pre-execute review findings applied:
  fixed degraded-mode ack-gated site (was "normal completion"); added T7 degraded-mode
  approach step; added tests 8, 9 to T3 stub list; added tests 37 (usage replace) and
  38 (degraded-mode ack) to T5/T7/T9; fixed T4 step 2 (EpochCleanup::drop checks
  bus_in_flight_deferred, not cleanup_run); fixed T7 Done-when to name test IDs;
  fixed T8 scope (T6 vs T8 lib.rs edits); moved implementation constraints out of
  spec Boundaries into plan Constraints; added startup delete-after-migrate ordering
  to Constraints; EC-5 made blocking; EC-6 sign-off tightened to require artifact.
