# Purpose

Definitions for `ElicitationMaps`, `EpochCleanup`, `TurnResult`, and supporting types — the shared state that coordinates the worker, actor, and NAPI layers.

Lock-poison recovery: all `Mutex::lock()` calls use `.unwrap_or_else(|p| p.into_inner())` throughout this design. This ensures cleanup runs even when a thread panicked while holding the lock. Documented once here; applied without explanation at each call site.

---

## CoreEvent definitions (src/event.rs)

### `CoreEvent::ElicitationCreated`

```rust
/// An MCP server inside a native ACP session asked the human a question.
/// `options: None` = free-text; `Some(v)` = radio-group (v is non-empty, guaranteed by parser).
/// A schema with enum/oneOf present but all choices non-representable is cancelled before
/// registration — no ElicitationCreated is emitted (I-5).
/// `prop_type` is the JSON Schema `type` of the single response property.
/// v1: only "string" and None emitted; integer/number/boolean cancelled before registration.
/// `epoch` is included for the `'elicit` drain arm: after `deliver` removes the sender
/// the worker checks `is_epoch_cancelled(session, epoch)` to detect the
/// ResolveElicitation-before-CancelRun race. The actor's EmitEvent suppression uses
/// `suppressed_creations` instead (survives cleanup_run; see actor-teardown §EmitEvent).
ElicitationCreated {
    session:         String,   // run_id — the ElicitationMaps ownership key
    epoch:           u64,      // dispatch-time epoch; used for tombstone check in actor
    elicitation_id:  String,
    message:         String,
    options:         Option<Vec<String>>,
    prop_type:       Option<String>,
}
```

JSON serialisation (`CoreEvent::to_json`):
```json
{
  "type": "elicitationCreated",
  "session": "run-id",
  "elicitationId": "uuid",
  "message": "Which environment?",
  "options": ["staging", "prod"],
  "propType": null
}
```

`options` and `propType` are always serialised explicitly — `null` or their typed value; never absent.

### `CoreEvent::ElicitationResolved`

```rust
/// Terminal outcome for a pending elicitation. Emitted by exec_turn_acp on every
/// removal path — human resolution, core-driven cancel, deadline expiry, and teardown.
/// Matches every ElicitationCreated (one resolved per created).
ElicitationResolved {
    session:         String,
    elicitation_id:  String,
    /// Effective wire action sent to the adapter: "accept" | "decline" | "cancel".
    action:          String,
    /// Reason: "human" | "session_prompt" | "timeout" | "teardown" | "adapter_write_failure".
    reason:          String,
}
```

Emitted **after** the `rpc_respond` adapter write attempt so the durable log records only actions the adapter actually received.

---

## `Command::ResolveElicitation` (src/command.rs)

```rust
/// Deliver a human's answer to a pending elicitation.
ResolveElicitation {
    run_id:          String,
    elicitation_id:  String,
    action:          String,                     // "accept" | "decline" | "cancel"
    response:        Option<serde_json::Value>,  // non-None only when action="accept"
    reply:           mpsc::SyncSender<anyhow::Result<()>>,  // std::sync::mpsc — no Tokio dep
},
```

---

## NAPI binding (crates/wicked-core-ts/src/lib.rs)

```rust
/// Deliver a human response to a pending elicitation.
/// `action`: "accept" | "decline" | "cancel". `response` required when action="accept".
/// Flat params — the crew adapter wrapper must unpack result.content?.response before calling.
#[napi(ts_return_type = "Promise<string>")]
pub fn resolve_elicitation(
    &self,
    run_id:          String,
    elicitation_id:  String,
    action:          String,
    response:        Option<serde_json::Value>,
) -> AsyncTask<CoreTask> { ... }
```

Crew adapter wrapper pattern (packages/core/src/adapters/):
```ts
const response = result.content?.response ?? null;
await wicked.resolveElicitation(runId, elicitationId, result.action, response);
// Must await — fire-and-forget hides delivery failures on the JS side.
```

Reading `result.response` directly would forward `undefined`, causing every accept to be rejected with "response is required when action=accept".

**Cargo.toml additions**:
- `uuid = { version = "1", features = ["v4", "serde"] }` — for `uuid::Uuid::new_v4()` and `Serialize`/`Deserialize` on `Option<Uuid>` in `DispatchedTask`.
- `tracing = "0.1"` — for all `tracing::warn!`, `tracing::info!`, `tracing::error!` call sites.
- `crates/wicked-core-ts/Cargo.toml`: add `"serde-json"` to the `napi` dependency feature list so napi-rs implements `FromNapiValue`/`ToNapiValue` for `serde_json::Value`.

---

## `ElicitationMaps` — combined struct (src/acp_runner.rs)

### Supporting types

```rust
pub(crate) struct ElicitationResult {
    pub(crate) action:   String,
    pub(crate) response: Option<serde_json::Value>,
}

/// Per-elicitation entry. Wrapping in a struct lets `deliver` validate membership
/// before consuming the sender, so a wrong answer can be rejected without losing
/// the ability to retry.
struct ElicitationEntry {
    tx:      mpsc::Sender<ElicitationResult>,
    /// Option list parsed from oneOf/enum at register time. None = free-text.
    /// Some(v) = resolved value must appear in v before sender is consumed.
    options: Option<Vec<String>>,
}
```

### Struct definition

```rust
/// Both maps under one lock — makes registration atomic w.r.t. teardown.
pub(crate) struct ElicitationMaps {
    /// Primary: elicitation_id → pending resolution entry.
    pending:             HashMap<String, ElicitationEntry>,
    /// Reverse index: run_id → [(elicitation_id, epoch)].
    run_index:           HashMap<String, Vec<(String, u64)>>,
    /// Monotonic epoch per run_id. Incremented by next_epoch only.
    run_epoch:           HashMap<String, u64>,
    /// Cancelled (run_id, epoch) pairs. Checked by register to prevent the
    /// pre-registration teardown race. Keyed by epoch so a relaunched run
    /// (epoch N+1) is not blocked by a tombstone for the previous attempt.
    cancelled_epochs:    HashSet<(String, u64)>,
    /// Count of live workers per run_id. Incremented by next_epoch; decremented
    /// by cleanup_run. run_epoch is removed when this count reaches zero.
    active_workers:      HashMap<String, usize>,
    /// Bus-dispatch tombstone guard. Run IDs inserted by tombstone_bus_run
    /// (gated on bus_dispatched_runs.contains). try_next_epoch_bus returns None
    /// for any run_id in this set.
    cancelled_runs:      HashSet<String>,
    /// Universal cancel tombstone — covers ALL dispatch paths (bus and local).
    /// Populated by tombstone_run (called by the CancelRun handler regardless of dispatch mode).
    /// The ReassignReady guard checks this to reject stale replies even when bus is disabled
    /// or was never used, because cancelled_runs only covers bus-dispatched runs.
    /// Cleared by begin_launch (on relaunch) and retire_launch_state (on terminal cleanup).
    all_cancelled_runs:  HashSet<String>,
    /// Monotonically-increasing launch counter per run_id. Incremented by
    /// begin_launch for every dispatch (bus and local). Zero is reserved for
    /// legacy deserialized payloads. Never decremented or reset.
    run_launch_seq:      HashMap<String, u64>,
    /// Tracks run IDs dispatched to the execution bus (WICKED_BUS_EXEC enabled).
    /// Set only after successful publication via mark_bus_dispatch.
    /// Used by tombstone_bus_run to avoid inserting local-only runs into cancelled_runs.
    bus_dispatched_runs: HashSet<String>,
    /// Set by set_shutdown_flag() during Command::Shutdown processing.
    /// Once true, register() and try_next_epoch_bus() reject all new allocations.
    shutdown_flag:       bool,
    /// Elicitation IDs for which cancel_epoch should suppress the queued
    /// EmitEvent(ElicitationCreated). Populated by cancel_epoch ONLY IF the actor has NOT
    /// yet announced the creation (i.e., not in creation_announced). The actor calls
    /// take_suppressed_creation atomically under the maps lock to check and remove.
    suppressed_creations: HashSet<String>,
    /// Elicitation IDs for which the actor has already fanned out ElicitationCreated.
    /// Marked by the actor under the maps lock immediately before fan-out.
    /// Checked by cancel_epoch: if an ID is already announced, suppression is too late —
    /// the creation event was already delivered and no suppression marker is inserted.
    /// Cleared by remove() and by cancel_epoch (panic-path drain) to prevent unbounded growth.
    creation_announced: HashSet<String>,
    /// Elicitation IDs whose ElicitationResolved must be suppressed because the paired
    /// ElicitationCreated was suppressed. Populated by the actor when take_suppressed_creation
    /// returns true (or shutdown_flag suppresses). The actor's ElicitationResolved handler
    /// calls take_suppressed_resolution before fan-out; if it returns true the event is dropped,
    /// preserving the paired-event contract (subscribers never see an orphaned resolved event).
    /// Entries are small and short-lived: removed when the resolution event arrives or never
    /// arrive if the epoch was already cleaned up.
    suppressed_resolutions: HashSet<String>,
    /// Idempotency guard for bus re-delivery within the same actor lifetime.
    /// Maps run_id → the launch_seq that was most recently activated by try_next_epoch_bus.
    /// When confirm_task_completed() fails (publish error), the bus cursor is NOT advanced
    /// (at-least-once) and the same task is re-delivered. Without this guard,
    /// try_next_epoch_bus would match the same (run_id, launch_seq) again and spawn a
    /// duplicate CLI — the at-least-once contract does not extend to duplicate execution.
    /// Cleared by begin_launch when a new launch sequence is started (new launch_seq ≠ old).
    /// Not cleared by retire_launch_state: the guard must persist until the next begin_launch
    /// so a late re-delivery during cleanup is still rejected.
    bus_activated_seqs: HashMap<String, u64>,
    /// In-flight tracker for bus workers, covering ALL runner types (ACP and non-ACP).
    /// Keys are (run_id, launch_seq) pairs — one entry per live worker.
    ///
    /// Using a Set<(run_id, launch_seq)> rather than Map<run_id, launch_seq> is required
    /// for reassignment: during a reassign, the old worker (seq=N) remains active while the
    /// replacement (seq=N+1) starts.  A HashMap<run_id, u64> would overwrite N with N+1 on
    /// mark_bus_in_flight, so when the replacement finishes first, clearing N+1 leaves the
    /// set empty and any_bus_worker_in_flight() returns false while the old worker is alive.
    ///
    /// begin_launch does NOT clear this set — each worker manages its own entry independently.
    /// The set is the single source of truth for "is ANY bus worker currently running?"
    bus_in_flight_workers: HashSet<(String, u64)>,
}

impl ElicitationMaps {
    pub(crate) fn new() -> Self {
        Self {
            pending:              HashMap::new(),
            run_index:            HashMap::new(),
            run_epoch:            HashMap::new(),
            cancelled_epochs:     HashSet::new(),
            active_workers:       HashMap::new(),
            cancelled_runs:       HashSet::new(),
            all_cancelled_runs:   HashSet::new(),
            run_launch_seq:       HashMap::new(),
            bus_dispatched_runs:  HashSet::new(),
            shutdown_flag:        false,
            suppressed_creations:   HashSet::new(),
            creation_announced:     HashSet::new(),
            suppressed_resolutions: HashSet::new(),
            bus_activated_seqs:     HashMap::new(),
            bus_in_flight_workers:  HashSet::new(),
        }
    }
}
```

### Methods

```rust
/// All run IDs that currently have at least one active worker.
/// Used by Command::Shutdown to tombstone pre-registration workers.
pub(crate) fn active_run_ids(&self) -> Vec<String> {
    self.active_workers
        .iter()
        .filter(|(_, &count)| count > 0)
        .map(|(id, _)| id.clone())
        .collect()
}

/// Permanently block new registrations and bus epoch allocations.
/// Called once during Command::Shutdown, before the write-lock registry snapshot.
pub(crate) fn set_shutdown_flag(&mut self) {
    self.shutdown_flag = true;
}

/// Read-only accessor for shutdown_flag.
/// Call under the maps lock; callers must NOT hold the lock across a subsequent child spawn.
pub(crate) fn shutdown_flag(&self) -> bool {
    self.shutdown_flag
}

/// Advance the epoch for run_id and return the new value.
/// Called by the run dispatcher when starting a NEW attempt.
/// Workers capture this epoch and pass it to every register call.
pub(crate) fn next_epoch(&mut self, run_id: &str) -> u64 {
    let e = self.run_epoch.entry(run_id.to_string()).or_insert(0);
    *e += 1;
    *self.active_workers.entry(run_id.to_string()).or_insert(0) += 1;
    *e
}

/// Current epoch for run_id (0 if next_epoch was never called).
pub(crate) fn current_epoch(&self, run_id: &str) -> u64 {
    *self.run_epoch.get(run_id).unwrap_or(&0)
}

/// Register an elicitation for the worker's captured epoch.
/// Returns None if the epoch has been cancelled or shutdown_flag is set.
/// The check and insertion are atomic under the same Mutex that cancel_epoch holds,
/// preventing the pre-registration teardown race.
pub(crate) fn register(
    &mut self,
    run_id:          &str,
    elicitation_id:  &str,
    epoch:           u64,
    options:         Option<Vec<String>>,
) -> Option<mpsc::Receiver<ElicitationResult>>
{
    if self.shutdown_flag { return None; }
    if self.cancelled_epochs.contains(&(run_id.to_string(), epoch)) { return None; }
    let (tx, rx) = mpsc::channel();
    self.pending.insert(elicitation_id.to_string(), ElicitationEntry { tx, options });
    self.run_index.entry(run_id.to_string()).or_default()
        .push((elicitation_id.to_string(), epoch));
    Some(rx)
}

/// True if (run_id, epoch) has been tombstoned.
/// Used in the 'elicit loop's Ok(result) arm to detect the ResolveElicitation-before-CancelRun
/// race: deliver may have removed the sender before cancel_epoch ran, so Disconnected never fired.
pub(crate) fn is_epoch_cancelled(&self, run_id: &str, epoch: u64) -> bool {
    self.cancelled_epochs.contains(&(run_id.to_string(), epoch))
}

/// Remove one elicitation (post-resolution or explicit cancel).
/// Called inside the final drain lock in exec_turn_acp before the adapter write.
/// Clears creation_announced to prevent unbounded growth. Does NOT touch
/// suppressed_creations — that marker must outlive remove() so the actor can still
/// suppress a queued EmitEvent(ElicitationCreated) that arrives after remove() returns.
/// (Suppression markers are cleared by take_suppressed_creation in the actor; cancel_epoch
/// gates insertion on creation_announced so the marker is never orphaned after fan-out.)
pub(crate) fn remove(&mut self, run_id: &str, elicitation_id: &str) {
    self.pending.remove(elicitation_id);
    if let Some(entries) = self.run_index.get_mut(run_id) {
        entries.retain(|(eid, _)| eid != elicitation_id);
        if entries.is_empty() { self.run_index.remove(run_id); }
    }
    self.creation_announced.remove(elicitation_id);
}

/// Cancel all pending elicitations for (run_id, epoch) by dropping their senders.
/// Tombstones the epoch so future register calls for the same epoch return None.
/// Does NOT bump the epoch counter — use next_epoch for that.
/// Returns the elicitation IDs drained from pending (non-empty only on panic paths).
pub(crate) fn cancel_epoch(&mut self, run_id: &str, epoch: u64) -> Vec<String> {
    self.cancelled_epochs.insert((run_id.to_string(), epoch));
    if let Some(entries) = self.run_index.get_mut(run_id) {
        let to_drop: Vec<String> = entries.iter()
            .filter(|(_, ep)| *ep == epoch)
            .map(|(eid, _)| eid.clone())
            .collect();
        entries.retain(|(_, ep)| *ep != epoch);
        if entries.is_empty() { self.run_index.remove(run_id); }
        for eid in &to_drop {
            let _ = self.pending.remove(eid);
            // Only suppress if the actor has NOT yet announced the creation.
            // If the actor already fanned out ElicitationCreated (i.e., eid is in
            // creation_announced), cancel_epoch arrived too late to suppress — the
            // event was already delivered to subscribers, so inserting a stale
            // suppression marker would cause an orphaned entry that is never taken.
            if !self.creation_announced.contains(eid) {
                self.suppressed_creations.insert(eid.clone());
            }
            // Always clear creation_announced here. On the normal path remove() clears it,
            // but if the worker panicked after mark_creation_announced and before remove(),
            // no remove() ever runs. Draining the ID here prevents a daemon-lifetime leak
            // of one entry per panic. Safe because cancel_epoch is called in drop() before
            // any code that would re-mark the same ID as announced.
            self.creation_announced.remove(eid);
        }
        to_drop
    } else {
        vec![]
    }
}

/// Per-epoch reclamation. Removes the cancelled_epochs entry for (run_id, epoch).
/// Decrements active_workers[run_id]; removes run_epoch[run_id] only when count reaches zero.
/// Clears bus_in_flight_workers for (run_id, launch_seq) via the sequence-aware guard.
/// Called exclusively by EpochCleanup::drop — no other call site.
///
/// `launch_seq` is 0 for non-bus-dispatched turns; clear_bus_in_flight is a no-op in that case
/// (no entry in bus_in_flight_workers ever carries seq 0 for a live bus task).
/// For ACP bus tasks this is the decisive in-flight clear — it fires on EVERY exit path
/// (normal completion, cancellation, panic) because EpochCleanup is a RAII guard.
pub(crate) fn cleanup_run(&mut self, run_id: &str, epoch: u64, launch_seq: u64) {
    // Clear the bus in-flight marker only on panic / cancellation / early-exit paths.
    // On the normal ACP bus completion path the marker must stay set until AFTER
    // confirm_task_completed — if the marker is cleared here and a redelivery races
    // before publication, is_bus_worker_in_flight returns false and the degraded path
    // falsely marks the run as failed.  The bus consumer sets bus_in_flight_deferred=true
    // after exec_turn returns normally; it then calls clear_bus_in_flight explicitly once
    // confirm_task_completed (success or failure) is done.
    if !self.bus_in_flight_deferred {
        self.clear_bus_in_flight(run_id, launch_seq);
    }
    self.cancelled_epochs.retain(|(rid, ep)| !(rid == run_id && *ep == epoch));
    if let Some(count) = self.active_workers.get_mut(run_id) {
        if *count > 0 { *count -= 1; }
        if *count == 0 {
            self.active_workers.remove(run_id);
            self.run_epoch.remove(run_id);
            // Prune ALL remaining cancelled_epochs entries for this run.
            // Without this, a later run that reaches the same epoch number would
            // find a stale tombstone and have its register() call rejected.
            self.cancelled_epochs.retain(|(rid, _)| rid != run_id);
            // NOTE: run_launch_seq and all_cancelled_runs are NOT pruned here.
            // EpochCleanup::drop fires before the worker enqueues ApplyStepResult.
            // If we removed run_launch_seq here, the ReassignReady guard (which reads
            // current_launch_seq AFTER the worker exits but BEFORE the actor processes
            // ApplyStepResult) would observe sequence 0 and falsely reject a valid result.
            // Pruning is deferred to retire_launch_state, called actor-side after result
            // handling is complete.
        }
    }
}

/// Retire per-run cancel tombstones once the run is fully terminal and all result
/// handling is complete. Called actor-side AFTER apply_step_result (or RunCancelled/RunFailed)
/// — never from EpochCleanup::drop, which fires before ApplyStepResult is enqueued.
/// Without this, long-lived daemons with unique run IDs accumulate unbounded state in
/// cancelled_runs and all_cancelled_runs.
///
/// NOTE: run_launch_seq is intentionally NOT removed here. The sequence must remain
/// monotonically increasing for the actor lifetime. If run_launch_seq[run_id] were removed
/// and the run_id relaunched, begin_launch would restart from 1, allowing a stale bus task
/// or cancelled-worker completion from the prior run (carrying an old launch_seq=1) to
/// pass the launch-token guard and apply stale work to the new run.
///
/// Growth bound: run_launch_seq retains one entry per unique run_id dispatched since the
/// last actor restart (one entry per `begin_launch` call for a previously-unseen run_id).
/// This is bounded by the number of unique run IDs in one actor lifetime — NOT by the
/// count of currently active runs. For most deployments (bounded session pool, periodic
/// actor restart), this is acceptable. If a daemon dispatches an unbounded number of unique
/// run IDs without restarting, consider an LRU cap or a composite identity that encodes
/// the actor restart epoch so that entries can be reclaimed safely.
pub(crate) fn retire_launch_state(&mut self, run_id: &str) {
    // run_launch_seq deliberately omitted — see doc comment.
    self.cancelled_runs.remove(run_id);
    self.all_cancelled_runs.remove(run_id);
}

/// Check and atomically remove an elicitation_id from suppressed_creations.
/// Returns true if the ID was suppressed by cancel_epoch and the actor should skip fan-out.
/// The actor calls this under the maps lock BEFORE calling mark_creation_announced.
/// If this returns true, the actor does NOT call mark_creation_announced (suppressed path).
pub(crate) fn take_suppressed_creation(&mut self, elicitation_id: &str) -> bool {
    self.suppressed_creations.remove(elicitation_id)
}

/// Mark that the actor has fanned out ElicitationCreated for this ID.
/// Must be called under the maps lock immediately before releasing the lock for fan-out.
/// After this returns, any concurrent cancel_epoch will see creation_announced and skip
/// inserting a stale suppression marker — ensuring one-way suppression ordering.
/// Called only on the non-suppressed path (take_suppressed_creation returned false).
pub(crate) fn mark_creation_announced(&mut self, elicitation_id: &str) {
    self.creation_announced.insert(elicitation_id.to_string());
}

/// Record that the ElicitationResolved for this elicitation_id must be suppressed.
/// Called by the actor when it suppresses ElicitationCreated (take_suppressed_creation
/// returned true, or shutdown_flag was set). Prevents the paired-event contract violation
/// where subscribers receive a terminal resolved event for an elicitation they never saw.
pub(crate) fn mark_resolution_suppressed(&mut self, elicitation_id: &str) {
    self.suppressed_resolutions.insert(elicitation_id.to_string());
}

/// Check and remove the suppression marker for this ElicitationResolved.
/// Returns true if the corresponding ElicitationCreated was suppressed;
/// the actor must then skip fan-out for the resolved event.
pub(crate) fn take_suppressed_resolution(&mut self, elicitation_id: &str) -> bool {
    self.suppressed_resolutions.remove(elicitation_id)
}

/// True if at least one worker for run_id has been dispatched and has not yet called cleanup_run.
pub(crate) fn has_active_run(&self, run_id: &str) -> bool {
    self.active_workers.get(run_id).map_or(0, |&c| c) > 0
}

/// True if the elicitation is still in the pending map (not yet resolved or drained).
/// Used by the EmitEvent(ElicitationCreated) handler to detect the race where the worker
/// resolves (calls remove()) before the actor drains the queued creation event. When the
/// ID is no longer pending, the actor must suppress the stale creation fan-out to avoid
/// emitting ElicitationCreated after ElicitationResolved.
pub(crate) fn is_pending(&self, elicitation_id: &str) -> bool {
    self.pending.contains_key(elicitation_id)
}

/// Deliver a resolution result. Validates that run_id owns the elicitation_id.
/// Options membership check runs BEFORE removing from pending — a wrong answer returns Err
/// without consuming the sender so the worker can receive a corrected answer on retry.
pub(crate) fn deliver(
    &mut self,
    run_id:          &str,
    elicitation_id:  &str,
    result:          ElicitationResult,
) -> anyhow::Result<()>
{
    let owned = self.run_index.get(run_id)
        .map_or(false, |entries| entries.iter().any(|(eid, _)| eid == elicitation_id));
    if !owned {
        let err = anyhow::anyhow!("elicitation_id {elicitation_id} not found for run {run_id}");
        tracing::warn!(elicitation_id, error = %err, "elicitation.deliver_failed");
        return Err(err);
    }
    // Validation — BEFORE removing from pending (preserves retry-ability on failure).
    // Invalid action is always rejected:
    if !matches!(result.action.as_str(), "accept" | "decline" | "cancel") {
        return Err(anyhow::anyhow!(
            "elicitation_id {elicitation_id}: invalid action {:?}; \
             must be \"accept\", \"decline\", or \"cancel\"",
            result.action
        ));
    }
    // For action="accept": response must be a JSON string in all cases.
    //   - options=Some: must also be a member of the allowed option strings.
    //   - options=None: free-text — still must be a JSON string, not None/null/number/etc.
    //     Skipping validation here would let None or a non-string value reach the worker,
    //     which would forward malformed content the human can no longer correct.
    if result.action == "accept" {
        if let Some(entry) = self.pending.get(elicitation_id) {
            match result.response.as_ref().and_then(|v| v.as_str()) {
                None => {
                    return Err(anyhow::anyhow!(
                        "elicitation_id {elicitation_id}: action=\"accept\" requires a \
                         string response (got {:?}); rejecting without consuming sender",
                        result.response
                    ));
                }
                Some(resp) => {
                    // Bound free-text length. Arbitrarily large strings would be cloned into
                    // a JSON-RPC frame, causing unbounded allocation and potentially blocking or
                    // killing the adapter. The inbound frame cap protects only adapter→worker;
                    // this cap protects worker→adapter. Options-constrained entries are already
                    // bounded by the option string lengths — only free-text needs this check.
                    const ELICITATION_FREE_TEXT_CAP: usize = 65_536;  // 64 KiB
                    if entry.options.is_none() && resp.len() > ELICITATION_FREE_TEXT_CAP {
                        return Err(anyhow::anyhow!(
                            "elicitation_id {elicitation_id}: free-text response exceeds \
                             {ELICITATION_FREE_TEXT_CAP} bytes; rejecting without consuming \
                             sender"
                        ));
                    }
                    // If options are present, also check membership.
                    if let Some(options) = &entry.options {
                        if !options.iter().any(|o| o == resp) {
                            return Err(anyhow::anyhow!(
                                "elicitation_id {elicitation_id}: response {:?} is not a \
                                 member of the allowed options; rejecting without consuming \
                                 sender",
                                resp
                            ));
                        }
                    }
                    // Free-text (options=None): any non-empty string ≤ ELICITATION_FREE_TEXT_CAP.
                }
            }
        }
    }
    match self.pending.remove(elicitation_id) {
        Some(entry) => entry.tx.send(result).map_err(|_| {
            anyhow::anyhow!(
                "elicitation_id {elicitation_id} resolved but worker exited before receiving"
            )
        }),
        None => {
            let err = anyhow::anyhow!("elicitation_id {elicitation_id} already resolved");
            tracing::warn!(elicitation_id, error = %err, "elicitation.deliver_failed");
            Err(err)
        }
    }
}

/// Insert into cancelled_runs only for runs that were bus-dispatched.
/// Gated on bus_dispatched_runs (not run_launch_seq.contains_key) to avoid
/// accumulating local-only runs in cancelled_runs unboundedly.
pub(crate) fn tombstone_bus_run(&mut self, run_id: &str) {
    if self.bus_dispatched_runs.contains(run_id) {
        self.cancelled_runs.insert(run_id.to_string());
        self.bus_dispatched_runs.remove(run_id);
    }
}

/// Universal cancel tombstone — insert for ALL dispatch paths (bus and local).
/// Called by the CancelRun handler unconditionally (regardless of whether bus is enabled).
/// This is distinct from tombstone_bus_run, which only fires for bus-dispatched runs.
/// The ReassignReady guard checks all_cancelled_runs via is_run_cancelled() so it also
/// detects local cancellations (bus may be disabled or the run may never have been published).
pub(crate) fn tombstone_run(&mut self, run_id: &str) {
    self.all_cancelled_runs.insert(run_id.to_string());
}

/// Increment run_launch_seq and clear the tombstones for the next launch.
/// Called for ALL dispatch paths (bus and local). Always pass is_bus_dispatch=false;
/// the bus marker is set separately via mark_bus_dispatch after successful publication.
pub(crate) fn begin_launch(&mut self, run_id: &str, is_bus_dispatch: bool) -> u64 {
    debug_assert!(!is_bus_dispatch, "bus marker must be set via mark_bus_dispatch post-publish");
    self.cancelled_runs.remove(run_id);
    self.all_cancelled_runs.remove(run_id);
    self.bus_dispatched_runs.remove(run_id);
    // Clear the idempotency guard. The new seq (current + 1) is different so the guard
    // would not match anyway — clearing is explicit and prevents confusion.
    self.bus_activated_seqs.remove(run_id);
    // Do NOT clear bus_in_flight_workers here. With HashSet<(run_id, launch_seq)> keying,
    // each worker manages its own entry; begin_launch may fire while a prior worker from a
    // reassignment is still in-flight (e.g. during overlapping reassign).  Clearing by run_id
    // at begin_launch time would remove the old worker's marker, masking it from
    // any_bus_worker_in_flight() while it's still active.  Each worker clears its own entry
    // on exit via clear_bus_in_flight.
    // Never reset run_launch_seq — it is monotonically increasing.
    let seq = self.run_launch_seq.entry(run_id.to_string()).or_insert(0);
    *seq += 1;
    *seq
}

/// Mark a run as bus-dispatched AFTER successful publication.
pub(crate) fn mark_bus_dispatch(&mut self, run_id: &str) {
    self.bus_dispatched_runs.insert(run_id.to_string());
}

/// Advance sequence without clearing tombstone.
/// Used by ReassignUnit to invalidate queued bus tasks atomically
/// (→ see DES-002-actor-teardown.md §ReassignUnit).
pub(crate) fn advance_launch_seq(&mut self, run_id: &str) {
    // Only advance if a launch-seq entry exists — prevents spurious entries for local-only runs.
    if let Some(seq) = self.run_launch_seq.get_mut(run_id) {
        *seq += 1;
    }
    // Does NOT clear cancelled_runs — reassignment is not a relaunch.
    // begin_launch (called inside dispatch_unit) will clear and increment again.
}

/// Return the current launch sequence for run_id, or 0 if not yet launched.
/// Used by ReassignUnit to capture expected_seq for the ReassignReady guard
/// (→ DES-002-actor-teardown.md §ReassignReady guard).
pub(crate) fn current_launch_seq(&self, run_id: &str) -> u64 {
    self.run_launch_seq.get(run_id).copied().unwrap_or(0)
}

/// True if the run has been tombstoned via ANY cancel path (bus or local).
/// Checks both cancelled_runs (bus-specific via tombstone_bus_run) and
/// all_cancelled_runs (universal via tombstone_run called by CancelRun handler).
/// Used by the ReassignReady guard to reject stale replies regardless of dispatch mode.
pub(crate) fn is_run_cancelled(&self, run_id: &str) -> bool {
    self.cancelled_runs.contains(run_id) || self.all_cancelled_runs.contains(run_id)
}

/// True if (run_id, launch_seq) was already activated by try_next_epoch_bus in this
/// actor lifetime. The bus consumer MUST call this BEFORE try_next_epoch_bus to
/// distinguish "already executed, output may be lost" (this guard) from "stale/cancelled"
/// (try_next_epoch_bus returning None). When this returns true the consumer MUST NOT
/// silently advance the cursor as if the task were merely stale — it must log a critical
/// error and record degraded outcome before advancing.
pub(crate) fn has_activated_seq(&self, run_id: &str, launch_seq: u64) -> bool {
    self.bus_activated_seqs.get(run_id).copied() == Some(launch_seq)
}

/// True if a bus worker for (run_id, launch_seq) is currently in-flight.
/// Unlike has_active_run(), this covers ALL runner types (ACP and non-ACP).
/// Used by the degraded-path check to distinguish "worker still running" (normal re-poll)
/// from "worker gone, completion was lost" (degraded mode requiring run failure + cursor advance).
pub(crate) fn is_bus_worker_in_flight(&self, run_id: &str, launch_seq: u64) -> bool {
    self.bus_in_flight_workers.contains(&(run_id.to_string(), launch_seq))
}

/// Mark a bus worker as in-flight. Called by try_next_epoch_bus immediately after activation
/// for both ACP and non-ACP tasks. The worker clears this via clear_bus_in_flight once
/// confirm_task_completed succeeds (or via cleanup_run for ACP tasks).
pub(crate) fn mark_bus_in_flight(&mut self, run_id: &str, launch_seq: u64) {
    self.bus_in_flight_workers.insert((run_id.to_string(), launch_seq));
}

/// Clear the in-flight tracker for a bus worker. Only removes the entry if the stored
/// launch_seq still matches — prevents a reassigned replacement from having its marker
/// removed by a lingering old worker that finishes after the new worker has been activated.
///
/// Call sites:
///   - ACP workers: called from cleanup_run (EpochCleanup::drop) when the worker exits,
///     regardless of whether confirm_task_completed succeeded or failed. cleanup_run fires
///     via the RAII guard on any exit path (normal, panic, cancelled).
///   - Non-ACP workers: called explicitly when the worker thread exits (success OR failure,
///     including when confirm_task_completed fails). Must NOT be gated on success — if
///     gated, a failed confirm leaves the marker set forever and the degraded path never fires.
///   - begin_launch: called unconditionally on new launch (see below).
pub(crate) fn clear_bus_in_flight(&mut self, run_id: &str, launch_seq: u64) {
    // Each worker removes exactly its own (run_id, launch_seq) entry.
    // During reassignment, old worker (seq=N) and replacement (seq=N+1) each have their
    // own entry; clearing N does not affect N+1, so the replacement's marker is preserved.
    self.bus_in_flight_workers.remove(&(run_id.to_string(), launch_seq));
}

/// True if ANY bus worker (ACP or non-ACP) is currently executing for any run.
/// Used by the Shutdown handler to decide whether cursor rows can be safely deleted:
/// deleting while a worker is in-flight can cause a subsequent actor to start at
/// the tail and miss the worker's late completion event.
pub(crate) fn any_bus_worker_in_flight(&self) -> bool {
    !self.bus_in_flight_workers.is_empty()
}

/// Bus consumer epoch allocation. Returns None if the task is stale, the run is cancelled,
/// launch_seq is zero, or shutdown_flag is set. Returns Some(0) for non-ACP tasks (is_acp=false).
/// Returns Some(epoch >= 1) for valid ACP tasks.
pub(crate) fn try_next_epoch_bus(
    &mut self,
    run_id:           &str,
    task_launch_seq:  u64,
    is_acp:           bool,
) -> Option<u64> {
    if self.shutdown_flag { return None; }
    // Zero sequences are unconditionally rejected — legacy tasks are identified by
    // process_gen == None and discarded before this method. A zero-sequence
    // current-generation task is malformed; it bypasses the cancellation and staleness guards.
    if task_launch_seq == 0 { return None; }
    if self.cancelled_runs.contains(run_id) { return None; }
    let current_seq = self.run_launch_seq.get(run_id).copied().unwrap_or(0);
    if task_launch_seq != current_seq { return None; }
    // Idempotency guard: reject same-actor re-delivery via has_activated_seq check.
    // When confirm_task_completed() fails, the bus cursor is NOT advanced (at-least-once),
    // so the same task can be re-delivered to the same actor with identical process_gen AND
    // launch_seq. Without this check, try_next_epoch_bus would activate again, allocating a
    // new epoch and spawning a duplicate CLI — violating the exactly-once execution contract.
    // begin_launch clears this entry when a new launch is started, so a relaunch (new seq)
    // is never accidentally rejected.
    //
    // IMPORTANT: callers MUST check has_activated_seq() BEFORE calling try_next_epoch_bus.
    // If has_activated_seq() returns true, the consumer must NOT advance the cursor silently;
    // it must log a critical error (output was lost) and advance only after recording the
    // degraded outcome. See DES-002-actor-teardown.md §Bus consumer call site.
    // try_next_epoch_bus itself still rejects here as a defence-in-depth guard.
    if self.bus_activated_seqs.get(run_id).copied() == Some(task_launch_seq) {
        return None;
    }
    if !is_acp {
        // Non-ACP runner: valid task, run without elicitation.
        // Do NOT increment run_epoch or active_workers (those are ACP-only).
        // Record activation for the idempotency guard AND the in-flight tracker.
        self.bus_activated_seqs.insert(run_id.to_string(), task_launch_seq);
        self.bus_in_flight_workers.insert(run_id.to_string(), task_launch_seq);
        return Some(0);
    }
    let e = self.run_epoch.entry(run_id.to_string()).or_insert(0);
    *e += 1;
    *self.active_workers.entry(run_id.to_string()).or_insert(0) += 1;
    self.bus_activated_seqs.insert(run_id.to_string(), task_launch_seq);
    self.bus_in_flight_workers.insert(run_id.to_string(), task_launch_seq);
    Some(*e)
}
```

---

## `EpochCleanup` RAII guard (src/acp_runner.rs)

Calls `cleanup_run` on ALL exit paths — early returns for fallback/startup/HTTP-ACP, normal exec_turn_acp completion, `StepStatus::Cancelled`, and panics.

```rust
struct EpochCleanup {
    maps:             Arc<Mutex<ElicitationMaps>>,
    run_id:           String,
    epoch:            u64,
    /// The bus launch_seq for this task.  Required to call clear_bus_in_flight inside
    /// cleanup_run.  For non-bus-dispatched tasks this is 0 (no-op — no entry exists).
    launch_seq:       u64,
    /// When true, the bus consumer will call clear_bus_in_flight explicitly AFTER
    /// confirm_task_completed — the in-flight marker must stay set until the completion
    /// is published so that redelivery during publication does not falsely trigger the
    /// degraded path.  Set to true by the bus consumer after exec_turn returns normally.
    ///
    /// When false (the default) — panic, cancellation, or early-exit path — cleanup_run
    /// clears the marker immediately so the degraded path can fire on redelivery.
    bus_in_flight_deferred: bool,
    /// Command sender for Command::EmitEvent(ElicitationResolved) on the panic path.
    tx:               mpsc::Sender<Command>,
    /// Panic-path elicitation ID to emit ElicitationResolved for.
    /// Set at register(); cleared (→ None) after emit_ev(ElicitationResolved) succeeds.
    /// maps.remove() is called inside the drain lock before the adapter write, so
    /// cancel_epoch in drop() cannot find the ID in pending on those panic paths —
    /// in_flight_id bridges the gap.
    in_flight_id:     Option<String>,
    /// Actual action/reason from the wire response, stored by set_in_flight_outcome()
    /// BEFORE rpc_respond so drop() can emit the effective result on panic paths.
    /// None until set; falls back to "cancel"/"teardown" (stale-worker default).
    in_flight_action: Option<String>,
    in_flight_reason: Option<String>,
}

impl Drop for EpochCleanup {
    fn drop(&mut self) {
        let mut maps = self.maps.lock().unwrap_or_else(|p| p.into_inner());
        // cancel_epoch FIRST: drops any pending sender not yet resolved.
        // Returns drained IDs — non-empty only on panic paths where maps.remove()
        // was not yet called.
        let drained_ids = maps.cancel_epoch(&self.run_id, self.epoch);
        // Iterate by reference so drained_ids remains available for the dedup check.
        for elicitation_id in &drained_ids {
            // If the worker panicked BEFORE sending EmitEvent(ElicitationCreated), the actor
            // never ran its ElicitationCreated handler and never called mark_resolution_suppressed.
            // In that case, cancel_epoch inserted the ID into suppressed_creations (creation not
            // announced). Consume that marker here and mark the resolution suppressed so the actor
            // drops the ElicitationResolved event — otherwise an orphaned resolved event fans out
            // for an elicitation subscribers never observed (paired-event contract violation).
            // take_suppressed_creation is idempotent: if the actor already consumed it first,
            // this returns false and mark_resolution_suppressed is a no-op (or already set).
            if maps.take_suppressed_creation(elicitation_id) {
                maps.mark_resolution_suppressed(elicitation_id);
            }
            let _ = self.tx.send(Command::EmitEvent(CoreEvent::ElicitationResolved {
                session:         self.run_id.clone(),
                elicitation_id:  elicitation_id.clone(),
                action:          "cancel".to_string(),
                reason:          "teardown".to_string(),
            }));
        }
        // Dedup: if a panic occurred BEFORE maps.remove(), cancel_epoch may have already
        // drained and emitted for in_flight_id — clear it to avoid a duplicate event.
        if let Some(ref id) = self.in_flight_id {
            if drained_ids.contains(id) {
                self.in_flight_id = None;
            }
        }
        // Emit for in_flight_id if still set — covers panics between maps.remove()
        // (inside the drain lock) and emit_ev(ElicitationResolved).
        // Use the stored action/reason set by set_in_flight_outcome() before rpc_respond;
        // fall back to "cancel"/"teardown" only if a panic occurred before the outcome was set.
        if let Some(elicitation_id) = self.in_flight_id.take() {
            let _ = self.tx.send(Command::EmitEvent(CoreEvent::ElicitationResolved {
                session:         self.run_id.clone(),
                elicitation_id,
                action:          self.in_flight_action.take().unwrap_or_else(|| "cancel".to_string()),
                reason:          self.in_flight_reason.take().unwrap_or_else(|| "teardown".to_string()),
            }));
        }
        // cleanup_run SECOND: removes tombstone, decrements active_workers, and clears
        // the bus_in_flight_workers entry for this (run_id, launch_seq).  Passing launch_seq
        // ensures the sequence-aware guard in clear_bus_in_flight only removes the marker if
        // the stored seq still matches — safe even if a reassignment already replaced it.
        maps.cleanup_run(&self.run_id, self.epoch, self.launch_seq);
    }
}

impl EpochCleanup {
    /// Call BEFORE rpc_respond (the adapter write) so drop() knows the intended outcome.
    /// set_in_flight_outcome("accept", "user") before the write; the guard then emits
    /// the correct action even if a panic occurs between rpc_respond and ElicitationResolved.
    pub(crate) fn set_in_flight_outcome(&mut self, action: &str, reason: &str) {
        self.in_flight_action = Some(action.to_string());
        self.in_flight_reason = Some(reason.to_string());
    }
}
```

### Installing the guard in `AcpStepRunner::exec_turn`

```rust
// Guard is only installed for epoch > 0. Epoch 0 means elicitation is disabled
// (no active_workers entry was allocated via next_epoch); installing it would call
// cleanup_run(run_id, 0) which decrements a counter that was never incremented.
let mut _epoch_guard: Option<EpochCleanup> = if input.elicitation_epoch > 0 {
    Some(EpochCleanup {
        maps:             Arc::clone(&self.elicitation_maps),
        run_id:           run_id.to_string(),
        epoch:            input.elicitation_epoch,
        // launch_seq is needed so cleanup_run can call clear_bus_in_flight.
        // For non-bus-dispatched turns input.launch_seq is 0 → clear_bus_in_flight is a no-op
        // (no bus_in_flight_workers entry ever has seq 0 for live bus tasks).
        launch_seq:            input.launch_seq,
        bus_in_flight_deferred: false,  // set true by bus consumer after exec_turn returns normally
        tx:                    self.tx.clone(),
        in_flight_id:          None,
        in_flight_action: None,
        in_flight_reason: None,
    })
} else {
    None  // epoch 0: no cleanup needed
};
// Pass to exec_turn_acp:
exec_turn_acp(..., _epoch_guard.as_mut())
// Use as_mut() — NOT as_deref_mut(). EpochCleanup does not implement DerefMut,
// so Option::<EpochCleanup>::as_deref_mut() is unavailable (E0277).
```

Inside `exec_turn_acp`, every set/clear uses `if let Some(ref mut g) = epoch_guard` (not `if let Some(g)`) to avoid moving the `Option<&mut EpochCleanup>` (E0382 use-after-move):

```rust
// After register() succeeds:
if let Some(ref mut g) = epoch_guard { g.in_flight_id = Some(elicitation_id.clone()); }

// After emit_ev(ElicitationResolved):
if let Some(ref mut g) = epoch_guard { g.in_flight_id = None; }
```

---

## Session generation: `drop_session_gen`

`SessionMap` stores entries keyed by `(run_id, cli_key)` with an associated monotonic `session_gen: u64`. On insert, atomically increment a per-`(run_id, cli_key)` counter; assign the incremented value as the entry's generation.

In the worker: after `sessions.insert(run_id, cli_key, session_data)`, capture the returned `session_gen` as `my_session_gen: u64`. Every worker-driven eviction call passes all three arguments:

```rust
self.drop_session_gen(&run_id, &cli_key, my_session_gen)
// cli_key is required: session_gen is monotonic per (run_id, cli_key).
// Two CLI sessions in one run can share the same generation number.
// Omitting cli_key would evict an unrelated warm CLI session.
```

`drop_session_gen` removes the `(run_id, cli_key)` entry ONLY if its stored generation matches `my_session_gen`. A replacement that inserted a newer entry (higher generation) is not evicted.

---

## `TurnResult`

```rust
struct TurnResult {
    // ... existing fields (output, usage, files) ...
    /// Human explicitly dismissed an elicitation (Ok{action:"cancel"} via resolveElicitation).
    cancelled:              bool,
    /// Elicitation-arm deadline expiry, adapter stdout disconnect, or run teardown.
    /// NOT set for ordinary (non-elicitation) turn timeouts or stopReason=="cancelled".
    elicitation_timed_out:  bool,
    /// Cancel-response write failed (broken stdin on suppressed path).
    /// AcpStepRunner::exec_turn checks this and calls drop_session_gen before returning.
    dead_session:           bool,
    /// Accept/decline response write failed for a non-suppressed path.
    /// exec_turn_acp returns Ok(TurnResult { write_failed_terminal: true }); exec_turn
    /// calls drop_session_gen and returns StepStatus::ElicitationFailed.
    write_failed_terminal:  bool,
}
```

### `TurnResult::default_at` constructor contract

```rust
// Produces a base result with all boolean flags false and status: StepStatus::Failed.
// The explicit StepStatus::Failed is required — a default that leaves status unspecified
// would produce StepStatus::Ok, incorrectly reporting a broken-transport exit as success.
fn default_at(output: String, usage: Option<Usage>, files: Vec<String>) -> TurnResult {
    // `output` is String, not Vec<String> — matches the exec_turn_acp accumulator type.
    // `files` is Vec<String> (file paths) — no File type exists in this codebase.
    // All call sites pass the accumulated String output (empty String for early-exit arms).
    TurnResult {
        status:                StepStatus::Failed,
        cancelled:             false,
        elicitation_timed_out: false,
        write_failed_terminal: false,
        dead_session:          false,
        output,
        usage,
        files,
    }
}
```

Usage at all immediate-cancel write failure sites (disabled arm, unsupported mode, etc.):
```rust
return Ok(TurnResult { dead_session: true, ..TurnResult::default_at(output, usage, files) });
```

Usage at the write_failed_terminal site (accept/decline write failure):
```rust
return Ok(TurnResult { write_failed_terminal: true, ..TurnResult::default_at(output, usage, files) });
```

Usage at the cancelled-startup and Err arms in `exec_turn`:
```rust
return StepOutput { status: StepStatus::ElicitationFailed, ..input.blank_step_output() };
// input.blank_step_output() returns a zero-output StepOutput with all header fields from input.
// StepOutput.output is String (not Vec).
```

---

## `AcpStepRunner` fields and constructors

```rust
pub(crate) struct AcpStepRunner {
    // ... existing fields ...
    elicitation_maps:   Arc<Mutex<ElicitationMaps>>,
    /// true ↔ this runner's maps Arc is the same Arc shared with the actor.
    /// If false, register creates a sender the actor can never deliver to (7200s hang).
    elicitation_shared: bool,
}
```

- `AcpStepRunner::new(...)` — sets `elicitation_shared = false`; constructs a fresh `Arc::new(Mutex::new(ElicitationMaps::new()))`. All ~13 existing callers continue to compile unchanged.
- `AcpStepRunner::new_with_maps(tx, arc, write_reg: WriteReg)` — sets `elicitation_shared = true`; stores `write_reg` so child sessions are registered in the actor's shared registry for cancellation sweeps. Used in `spawn_with_acp_sessions`.

`pub(crate) fn elicitation_maps(&self) -> &Arc<Mutex<ElicitationMaps>>` — accessor for the `debug_assert!(Arc::ptr_eq(...))` in `spawn_with_acp_sessions`.

**Epoch is per-dispatch, not per-runner.** `Core::spawn_with_acp_sessions` constructs one `AcpStepRunner` shared across every run. The epoch is captured as a local `u64` at dispatch time and threaded into `exec_turn_acp` as an explicit parameter via `StepInput.elicitation_epoch`.

```rust
pub struct StepInput {
    // ... existing fields ...
    /// Dispatch-time epoch from ElicitationMaps::next_epoch.
    /// 0 for non-ACP runners and tool_cmd units.
    pub elicitation_epoch: u64,
    /// Cross-restart staleness guard. Option<Uuid>: None for legacy bus payloads (no process_gen
    /// field), Some for all current-generation dispatches. Local-dispatch path wraps the bare
    /// uuid::Uuid from dispatch_unit with Some(process_gen).
    pub process_gen:       Option<uuid::Uuid>,
    /// Per-launch monotonic counter for stale-completion rejection.
    pub launch_seq:        u64,
}
```

→ See DES-002-exec-turn-acp.md for how `exec_turn_acp` uses `run_epoch`.
→ See DES-002-actor-teardown.md for `dispatch_unit` epoch allocation and `actor::run` signature.
