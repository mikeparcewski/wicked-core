# Purpose

Actor integration — `actor::run` signature, epoch allocation in `dispatch_unit`, EmitEvent suppression, `ResolveElicitation` handler, `shared_run_terminal`, `CancelRun`, `ReassignUnit`, Shutdown drain, `initialize` capability, and the handshake-phase elicitation guard.

Lock-poison recovery: `unwrap_or_else(|p| p.into_inner())` at every `Mutex::lock()` call — documented in DES-002-overview.md, applied uniformly throughout.

---

## `actor::run` signature (src/actor.rs)

```rust
pub(crate) fn run(
    store:            &mut Store,
    rx:               Receiver<Command>,
    // ... existing params ...
    is_acp:           bool,                                  // ← new
    elicitation_maps: Option<Arc<Mutex<ElicitationMaps>>>,  // ← new
    write_reg:        WriteReg,  // ← new: registry for CancelRun/Shutdown sweeps
                                 // WriteReg = Arc<Mutex<HashMap<...>>> — do NOT double-wrap
) { ... }
```

`write_reg` is the write-lock session registry. `WriteReg` is already `Arc<Mutex<HashMap<...>>>` — passing `Arc<Mutex<WriteReg>>` would double-wrap and not compile with the lock sites that call `.lock()` once to yield the map. Use the type alias directly.

It is accessed by `shared_run_terminal` (CancelRun sweep), `ReassignUnit` (second sweep), and `Command::Shutdown` (full drain). PTY and inner sessions have no ACP children, so their actors receive an empty registry — the sweep finds nothing, which is correct.

`is_acp` is passed explicitly rather than derived from map presence because PTY runners also receive `Some(lifecycle_arc)` for bus sequencing — deriving `is_acp` from map presence would incorrectly mark PTY tasks as ACP and increment `active_workers`.

**Three call sites in `src/lib.rs`**:

```rust
// spawn_with_acp_sessions:
let elicitation_maps = Arc::new(Mutex::new(ElicitationMaps::new()));
let write_reg: WriteReg = Arc::new(Mutex::new(HashMap::new()));
let runner = AcpStepRunner::new_with_maps(tx.clone(), elicitation_maps.clone(), Arc::clone(&write_reg));
let actor_maps = elicitation_maps.clone();
debug_assert!(Arc::ptr_eq(runner.elicitation_maps(), &actor_maps),
    "BUG: runner and actor hold different ElicitationMaps Arcs");
thread::spawn(move || actor::run(&mut store, rx, ..., is_acp: true,
    Some(actor_maps.clone()), Arc::clone(&write_reg)));

// spawn_with_pty_sessions — ALWAYS Some(lifecycle_arc.clone()):
// PTY actors need begin_launch and tombstone_bus_run even when the exec bus is off.
// write_reg is empty — PTY sessions have no ACP children to signal on CancelRun/Shutdown.
thread::spawn(move || actor::run(&mut store, rx, ..., is_acp: false,
    Some(lifecycle_arc.clone()), Arc::new(Mutex::new(HashMap::new()))));

// spawn_inner — ALWAYS Some(lifecycle_arc.clone()):
// Backs Core::spawn_with_engine and Core::spawn_with_engine_exec.
// write_reg is empty — no ACP children registered for injected runners.
thread::spawn(move || actor::run(&mut store, rx, ..., is_acp: false,
    Some(lifecycle_arc.clone()), Arc::new(Mutex::new(HashMap::new()))));
```

The lifecycle Arc is **unconditional** for all real (non-unit-test) actor paths. ACP delivery capability (`actor_maps`) is separate — `Some` only for ACP runners.

### actor_maps vs elicitation_maps distinction

Inside `actor::run`, two distinct references to `ElicitationMaps` are held:

- **`elicitation_maps`** — present for ALL spawn paths (even non-ACP); used for `begin_launch`, `advance_launch_seq`, `tombstone_bus_run`, and `cleanup_run`. This is the unconditional lifecycle-sequencing arc.
- **`actor_maps: Option<Arc<...>>`** — `Some` for ACP runners only; used for `deliver` (via `ResolveElicitation`), `EmitEvent` suppression, `cancel_epoch` from `shared_run_terminal`, and `has_active_run` checks.

For ACP runners, `elicitation_maps` and `actor_maps.unwrap()` are the same Arc (`Arc::ptr_eq`). The split is a type-level guarantee: code that calls `deliver` or epoch-tombstone operations must go through `actor_maps` (always guarded by `is_some()`), while lifecycle operations go through `elicitation_maps` (always available).

---

## `dispatch_unit` — epoch allocation (src/actor.rs)

```rust
fn dispatch_unit(
    run_id:           &str,
    unit:             &Unit,
    /* ... existing params ... */
    elicitation_maps: Arc<Mutex<ElicitationMaps>>,   // unconditional lifecycle arc (ALL runners)
    actor_maps:       Option<Arc<Mutex<ElicitationMaps>>>, // Some(same Arc) for ACP, None otherwise
    process_gen:      uuid::Uuid,
    is_acp:           bool,   // explicit bool — NOT derived from actor_maps.is_some()
) -> anyhow::Result<()>
```

`process_gen` is a `uuid::Uuid` generated once per actor invocation — at the top of each real spawn path (e.g., in `spawn_with_acp_sessions`, `spawn_with_pty_sessions`, `spawn_inner`) or at the start of `actor::run` itself — and threaded through `actor::run` and `dispatch_unit` as a bare `uuid::Uuid`. It is NOT a global singleton: each actor lifetime gets its own token so that multiple sequential or concurrent actors in the same process do not share or collide on the same value. It is wrapped into `Some(process_gen)` when stored in `DispatchedTask` and `StepInput` (both use `Option<Uuid>`; `None` means legacy payload). Requires `uuid = { version = "1", features = ["v4", "serde"] }` in `Cargo.toml`.

**`is_acp` must not be derived from `actor_maps.is_some()`**: under `WICKED_BUS_EXEC`, PTY/non-ACP actors receive `Some(bus_maps)` in `actor_maps` for bus sequencing, making `actor_maps.is_some()` true for them too.

### `begin_launch` — called for ALL dispatch paths

```rust
// Called for bus and local, before the is_exec_enabled() branch.
// Always pass is_bus_dispatch=false; the bus marker is set after successful publication.
let launch_seq = {
    let mut maps = elicitation_maps.lock().unwrap_or_else(|e| e.into_inner());
    maps.begin_launch(run_id, false)
};
```

`begin_launch` increments the per-run launch sequence counter and returns the new value. The sequence is monotonically increasing. Zero is reserved as a sentinel: `try_next_epoch_bus` unconditionally rejects `launch_seq == 0`. This means a task queued with launch_seq=0 is always stale — the bus consumer discards it without inspecting epoch or process_gen.

### Bus path (WICKED_BUS_EXEC enabled)

```rust
if is_exec_enabled() {
    let dispatched = DispatchedTask {
        /* existing fields */,
        elicitation_epoch: 0,           // sentinel: "consumer will activate"
        launch_seq,
        process_gen: Some(process_gen),
        // is_acp must exclude tool_cmd units. For ACP actors, every unit has is_acp=true
        // at the dispatch_unit call site, but tool_cmd units never install EpochCleanup.
        // If is_acp=true reaches the bus consumer for a tool_cmd, try_next_epoch_bus
        // increments active_workers with no corresponding cleanup_run — the count leaks
        // and blocks future cancel_epoch from reclaiming per-run state.
        is_acp: is_acp && unit.tool_cmd.is_none(),
    };
    if try_publish_dispatched(dispatched).is_ok() {
        let mut maps = elicitation_maps.lock().unwrap_or_else(|e| e.into_inner());
        maps.mark_bus_dispatch(run_id);  // set ONLY after successful publication
        return Ok(());
    }
    // Bus publish failed — fall through to local spawn.
}
```

The bus consumer calls `try_next_epoch_bus(run_id, task.launch_seq, task.is_acp)` (not `next_epoch`) and discards the task if it returns `None`. On discard, the consumer MUST advance and persist the cursor before continuing.

**`elicitation_epoch: 0` sentinel semantics**: the bus consumer is responsible for calling `try_next_epoch_bus`, which allocates the epoch (if `is_acp`) or returns 0 (if not ACP). The local dispatch path uses `next_epoch` directly. The sentinel 0 in the task struct is overwritten in both paths before being placed in `StepInput.elicitation_epoch`. No worker ever executes with an elicitation_epoch that was not freshly allocated at dispatch time.

**Legacy task rule**: a task is legacy if `process_gen == None`. Legacy tasks are discarded by the generation-first check before reaching `try_next_epoch_bus`. A zero `launch_seq` in a current-generation task is malformed and unconditionally rejected. Together these two checks prevent three classes of stale task delivery:
- Cross-restart: `process_gen` mismatch
- Stale-after-reassign: `launch_seq` below current counter (or zero sentinel)
- Cancelled-before-dispatch: `try_next_epoch_bus` returns None when run is tombstoned

### Bus consumer: actor-scoped cursor keys

When multiple Core actors share one `WICKED_BUS_DB`, the durable consumer cursor rows
MUST be keyed by actor identity. A process-wide constant (e.g. `"cli-runner"`) causes all
actors to overwrite the same row: actor A advancing its cursor also advances actor B's
restart floor, potentially dropping B's unfinished work on the next restart.

```rust
// The consumer name incorporates actor_process_gen so each actor owns its own cursor row.
// This is what makes "advance past a foreign task" safe (see below): advancing this
// actor's row does not touch any other actor's row.
let consumer_name = format!("cli-runner-{}", actor_process_gen);
let completed_consumer_name = format!("cli-runner-completed-{}", actor_process_gen);

// advance_and_persist_cursor is a local helper closed over the actor's consumer_name:
let advance_and_persist_cursor = || { bus.save_cursor(&consumer_name, current_position); };
```

**Cursor row lifecycle**: each actor generates a fresh UUID, so cursor rows accumulate (two per actor lifetime) unless explicitly reclaimed. Clean shutdown deletes them; crashes cannot. Two reclamation mechanisms are required:

**(A) Clean shutdown** — `Command::Shutdown` drain may delete cursor rows ONLY after all bus workers have quiesced. PTY and injected runners have empty write registries (intentionally), so the Shutdown sweep cannot wait on them via the write-lock signal. Their in-flight completions can arrive after cursor deletion; a subsequent actor would then start at the tail and skip that completion.

Safe approach: delete cursor rows only when `bus_in_flight_workers` is empty at shutdown time. If any workers are still in-flight, leave the rows for startup reclamation on the next startup.

```rust
// In Command::Shutdown drain (after all queued commands are processed):
let no_workers_in_flight = {
    let maps = elicitation_maps.lock().unwrap_or_else(|p| p.into_inner());
    maps.any_bus_worker_in_flight()  // returns !bus_in_flight_workers.is_empty()
};
if !no_workers_in_flight {
    let _ = bus.delete_cursor(&consumer_name);
    let _ = bus.delete_cursor(&completed_consumer_name);
}
// If workers are in-flight, leave rows. Startup reclamation (mechanism B) will clean
// predecessor rows on the next Core startup.  Rows accumulate at most 2 per actor
// lifetime and are bounded by the number of restarts within one Core instance lifetime.
```

**(B) Startup reclamation (handles crashes)** — on actor startup, look up any predecessor cursor names stored under a stable, per-Core-instance key, delete those rows, then write the new names.

The owner key MUST be scoped to the specific Core instance (not process-wide). When multiple Core actors share one `WICKED_BUS_DB`, a process-wide key would cause actor B to treat actor A's cursor as its predecessor and delete A's live rows. Use a per-Core stable ID (e.g., a UUID stored in the DB the first time this Core instance starts, separate from `actor_process_gen`):

```rust
// core_stable_id is a UUID that MUST survive process restarts — otherwise the new process
// generates a different UUID, constructs a different owner_key, and cannot find the
// predecessor cursor rows.  It must be looked up from a FIXED, caller-supplied stable key
// in the bus DB (NOT derived from the UUID itself, which would be circular).
//
// `workspace_id` (or equivalent) is a deterministic, caller-provided string that uniquely
// identifies this Core instance across restarts (e.g. a workspace path hash or a project ID
// supplied at Core::new() time, NOT a process-scoped UUID).
const CORE_STABLE_ID_KEY_PREFIX: &str = "wicked-core-instance-stable-id-";
let stable_key = format!("{}{}", CORE_STABLE_ID_KEY_PREFIX, workspace_id);
let core_stable_id: String = match bus.get_stable(&stable_key) {
    Some(existing) => existing,                    // reuse across restarts
    None => {
        let new_id = uuid::Uuid::new_v4().to_string();
        bus.set_stable(&stable_key, &new_id);
        new_id
    }
};
let owner_key = format!("cli-runner-cursor-owner-{}", core_stable_id);

// Capture the OLD owner BEFORE overwriting — get_stable returns the value stored on the
// PREVIOUS startup.  set_stable must run AFTER this read; inverting the order makes
// get_stable return the NEW consumer_name and the filter always produces None, meaning
// predecessor_gen is always None and predecessor tasks are never terminalized.
//
// get_stable now returns Result<Option<String>>; propagate the error (fail closed —
// a missing predecessor is safer than treating an I/O error as "no predecessor").
let old_consumer_opt: Option<String> = bus.get_stable(&owner_key)?
    .filter(|c| c != &consumer_name);

// Derive predecessor_gen BEFORE any migration so the value is available even after
// old_consumer_opt is consumed below.
let predecessor_gen: Option<uuid::Uuid> = old_consumer_opt.as_deref()
    .and_then(|c| c.strip_prefix("cli-runner-"))
    .and_then(|u| uuid::Uuid::parse_str(u).ok());

if let Some(ref old_consumer) = old_consumer_opt {
    // Reconstruct the exact completed-consumer name from old_consumer.
    // consumer_name = format!("cli-runner-{}", actor_process_gen)
    // completed_consumer_name = format!("cli-runner-completed-{}", actor_process_gen)
    // So if old_consumer = "cli-runner-<old_uuid>",
    // the completed name is "cli-runner-completed-<old_uuid>" (NOT "cli-runner-<old_uuid>-completed").
    let old_uuid = old_consumer.strip_prefix("cli-runner-").unwrap_or("");
    let old_completed = format!("cli-runner-completed-{}", old_uuid);

    // IMPORTANT: migrate cursor positions BEFORE deleting predecessor rows.
    // Deleting without migrating causes the new consumer to start at the current tail,
    // skipping any pending dispatch/completion events from the predecessor. Runs that
    // were in-flight at the old position remain stuck in Executing permanently.
    // Copy the predecessor's positions to the new consumer names so the new consumer
    // resumes from where the predecessor stopped. Tasks with the predecessor's process_gen
    // are terminalized by the execute_dispatched_task foreign-task branch (see below) —
    // the predecessor is dead so no other consumer will ever apply a result for them.
    if let Some(old_pos) = bus.read_cursor(old_consumer) {
        bus.save_cursor(&consumer_name, old_pos)?;
    }
    if let Some(old_pos) = bus.read_cursor(&old_completed) {
        bus.save_cursor(&completed_consumer_name, old_pos)?;
    }
    bus.delete_cursor(old_consumer)?;
    bus.delete_cursor(&old_completed)?;
}
// Write the new owner AFTER migration — this is the correct ordering.
bus.set_stable(&owner_key, &consumer_name)?;
```

Without (B), process crashes leave rows permanently accumulating because the new actor cannot identify predecessor names from its own UUID alone.

### Bus consumer: `try_next_epoch_bus` call site

```rust
// In the bus consumer (cli_runner.rs or equivalent):
// actor_process_gen: the Uuid generated once in Core::new() for this actor lifetime.
// Threaded from actor::run into the bus consumer closure — NOT read from the global
// PROCESS_GEN singleton, which is unsafe when multiple Core actors exist sequentially
// or concurrently (a singleton set by the first actor rejects all later actors).
fn execute_dispatched_task(
    task:              DispatchedTask,
    maps_arc:          &Arc<Mutex<ElicitationMaps>>,
    actor_process_gen: uuid::Uuid,
    actor_tx:          &mpsc::Sender<Command>,  // for terminal failure emission (degraded + predecessor paths)
    predecessor_gen:   Option<uuid::Uuid>,       // Some if we migrated from a crashed predecessor at startup
) {
    // Generation check (first):
    let Some(task_gen) = task.process_gen else {
        tracing::warn!("legacy task (no process_gen); discarding");
        advance_and_persist_cursor();
        return;
    };
    if task_gen != actor_process_gen {
        if Some(task_gen) == predecessor_gen {
            // Predecessor task from migrated cursor: the predecessor actor is dead, so no
            // consumer will ever apply a result for this task.  Persist a non-retriable terminal
            // result BEFORE advancing the cursor; otherwise the run stays permanently Executing.
            //
            // Stale-result guard requirement: the actor's ApplyStepResult handler checks
            // both process_gen (against the unit's stored gen) and launch_seq (against
            // ElicitationMaps.current_launch_seq).  After a crash, ElicitationMaps is
            // re-created empty, so current_launch_seq returns 0.  For predecessor tasks
            // with launch_seq > 0 the guard would reject the result as stale.
            //
            // Fix: before processing predecessor tasks, restore run_launch_seq from the
            // session DB for all Executing runs.  At startup, the actor must query all
            // sessions in Executing state, read their stored launch_seq (persisted to the
            // session row at dispatch time alongside process_gen), and call
            //   maps.lock().restore_launch_seq(run_id, stored_launch_seq)
            // so current_launch_seq(run_id) returns the correct value.
            // `restore_launch_seq` is a new ElicitationMaps method that inserts into
            // run_launch_seq WITHOUT incrementing (contrast with begin_launch which increments).
            //
            // Use task.process_gen (predecessor's gen) for ApplyStepResult — the actor-side guard
            // checks that process_gen matches what was stored at dispatch time, which it does
            // (the task was dispatched under the predecessor's gen).
            tracing::warn!(task_gen = %task_gen, run_id = %task.run_id, launch_seq = task.launch_seq,
                "predecessor task from migrated cursor; predecessor dead; failing non-retriably to unblock session");
            let failed_output = StepOutput {
                status:   StepStatus::ElicitationFailed,
                output:   String::new(),
                run_id:   task.run_id.clone(),
                unit_ix:  task.unit_ix,
                attempt:  task.attempt,
                usage:    None,
                files:    Vec::new(),
                governed: false,
            };
            // BEFORE emitting a synthetic terminal result, check the completed consumer stream
            // for a matching completion event.  This handles a specific crash scenario:
            //   1. Predecessor executed the task and called confirm_task_completed (success).
            //   2. Predecessor crashed BEFORE advancing its dispatch cursor.
            //   3. Startup migrated both the dispatch cursor (pointing to this task) AND
            //      the completed cursor (pointing to the matching CompletedTask event).
            //
            // In this scenario, emitting a synthetic ElicitationFailed would race the real
            // completion — whichever arrives first wins.  If the synthetic failure wins, the
            // run ends as Failed even though it actually succeeded.
            //
            // peek_next_completed reads the next event in the completed stream WITHOUT
            // advancing the completed cursor.  If it matches this task's run_id + launch_seq,
            // process the real result instead of emitting a synthetic failure.
            // find_completed scans the completed stream from the current cursor position,
            // skipping unrelated events without advancing the cursor, and returns the first
            // matching CompletedTask for (run_id, launch_seq).  This handles interleaving:
            // other runs' completion events may precede this task's completion in the stream.
            // peek_next_completed (checking only the very next event) would return None
            // whenever any unrelated event is interleaved, incorrectly terminalizing a
            // task whose completion is present but not at the head.
            if let Ok(Some(completion)) = bus.find_completed(
                &completed_consumer_name, task.run_id.as_str(), task.launch_seq
            ) {
                // Use std::sync::mpsc::sync_channel(0) (rendezvous) — no Tokio/oneshot dependency.
// Bound 0: send blocks until recv is called, but does NOT guarantee persistence;
// the ack_rx.recv().is_ok() check (below) catches actor death between dequeue and commit.
let (ack_tx, ack_rx) = std::sync::mpsc::sync_channel::<()>(0);
                if actor_tx.send(Command::ApplyStepResult {
                    run_id:       task.run_id.clone(),
                    process_gen:  task.process_gen,
                    launch_seq:   task.launch_seq,
                    output:       completion.into_step_output(task.unit_ix, task.attempt, task.run_id.clone()),
                    agent_verdict: completion.agent_verdict,
                    ack:          Some(ack_tx),
                }).is_ok() {
                    // Gate both cursor advances on ack — recv() Err means actor died between
                    // dequeue and commit; leave cursors behind for redelivery on restart.
                    if ack_rx.recv().is_ok() {
                        advance_and_persist_cursor();               // advance dispatch cursor
                        advance_completed_cursor(completion.position); // advance completed cursor
                    }
                }
                return;
            }
            // No valid completion found in the completed stream for this task.
            // The predecessor is truly dead with no result — emit a synthetic terminal failure.

            // Use a oneshot ack so the actor signals AFTER it has committed the result to the
            // store.  Without the ack, advancing the cursor immediately after send() would leave
            // a crash window: if the actor dies between dequeue and commit, the dispatch cursor
            // is already past this task and the run stays Executing permanently.
            //
            // The actor sends ack.send(()) immediately after apply_step_result commits.
            // If the channel is closed before send (actor shutting down), is_ok() returns false
            // and we leave the cursor behind so the task is redelivered on the next restart.
            // Use std::sync::mpsc::sync_channel(0) (rendezvous) — no Tokio/oneshot dependency.
// Bound 0: send blocks until recv is called, but does NOT guarantee persistence;
// the ack_rx.recv().is_ok() check (below) catches actor death between dequeue and commit.
let (ack_tx, ack_rx) = std::sync::mpsc::sync_channel::<()>(0);
            if actor_tx.send(Command::ApplyStepResult {
                run_id:       task.run_id.clone(),
                process_gen:  task.process_gen,
                launch_seq:   task.launch_seq,
                output:       failed_output,
                agent_verdict: None,
                ack:          Some(ack_tx),
            }).is_ok() {
                // Gate cursor advance on ack — recv() Err means actor died between dequeue
                // and commit; leaving cursors behind ensures redelivery on next restart.
                if ack_rx.recv().is_ok() {
                    advance_and_persist_cursor();
                }
            }
            return;
        }
        // Truly foreign task from a different live actor (not our migrated predecessor).
        // Advancing this actor's cursor past a foreign event does NOT affect any other actor's
        // cursor row, so the other actor's task is not lost.  NOT advancing leaves the floor
        // below the foreign event; if foreign events fill an entire poll batch, this actor's
        // own later tasks are unreachable, wedging it permanently in degenerate cases.
        tracing::warn!(task_gen = %task_gen, actor_gen = %actor_process_gen,
            "foreign task from different live actor; advancing cursor to unblock this actor");
        advance_and_persist_cursor();
        return;
    }

    // Epoch activation (second):
    // Check has_activated_seq BEFORE try_next_epoch_bus to distinguish two None cases:
    //   a) has_activated_seq=true  → task ran; confirm_task_completed failed; worker gone.
    //      Output is permanently lost. Log critical error, advance cursor (accept data loss).
    //      Workers SHOULD retry confirm_task_completed with exponential backoff to minimise this.
    //   b) has_activated_seq=false → task is stale/cancelled. Silent discard, advance cursor.
    let epoch = {
        let mut maps = maps_arc.lock().unwrap_or_else(|p| p.into_inner());
        if maps.has_activated_seq(task.run_id.as_str(), task.launch_seq) {
            // has_activated_seq=true can mean two distinct situations:
            //   (a) Worker is STILL RUNNING — cursor intentionally unadvanced, normal re-poll.
            //       is_bus_worker_in_flight(run_id, launch_seq) returns true.
            //       Do NOT fail the run; return so the event is re-polled next interval.
            //   (b) Worker FINISHED but confirm_task_completed failed (crash or publish error).
            //       is_bus_worker_in_flight returns false (worker cleared it or never ran again).
            //       Enter degraded path: fail run, advance cursor.
            //
            // NOTE: has_active_run() is NOT used here — it only tracks ACP workers.
            // PTY and injected workers (non-ACP) don't increment active_workers, so
            // has_active_run() would return false even while the worker is still executing.
            // bus_in_flight_workers covers ALL runner types.
            if maps.is_bus_worker_in_flight(task.run_id.as_str(), task.launch_seq) {
                // Normal re-poll: worker still active. Consumer returns without advancing.
                // The event will be re-delivered on the next polling interval.
                return;
            }
            // Degraded mode: task was already executed but confirm_task_completed failed
            // (e.g. worker crashed before publishing). Output is permanently lost.
            // MUST send a terminal non-retriable result before advancing the cursor — without
            // it the session remains Executing indefinitely. Use StepStatus::ElicitationFailed
            // which the actor routes to terminal path, bypassing failure triage and retry.
            // (StepStatus::Failed would trigger FailureTriageReady for attempt-0 runs, which
            //  can return Retry — re-executing a task with already-occurred side effects.)
            tracing::error!(run_id = %task.run_id, launch_seq = task.launch_seq,
                "bus task already activated and worker gone; confirm_task_completed never published; \
                 failing run non-retriably to unblock session (data loss)");
            drop(maps);
            // process_gen + launch_seq allow the actor to reject this if the run was
            // already superseded by a reassignment (stale ApplyStepResult guard).
            // Construct failed StepOutput using task fields (unit_ix, attempt, run_id are
            // existing DispatchedTask fields). All required StepOutput fields must be populated:
            //   run_id, unit_ix, attempt (from task); status, output, usage (synthetic);
            //   files (empty); governed (false — no gate was executed).
            // ApplyStepResult.run_id is an EXISTING field (not added by this design).
            let failed_output = StepOutput {
                status:   StepStatus::ElicitationFailed,  // non-retriable terminal status
                output:   String::new(),
                run_id:   task.run_id.clone(),
                unit_ix:  task.unit_ix,
                attempt:  task.attempt,
                usage:    None,
                files:    Vec::new(),
                governed: false,
            };
            // Use a oneshot ack so the actor signals AFTER it has committed the result to the
            // store.  Without the ack, advancing the cursor immediately after send() leaves a
            // crash window: if the actor dies between dequeue and commit, the dispatch cursor
            // is already past this task and the run stays Executing permanently on the next
            // restart.  Leaving the cursor behind on any failure ensures the task is
            // redelivered, re-entering the degraded path on the next startup.
            // Use std::sync::mpsc::sync_channel(0) (rendezvous) — no Tokio/oneshot dependency.
// Bound 0: send blocks until recv is called, but does NOT guarantee persistence;
// the ack_rx.recv().is_ok() check (below) catches actor death between dequeue and commit.
let (ack_tx, ack_rx) = std::sync::mpsc::sync_channel::<()>(0);
            if actor_tx.send(Command::ApplyStepResult {
                run_id:       task.run_id.clone(),
                process_gen:  task.process_gen,
                launch_seq:   task.launch_seq,
                output:       failed_output,
                agent_verdict: None,
                ack:          Some(ack_tx),
            }).is_ok() {
                // Gate cursor advance on ack — recv() Err means actor died between dequeue
                // and commit; leaving cursors behind ensures redelivery on next restart.
                if ack_rx.recv().is_ok() {
                    advance_and_persist_cursor();
                }
            }
            return;
        }
        match maps.try_next_epoch_bus(task.run_id.as_str(), task.launch_seq, task.is_acp) {
            Some(ep) => ep,
            None => {
                tracing::warn!(run_id = %task.run_id, "task cancelled before bus activation");
                drop(maps);
                advance_and_persist_cursor();
                return;
            }
        }
    };

    let input = StepInput {
        /* ... */,
        elicitation_epoch: epoch,
        process_gen: task.process_gen,
        launch_seq:  task.launch_seq,
    };
    // Spawn worker thread with input.
    // Do NOT advance_and_persist_cursor here (at spawn time). Advancing before the
    // worker publishes task.completed breaks at-least-once delivery: if the worker
    // panics or exits before publishing, the durable cursor has already skipped this
    // event and it cannot be replayed, leaving the session in Executing indefinitely.
    // advance_and_persist_cursor() must be called by the worker (or completion handler)
    // only after confirm_task_completed() returns Ok — preserving the at-least-once
    // invariant. The consumer must tolerate re-delivering the same task after a crash.
    // Same-actor re-delivery idempotency is provided by ElicitationMaps::bus_activated_seqs:
    //   try_next_epoch_bus returns None if (run_id, launch_seq) was already activated,
    //   preventing duplicate CLI execution when confirm_task_completed fails and the
    //   cursor was not yet advanced.  Cross-restart re-delivery is caught by process_gen mismatch.
}
```

`try_next_epoch_bus` unconditionally rejects `launch_seq == 0`. This handles the sentinel case: a bus-dispatched task that somehow carries `launch_seq=0` (malformed; `begin_launch` always returns ≥1) is discarded without touching epoch state.

### Local path

```rust
// Gate epoch allocation on is_acp AND unit kind.
// tool_cmd units are dispatched to run_tool_cmd, never exec_turn — no EpochCleanup installed.
let elicitation_epoch = if is_acp && unit.tool_cmd.is_none() {
    let maps_arc = actor_maps.as_ref()
        .expect("is_acp=true but actor_maps=None; caller bug");
    let mut maps = maps_arc.lock().unwrap_or_else(|e| e.into_inner());
    maps.next_epoch(run_id)  // epoch >= 1; has_active_run() returns true
} else {
    0  // non-ACP runner or tool_cmd unit; no EpochCleanup guard
};
// dispatch_unit receives process_gen as bare uuid::Uuid; StepInput stores it as Option<Uuid>.
// Wrap here — this is the only call site that bridges the bare → Option boundary.
let input = StepInput {
    /* existing fields */,
    elicitation_epoch,
    process_gen: Some(process_gen),  // bare Uuid from dispatch_unit → Option<Uuid> for StepInput
    launch_seq,
};
// Worker spawned with input; EpochCleanup guard decrements active_workers on exit.
```

→ See DES-002-elicitation-maps.md §EpochCleanup for the guard installation code.

**tool_cmd rationale**: `dispatch_unit` is called for both turn-based and command-based units. `tool_cmd` units call `run_tool_cmd`, which does not receive an `EpochCleanup` guard. If `next_epoch` were called for tool_cmd units, `active_workers` would be incremented with no decrement path — `cleanup_run` would never fire. The guard `unit.tool_cmd.is_none()` prevents this.

---

## `Command::EmitEvent` handler — ElicitationCreated suppression

```rust
Command::EmitEvent(ev) => {
    // ── ElicitationCreated suppression ───────────────────────────────────────────
    // Suppress a stale ElicitationCreated when cancel_epoch ran before the actor processed it.
    // Use suppressed_creations — NOT the epoch tombstone.
    //
    // Suppression uses suppressed_creations + creation_announced (both under maps lock).
    // cancel_epoch only inserts a suppression marker if the actor has NOT yet announced
    // the creation (creation_announced is empty for that ID). The actor marks it announced
    // BEFORE releasing the lock, ensuring the check-then-mark is atomic w.r.t. cancel_epoch.
    if let CoreEvent::ElicitationCreated { ref elicitation_id, .. } = ev {
        if let Some(ref maps) = actor_maps {
            let mut maps = maps.lock().unwrap_or_else(|e| e.into_inner());
            // Always call take_suppressed_creation (removes the marker if present).
            let was_suppressed = maps.take_suppressed_creation(elicitation_id);
            // Three suppression conditions:
            // 1. shutdown_flag: actor is shutting down; cancel all pending creations.
            // 2. was_suppressed: cancel_epoch already ran before this event was drained.
            // 3. !is_pending: the worker resolved the elicitation (called remove()) before
            //    the actor processed the queued EmitEvent. Without this check, the actor
            //    fans out ElicitationCreated after ElicitationResolved — violating ordering.
            //    Occurs when session/prompt, deadline, or disconnect resolves elicitation
            //    while ElicitationCreated is still queued in the actor's channel.
            let already_resolved = !maps.is_pending(elicitation_id);
            if maps.shutdown_flag() || was_suppressed || already_resolved {
                // Mark the paired ElicitationResolved for suppression too.
                // - For was_suppressed/shutdown: EpochCleanup::drop will emit the resolved event.
                // - For already_resolved: the resolved event was already fanned out by the
                //   worker. mark_resolution_suppressed is idempotent if no future resolved
                //   arrives, and correctly suppresses one if EpochCleanup also fires.
                maps.mark_resolution_suppressed(elicitation_id);
                tracing::warn!(elicitation_id,
                    "elicitation: suppressing stale ElicitationCreated; \
                     paired resolved will also be suppressed");
                continue; // skip fan-out
            }
            // Not suppressed: mark as announced BEFORE releasing lock.
            // After this, concurrent cancel_epoch sees creation_announced and skips
            // inserting a stale suppression marker.
            maps.mark_creation_announced(elicitation_id);
        }
    }
    // ── ElicitationResolved suppression ──────────────────────────────────────────
    // If the paired ElicitationCreated was suppressed, suppress the resolved event too.
    // Subscribers must not receive a terminal event for an elicitation they never observed.
    if let CoreEvent::ElicitationResolved { ref elicitation_id, .. } = ev {
        if let Some(ref maps) = actor_maps {
            let mut maps = maps.lock().unwrap_or_else(|e| e.into_inner());
            if maps.take_suppressed_resolution(elicitation_id) {
                tracing::warn!(elicitation_id,
                    "elicitation: suppressing ElicitationResolved (paired creation was suppressed)");
                continue; // skip fan-out — no subscriber saw the creation
            }
        }
    }
    // Fan-out OUTSIDE the lock (lock released when `maps` above goes out of scope).
    emit(&mut subscribers, ev);
}
```

**Important**: The creation suppression guard requires two atomic operations under the same lock:
1. `take_suppressed_creation(eid)` — if cancel_epoch already ran, suppress AND call `mark_resolution_suppressed(eid)`.
2. `mark_creation_announced(eid)` — if actor won the race, mark before releasing lock.

When suppression fires (step 1), `mark_resolution_suppressed` is called in the same lock hold so the paired `ElicitationResolved` is also suppressed. This preserves the paired-event contract: subscribers either see both events or neither.

After step 2, any concurrent `cancel_epoch` sees `creation_announced` and skips inserting a stale marker. This prevents the race where `remove()` or `cleanup_run` deleted the marker before the actor processed the queued event. `ElicitationCreated` still carries `epoch: u64` for the `'elicit` drain arm's tombstone check.

### Suppression ordering guarantee

- **actor first**: acquires lock → `take` returns false → `mark_creation_announced` → releases lock → fans out. Concurrent `cancel_epoch`: acquires lock → sees ID in `creation_announced` → skips suppression. ✓
- **cancel_epoch first**: acquires lock → `creation_announced` empty → inserts into `suppressed_creations` → releases lock. Actor: acquires lock → `take` returns true → calls `mark_resolution_suppressed` → suppresses creation (does NOT mark announced). Later `ElicitationResolved` hits `take_suppressed_resolution` → suppressed. ✓
- No ABA race: lock prevents interleaving between the two-step check-then-mark sequence.

---

## `Command::ResolveElicitation` handler

```rust
Command::ResolveElicitation { run_id, elicitation_id, action, response, reply } => {
    let res = match &actor_maps {
        None => Err(anyhow::anyhow!("elicitation not supported for this runner")),
        Some(maps) => {
            let result = ElicitationResult { action, response };
            maps.lock().unwrap_or_else(|e| e.into_inner())
                .deliver(&run_id, &elicitation_id, result)
        }
    };
    let _ = reply.send(res);
}
```

`reply` is a `mpsc::SyncSender<anyhow::Result<()>>` (capacity 1, from `sync_channel(1)`). The NAPI binding blocks on the Rust reply, propagating error to TypeScript as a rejected Promise. If `deliver` fails (run_id mismatch, elicitation not found), the Promise rejects — crew logs the error and the `ElicitationCache` entry persists until `reconcile()` prunes it. No Tokio dependency — the root crate uses `std::sync::mpsc` throughout.

`let _ = reply.send(res)` — the receiver may be dropped (NAPI layer timeout, JS GC). Ignoring the send error is correct; there is no recovery path if the reply channel is closed.

---

## `shared_run_terminal` — ordered teardown (src/actor.rs)

All terminal paths (cancel_run, fail_run, finalize_run, human-dismiss) delegate to this helper. Ordering is critical: `cancel_epoch` must be called **before** the terminal status event is published.

```rust
fn shared_run_terminal(run_id: &str, elicitation_maps: &Arc<Mutex<ElicitationMaps>>, ...) {
    // 1. Snapshot session handles (write_lock, kill_handle) from the registry.
    //    Hold write_reg lock ONLY during the snapshot; release before acquiring maps.
    //    Lock ordering: write_reg BEFORE maps — never hold both simultaneously.
    let sessions: Vec<(Arc<Mutex<()>>, Arc<KillHandle>)> = {
        let reg = write_reg.lock().unwrap_or_else(|p| p.into_inner());
        reg.iter()
            .filter(|((r, _, _), _)| r.as_str() == run_id)
            .map(|(_, (wl, kh))| (Arc::clone(wl), Arc::clone(kh)))
            .collect()
    };

    // 2. Per-session cancel: try_lock write_lock + tombstone + signal.
    //
    // INVARIANT: every proc.stdin write — including the initial rpc_send(session/prompt),
    // handshake rpc_send calls, and elicitation rpc_respond calls — MUST hold the session's
    // write_lock for the duration of the write. Teardown's try_lock check only establishes
    // "no write is active" if this invariant holds everywhere. A write that bypasses the
    // lock leaves teardown unable to order itself w.r.t. an in-flight write, violating
    // terminal ordering and risking a full-pipe hang.
    for (wl, kh) in &sessions {
        match wl.try_lock() {
            Ok(_guard) => {
                // Guard held — writer cannot acquire. Tombstone under maps lock.
                let mut maps = elicitation_maps.lock().unwrap_or_else(|p| p.into_inner());
                if maps.has_active_run(run_id) {
                    maps.cancel_epoch(run_id, maps.current_epoch(run_id));
                }
            }
            Err(_) => {
                // Write in-flight. Tombstone without write_lock, then signal.
                {
                    let mut maps = elicitation_maps.lock().unwrap_or_else(|p| p.into_inner());
                    if maps.has_active_run(run_id) {
                        maps.cancel_epoch(run_id, maps.current_epoch(run_id));
                    }
                }
            }
        }
        kh.signal();  // unconditional — covers rpc_expect suspensions
    }

    // 3. Tombstone under maps lock (covers pre-registration workers not yet in write_reg).
    //    has_active_run guard: returns false for PTY runs, tool_cmd units, and workers
    //    where cleanup_run already ran — avoids stale (run_id, 0) tombstones.
    {
        let mut maps = elicitation_maps.lock().unwrap_or_else(|p| p.into_inner());
        maps.tombstone_bus_run(run_id);  // bus guard
        if maps.has_active_run(run_id) {
            maps.cancel_epoch(run_id, maps.current_epoch(run_id));
        }
    }

    // 4. Emit RunCancelled / RunFailed event (AFTER tombstoning).
    //    This ordering ensures: worker cannot write `accept` after RunCancelled because
    //    the epoch is tombstoned (and adapter process killed if a write was in flight)
    //    BEFORE the terminal event is emitted.

    // 5. Call runner.on_run_complete.
    //    The cancel_epoch call inside on_run_complete is now a safe no-op (epoch already
    //    tombstoned in step 2/3). cleanup_run still runs via EpochCleanup::drop.

    // 6. Second registry sweep — catches sessions inserted between step 1 and now.
    //    (Worker spawns child after step 1's snapshot, inserts into registry, passes STEP B
    //    post-spawn check without seeing tombstone. Second sweep kills these late arrivals.)
    let late_sessions: Vec<Arc<KillHandle>> = {
        let reg = write_reg.lock().unwrap_or_else(|p| p.into_inner());
        reg.iter()
            .filter(|((r, _, _), _)| r.as_str() == run_id)
            .map(|(_, (_, kh))| Arc::clone(kh))
            .collect()
    };
    for kh in late_sessions { kh.signal(); }
}
```

### Lock ordering: write_reg BEFORE maps

`shared_run_terminal` acquires `write_reg` in step 1, releases it, then acquires `maps` in step 2. It never holds both locks simultaneously. The session-start helpers (STEP B post-spawn check) acquire `maps` then release it — they never hold `maps` while acquiring `write_reg`. This total ordering (`write_reg before maps` for teardown; `maps alone` for session-start) prevents ABBA deadlock.

If a new code path needs both locks simultaneously, it must acquire `write_reg` first, then `maps`. Document this invariant in a module-level comment in `actor.rs`.

### `on_run_complete` must NOT call `cleanup_run`

`EpochCleanup::drop` is the sole call site for `cleanup_run`. `on_run_complete` only calls `cancel_epoch`.

```rust
fn on_run_complete(&self, run_id: &str) {
    {
        let mut maps = self.elicitation_maps.lock().unwrap_or_else(|p| p.into_inner());
        if maps.has_active_run(run_id) {
            let epoch = maps.current_epoch(run_id);
            maps.cancel_epoch(run_id, epoch);
        }
        // cancelled_runs insertion belongs in the ACTOR's terminal path (shared_run_terminal),
        // not in AcpStepRunner::on_run_complete. Non-ACP runners (PTY) use
        // PersistentStepRunner::on_run_complete which has no ElicitationMaps access.
    }
    // Capture old session entries SYNCHRONOUSLY before spawning the drop thread,
    // keyed by (run_id, cli_key, old_session_gen) to avoid colliding with replacement entries.
    let tx = self.tx.clone();
    thread::spawn(move || { /* drop old_session_data + old_injects */ });
}
```

**`cleanup_run` must not be called from `on_run_complete`** because `EpochCleanup::drop` also calls it, and `drop` always fires (RAII). Double-calling `cleanup_run` decrements `active_workers` twice with one increment: if two concurrent units are active for a run (valid during reassign), a double-decrement would underflow, making `has_active_run` return false prematurely and causing the next `cancel_epoch` to skip the tombstone.

### `has_active_run` guard semantics

`maps.has_active_run(run_id)` returns `true` when `run_epoch.contains_key(run_id)` AND the stored epoch is > 0. Specifically:
- Returns `false` for PTY runs (never call `next_epoch`; no entry in `run_epoch`).
- Returns `false` for `tool_cmd` units (epoch allocated as 0; the entry is not inserted).
- Returns `false` after `cleanup_run` runs (removes the `run_epoch` entry when `active_workers` reaches 0).
- Returns `true` for ACP workers that called `next_epoch` and have not yet exited (epoch ≥ 1 in `run_epoch`).

This guard prevents stale tombstones on PTY runs — tombstoning a PTY run that has no epoch entry would insert a (run_id, 0) entry into `cancelled_epochs`, which a future ACP worker for the same run_id might incorrectly see as "epoch 0 cancelled."

---

## `CancelRun` handler

Delegates to `shared_run_terminal`. Key difference from `ReassignUnit`: kills ALL sessions for the run (not filtered by `cli_key`).

The actor's write-lock registry is keyed by `(run_id, session_key, session_gen)`. For `CancelRun`, filter on `run_id` only. For `ReassignUnit`, filter on `(run_id, previous_cli)` only — killing unrelated warm sessions for the same run is wrong.

**Universal tombstone** — `CancelRun` must also call `maps.tombstone_run(run_id)` under the maps lock. This inserts into `all_cancelled_runs` (the dispatch-mode-agnostic tombstone). Without this, a `ReassignReady` reply that arrives after `CancelRun` but before `expected_seq` advances would pass the `is_run_cancelled` guard when bus is disabled, because `tombstone_bus_run` only fires for bus-dispatched runs and `CancelRun` does not advance `run_launch_seq`. With `tombstone_run`, `is_run_cancelled()` returns true for both local and bus cancellations.

**Sequence advance before retire** — `CancelRun` must call `maps.advance_launch_seq(run_id)` BEFORE calling `retire_launch_state`. Without this, `retire_launch_state` clears `cancelled_runs` and `all_cancelled_runs` while the launch sequence is unchanged. If a run with the same `run_id` is later relaunched, `begin_launch` increments from the same base — but stale bus tasks from the prior cancelled run still carry the old sequence and would pass the launch-seq guard. Similarly, a delayed `ReassignReady` from the prior cancellation carries the same `expected_seq` and passes after the tombstones are cleared. Advancing the seq during cancellation ensures any prior tasks/replies are stale relative to the next launch's sequence, making tombstone removal safe.

---

## `ReassignUnit` path

```rust
// 0. Snapshot and kill sessions for (run_id, previous_cli) only.
//    Second sweep required (same pattern as CancelRun) for late-arriving sessions.

// 1. Lock maps; read old_epoch = current_epoch(run_id).
// 2. cancel_epoch(run_id, old_epoch) — drops the old worker's sender or sets tombstone.
//    Gate on has_active_run: PTY and inner actors have current_epoch == 0 and no
//    active_workers entry. Unconditionally calling cancel_epoch(run_id, 0) for them
//    permanently inserts (run_id, 0) into cancelled_epochs — stale tombstone for all time.
//    Also call advance_launch_seq(run_id) in the SAME lock hold:
{
    let mut maps = elicitation_maps.lock().unwrap_or_else(|p| p.into_inner());
    // cancel_epoch is gated: PTY/inner actors have current_epoch=0 and no active_workers.
    // Unconditionally calling cancel_epoch(run_id, 0) inserts a permanent stale tombstone.
    if maps.has_active_run(run_id) {
        let old_epoch = maps.current_epoch(run_id);
        maps.cancel_epoch(run_id, old_epoch);
    }
    // advance_launch_seq is ALWAYS called — not gated on has_active_run.
    // Reassignment can occur before a bus task is consumed (has_active_run is false because
    // no epoch is allocated yet) or for non-ACP tasks. Without advancing the sequence here,
    // the old queued bus task still matches the current sequence after reassignment and can
    // execute under the superseded assignment, including workspace side effects.
    maps.advance_launch_seq(run_id);
}

// 3. Unlock. Do NOT call next_epoch here.
//    dispatch_unit is the sole epoch allocator. Calling next_epoch in ReassignUnit
//    AND again in dispatch_unit would double-increment active_workers with only one
//    EpochCleanup decrement.

// 4. Call dispatch_unit for the replacement.
//    Inside dispatch_unit: begin_launch increments the sequence again (replacement gets
//    old_seq + 2); next_epoch(run_id) allocates the replacement epoch.

// 5. exec_turn reads input.elicitation_epoch; the new worker registers under new_epoch
//    which is not tombstoned.

// Second registry sweep (old cli_key only, session_gen bounded):
// Capture the session_gen high-water mark BEFORE dispatch_unit assigns the replacement.
// Any session registered after dispatch_unit returns carries a generation > old_max_gen
// and must NOT be killed — it belongs to the replacement worker.
//
// Without this bound the sweep kills any session for (run_id, previous_cli), including
// sessions the replacement worker registered between the first snapshot and this sweep.
let old_max_gen: u64 = {
    let reg = write_reg.lock().unwrap_or_else(|p| p.into_inner());
    reg.keys()
        .filter(|(r, cli, _)| r.as_str() == run_id && cli.as_str() == previous_cli)
        .map(|(_, _, gen)| *gen)
        .max()
        .unwrap_or(0)
};
// ReassignUnit{new_cli:None} guard: CancelRun can complete while the off-thread council
// awaits its decision; the ReassignReady handler must not dispatch a new worker after the
// run has been cancelled or the launch sequence has advanced past the expected token.
//
// At ReassignUnit time, capture expected_seq immediately after advance_launch_seq (inside
// the same lock hold):
//   let expected_seq = maps.current_launch_seq(run_id);
// Carry expected_seq into the ReassignReady payload and, in the ReassignReady handler,
// check before dispatch:
{
    let maps = elicitation_maps.lock().unwrap_or_else(|p| p.into_inner());
    if maps.is_run_cancelled(run_id) || maps.current_launch_seq(run_id) != expected_seq {
        tracing::warn!(run_id = %run_id, "ReassignReady: stale or cancelled; discarding");
        // `continue`, not `return` — return exits the entire actor::run loop and
        // disconnects all subsequent Core operations. continue discards only this
        // stale command and resumes the actor's command loop.
        continue;
    }
}
// dispatch_unit runs here; replacement workers registered after this point have gen > old_max_gen.
dispatch_unit(/* ... */);
let late_sessions: Vec<Arc<KillHandle>> = {
    let reg = write_reg.lock().unwrap_or_else(|p| p.into_inner());
    reg.iter()
        .filter(|((r, cli, gen), _)| {
            r.as_str() == run_id
                && cli.as_str() == previous_cli
                && *gen <= old_max_gen  // only pre-replacement sessions
        })
        .map(|(_, (_, kh))| Arc::clone(kh))
        .collect()
};
for kh in late_sessions { kh.signal(); }
```

### Why `advance_launch_seq` in ReassignUnit

`advance_launch_seq` increments the per-run sequence counter without allocating an epoch. Its purpose here is to invalidate any bus tasks that were published for the old assignment. A bus task for the old cli_key carries `launch_seq = L`. After `advance_launch_seq`, the counter is `L+1`. When the replacement is dispatched, `begin_launch` increments again to `L+2`. The bus consumer calls `try_next_epoch_bus(run_id, task.launch_seq=L, ...)` and sees `L < current counter` — the task is stale and discarded.

`advance_launch_seq` must be called in the SAME lock hold as `cancel_epoch` to prevent an interleaving where the bus consumer reads the old sequence before it's invalidated but after the old epoch is tombstoned.

### Epoch allocation constraint: dispatch_unit is the sole allocator

`next_epoch` must NOT be called from `ReassignUnit`. If it were:
1. `ReassignUnit` calls `next_epoch` → allocates epoch N, increments `active_workers`.
2. `dispatch_unit` calls `next_epoch` → allocates epoch N+1, increments `active_workers` again.
3. Only one `EpochCleanup` guard is created (for N+1); it decrements `active_workers` once.
4. `active_workers` is off by one for the lifetime of the run.
5. `cleanup_run` never reaches zero (if count was 1 before), so `run_epoch` is never removed.

---

## `Command::Shutdown` drain

```rust
Command::Shutdown => {
    // Step 1a: tombstone all active epochs + set shutdown_flag (single lock hold).
    {
        let mut maps = elicitation_maps.lock().unwrap_or_else(|p| p.into_inner());
        for run_id in maps.active_run_ids() {
            maps.cancel_epoch(&run_id, maps.current_epoch(&run_id));
        }
        maps.set_shutdown_flag();  // prevents future register() calls
    }

    // Step 1b: kill all registered sessions.
    let sessions: Vec<_> = {
        let reg = write_reg.lock().unwrap_or_else(|p| p.into_inner());
        reg.iter().map(|((rid, _, _), (wl, kh))| (rid.clone(), Arc::clone(wl), Arc::clone(kh))).collect()
    };
    for (_run_id, _wl, kh) in sessions {
        kh.signal();
    }

    // Step 1c: post-shutdown registration guard.
    //   ElicitationMaps::register() checks shutdown_flag and returns None immediately,
    //   causing the worker to see an unregistered elicitation and take the cancelled-epoch path.

    // Step 1d: late-child guard.
    //   Workers check shutdown_flag in the maps lock (STEP A) before spawning.
    //   Workers recheck (STEP B) after inserting into write_reg and before calling rpc_expect.
    //   Both checks take the maps lock, making them race-free with set_shutdown_flag().

    // Step 1e: second registry sweep — catches sessions inserted between step 1b and now.
    let late_sessions: Vec<Arc<KillHandle>> = {
        let reg = write_reg.lock().unwrap_or_else(|p| p.into_inner());
        reg.values().map(|(_, kh)| Arc::clone(kh)).collect()
    };
    for kh in late_sessions { kh.signal(); }

    // Step 2: reap PTYs (existing behavior).
    // Step 3: break actor loop.
    break;
}
```

### Why set_shutdown_flag inside the same lock as the tombstone loop

`set_shutdown_flag` and the `cancel_epoch` loop must be inside one lock hold. A worker that survives the Step 1a tombstone sweep (not yet in `active_run_ids`) might call `register` before or after `set_shutdown_flag`. If `set_shutdown_flag` is called in a separate lock hold:
- Window: tombstone sweep completes, lock released; `set_shutdown_flag` not yet called; worker acquires lock, calls `register`, succeeds (returns `Some(rx)`); worker suspends in `'elicit`; actor sets flag; actor kills sessions (step 1b does NOT see this new worker); actor exits; worker hangs.
- Collapse: same lock hold closes the window: any `register` that doesn't see the tombstone must see the flag (because the flag is set while holding the same mutex).

### Pre/post-spawn checks in session-start helpers

```rust
// STEP A: pre-spawn check (under maps lock, before spawning the child).
{
    let maps = elicitation_maps.lock().unwrap_or_else(|p| p.into_inner());
    if maps.shutdown_flag() || maps.is_epoch_cancelled(run_id, run_epoch) {
        return Err(anyhow::anyhow!("session startup aborted: shutdown/cancel"));
    }
}
let child = cmd.spawn()?;
let my_key = (run_id.to_string(), session_key.to_string(), gen);
write_reg.lock().unwrap_or_else(|p| p.into_inner())
    .insert(my_key.clone(), (Arc::clone(&write_lock), Arc::clone(&kill_handle)));

// STEP B: post-spawn recheck (after registry insertion).
// Race window: Shutdown/CancelRun can run between STEP A and the registry insert.
// If it did, it snapshots an empty registry and never signals the new child.
//
// Lock ordering: write_reg BEFORE maps (see §Lock ordering: write_reg BEFORE maps).
// Compute the cancellation condition in a nested scope so the maps guard is dropped
// before acquiring write_reg. Holding maps while locking write_reg would violate the
// declared ordering and can deadlock with shared_run_terminal (step 1 holds write_reg,
// step 2 acquires maps).
{
    let cancelled = {
        let maps = elicitation_maps.lock().unwrap_or_else(|p| p.into_inner());
        maps.shutdown_flag() || maps.is_epoch_cancelled(run_id, run_epoch)
    };  // maps lock released here — before write_reg is acquired below
    if cancelled {
        kill_handle.signal();
        write_reg.lock().unwrap_or_else(|p| p.into_inner()).remove(&my_key);
        return Err(anyhow::anyhow!("session killed: shutdown/cancel after spawn"));
    }
}
// Safe to call rpc_expect — the child is registered and not yet cancelled.
```

STEP A and STEP B together close the ABA race:
- STEP A prevents starting a child into an already-cancelled state (common path).
- STEP B kills the child if Shutdown/CancelRun ran after STEP A but before the registry insert.

Without STEP B, a child process spawned in the race window would never receive a `signal()` call because `shared_run_terminal`'s registry snapshot (step 1) ran before the insert.

---

## Write-lock registry

```rust
type SessionHandles = (Arc<Mutex<()>>, Arc<KillHandle>);
type WriteReg = Arc<Mutex<HashMap<(String, String, u64), SessionHandles>>>;
//                                   key: (run_id, session_key, session_gen)
```

Created in `spawn_with_acp_sessions` (NOT in `actor::run`) — `spawn_with_acp_sessions` constructs `AcpStepRunner` before starting `actor::run`, so a registry created inside `actor::run` cannot be shared with the already-constructed runner.

```rust
// In spawn_with_acp_sessions:
let write_reg: WriteReg = Arc::new(Mutex::new(HashMap::new()));
let runner = AcpStepRunner::new(..., Arc::clone(&write_reg));
thread::spawn(move || actor::run(..., write_reg));
```

### Session generation purpose

The session_gen (`u64`) in the write_reg key prevents a torn eviction: when a run has a warm session (`gen=1`) and a `ReassignUnit` installs a replacement session (`gen=2`), `drop_session_gen(run_id, cli_key, 1)` removes only `gen=1` from the registry without touching `gen=2`. Without session_gen, `drop_session_gen` would have to remove ALL entries for `(run_id, cli_key)`, which could evict the replacement if the drop races with its registration.

Session_gen is monotonically increasing per `(run_id, cli_key)`. `AcpStepRunner::next_session_gen(run_id, cli_key)` atomically increments and returns the next value.

### `KillHandle` reuse-safety

A raw PID is not safe because the background reaper's `wait()` releases the PID, which the OS can then assign to an unrelated process — a subsequent `kill(pid, SIGKILL)` would signal the wrong process.

Preferred implementations:
1. **Linux `pidfd`**: file-descriptor reference to a specific process; `kill` via `pidfd_send_signal`; OS guarantees no PID reuse.
2. **`Arc<Mutex<Option<Child>>>`** with a reaper protocol:
   - Acquire lock; call `child.kill()`; take the child (`child_opt.take()`); release lock; call `taken.wait()` outside the lock.
   - `take()` moves the child out, preventing double-kill. The lock serializes kill and reap.

The reaper-protocol version must NOT call `child.wait()` while holding the lock — `wait()` blocks until the child exits, and holding the mutex would prevent a concurrent `kill()` from acquiring it on a different thread.

---

## `initialize` capability declaration (src/acp_runner.rs)

```rust
const ELICITATION_VERIFIED_ADAPTERS: &[&str] = &["claude-agent-acp", "codex-acp"];

// form_enabled is computed per-adapter AND per-session-kind; stored on the AcpProcess/session.
// Passed as elicitation_enabled to both exec_turn and exec_turn_acp.
// Chat sessions MUST NOT advertise form capability even when the global flag is on:
// chat_turn currently routes elicitation as disabled (OQ-R-7 not resolved), so advertising
// form would cause allowlisted adapters to issue elicitation/create that wicked-core
// immediately cancels — wasted RPC round-trips and confusing adapter behavior.
let form_enabled = elicitation_enabled_global_flag
    && !is_chat_session   // session kind check: omit for chat until OQ-R-7 resolved
    && ELICITATION_VERIFIED_ADAPTERS.contains(&adapter_name);

let elicitation_cap: Value = if form_enabled {
    json!({ "form": {} })  // ACP SDK v1.3.0: nested "form" key required
} else {
    json!({})              // omit "form" → conforming adapters skip elicitation/create
};

// In clientCapabilities:
"clientCapabilities": {
    "fs": {},
    "terminal": false,
    "elicitation": elicitation_cap,
}
```

Gate the capability advertisement behind `WICKED_ELICITATION_ENABLED=false` by default. Backout: re-set the flag to false or remove the `elicitation.form` key.

### `{"form": {}}` vs `{}`

ACP SDK v1.3.0 tests `clientCapabilities.elicitation.form !== undefined`. An empty object `{}` sets `elicitation` to a defined value but `elicitation.form` to `undefined` — conforming adapters skip `elicitation/create`. `{"form": {}}` sets `form` to a defined value (empty object), enabling the elicitation flow.

**v1 restriction — property types**: only single-property schemas with `type: "string"` (or absent/null type) are handled. Integer, number, and boolean property types are cancelled before registration. Schemas with these types receive an immediate `action:"cancel"` without an `ElicitationCreated` event. This aligns with crew submitting free-text strings only; it cannot produce schema-valid integer/boolean JSON without a separate TS-side extension.

### Per-adapter enablement vs global flag

`ELICITATION_VERIFIED_ADAPTERS` is a compile-time allowlist. The global flag `elicitation_enabled_global_flag` reads `WICKED_ELICITATION_ENABLED`. An adapter NOT in the allowlist never sees `elicitation.form` in `clientCapabilities`, regardless of the flag — conforming adapters skip `elicitation/create`.

To enable a new adapter: add to `ELICITATION_VERIFIED_ADAPTERS` after resolving EC-3 (OQ-R-6 verified). To disable all at runtime: clear `WICKED_ELICITATION_ENABLED`.

---

## Handshake-phase elicitation guard (rpc_expect)

ACP v1.3.0 permits `elicitation/create` before a session exists (during startup authentication). The updated `rpc_expect` read loop:

```rust
pub fn rpc_expect<W: Write>(
    rx:          &Receiver<String>,
    id:          u64,
    timeout:     Duration,
    writer:      &mut W,          // ← new: allows responding to elicitation/create
    write_lock:  &Arc<Mutex<()>>, // ← new: serializes writes
    kill_handle: &Arc<KillHandle>,// ← new: watchdog needs kill handle
) -> anyhow::Result<Value> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() { return Err(anyhow::anyhow!("rpc_expect timeout")); }
        let line = match rx.recv_timeout(remaining) {
            Ok(l) => l,
            Err(_) => return Err(anyhow::anyhow!("rpc_expect timeout or disconnect")),
        };
        if line.len() > FRAME_BYTE_CAP {
            tracing::warn!(frame_len = line.len(), "rpc_expect: frame exceeds cap; dropping");
            continue;
        }
        let v: Value = match serde_json::from_str(&line) {
            Ok(v) => v, Err(_) => continue,  // preserve banner/log skip
        };
        // Cancel only elicitation/create; skip other method-bearing frames without responding.
        if v["method"].as_str() == Some("elicitation/create") {
            if let Some(req_id) = v.get("id") {
                {
                    let _wg = write_lock.lock().unwrap_or_else(|p| p.into_inner());
                    let watchdog = WriteWatchdog::new(Arc::clone(kill_handle), WRITE_WATCHDOG_MS);
                    let write_result = rpc_respond(writer, req_id, json!({"action": "cancel"}));
                    let watchdog_fired = watchdog.complete();
                    write_result?;  // propagate write error only after watchdog is stopped
                    if watchdog_fired {
                        return Err(anyhow::anyhow!(
                            "watchdog fired during handshake-phase elicitation cancel"
                        ));
                    }
                }
                tracing::info!("elicitation: handshake-phase elicitation/create cancelled");
            }
            continue;
        }
        // Skip other method-bearing frames (notifications, etc.) — no response.
        if v.get("method").is_some() { continue; }
        // Only match responses (no "method" field).
        if v.get("id") == Some(&Value::Number(id.into())) {
            // Check for JSON-RPC error before returning — initialize or session/new
            // can reject with {"id":N,"error":{...}}. Returning Ok(v) here would cause
            // the caller to interpret the frame as a successful result, producing a
            // misleading "missing field" error instead of the server's actual rejection.
            if v.get("error").is_some() {
                return Err(anyhow::anyhow!(
                    "rpc_expect: server returned error for id {id}: {}",
                    v["error"]
                ));
            }
            return Ok(v);
        }
    }
}
```

Note: `rx` is `&Receiver<String>` (borrow, not owned) — `start_acp_process` calls `rpc_expect` twice on the same receiver (for `initialize` and `session/new`). The borrowed form allows sequential reuse without ownership transfer.

The caller (session-start helper) registers the child's handles in `write_reg` immediately after `child.spawn()` — before calling `rpc_expect`. This makes the handshake write visible to `CancelRun` and bounded by the watchdog. Both STEP A (pre-spawn) and STEP B (post-spawn) checks must be in place: `rpc_expect` is only reached after STEP B passes, meaning the child is registered and not yet cancelled.

---

## `WriteWatchdog` — 3-state atomic protocol

`WriteWatchdog` wraps a write operation with a process-kill deadline. It prevents the actor thread from hanging if the adapter's stdin pipe is full (unlikely for small JSON-RPC frames, but required for correctness).

```rust
// Conceptual implementation (not the production type):
const WD_RUNNING: u32 = 0;
const WD_DONE:    u32 = 1;
const WD_FIRED:   u32 = 2;

pub struct WriteWatchdog {
    state:       Arc<AtomicU32>,
    condvar:     Arc<(Mutex<()>, Condvar)>,
    kill_handle: Arc<KillHandle>,
}

impl WriteWatchdog {
    pub fn new(kill_handle: Arc<KillHandle>, timeout_ms: u64) -> Self {
        let state = Arc::new(AtomicU32::new(WD_RUNNING));
        let condvar = Arc::new((Mutex::new(()), Condvar::new()));
        let state2 = Arc::clone(&state);
        let condvar2 = Arc::clone(&condvar);
        let kh = Arc::clone(&kill_handle);
        std::thread::spawn(move || {
            let (lock, cv) = &*condvar2;
            let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms);
            let mut guard = lock.lock().unwrap();
            // Predicate loop: checks state BEFORE sleeping to avoid lost-notification race.
            // Race: complete() can call notify_all() BEFORE the thread reaches wait_timeout,
            // so the notify is lost and the thread would sleep for the full remaining timeout.
            // Checking state before each wait ensures we exit immediately if already completed.
            loop {
                let now = std::time::Instant::now();
                if now >= deadline {
                    // True timeout: try to transition WD_RUNNING → WD_FIRED.
                    if state2.compare_exchange(
                        WD_RUNNING, WD_FIRED, Ordering::SeqCst, Ordering::SeqCst
                    ).is_ok() {
                        kh.signal();
                    }
                    break;
                }
                // Check state BEFORE sleeping — covers the case where complete() already
                // transitioned to WD_DONE (and notified) before this thread reached wait_timeout.
                if state2.load(Ordering::SeqCst) != WD_RUNNING { break; }
                let remaining = deadline - now;
                let (g, _result) = cv.wait_timeout(guard, remaining).unwrap();
                guard = g;
                // Whether woken by notify or spuriously: recheck state at top of loop.
                // Do not check timed_out here — always re-evaluate deadline and state.
            }
        });
        Self { state, condvar, kill_handle }
    }

    /// Call after the write completes. Returns true if watchdog already fired.
    pub fn complete(self) -> bool {
        let fired = self.state.compare_exchange(
            WD_RUNNING, WD_DONE, Ordering::SeqCst, Ordering::SeqCst
        ).is_err();
        let (lock, cv) = &*self.condvar;
        let _g = lock.lock().unwrap();
        cv.notify_all();
        fired  // true iff watchdog_fired=true (CAS failed, state was already WD_FIRED)
    }
}
```

**`complete()` is the correct termination call — not `drop`**. When the write completes successfully, call `complete()`:
- If the watchdog hasn't fired (state `WD_RUNNING → WD_DONE`): notifies the condvar, watchdog thread wakes, exits without signalling. Returns `false`.
- If the watchdog already fired (CAS fails; state is `WD_FIRED`): returns `true`. Caller should return an error — the child process was already killed; the write completed to a dead pipe.

Dropping `WriteWatchdog` without calling `complete()` leaves the watchdog thread running until its timeout. The background thread will call `kh.signal()` after the write is already done. Use `complete()` consistently; reserve `drop` for panic paths only.

---

## `StepStatus::ElicitationFailed` bus serialization (src/cli_runner.rs)

When `WICKED_BUS_EXEC` is enabled, `run_cli_runner` serializes `StepOutput.status` with `status_to_str` and reconstructs it with `status_from_str`. The fourth variant must have matching wire tokens:

```rust
fn status_to_str(s: StepStatus) -> &'static str {
    match s {
        StepStatus::Ok                => "ok",
        StepStatus::Failed            => "failed",
        StepStatus::Cancelled         => "cancelled",
        StepStatus::ElicitationFailed => "elicitation_failed",  // ← new
    }
}

fn status_from_str(s: &str) -> StepStatus {
    match s {
        "ok"                  => StepStatus::Ok,
        "failed"              => StepStatus::Failed,
        "cancelled"           => StepStatus::Cancelled,
        "elicitation_failed"  => StepStatus::ElicitationFailed,  // ← new
        _                     => StepStatus::Ok,  // existing wildcard
    }
}
```

Without matching cases, the serializer's wildcard converts `ElicitationFailed` to `StepStatus::Ok`, turning elicitation timeout/teardown into success. The bus path is only exercised when `WICKED_BUS_EXEC` is set in the environment; the wire token mismatch is silent in standard deployments and surfaces only under load or integration testing with bus enabled.

**Actor routing** (`src/actor.rs: step_status(...)` match):
```rust
StepStatus::ElicitationFailed => {
    // Route directly to run-terminal path.
    // Do NOT enter the unrecognized-failure triage path — triage can produce Retry,
    // silently redispatching the unit after a deadline or adapter disconnect.
    // This case is equivalent to a fatal, non-retriable step failure.
    handle_run_terminal(run_id, "elicitation_failed");
}
```

`ElicitationFailed` must appear in every exhaustive `match` over `StepStatus`. The compiler catches missing arms for `match s { ... }` forms, but NOT for arms that delegate to a fallible function and ignore ElicitationFailed via a wildcard. Audit all `match step_output.status` and `match s` blocks — see the exhaustive match sites table in DES-002-tests.md.
