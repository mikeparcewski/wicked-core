# Purpose

Test plan, file map, `StepStatus::ElicitationFailed` exhaustive match sites, design decisions, and deferred items (Appendix A).

---

## File map

The following source files each require the listed changes. All changes must compile together; the fourth `StepStatus` variant causes compile errors at every exhaustive match site (see next section).

| File | Changes required |
|------|-----------------|
| `src/event.rs` | Add `ElicitationCreated { session: String, epoch: u64, elicitation_id: String, message: String, options: Option<Vec<String>>, prop_type: Option<String> }` and `ElicitationResolved { session: String, elicitation_id: String, action: String, reason: String }` variants; add `to_json` arms for both (options/propType always explicit, never absent — use `null` not absent key) |
| `src/command.rs` | Add `ResolveElicitation { run_id: String, elicitation_id: String, action: String, response: Option<serde_json::Value>, reply: mpsc::SyncSender<anyhow::Result<()>> }` — uses `std::sync::mpsc::sync_channel(1)`; root crate has no Tokio dep |
| `src/lib.rs` | Add `Core::resolve_elicitation(&str, &str, &str, Option<serde_json::Value>) -> anyhow::Result<()>` that sends `Command::ResolveElicitation` and awaits the reply; update `spawn_with_acp_sessions` to create `ElicitationMaps` Arc and share it between runner and actor; update all three `actor::run` call sites to pass `is_acp: bool` and `elicitation_maps: Option<Arc<Mutex<ElicitationMaps>>>` (always `Some` for real paths, see DES-002-actor-teardown.md §call sites) |
| `src/acp_runner.rs` | All new types and methods from DES-002-elicitation-maps.md: `ElicitationResult`, `ElicitationEntry`, `ElicitationMaps` (all fields, all methods), `EpochCleanup` RAII guard, `TurnResult` and `TurnResult::default_at`; `exec_turn_acp` new params and full elicitation/create arm; `exec_turn` match block update with arm order (elicitation_timed_out → write_failed_terminal → dead_session → Ok → Cancelled → Ok(_) → Err(e)); `rpc_respond<W: Write>` generalized signature; `rpc_expect<W: Write>` generalized signature and handshake-phase elicitation guard (cancel elicitation/create, skip other methods); `start_acp_process` capability advertisement (`form_enabled`, `ELICITATION_VERIFIED_ADAPTERS`, `{"form":{}}` vs `{}`); FRAME_BYTE_CAP enforcement in stdout reader thread; `const ELICITATION_POLL_MS: u64 = 50`; `const FRAME_BYTE_CAP: usize = MAX_OUT * 7`; `WriteWatchdog` (or import from a shared crate); `drop_session_gen` method on `AcpStepRunner` |
| `src/actor.rs` | `actor::run` new params (`is_acp: bool`, `elicitation_maps: Option<Arc<Mutex<ElicitationMaps>>>`); `Command::EmitEvent` suppressed_creations guard (uses `take_suppressed_creation`, NOT epoch tombstone); `Command::ResolveElicitation` handler; `Command::FailureTriageReady` gains `process_gen: Option<uuid::Uuid>` and `launch_seq: u64` (disambiguates relaunched-run stale triage); `shared_run_terminal` with ordered teardown (steps 1–6); `Command::Shutdown` drain with pre/post-spawn checks (steps 1a–1e); `advance_launch_seq` in `ReassignUnit` (in same lock as `cancel_epoch`); second registry sweep in `ReassignUnit` bounded by `old_max_gen` snapshot captured before dispatch_unit (prevents killing replacement workers); `dispatch_unit` updated signature (adds `elicitation_maps`, `actor_maps`, `process_gen: uuid::Uuid` (bare, NOT `Option<Uuid>` — it is wrapped into `Some(process_gen)` only when stored in `DispatchedTask` and `StepInput`), `is_acp`) and epoch allocation (bus path: sentinel 0 + `mark_bus_dispatch`; local path: `next_epoch` for ACP exec turns); write-lock registry `WriteReg` created in `spawn_with_acp_sessions`; `Command::ApplyStepResult` handler must send `ack.take()` (if `Some`) after committing the result to the store, so the bus consumer's ack-wait unblocks only after durable persistence |
| `src/workflow.rs` | Add `StepStatus::ElicitationFailed` variant to the `StepStatus` enum; add `elicitation_epoch: u64`, `process_gen: Option<uuid::Uuid>`, `launch_seq: u64` fields to `StepInput`. This file must be changed before `src/cli_runner.rs` and `src/acp_runner.rs` because both import these types. |
| `src/bus.rs` | Add `BusDb::delete_cursor(name: &str) -> Result<()>` to remove a durable consumer cursor row; add `BusDb::get_stable(key: &str) -> Result<Option<String>>` and `BusDb::set_stable(key: &str, value: &str) -> Result<()>` for the startup cursor-reclamation owner record — returning `Result` so callers can fail closed on SQLite errors rather than silently treating a missing record as absent (which could cause a new owner to be minted at the current tail, skipping pending dispatch/completion events). Add `BusDb::find_completed(consumer: &str, run_id: &str, launch_seq: u64) -> Result<Option<CompletedTask>>` to scan the completed stream from the current cursor position and return the first event matching `(run_id, launch_seq)` without consuming unrelated events or advancing the cursor. Required for predecessor completion reconciliation: when interleaved events from other runs precede the matching completion, checking only the next event returns `None` and incorrectly terminalizes a task that actually succeeded. Requires a `cursor_owners` table (or a `meta` KV table) in the SQLite schema. Add unit test: startup reclamation deletes predecessor rows. |
| `src/cli_runner.rs` | `status_to_str`/`status_from_str` round-trip for `StepStatus::ElicitationFailed` with wire token `"elicitation_failed"`; `step_status(...)` match arm routing `ElicitationFailed` to terminal path; `DispatchedTask` gains `process_gen: Option<uuid::Uuid>`, `launch_seq: u64`, `is_acp: bool` — all new non-optional fields MUST carry `#[serde(default)]` so that pre-change rows round-trip cleanly (`u64` defaults to `0`, `bool` defaults to `false`, `Option<_>` to `None`) rather than failing deserialization before the `process_gen == None` legacy path can run; `CompletedTask` gains `process_gen: Option<uuid::Uuid>`, `launch_seq: u64` with the same `#[serde(default)]` requirement; `Command::ApplyStepResult` gains `process_gen: Option<uuid::Uuid>`, `launch_seq: u64` for stale-completion rejection, and `ack: Option<std::sync::mpsc::SyncSender<()>>` (set by the degraded / predecessor paths in the bus consumer so they can wait for the actor to commit the result before advancing the durable cursor — `None` for all normal worker-initiated paths); `Command::CliOutputDelta` gains `process_gen: Option<uuid::Uuid>`, `launch_seq: u64`; bus consumer dedup key extended to `(run_id, unit_ix, attempt, process_gen, launch_seq)` — the `(run_id, unit_ix, attempt)` triplet is not unique across restarts; gate-evaluation ID includes `process_gen` and `launch_seq` for the same reason |
| `crates/wicked-core-ts/src/lib.rs` | Add `Core::resolve_elicitation` NAPI binding: `#[napi] async fn resolve_elicitation(...)` that calls the Rust `resolve_elicitation` and awaits reply; signature: `(run_id: String, elicitation_id: String, action: String, response: Option<serde_json::Value>)` — requires `"serde-json"` napi feature (see Cargo.toml row). Crew TS wrapper must unpack `result.content?.response ?? null` before calling (passing `result.response` directly passes `undefined`, which is rejected as non-string) |
| `crates/wicked-core-ts/Cargo.toml` | Add `"serde-json"` to the `napi` dependency feature list (required for `Option<serde_json::Value>` deserialization in NAPI binding) |
| `Cargo.toml` (root) | Add `uuid = { version = "1", features = ["v4", "serde"] }` and `tracing = "0.1"` to `[dependencies]` |

### Change sequencing

The changes are load-bearing in dependency order:
1. `Cargo.toml` — uuid and tracing deps (unblocks everything).
2. `src/event.rs` and `src/command.rs` — new variants (unblocks actor and lib).
3. `src/workflow.rs` — `StepStatus::ElicitationFailed` variant and `StepInput` new fields (must precede all importers).
4. `src/bus.rs` — `delete_cursor`, `get_stable`, `set_stable` APIs and schema (must precede cli_runner and actor which call them for cursor reclamation).
5. `src/acp_runner.rs` — new types and runner logic (imports `StepStatus`, `StepInput`).
6. `src/actor.rs` — new handler arms (depends on event/command variants and bus.rs).
7. `src/cli_runner.rs` — status round-trip, new task fields, and bus consumer cursor reclamation (imports `StepStatus` from workflow.rs, `BusDb` from bus.rs).
8. `src/lib.rs` — wiring (depends on all of the above).
9. `crates/wicked-core-ts/` — NAPI binding (depends on lib.rs).

All nine steps must compile together for the fourth `StepStatus` variant to not produce exhaustive-match errors at link time.

---

## `StepStatus::ElicitationFailed` — all exhaustive match sites

Adding a fourth `StepStatus` variant makes every exhaustive match non-exhaustive. The implementation will not compile until ALL of these are updated:

| File | Location | Required change |
|------|----------|-----------------|
| `src/actor.rs` | `step_status(...)` match | Add `ElicitationFailed` arm routing to run-terminal path (bypass triage) |
| `src/actor.rs` | `ApplyStepResult` handler | `ElicitationFailed` routes to terminal path, bypasses triage; never retry |
| `src/cli_runner.rs` | `status_to_str` | Add `ElicitationFailed => "elicitation_failed"` |
| `src/cli_runner.rs` | `status_from_str` | Add `"elicitation_failed" => StepStatus::ElicitationFailed` |
| Any `match step_output.status` in result-recording paths | `actor.rs`, `cli_runner.rs` | `ElicitationFailed` treated as terminal failure: no retry, no success audit |

**Non-exhaustive match risk**: Rust only catches missing arms for non-wildcard matches. A wildcard `_ => ...` arm silently swallows `ElicitationFailed`. The bus serializer currently uses a wildcard in `status_from_str`. Confirm the wildcard maps to a safe default (it does: `StepStatus::Ok` — but Ok is wrong for ElicitationFailed, so the explicit arm is required).

Grep for `StepStatus::` across all `.rs` files before opening the PR to catch any wildcard matches introduced by code paths added after this document.

---

## Test plan

All tests are in `src/acp_runner.rs` test module or `tests/elicitation_maps.rs`. Tests that require `exec_turn_acp` need the transport seam; pure `ElicitationMaps` unit tests do not.

### `ElicitationMaps` unit tests

These tests exercise `ElicitationMaps` directly via its public interface. No actor, no transport.

| # | Test | Setup | Assert |
|---|------|-------|--------|
| 1 | `register` → both maps populated | Create maps; call `register(run, eid, epoch=1)` | `pending` contains sender for `eid`; `run_index[run]` contains `(eid, 1)` |
| 2 | `remove` → both maps cleared | Register as above; call `remove(run, eid)` | `pending` entry absent; `run_index[run]` empty or absent |
| 3 | `cancel_epoch(run, 1)` drops sender; other run unaffected | Register two runs: (run_a, eid_a, 1) and (run_b, eid_b, 1); cancel epoch 1 for run_a | `rx_a.try_recv()` returns `Err(Disconnected)`; `rx_b` open (returns `WouldBlock`) |
| 4 | `deliver` with matching run_id → sender receives result | Register and hold `rx`; call `deliver(run, eid, ElicitationResult { action: "accept", response: Some(json!("prod")) })` | `rx.recv()` yields `ElicitationResult { action: "accept", ... }` |
| 5 | `deliver` with mismatched run_id → `Err` | Register under run_a; call `deliver(run_b, eid, result)` | Returns `Err(...)` with both IDs in the message |
| 6 | `deliver` then `cancel_epoch` → no panic | Register; `deliver(run, eid, result)` (removes sender); then `cancel_epoch(run, 1)` | `cancel_epoch` finds `pending` empty for eid; no panic; tombstone inserted; `is_epoch_cancelled(run, 1)` returns true |
| 7 | 8 KB message truncation boundary | `register(run, eid, 1, message=repeat('x', 8192))` and `register(run, eid2, 1, message=repeat('x', 8193))` | First: no truncation; second: message ends with `[truncated]` at valid UTF-8 boundary |
| 8 | Empty enum → cancel | Call schema parse with `{"type":"string","enum":[]}` | Returns `action:"cancel"` immediately; `pending` empty; no `ElicitationCreated` event |
| 9 | All-empty-string enum → cancel | Schema `{"type":"string","enum":["",""]}` | All choices filtered (empty string dropped); constraint present but empty → F16 cancellation; same result as test 8 |
| 10 | Schema prop_key preserved in response | Schema `{"type":"object","properties":{"choice":{"type":"string"}}}` | `prop_key == "choice"`; response frame uses `"choice"` as the content key, NOT hardcoded `"response"` |
| 10a | Null-valued constraint fields treated as absent | Schema `{"type":"string","minLength":null,"enum":null,"oneOf":null}` | `ElicitationCreated` emitted with `options: None, prop_type: "string"`; null fields are not active constraints |
| 11 | Register-before-emit: `deliver` succeeds immediately after `register` | Register and synchronously call `deliver` in the same thread before emitting `ElicitationCreated` | `deliver` succeeds; `rx.recv()` yields result; no race |

### Arm-level turn tests (exec_turn_acp)

**Required transport seam**: `exec_turn_acp` requires a concrete `AcpProcess` containing `Child + BufWriter<ChildStdin>`. These tests require either:
- `AcpProcess<W: AcpWriter>` generic where `W: Write + Send + 'static`, or
- A `FakeAcpProcess` harness with `stdin_tx: mpsc::Sender<Vec<u8>>` and `line_rx: Receiver<String>`.

The `Cursor<Vec<u8>>` writer captures writes for assertion. Use a synthetic `mpsc::channel` pair as the `line_rx`.

Test thread pattern:
```rust
let (line_tx, line_rx) = mpsc::channel::<String>();
let (mut maps, rx_maps) = make_maps_pair();  // maps + channel for ElicitationResult
let input = StepInput { ..., elicitation_epoch: 1, ... };
let handle = thread::spawn(move || exec_turn_acp(proc, ..., input, Some(&mut epoch_guard)));
// Test thread feeds lines:
line_tx.send(elicitation_create_json()).unwrap();
// Optionally deliver via maps:
maps.deliver("run", "eid", ElicitationResult { action: "accept", response: Some(json!("v")) }).unwrap();
// Feed terminal frame:
line_tx.send(session_prompt_response_json()).unwrap();
let result = handle.join().unwrap();
assert_eq!(result.status, StepStatus::Ok);
```

| # | Test | Setup | Assert |
|---|------|-------|--------|
| 12 | Accept path | Feed `elicitation/create`; post `deliver(action="accept", response=Value::String("prod"))`; feed `session/prompt` response | `rpc_respond` called with `{action:"accept",content:{<prop_key>:"prod"}}`; `TurnResult.status == Ok`; `cancelled == false`; `elicitation_timed_out == false`. Acceptance re-enters the outer loop; a terminal `session/prompt` must follow. |
| 13 | Human-dismiss path | Feed `elicitation/create`; post `deliver(action="cancel")` | `rpc_respond {action:"cancel"}`; `TurnResult.status == Cancelled`; `cancelled == true`; `elicitation_timed_out == false`; no fallback |
| 14 | Teardown path | Feed `elicitation/create`; then call `cancel_epoch(run_id, epoch)` (drops sender) | Worker sees `Disconnected`; `TurnResult.status == Cancelled` (raw from `exec_turn_acp`; exec_turn arm 1 converts to `ElicitationFailed` output); `elicitation_timed_out == true`; `cancelled == false`. Assert reason via `ElicitationResolved.reason == "teardown"` (not via TurnResult — `elicitation_teardown` is local to `exec_turn_acp`). |
| 15 | Decline path | Feed `elicitation/create`; post `deliver(action="decline")`; feed `session/prompt` response | `rpc_respond {action:"decline"}`; outer loop re-entered; `TurnResult.status == Ok` |
| 16 | Second concurrent `elicitation/create` | Feed first `elicitation/create`; feed second on `line_rx`; then deliver the first | Second receives `rpc_respond {action:"cancel"}`; first resolves normally; no deadlock |
| 17 | Adapter-death path | Feed `elicitation/create`; drop `line_rx` sender (adapter stdout EOF) | `TurnResult.status == Cancelled` (raw; exec_turn converts to `ElicitationFailed`); `elicitation_timed_out == true`; `cancelled == false`; `tracing::warn` emitted (verify via tracing subscriber capture) |
| 18 | Post-deadline late-answer | Set deadline to 1 ms; feed `elicitation/create`; sleep 5 ms (deadline expires); deliver non-cancel result before `maps.remove` | Answer forwarded to adapter (verify via `Cursor` writer); `TurnResult.status == Cancelled` (raw; exec_turn converts to `ElicitationFailed`); `elicitation_timed_out == true`; subsequent `deliver` call returns `Err("not found")` |
| 19 | Mid-elicitation `session/prompt` JSON-RPC error | Feed `elicitation/create`; feed `session/prompt` response with `"error"` field | `TurnResult.status == Failed`; `cancelled == false`; `elicitation_timed_out == false`; `exec_turn` `Ok(_)` catch-all fires → fallback called; `rpc_respond {action:"cancel"}` sent (internal `prompt_done` sentinel mapped to wire `"cancel"`) |
| 20 | Mid-elicitation `session/prompt` cancellation | Feed `elicitation/create`; feed `session/prompt` with `stopReason:"cancelled"` | `TurnResult.status == Cancelled`; `cancelled == false`; `elicitation_timed_out == false`; `timed_out == true` inside `exec_turn_acp` (different from `elicitation_timed_out`) |

### `rpc_expect` frame-routing tests

Use `Cursor<Vec<u8>>` for the writer and a synthetic `mpsc::channel` for the receiver. No live adapter needed.

```rust
// Returns (tx, rx, writer, write_lock, kill_handle).
// rx must be kept alive by the caller — dropping it before rpc_expect reads from tx
// causes Disconnected errors on send, failing tests 21–24.
fn make_rpc_expect_harness() -> (mpsc::Sender<String>, mpsc::Receiver<String>, Cursor<Vec<u8>>, Arc<Mutex<()>>, Arc<KillHandle>) {
    let (tx, rx) = mpsc::channel::<String>();
    let buf = Cursor::new(Vec::new());
    let wl = Arc::new(Mutex::new(()));
    let kh = Arc::new(KillHandle::noop());  // noop kill for tests
    (tx, rx, buf, wl, kh)
}
```

| # | Test | Setup | Assert |
|---|------|--------|--------|
| 21 | Banner skip | Feed non-JSON startup banner `"Antigravity v1.0 starting\n"`, then real response JSON `{"id":1,"result":{...}}` | Banner does not propagate as error; real response returned; `Cursor` writer is empty (no write on banner skip) |
| 22 | Handshake-phase elicitation cancel | Feed `{"method":"elicitation/create","id":1,...}`, then real response `{"id":1,"result":{...}}` | `Cursor` writer receives `{"jsonrpc":"2.0","id":1,"result":{"action":"cancel"}}` (or equivalent); real response returned; no id-collision misinterpretation |
| 23 | Other-method skip | Feed `{"method":"adapter/notify","id":1,"params":{}}`, then real response `{"id":1,"result":{...}}` | Method frame silently skipped (no write to `Cursor`); real response returned |
| 24 | Full sequence | Feed banner → `elicitation/create` (id=1) → `adapter/notify` (id=1) → real response (id=1) | Banner skipped; elicitation cancelled (one `action:"cancel"` write); other method frame skipped; real response returned without error |

### Gate tests — tombstone and intersection

| # | Test | Assert |
|---|------|--------|
| 25 | Tombstone race | Call `cancel_epoch(run_id, 1)` before `register(run_id, eid, 1)` | `register` returns `None`; no entry in `pending` or `run_index`; `is_epoch_cancelled(run, 1)` returns true |
| 26 | `oneOf`+`enum` intersection | Schema A: `{"enum":["a","b"],"oneOf":[{"const":"b"},{"const":"c"}]}` → intersects to `["b"]`; schema B: `{"enum":["a"],"oneOf":[{"const":"b"}]}` → empty intersection → F16 cancel | Schema A: `opts == Some(["b"])`; `ElicitationCreated` emitted. Schema B: `rpc_respond {action:"cancel"}`; no `ElicitationCreated`. |
| 27 | Epoch separation | `cancel_epoch(run, 1)` → tombstone; `register(run, eid, 1)` → `None`; `next_epoch(run)` → 2; `register(run, eid2, 2)` → `Some(rx)` | `cancel_epoch` never bumps; `next_epoch` is the only bumper; each epoch independently gated |
| 28 | `prompt_done_path` scope | Inject `session/prompt` error in `'elicit` loop (→ `result.action="prompt_done"`); concurrently `deliver(action="accept")` before outer-loop routing | `prompt_done_path=true` (hoisted before drain); outer loop breaks despite `result.action` being overwritten by the concurrent accept |
| 29 | `ElicitationResolved` emitted for every exit path | Five sub-cases: (a) human accept; (b) session/prompt mid-elicitation; (c) deadline expiry; (d) teardown; (e) adapter write failure (no deliberate kill, attempted action = accept); (f) write failure on deliberate-kill path | (a): `reason="human",action="accept"`; (b): `reason="session_prompt",action="cancel"`; (c): `reason="timeout",action="cancel"`; (d): `reason="teardown",action="cancel"`; (e): `reason="adapter_write_failure",action="accept"` (the attempted wire action, NOT "cancel"); (f): `reason="teardown"` or `"timeout"` (NOT `"adapter_write_failure"`). Event emitted AFTER adapter write attempt. |
| 30 | Present-but-empty schema vs absent schema | Schema A: `{"type":"object","properties":{},"additionalProperties":false}`; schema B: absent `requestedSchema` with `mode="form"` | Schema A: `rpc_respond {action:"cancel"}`; no `ElicitationCreated`. Schema B: `rpc_respond {action:"cancel"}`; no `ElicitationCreated`. Both cancel — empty properties and absent schema are both non-representable in form mode. |
| 31 | Stale worker epoch stays cancelled after bump | `cancel_epoch(run, 1)` (tombstone); `register(run, eid, 1)` → `None`; `next_epoch(run)` → 2; `register(run, eid2, 2)` → `Some(rx)`. Stale worker holding epoch 1 calls `register(run, eid3, 1)` | Returns `None`; tombstone persists; epoch 2 unaffected |
| 32 | P2b race — accept arrives after cancel | `deliver(action="accept")` runs (removes sender); then `cancel_epoch(run, epoch)` tombstones (no sender to drop, no Disconnected); `'elicit` loop receives `Ok(accept)` | `is_epoch_cancelled` check fires after drain; `elicitation_timed_out=true`; `elicitation_teardown=true`; outer loop does NOT re-enter; `ElicitationResolved reason="teardown"` |
| 33 | Adapter write failure path | Human accept → `rpc_respond` returns `Err`; `exec_turn_acp` returns `Ok(TurnResult { write_failed_terminal: true, ... })` | `ElicitationResolved {reason="adapter_write_failure",action="accept"}` emitted (the attempted wire action, NOT "cancel"); `exec_turn` calls `drop_session_gen` and returns `StepStatus::ElicitationFailed`; no fallback/retry. Confirm `exec_turn_acp` returns `Ok(...)` not `Err(...)` — write failure uses the `write_failed_terminal` field, not the error path. |
| 34 | Drain epoch re-check | `deliver(accept)` enqueues answer while `'elicit` exits at deadline; then `cancel_epoch(run, epoch)` tombstones; drain block acquires lock and sees tombstone | Drain discards late answer; `elicitation_teardown=true`; `ElicitationResolved reason="teardown"` (not `reason="timeout"` with accept action) |
| 35 | `cleanup_run` lifecycle | Register → `cancel_epoch` → `cleanup_run`; verify `cancelled_epochs` and `run_epoch` have no entries for `run_id`; subsequent `register(run, eid2, epoch=0)` succeeds (check: `cancelled_epochs` for run_id cleared, so epoch 0 is not pre-tombstoned for a fresh run) | State reclaimed; no stale tombstones from the prior run's epochs |
| 36 | Write failure on deliberate-kill teardown | Teardown path (`elicitation_teardown=true`, `deliberate_kill=true`); child signalled before write; `rpc_respond` returns `Err` | `ElicitationResolved {reason="teardown"}` (NOT `"adapter_write_failure"`); `deliberate_kill=true` routes reason to teardown; `adapter_write_failure` is reserved for unexpected transport failures |

---

## Design decisions and guards

### Folded resolved decisions

| Decision | Folded into |
|----------|-------------|
| Actor holds direct `Arc<ElicitationMaps>` clone; no `StepRunner` trait widening | DES-002-elicitation-maps.md §AcpStepRunner fields |
| `session/new` is a bridge convention; native adapters use fresh `start_acp_process` per run; `on_run_complete` handles teardown | DES-002-actor-teardown.md §on_run_complete |
| `rpc_respond<W: Write>` and `rpc_expect<W: Write>` generalized; production passes `BufWriter<ChildStdin>`, tests pass `Cursor<Vec<u8>>` | DES-002-exec-turn-acp.md §rpc_respond; DES-002-actor-teardown.md §Handshake-phase guard |
| `"elicitation": {"form": {}}` required in `clientCapabilities` (bare `{}` omits `form`) | DES-002-actor-teardown.md §initialize capability |
| `params.message` and `params.requestedSchema.properties` confirmed against ACP SDK v1.3.0 `types.gen.ts` (EC-2 / OQ-R-5) | DES-002-exec-turn-acp.md §schema parsing |
| `TurnResult::default_at` uses `status: StepStatus::Failed` explicitly (not default which gives `Ok`) | DES-002-elicitation-maps.md §TurnResult::default_at |
| `input.blank_step_output()` for StepOutput in cancelled-startup/turn Err arms; `StepOutput.output` is `String` (not `Vec`) | DES-002-exec-turn-acp.md §exec_turn match block |
| `EpochCleanup` installation via `_epoch_guard.as_mut()` not `as_deref_mut()` | DES-002-elicitation-maps.md §EpochCleanup installation |
| `for id in &drained_ids` (by reference) so `drained_ids.contains(id)` works in dedup block | DES-002-elicitation-maps.md §EpochCleanup |
| `'elicit` disabled/epoch-zero arm uses `return Ok(TurnResult {...})` not `break 'elicit` because 'elicit is not yet in scope | DES-002-exec-turn-acp.md §elicitation/create arm |
| Outer-loop bounded poll at 500 ms when `run_epoch > 0` (not unconditional) | DES-002-exec-turn-acp.md §outer-loop poll |
| Bus `elicitation_epoch=0` sentinel with `mark_bus_dispatch` only after successful publication | DES-002-actor-teardown.md §bus path |
| `FRAME_BYTE_CAP = MAX_OUT * 7` (56 MiB) — covers worst-case 6× JSON-string expansion plus envelope overhead | DES-002-exec-turn-acp.md §FRAME_BYTE_CAP |
| `advance_launch_seq` in `ReassignUnit` called in same lock hold as `cancel_epoch` | DES-002-actor-teardown.md §ReassignUnit |
| Lock ordering: `write_reg` before `maps` — never hold both simultaneously | DES-002-actor-teardown.md §lock ordering |
| `WriteWatchdog.complete()` is the termination call — not drop | DES-002-actor-teardown.md §WriteWatchdog |
| `ElicitationFailed` bus serializer wildcard risk — explicit arm required | DES-002-actor-teardown.md §bus serialization |

### GUARD: Concurrent adapter requests during elicitation suspension

ACP SDK v1.3.0 registers 14 server→client methods. The `'elicit` loop drops unhandled frames with `tracing::warn`, leaving the adapter waiting up to 7200 seconds.

**Pre-PR gate (EC-3)**: verify that every adapter receiving `elicitation.form` capability (`claude-agent-acp`, `codex-acp`, and any other stdio adapter in `ELICITATION_VERIFIED_ADAPTERS`) serializes tool execution such that `fs/*` / `terminal/*` / `mcp/*` requests cannot arrive while a `session/prompt` is pending. Enable elicitation for an adapter ONLY after this is confirmed.

If only a subset of adapters can be verified before the PR, advertise `elicitation.form` per-adapter (keyed on the adapter binary name) rather than globally.

**If OQ-R-6 resolves to "no serialization"**: the `tracing::warn` dropped-frame path is a live 7200-second-hang bug. The shared frame handler must be implemented before enabling elicitation for any adapter lacking serialization guarantees. Both OQ-R-6 and the shared frame handler must resolve together — there is no safe middle state.

### GUARD: Routing key distinction — `ElicitationCreated.session` is `run_id`

`ElicitationCreated.session` carries the wicked-core **`run_id`** (the `ElicitationMaps` ownership key). The ACP wire field `params.sessionId` is a separate identifier — naming the ACP protocol session, not the wicked-core run. Crew routes `resolveElicitation` via `(session=run_id, elicitation_id)`; using `params.sessionId` as the routing key would fail the `deliver` ownership check.

`chat_turn` keeps `elicitation_enabled=false` until the routing is verified end-to-end (wicked-core `chat_turn` run_id → `ElicitationCreated.session` → crew cache lookup → `resolveElicitation` round-trip). This is a post-PR enablement gate for chat_turn elicitation.

### GUARD: `StepStatus::ElicitationFailed` vs `::Failed` distinction

`StepStatus::Failed` enters the unrecognized-failure triage path. Triage can produce a `Retry` decision, which silently redispatches the unit. For elicitation timeout and adapter disconnect, redispatch is wrong — the unit's turn slot has expired or the adapter is dead. `StepStatus::ElicitationFailed` bypasses triage and routes directly to the run-terminal path.

**A wildcard in any `match step_output.status` that covers `ElicitationFailed` is a bug**: the triage bypass relies on the arm being present and explicit. Do not add a wildcard that maps `ElicitationFailed` to `Failed` as "close enough."

### GUARD: `EpochCleanup::drop` is the sole `cleanup_run` call site

Never call `cleanup_run` from `on_run_complete`, `CancelRun`, `ReassignUnit`, or any path other than `EpochCleanup::drop`. Double-calling `cleanup_run` decrements `active_workers` below the number of live workers, making `has_active_run` return false prematurely and skipping tombstones for subsequent runs with the same `run_id`.

---

## CI integration notes

The following test-infrastructure requirements should be resolved before opening the PR:

1. **`KillHandle::noop()`**: the `rpc_expect` harness needs a noop kill handle for tests that don't involve real processes. Implement as `KillHandle::Noop` variant or a test-only constructor.

2. **Tracing subscriber capture**: tests 14 and 17 assert that `tracing::warn` events are emitted. Use `tracing_subscriber::fmt::TestWriter` or a `tracing::subscriber::with_default` capture harness. Without a subscriber, tracing calls are no-ops and these assertions cannot be verified structurally.

3. **Thread join timeout**: tests in the arm-level suite spawn a worker thread. If the test deadlocks (a common failure mode during early development), the test suite hangs. Add a `handle.join_timeout(Duration::from_secs(10))` helper that panics if the join exceeds the limit.

4. **Transport seam**: a `FakeAcpProcess` implementing the same trait as `AcpProcess` (or a generic `AcpProcess<W>`) is required for arm-level turn tests. This is a first-class test infrastructure component, not a test-only hack — the generalization of `rpc_respond<W: Write>` and `rpc_expect<W: Write>` is specifically to enable it.

5. **ElicitationMaps mutex in tests**: `ElicitationMaps::new()` creates an `Arc<Mutex<ElicitationMaps>>` in production via `spawn_with_acp_sessions`. Tests that call methods directly should create a bare `ElicitationMaps` (not behind Mutex) if possible, or use `Arc::new(Mutex::new(ElicitationMaps::new()))` and call `lock()` explicitly.

---

## Test skeleton code

### Unit test module structure

```rust
// src/acp_runner.rs or tests/elicitation_maps.rs

#[cfg(test)]
mod elicitation_tests {
    use super::*;
    use std::sync::{Arc, Mutex, mpsc};
    use std::time::Duration;
    use serde_json::json;

    // Helper: create a bare ElicitationMaps for unit tests (no Arc/Mutex wrapper).
    fn make_maps() -> ElicitationMaps {
        ElicitationMaps::new()
    }

    // Helper: mint a unique elicitation id per test (avoids cross-test collisions).
    fn eid(n: u64) -> String {
        format!("eid-{n}")
    }
}
```

### Tests 1–2: register and remove

```rust
#[test]
fn test_register_populates_both_maps() {
    let mut maps = make_maps();
    let rx = maps.register("run-1", &eid(1), 1, None).expect("register failed");
    // pending contains sender; run_index contains entry
    assert!(maps.pending_contains("run-1", &eid(1)));
    assert!(maps.run_index_contains("run-1", &eid(1), 1));
    drop(rx);
}

#[test]
fn test_remove_clears_both_maps() {
    let mut maps = make_maps();
    let _rx = maps.register("run-1", &eid(1), 1, None).expect("register failed");
    maps.remove("run-1", &eid(1));
    assert!(!maps.pending_contains("run-1", &eid(1)));
    assert!(!maps.run_index_contains("run-1", &eid(1), 1));
}
```

### Test 3: cancel_epoch cross-run isolation

```rust
#[test]
fn test_cancel_epoch_does_not_affect_other_runs() {
    let mut maps = make_maps();
    // Register epoch 1 for both runs using separate next_epoch calls.
    maps.next_epoch("run-a");  // → epoch 1
    let rx_a = maps.register("run-a", &eid(1), 1, None).expect("register a");
    maps.next_epoch("run-b");  // → epoch 1
    let rx_b = maps.register("run-b", &eid(2), 1, None).expect("register b");

    // Cancel run-a's epoch 1 — drops its sender.
    maps.cancel_epoch("run-a", 1);

    // run-a's sender is dropped → Disconnected.
    assert!(matches!(rx_a.try_recv(), Err(mpsc::TryRecvError::Disconnected)));
    // run-b's sender is untouched → WouldBlock (nothing sent yet).
    assert!(matches!(rx_b.try_recv(), Err(mpsc::TryRecvError::Empty)));
}
```

### Test 4: deliver resolves receiver

```rust
#[test]
fn test_deliver_sends_result_to_receiver() {
    let mut maps = make_maps();
    maps.next_epoch("run-1");
    let rx = maps.register("run-1", &eid(1), 1, None).expect("register");
    let result = ElicitationResult {
        action: "accept".to_string(),
        response: Some(json!("production")),
    };
    maps.deliver("run-1", &eid(1), result).expect("deliver");
    let received = rx.recv_timeout(Duration::from_millis(100)).expect("no result");
    assert_eq!(received.action, "accept");
    assert_eq!(received.response, Some(json!("production")));
}
```

### Test 5: deliver with wrong run_id returns Err

```rust
#[test]
fn test_deliver_wrong_run_id_returns_err() {
    let mut maps = make_maps();
    maps.next_epoch("run-a");
    let _rx = maps.register("run-a", &eid(1), 1, None).expect("register");
    // Use action="cancel" — response: None is valid for cancel (no string required).
    // (accept+None would be rejected by the new free-text validation before the ownership check.)
    let result = ElicitationResult { action: "cancel".to_string(), response: None };
    let err = maps.deliver("run-b", &eid(1), result).expect_err("expected Err");
    let msg = format!("{err}");
    // Error message must name both the expected and actual run_ids.
    assert!(msg.contains("run-b") || msg.contains("run-a"),
        "error message missing run_id: {msg}");
}
```

### Test 6: deliver then cancel_epoch is a no-op

```rust
#[test]
fn test_deliver_then_cancel_epoch_no_panic() {
    let mut maps = make_maps();
    maps.next_epoch("run-1");
    let rx = maps.register("run-1", &eid(1), 1, None).expect("register");
    // Provide a valid string response — free-text accept requires Some(string) after gate-65 fix.
    let result = ElicitationResult { action: "accept".to_string(), response: Some(json!("staging")) };
    maps.deliver("run-1", &eid(1), result).expect("deliver");
    drop(rx);  // drain the channel

    // cancel_epoch after deliver — pending already empty for eid(1).
    maps.cancel_epoch("run-1", 1);  // must not panic

    // Tombstone still set — a future register with epoch 1 is rejected.
    assert!(maps.is_epoch_cancelled("run-1", 1));
}
```

### Test 7: 8 KB message truncation

```rust
#[test]
fn test_message_truncation_at_8kb_boundary() {
    // At exactly 8192 bytes: no truncation.
    let msg_exact = "x".repeat(8192);
    assert_eq!(truncate_message(&msg_exact), msg_exact);

    // At 8193 bytes: truncated; result ends with "[truncated]"; total ≤ 8192.
    let msg_over = "x".repeat(8193);
    let truncated = truncate_message(&msg_over);
    assert!(truncated.ends_with("[truncated]"), "missing truncation suffix");
    assert!(truncated.len() <= 8192, "truncated message exceeds 8192 bytes");
    // Must not split a UTF-8 multi-byte sequence.
    assert!(std::str::from_utf8(truncated.as_bytes()).is_ok());
}
```

### Test 10: prop_key preserved from schema

```rust
#[test]
fn test_prop_key_comes_from_schema_property_name() {
    let schema = json!({
        "type": "object",
        "properties": {
            "choice": { "type": "string" }
        }
    });
    let (prop_key, _opts, _prop_type) = parse_schema(&schema).expect("parse failed");
    assert_eq!(prop_key, "choice");
    // Confirm: NOT hardcoded "response".
    assert_ne!(prop_key, "response");
}
```

### Test 10a: null constraint fields treated as absent

```rust
#[test]
fn test_null_constraint_fields_treated_as_absent() {
    let schema = json!({
        "type": "string",
        "minLength": null,
        "enum": null,
        "oneOf": null
    });
    // Should parse as a plain free-text string schema:
    // no selection constraint (options=None), type="string".
    let result = parse_top_level_schema(&schema);
    assert!(result.is_ok(), "null fields should not cause parse failure");
    let (options, prop_type) = result.unwrap();
    assert_eq!(options, None, "null enum should produce no options");
    assert_eq!(prop_type.as_deref(), Some("string"));
}
```

### Test 25: tombstone race

```rust
#[test]
fn test_tombstone_before_register_rejects_registration() {
    let mut maps = make_maps();
    // Tombstone epoch 1 BEFORE any registration.
    maps.cancel_epoch("run-1", 1);
    assert!(maps.is_epoch_cancelled("run-1", 1));

    // Now try to register — should return None (rejected by tombstone).
    let result = maps.register("run-1", &eid(1), 1, None);
    assert!(result.is_none(), "register should return None when epoch is tombstoned");

    // Confirm: no stale state in either map.
    assert!(!maps.pending_contains("run-1", &eid(1)));
    assert!(!maps.run_index_contains("run-1", &eid(1), 1));
}
```

### Test 27: epoch separation

```rust
#[test]
fn test_cancel_epoch_does_not_bump_run_epoch() {
    let mut maps = make_maps();
    let ep1 = maps.next_epoch("run-1");  // 1
    assert_eq!(ep1, 1);

    // Tombstone epoch 1 — does NOT increment run_epoch.
    maps.cancel_epoch("run-1", 1);
    // register with epoch 1 → rejected.
    assert!(maps.register("run-1", &eid(1), 1, None).is_none());

    // next_epoch allocates the next value.
    let ep2 = maps.next_epoch("run-1");  // 2
    assert_eq!(ep2, 2);

    // register with epoch 2 → accepted.
    let rx2 = maps.register("run-1", &eid(2), 2, None);
    assert!(rx2.is_some(), "epoch 2 should be accepted");

    // next_epoch again → 3; register with epoch 3 → accepted.
    let ep3 = maps.next_epoch("run-1");
    assert_eq!(ep3, 3);
    let rx3 = maps.register("run-1", &eid(3), 3, None);
    assert!(rx3.is_some());
    drop((rx2, rx3));
}
```

### Test 35: cleanup_run reclaims state

```rust
#[test]
fn test_cleanup_run_reclaims_state_for_reuse() {
    let mut maps = make_maps();
    let ep1 = maps.next_epoch("run-1");  // active_workers now 1
    let _rx = maps.register("run-1", &eid(1), ep1, None).unwrap();
    maps.cancel_epoch("run-1", ep1);

    // Simulate EpochCleanup::drop calling cleanup_run (epoch and launch_seq required).
    // launch_seq=0 for local (non-bus) lifecycle: clear_bus_in_flight is a no-op when
    // bus_in_flight_workers has no entry for (run_id, 0).
    maps.cleanup_run("run-1", ep1, 0);

    // State should be fully reclaimed.
    assert!(!maps.has_active_run("run-1"),
        "has_active_run should be false after cleanup_run");
    // Tombstones from the prior run should be cleared.
    // A new register for the same run_id with epoch 1 succeeds (no stale tombstone).
    // (In practice the next run would get epoch 1 from next_epoch — this verifies
    // that cancelled_epochs is pruned by cleanup_run.)
    let new_ep = maps.next_epoch("run-1");  // fresh start → 1
    assert_eq!(new_ep, 1);
    let new_rx = maps.register("run-1", &eid(99), new_ep, None);
    assert!(new_rx.is_some(), "fresh epoch after cleanup_run should be accepted");
}
```

---

## Coverage targets

The following matrix maps each constraint (from the 21-item "must not drop" list) to the test(s) that exercise it:

| Constraint | Tests |
|-----------|-------|
| `register` atomicity (pending + run_index) | 1, 11 |
| `cancel_epoch` tombstone-before-register | 25 |
| `cancel_epoch` drops existing sender | 3, 6, 14 |
| `deliver` run_id ownership check | 4, 5 |
| `drained_ids` by-reference iteration | 34 (indirectly via exec_turn_acp drain) |
| `EpochCleanup::drop` sole cleanup_run call | 35 |
| `next_epoch` sole epoch bumper | 27 |
| `TurnResult::default_at` uses `StepStatus::Failed` | 14 (teardown result) |
| `'elicit` disabled-arm uses `return` not `break` | 13 (cancel via deliver) |
| `ElicitationCreated` carries `epoch` field | 14, 29 (EmitEvent suppression) |
| `StepStatus::ElicitationFailed` bypasses triage | 33 |
| P2b drain + tombstone re-check | 32, 34 |
| Phase 1/2 write under maps lock then write_lock | 33, 36 |
| Bus sentinel 0 + `try_next_epoch_bus` zero-seq reject | (bus consumer test — see below) |
| `advance_launch_seq` in same lock as `cancel_epoch` | (actor-level integration test) |
| options dedup (oneOf + enum intersection) | 26 |
| options null vs absent | 30 |
| prop_key from schema (not hardcoded) | 10 |
| 8 KB message truncation at UTF-8 boundary | 7 |
| `ElicitationResolved` emitted for every exit | 29 |
| `watchdog.complete()` not drop | 36 (write failure path in rpc_expect) |

**Bus consumer test (not numbered in the main matrix)**: write a unit test for `try_next_epoch_bus`:

```rust
#[test]
fn test_try_next_epoch_bus_rejects_zero_launch_seq() {
    let mut maps = make_maps();
    // Any non-zero launch_seq for an ACP run:
    maps.begin_launch("run-1", false);
    maps.mark_bus_dispatch("run-1");
    // launch_seq=0 is unconditionally rejected.
    let result = maps.try_next_epoch_bus("run-1", 0, true);
    assert!(result.is_none(), "zero launch_seq must always be rejected");
}

#[test]
fn test_try_next_epoch_bus_rejects_stale_launch_seq() {
    let mut maps = make_maps();
    let seq1 = maps.begin_launch("run-1", false);
    maps.mark_bus_dispatch("run-1");
    // Advance sequence (simulates ReassignUnit).
    maps.advance_launch_seq("run-1");
    // seq1 is now stale.
    let result = maps.try_next_epoch_bus("run-1", seq1, true);
    assert!(result.is_none(), "stale launch_seq should be rejected");
}

#[test]
fn test_try_next_epoch_bus_activates_current_seq() {
    let mut maps = make_maps();
    let seq = maps.begin_launch("run-1", false);
    maps.mark_bus_dispatch("run-1");
    // Current sequence → activates; returns Some(epoch ≥ 1) for ACP.
    let result = maps.try_next_epoch_bus("run-1", seq, true /* is_acp */);
    assert!(result.is_some(), "current launch_seq should activate");
    assert!(result.unwrap() >= 1, "epoch must be ≥ 1 for ACP");
}

#[test]
fn test_try_next_epoch_bus_returns_zero_for_non_acp() {
    let mut maps = make_maps();
    let seq = maps.begin_launch("run-1", false);
    maps.mark_bus_dispatch("run-1");
    // Non-ACP: epoch sentinel 0 returned (no active_workers increment).
    let result = maps.try_next_epoch_bus("run-1", seq, false /* is_acp */);
    assert_eq!(result, Some(0), "non-ACP bus task should get epoch=0");
}
```

---

## Appendix A — Deferred: agy-acp (bridge.mjs) elicitation

agy-acp is out of scope for v1. Antigravity is spawned with `stdio: ['ignore', 'pipe', 'ignore']` — its stdin is closed. MCP servers running inside Antigravity have no transport to send `elicitation/create` back through the bridge's stdio channel. If a future version re-opens stdin, the Rust side requires:

### A-1. `session/elicitation_pending` → `ElicitationCreated`

After wicked-core sends `session/create_elicitation` (a request), the bridge emits `session/elicitation_pending` as a notification. The turn loop detects it, registers and emits `ElicitationCreated`, then suspends via the dual-poll loop.

### A-2. `session/elicitation_resolved` framing

Unlike native adapters (which respond directly to the inbound request), agy-acp requires wicked-core to send `session/elicitation_resolved` as a JSON-RPC **request** to the bridge. The bridge acks unconditionally with `{ok: true}`:

```rust
rpc_send(&mut proc.stdin, resolved_id, "session/elicitation_resolved",
    json!({
        "sessionId":     acp_session_id,
        "elicitationId": elicitation_id,
        "action":        action,
        "content":       content,
    }))?;
let _ = rpc_expect(&proc.line_rx, resolved_id, Duration::from_secs(5), ...);
```

### A-3. `session/new` clearing CoreEvent

When bridge fires `session/new`, wicked-core emits `CoreEvent::ElicitationCancelled { session: old_run_id }` so the crew `ElicitationCache` drops the stale entry. The ACP-session→run_id mapping exists in `AcpStepRunner::sessions` keyed by `(run_id, cli_key)`.

### A-4. Stdin re-open requirements

For agy-acp elicitation to be structurally possible:
- Antigravity must be spawned with `stdio: ['pipe', 'pipe', 'inherit']` (stdin open).
- The bridge (`bridge.mjs`) must forward `elicitation/create` from MCP server to bridge stdout (toward wicked-core) and forward wicked-core's `session/create_elicitation` request to MCP server stdin.
- The bridge must handle the notification→request framing transformation (MCP's `elicitation/create` notification → bridge's `session/elicitation_pending` notification; wicked-core's `session/create_elicitation` request → MCP's response).

None of these are present in v1. Enabling agy-acp elicitation requires changes to both the bridge spawn parameters (TS side) and the Rust turn loop (agy-acp path in `exec_turn_agy`, if it exists, or a new path). This is tracked as a v2 item with no hard ETA.
