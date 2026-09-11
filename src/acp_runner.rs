//! ACP (Agent Client Protocol) session runner — multi-CLI extension of wicked-core#13.
//!
//! Drives persistent multi-turn sessions using the standardised JSON-RPC 2.0 ndjson
//! (stdin/stdout) ACP protocol. Each CLI runs its own ACP server — a wrapper binary
//! or the CLI's native ACP mode; wicked-core is the ACP client. The registry maps CLI
//! keys to their ACP invocation:
//!
//! | CLI      | ACP binary / invocation                              | Transport |
//! |----------|------------------------------------------------------|-----------|
//! | claude   | claude-agent-acp (@agentclientprotocol, Agent SDK)   | stdio     |
//! | codex    | codex-acp (@agentclientprotocol, Rust)               | stdio     |
//! | pi       | pi-acp (community adapter)                           | stdio     |
//! | agy      | agy-acp (wicked-crew packages/agent-acp-bridges)     | stdio     |
//! | copilot  | copilot --acp (native)                               | stdio     |
//! | opencode | opencode acp (native)                                | stdio     |
//!
//! When an ACP binary is unavailable or fails during the handshake, `AcpStepRunner`
//! emits a warning and prepends it to `StepOutput.output` so it is visible in both
//! streaming and persisted contexts. The run then continues with single-shot fallback.
//! HTTP transport is not yet implemented (no registry entry uses it today).
//!
//! # Session lifecycle
//! - **Open (lazy)**: on the first unit for a `(run_id, cli_key)` pair, the binary is
//!   spawned and the `initialize` + `session/new` JSON-RPC handshake completes.
//! - **Reuse**: subsequent units send `session/prompt` to the same process and stream
//!   `session/update` text chunks until `stopReason` arrives — sharing prompt-cache
//!   across governance turns without a per-unit cold start.
//! - **Close**: [`AcpStepRunner::drop_session`] kills all CLI processes for a `run_id`.
//!   Call it after the last unit of a run (mirrors [`PersistentStepRunner::drop_session`]).
//!
//! # Protocol
//! JSON-RPC 2.0 ndjson over stdin/stdout. Non-JSON startup banners and log lines
//! are silently skipped during both handshake and turn execution.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::process::{ChildStdin, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::command::Command;
use crate::event::CoreEvent;
use crate::execute_wrapped::{unit_prompt, SkillForm, WrappedCliStepRunner};
use crate::workflow::{
    DeltaSink, PriorUnitOutput, StepInput, StepOutput, StepRunner, StepStatus, Usage,
};
use wicked_apps_core::HardenedCommand;
use wicked_council::types::{AcpConfig, AcpTransport};

// ── KillHandle and WriteReg (DES-002 T6) ─────────────────────────────────────

/// A kill handle for an in-flight ACP child process.
///
/// Carries an `Arc<Mutex<Option<Child>>>` so that multiple callers (teardown step 1,
/// step 6 second sweep, `EpochCleanup::drop`) can all safely signal the child without
/// PID-reuse races. After the first `signal()` takes the child, subsequent calls are no-ops.
pub struct KillHandle {
    inner: Mutex<Option<std::process::Child>>,
}

impl KillHandle {
    /// Construct a no-op handle for tests (no child to kill).
    pub fn noop() -> Self {
        Self {
            inner: Mutex::new(None),
        }
    }

    /// Construct a handle that will kill `child` on `signal()`.
    pub fn new(child: std::process::Child) -> Self {
        Self {
            inner: Mutex::new(Some(child)),
        }
    }

    /// Report the child's exit status if it has ALREADY exited — non-blocking, never killing
    /// (`try_wait` leaves an unexited child untouched, and reaps one that has exited). `None`
    /// when the child is still running,
    /// was already taken by `signal()`, or this is a no-op handle. crew#267: the SESSION_DIED
    /// arms use this so a bridge death reports HOW the process ended, not only that it stopped
    /// answering.
    pub fn try_exit_status(&self) -> Option<std::process::ExitStatus> {
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        guard.as_mut().and_then(|c| c.try_wait().ok().flatten())
    }

    /// Kill and reap the child process. Idempotent: the first call kills; subsequent calls
    /// are no-ops (the child has been taken). Releases the mutex before `wait()` so a
    /// concurrent `signal()` on another thread never deadlocks.
    pub fn signal(&self) {
        let taken = {
            let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            guard.take()
        };
        if let Some(mut child) = taken {
            // The bridge was spawned in its OWN process group (`process_group(0)` at spawn), so
            // kill the GROUP — the bridge, the CLI it wraps and anything either backgrounded — and
            // reap BOUNDED (adversarial review on #414: a direct-child kill left a bridge's shell
            // children alive to write past a no-code unit's final snapshot).
            crate::validator::kill_child_tree(&mut child);
            crate::validator::reap_bounded(&mut child);
        }
    }
}

/// Per-session handles stored in the write-lock registry.
pub type SessionHandles = (Arc<Mutex<()>>, Arc<KillHandle>);

/// The write-lock session registry.
///
/// Key: `(run_id, session_key, launch_seq)` — the launch token prevents a torn eviction when
/// a replacement session has the same `(run_id, cli_key)` pair as the one being evicted.
/// Value: `(write_lock, kill_handle)` — `shared_run_terminal` uses these to serialise
/// teardown with in-flight writes and to signal the child process.
///
/// Created in `spawn_with_acp_sessions` (NOT in `actor::run`) so the runner and actor
/// share the same `Arc`. PTY and injected runners receive an empty registry — their
/// sessions have no ACP child to signal.
pub type WriteReg = Arc<Mutex<HashMap<(String, String, u64), SessionHandles>>>;

// ── ElicitationMaps (DES-002) ─────────────────────────────────────────────────

/// A human response delivered via `resolveElicitation` — the value that
/// unblocks the `'elicit` dual-poll loop inside `exec_turn_acp`.
#[derive(Debug, Clone)]
pub struct ElicitationResult {
    pub action: String,
    pub response: Option<serde_json::Value>,
}

/// One in-flight elicitation registration; lives in `ElicitationMaps::pending`
/// until `remove` (normal), `deliver` (resolved), or `cancel_epoch` (terminal).
struct ElicitationEntry {
    run_id: String,
    epoch: u64,
    /// Filtered options shown to the operator. Retained so `deliver` can reject a
    /// stale or forged selection without consuming the pending elicitation.
    options: Option<Vec<String>>,
    /// Rendezvous channel to `exec_turn_acp`'s dual-poll loop. `SyncSender<_>` with
    /// capacity 1 so the actor never blocks on send (I-8: no Tokio runtime in wicked-core).
    tx: std::sync::mpsc::SyncSender<ElicitationResult>,
}

/// The single shared coordination point for all ACP elicitation state.
///
/// One `Arc<Mutex<ElicitationMaps>>` lives in `AcpStepRunner` and is threaded
/// through to every `exec_turn_acp` invocation and to the actor's
/// `Command::ResolveElicitation` handler. Every mutation must acquire this lock
/// (O(1) hold time — only HashMap ops); the dual-poll loop in `exec_turn_acp`
/// releases it before sleeping.
///
/// # Bus-consumer coordination fields
///
/// Several fields coordinate the actor with the off-actor CLI bus consumer
/// (T7 / `cli_runner.rs`):
///
/// - `bus_in_flight_workers`: `HashSet<(run_id, launch_seq)>` — one entry per
///   live bus-dispatched worker. Tracked independently so a reassigned run
///   (two workers alive simultaneously) doesn't lose the older entry.
/// - `bus_activated_seqs`: maps `run_id → highest launch_seq` that crossed the
///   ack-gated path; used for the degraded-mode bus dispatch check.
/// - `run_launch_seq`: monotonic per-run counter incremented at every
///   `begin_launch`; forms the second coordinate of the stale-completion guard.
pub struct ElicitationMaps {
    /// `elicitation_id → ElicitationEntry` for in-flight registrations.
    pending: HashMap<String, ElicitationEntry>,
    /// `run_id → [(elicitation_id, epoch)]` for bulk cancel/cleanup.
    run_index: HashMap<String, Vec<(String, u64)>>,
    /// Exact `(run_id, launch_seq)` tokens for ACP workers currently alive. Exact
    /// tokens make completion-publication retries idempotent and keep concurrent
    /// reassignments independent.
    active_workers: HashSet<(String, u64)>,
    /// `(run_id, epoch)` pairs marked as cancelled; `register` checks this before
    /// adding to `pending` (creation suppression for late-arriving registrations).
    cancelled_epochs: Vec<(String, u64)>,
    /// Elicitation ids whose `ElicitationCreated` event was already emitted; the
    /// `exec_turn_acp` creation-announcement guard checks this so a retry does not
    /// re-emit the event.
    suppressed_creations: HashSet<String>,
    /// `(run_id, launch_seq)` for every live bus-dispatched worker (see module doc).
    bus_in_flight_workers: HashSet<(String, u64)>,
    /// `run_id → highest launch_seq` that reached the ack-gated cursor-advance
    /// path; used for the degraded-mode dispatch check in the bus consumer.
    bus_activated_seqs: HashMap<String, u64>,
    /// Per-run monotonic launch counter. Incremented at every `begin_launch`.
    run_launch_seq: HashMap<String, u64>,
    /// Set to `true` when the actor enters shutdown; workers poll this so they
    /// can exit early rather than block on a unit that will never finish.
    shutdown_flag: bool,
    // ── DES-002 T6 additions ─────────────────────────────────────────────────────
    /// `run_id → current epoch` — tracks the live epoch per run. Populated by
    /// `next_epoch`; used by `has_active_run` and `current_epoch`.
    /// Zero is not stored (only epochs ≥ 1 represent active runs).
    run_epoch: HashMap<String, u64>,
    /// Dispatch-mode-agnostic tombstone set. Populated by `tombstone_run` (CancelRun
    /// universal path) and `tombstone_bus_run` (shared_run_terminal bus guard).
    /// `is_run_cancelled` checks this so `try_next_epoch_bus` can reject stale bus tasks
    /// for both locally-cancelled and bus-cancelled runs.
    all_cancelled_runs: HashSet<String>,
    /// Elicitation ids for which `ElicitationCreated` has been announced to subscribers.
    /// Used by the EmitEvent suppression guard: once announced, a concurrent `cancel_epoch`
    /// must NOT insert a stale suppression marker (the event is already out).
    creation_announced: HashSet<String>,
    /// Elicitation ids whose paired `ElicitationResolved` event must be suppressed.
    /// Set when the `ElicitationCreated` was suppressed; cleared by `take_suppressed_resolution`.
    suppressed_resolutions: HashSet<String>,
}

impl ElicitationMaps {
    pub fn new() -> Self {
        Self {
            pending: HashMap::new(),
            run_index: HashMap::new(),
            active_workers: HashSet::new(),
            cancelled_epochs: Vec::new(),
            suppressed_creations: HashSet::new(),
            bus_in_flight_workers: HashSet::new(),
            bus_activated_seqs: HashMap::new(),
            run_launch_seq: HashMap::new(),
            shutdown_flag: false,
            run_epoch: HashMap::new(),
            all_cancelled_runs: HashSet::new(),
            creation_announced: HashSet::new(),
            suppressed_resolutions: HashSet::new(),
        }
    }

    /// Register a new elicitation and return the receiver end of the reply channel.
    ///
    /// Fails (returns `None`) when the epoch was already cancelled via
    /// `cancel_epoch` (creation-suppression guard). On success, `pending` and
    /// `run_index` are updated atomically under the caller's lock.
    ///
    /// The message is byte-capped at 8 KB (truncated); individual `options` entries
    /// larger than 512 bytes are dropped; `options` list is capped at 100 entries;
    /// empty-string options entries are dropped.
    #[allow(clippy::type_complexity)]
    pub fn register(
        &mut self,
        run_id: &str,
        epoch: u64,
        elicitation_id: &str,
        message: &str,
        options: Option<Vec<String>>,
        prop_key: &str,
    ) -> Option<(
        std::sync::mpsc::Receiver<ElicitationResult>,
        String,
        Option<Vec<String>>,
        String,
    )> {
        // Creation-suppression guard: if the epoch was already cancelled, refuse.
        if self
            .cancelled_epochs
            .iter()
            .any(|(r, e)| r == run_id && *e == epoch)
        {
            return None;
        }

        // Cap message at 8 KB byte-length (not character count).
        const MSG_CAP: usize = 8 * 1024;
        let message = if message.len() > MSG_CAP {
            // Truncate on a UTF-8 boundary and append marker.
            let mut truncated = message[..msg_floor_at(message, MSG_CAP)].to_string();
            truncated.push_str("[truncated]");
            truncated
        } else {
            message.to_string()
        };

        // Filter options: drop entries > 512 bytes or empty string; cap list at 100.
        let options = options.map(|opts| {
            const OPT_CAP: usize = 512;
            const LIST_CAP: usize = 100;
            let mut filtered: Vec<String> = opts
                .into_iter()
                .filter(|o| {
                    if o.is_empty() {
                        return false;
                    }
                    if o.len() > OPT_CAP {
                        tracing::warn!(
                            elicitation_id,
                            "options entry exceeds {} bytes — dropped",
                            OPT_CAP
                        );
                        return false;
                    }
                    true
                })
                .collect();
            filtered.truncate(LIST_CAP);
            filtered
        });

        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let entry = ElicitationEntry {
            run_id: run_id.to_string(),
            epoch,
            options: options.clone(),
            tx,
        };
        self.pending.insert(elicitation_id.to_string(), entry);
        self.run_index
            .entry(run_id.to_string())
            .or_default()
            .push((elicitation_id.to_string(), epoch));
        Some((rx, message, options, prop_key.to_string()))
    }

    /// Remove a registration from `pending` and `run_index`.
    ///
    /// Called AFTER `ElicitationResolved` has been emitted (happy path) or
    /// immediately when a terminal path fires. Idempotent: a missing id is a no-op.
    pub fn remove(&mut self, run_id: &str, elicitation_id: &str) {
        self.pending.remove(elicitation_id);
        self.creation_announced.remove(elicitation_id);
        if let Some(v) = self.run_index.get_mut(run_id) {
            v.retain(|(id, _)| id != elicitation_id);
            if v.is_empty() {
                self.run_index.remove(run_id);
            }
        }
    }

    /// Deliver a human response to a waiting `exec_turn_acp` dual-poll loop.
    ///
    /// Returns `Err` if `elicitation_id` is unknown or the `run_id` does not match
    /// the registered entry (cross-run delivery guard).
    pub fn deliver(
        &mut self,
        run_id: &str,
        elicitation_id: &str,
        action: String,
        response: Option<serde_json::Value>,
    ) -> anyhow::Result<()> {
        let entry = self
            .pending
            .get(elicitation_id)
            .ok_or_else(|| anyhow::anyhow!("elicitation not found: {}", elicitation_id))?;
        if entry.run_id != run_id {
            anyhow::bail!(
                "elicitation {} belongs to run {}, not {}",
                elicitation_id,
                entry.run_id,
                run_id
            );
        }
        if !matches!(action.as_str(), "accept" | "decline" | "cancel") {
            anyhow::bail!(
                "elicitation {} has invalid action {:?}; expected accept, decline, or cancel",
                elicitation_id,
                action
            );
        }
        if action == "accept" {
            let value = response
                .as_ref()
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "elicitation {} requires a string response for action=accept",
                        elicitation_id
                    )
                })?;
            const FREE_TEXT_CAP: usize = 64 * 1024;
            match &entry.options {
                Some(options) if !options.iter().any(|option| option == value) => {
                    anyhow::bail!(
                        "elicitation {} response is not one of the allowed options",
                        elicitation_id
                    );
                }
                None if value.len() > FREE_TEXT_CAP => {
                    anyhow::bail!(
                        "elicitation {} response exceeds {} bytes",
                        elicitation_id,
                        FREE_TEXT_CAP
                    );
                }
                _ => {}
            }
        }

        // Remove before sending. This makes a second resolution fail immediately instead
        // of blocking the single actor thread on an already-full sync channel.
        let entry = self
            .pending
            .remove(elicitation_id)
            .ok_or_else(|| anyhow::anyhow!("elicitation already resolved: {}", elicitation_id))?;
        if let Some(entries) = self.run_index.get_mut(run_id) {
            entries.retain(|(id, _)| id != elicitation_id);
            if entries.is_empty() {
                self.run_index.remove(run_id);
            }
        }
        self.creation_announced.remove(elicitation_id);
        entry
            .tx
            .send(ElicitationResult { action, response })
            .map_err(|_| anyhow::anyhow!("elicitation worker exited before receiving the response"))
    }

    /// Cancel all pending elicitations for `(run_id, epoch)`.
    ///
    /// Records the cancelled epoch (creation-suppression) and sends a synthetic
    /// `action:"cancel"` on every matching entry's channel. Idempotent.
    pub fn cancel_epoch(&mut self, run_id: &str, epoch: u64) {
        // Record for creation-suppression guard.
        if !self
            .cancelled_epochs
            .iter()
            .any(|(r, e)| r == run_id && *e == epoch)
        {
            self.cancelled_epochs.push((run_id.to_string(), epoch));
        }
        // Cancel all in-flight elicitations for this (run, epoch).
        let ids_to_cancel: Vec<String> = self
            .pending
            .iter()
            .filter(|(_, e)| e.run_id == run_id && e.epoch == epoch)
            .map(|(id, _)| id.clone())
            .collect();
        for id in &ids_to_cancel {
            if let Some(entry) = self.pending.remove(id) {
                let _ = entry.tx.send(ElicitationResult {
                    action: "cancel".to_string(),
                    response: None,
                });
            }
            if !self.creation_announced.remove(id) {
                self.suppressed_creations.insert(id.clone());
            }
        }
        if let Some(entries) = self.run_index.get_mut(run_id) {
            entries.retain(|(id, entry_epoch)| {
                *entry_epoch != epoch || !ids_to_cancel.iter().any(|cancelled| cancelled == id)
            });
            if entries.is_empty() {
                self.run_index.remove(run_id);
            }
        }
    }

    /// Increment `active_workers` and advance the per-run launch sequence.
    ///
    /// Returns the new monotonically-increasing `launch_seq` for this dispatch.
    /// Zero is reserved as a sentinel (`try_next_epoch_bus` unconditionally rejects 0).
    ///
    /// `tracks_elicitation_worker` records the exact launch token for ACP units;
    /// non-ACP launches still receive a sequence but do not participate in epoch cleanup.
    ///
    /// Does NOT clear `bus_in_flight_workers` — each worker manages its own
    /// `(run_id, launch_seq)` entry independently (re-assignment invariant).
    pub fn begin_launch(&mut self, run_id: &str, tracks_elicitation_worker: bool) -> u64 {
        let launch_seq = self.advance_launch_seq(run_id);
        if tracks_elicitation_worker {
            self.active_workers.insert((run_id.to_string(), launch_seq));
        }
        // A genuine new launch supersedes any terminal tombstone from an earlier attempt.
        self.all_cancelled_runs.remove(run_id);
        self.bus_activated_seqs.remove(run_id);
        launch_seq
    }

    /// Record that a bus-dispatched worker for `(run_id, launch_seq)` is now in-flight.
    pub fn mark_bus_in_flight(&mut self, run_id: &str, launch_seq: u64) {
        self.bus_in_flight_workers
            .insert((run_id.to_string(), launch_seq));
    }

    /// Check whether a specific `(run_id, launch_seq)` worker is still in-flight.
    pub fn is_bus_worker_in_flight(&self, run_id: &str, launch_seq: u64) -> bool {
        self.bus_in_flight_workers
            .contains(&(run_id.to_string(), launch_seq))
    }

    /// Remove the in-flight marker for `(run_id, launch_seq)`.
    ///
    /// Called by the bus consumer's ack-gated cursor advance AFTER the actor has
    /// committed `ApplyStepResult` (normal completion), or immediately by
    /// `EpochCleanup::drop` on panic/cancel.
    pub fn clear_bus_in_flight(&mut self, run_id: &str, launch_seq: u64) {
        self.bus_in_flight_workers
            .remove(&(run_id.to_string(), launch_seq));
    }

    /// Roll back activation after `task.completed` publication fails. The dispatch cursor
    /// is intentionally left behind, so the next poll may execute this task again.
    pub fn reset_bus_activation(&mut self, run_id: &str, launch_seq: u64) {
        self.clear_bus_in_flight(run_id, launch_seq);
        if self.bus_activated_seqs.get(run_id) == Some(&launch_seq) {
            self.bus_activated_seqs.remove(run_id);
        }
    }

    /// Returns `true` if ANY bus-dispatched worker is still in-flight (across all runs).
    pub fn any_bus_worker_in_flight(&self) -> bool {
        !self.bus_in_flight_workers.is_empty()
    }

    /// Remove and return whether `elicitation_id` was in the creation-suppressed set.
    pub fn take_suppressed_creation(&mut self, elicitation_id: &str) -> bool {
        self.suppressed_creations.remove(elicitation_id)
    }

    /// Advance the per-run launch sequence counter and return the NEW value.
    ///
    /// Starts from 1 on first call for a run (0 is reserved for "no launch_seq").
    pub fn advance_launch_seq(&mut self, run_id: &str) -> u64 {
        let seq = self.run_launch_seq.entry(run_id.to_string()).or_insert(0);
        *seq += 1;
        *seq
    }

    /// Restore the per-run launch sequence counter to `seq` (rollback on dispatch failure).
    pub fn restore_launch_seq(&mut self, run_id: &str, seq: u64) {
        self.run_launch_seq.insert(run_id.to_string(), seq);
    }

    /// Returns `true` if `run_id` has an active epoch (≥ 1) allocated via `next_epoch`.
    ///
    /// - Returns `false` for PTY runs (never call `next_epoch`; no entry in `run_epoch`).
    /// - Returns `false` for `tool_cmd` units (epoch allocated as 0; entry not inserted).
    /// - Returns `false` after `cleanup_run` runs (removes the `run_epoch` entry when
    ///   `active_workers` reaches 0).
    /// - Returns `true` for ACP workers that called `next_epoch` and have not yet exited.
    pub fn has_active_run(&self, run_id: &str) -> bool {
        self.run_epoch.get(run_id).is_some_and(|&e| e > 0)
    }

    /// Returns `true` if the specific `(run_id, launch_seq)` has crossed the ack-gated
    /// activation path (i.e. `bus_activated_seqs[run_id] >= launch_seq`).
    pub fn has_activated_seq(&self, run_id: &str, launch_seq: u64) -> bool {
        self.bus_activated_seqs
            .get(run_id)
            .is_some_and(|&s| s >= launch_seq)
    }

    /// Set the shutdown flag. Workers poll this and exit early.
    pub fn set_shutdown_flag(&mut self) {
        self.shutdown_flag = true;
    }

    /// Whether the shutdown flag has been set.
    pub fn is_shutdown(&self) -> bool {
        self.shutdown_flag
    }

    /// Whether the shutdown flag has been set (alias used in EmitEvent suppression guard).
    pub fn shutdown_flag(&self) -> bool {
        self.shutdown_flag
    }

    /// Decrement `active_workers` and prune all registrations for `(run_id, epoch)`.
    ///
    /// Called ONLY by `EpochCleanup::drop` — the sole call site ensures no
    /// double-decrement of `active_workers` (spec Never do).
    ///
    /// Does NOT clear `bus_in_flight_workers` — the `bus_in_flight_deferred` flag
    /// on `EpochCleanup` decides that; on panic/cancel it's cleared before this call;
    /// on normal bus completion the bus consumer clears it after the cursor advance.
    pub fn cleanup_run(&mut self, run_id: &str, epoch: u64, launch_seq: u64) {
        self.active_workers
            .remove(&(run_id.to_string(), launch_seq));
        let last_worker_for_run = !self
            .active_workers
            .iter()
            .any(|(active_run, _)| active_run == run_id);
        // Remove all pending registrations for this (run_id, epoch).
        if let Some(ids) = self.run_index.get(run_id) {
            let to_remove: Vec<String> = ids
                .iter()
                .filter(|(_, e)| *e == epoch)
                .map(|(id, _)| id.clone())
                .collect();
            for id in &to_remove {
                self.pending.remove(id);
            }
        }
        if let Some(v) = self.run_index.get_mut(run_id) {
            v.retain(|(_, e)| *e != epoch);
            if v.is_empty() {
                self.run_index.remove(run_id);
            }
        }
        // Prune the cancelled_epochs list for this (run_id, epoch).
        self.cancelled_epochs
            .retain(|(r, e)| !(r == run_id && *e == epoch));
        // When no workers remain for this run, remove the run_epoch entry so
        // `has_active_run` returns false (prevents stale tombstones on reuse).
        if last_worker_for_run {
            self.run_epoch.remove(run_id);
            self.cancelled_epochs.retain(|(r, _)| r != run_id);
        }
    }

    // ── DES-002 T6: epoch lifecycle methods ──────────────────────────────────────

    /// Allocate the next epoch for `run_id` and store it in `run_epoch`.
    ///
    /// Each call increments the per-run epoch counter. Epoch 0 is never returned
    /// (counter starts at 1 on first call). After this, `has_active_run(run_id)` returns `true`.
    pub fn next_epoch(&mut self, run_id: &str) -> u64 {
        let epoch = self.run_epoch.entry(run_id.to_string()).or_insert(0);
        *epoch += 1;
        *epoch
    }

    /// Return the current epoch for `run_id`, or 0 if none is allocated.
    pub fn current_epoch(&self, run_id: &str) -> u64 {
        *self.run_epoch.get(run_id).unwrap_or(&0)
    }

    /// Returns `true` if `(run_id, epoch)` was tombstoned via `cancel_epoch`.
    pub fn is_epoch_cancelled(&self, run_id: &str, epoch: u64) -> bool {
        self.cancelled_epochs
            .iter()
            .any(|(r, e)| r == run_id && *e == epoch)
    }

    /// Return all run-ids that currently have an active epoch (epoch ≥ 1).
    /// Used by `Command::Shutdown` to tombstone all active epochs in one lock hold.
    pub fn active_run_ids(&self) -> Vec<String> {
        self.run_epoch
            .iter()
            .filter(|(_, &e)| e > 0)
            .map(|(r, _)| r.clone())
            .collect()
    }

    /// Tombstone `run_id` for bus-dispatched tasks — inserts into `all_cancelled_runs`
    /// so `is_run_cancelled` returns true and `try_next_epoch_bus` rejects stale tasks.
    /// Called unconditionally from `shared_run_terminal` step 3 (no `has_active_run` guard).
    pub fn tombstone_bus_run(&mut self, run_id: &str) {
        self.all_cancelled_runs.insert(run_id.to_string());
    }

    /// Universal tombstone — inserts `run_id` into `all_cancelled_runs` so
    /// `is_run_cancelled` returns true for both local and bus dispatch paths.
    /// Called by `CancelRun` after `advance_launch_seq`.
    pub fn tombstone_run(&mut self, run_id: &str) {
        self.all_cancelled_runs.insert(run_id.to_string());
    }

    /// Returns `true` if `run_id` was tombstoned via `tombstone_run` or `tombstone_bus_run`.
    pub fn is_run_cancelled(&self, run_id: &str) -> bool {
        self.all_cancelled_runs.contains(run_id)
    }

    /// Return the current launch sequence for `run_id`, or 0 if none.
    pub fn current_launch_seq(&self, run_id: &str) -> u64 {
        *self.run_launch_seq.get(run_id).unwrap_or(&0)
    }

    /// Clear tombstone state for `run_id` after it has gone terminal (all bus tasks stale).
    /// Called after `advance_launch_seq` so any in-flight bus tasks are invalidated
    /// before the tombstone is removed.
    pub fn retire_launch_state(&mut self, run_id: &str) {
        self.all_cancelled_runs.remove(run_id);
    }

    /// Mark the paired `ElicitationResolved` for `elicitation_id` as suppressed.
    /// Called when `ElicitationCreated` was suppressed so subscribers never see
    /// a resolved event for an elicitation they never observed.
    pub fn mark_resolution_suppressed(&mut self, elicitation_id: &str) {
        self.suppressed_resolutions
            .insert(elicitation_id.to_string());
    }

    /// Mark `elicitation_id` as announced (its `ElicitationCreated` event was fanned out).
    /// After this, a concurrent `cancel_epoch` will skip inserting a stale suppression marker.
    pub fn mark_creation_announced(&mut self, elicitation_id: &str) {
        self.creation_announced.insert(elicitation_id.to_string());
    }

    /// Remove and return whether `elicitation_id` was in the suppressed-resolutions set.
    /// Returns `true` if it was suppressed (and removes it); `false` otherwise.
    pub fn take_suppressed_resolution(&mut self, elicitation_id: &str) -> bool {
        self.suppressed_resolutions.remove(elicitation_id)
    }

    /// Returns `true` if `elicitation_id` is still registered in `pending`.
    pub fn is_pending(&self, elicitation_id: &str) -> bool {
        self.pending.contains_key(elicitation_id)
    }

    /// Bus consumer epoch activation.
    ///
    /// Called from the bus consumer when consuming a `DispatchedTask`. Returns the
    /// allocated epoch (`is_acp=true`) or `0` (`is_acp=false`), or `None` if the task
    /// should be discarded (cancelled, stale, or malformed).
    ///
    /// Rejects when:
    /// - `launch_seq == 0` (sentinel / malformed)
    /// - `is_run_cancelled(run_id)` (run was tombstoned)
    /// - `launch_seq < current_launch_seq(run_id)` (stale; superseded by reassign)
    ///
    /// On success, marks `(run_id, launch_seq)` as bus-in-flight.
    pub fn try_next_epoch_bus(
        &mut self,
        run_id: &str,
        launch_seq: u64,
        is_acp: bool,
    ) -> Option<u64> {
        // Unconditionally reject the sentinel / malformed case.
        if launch_seq == 0 {
            return None;
        }
        // Run cancelled check.
        if self.is_run_cancelled(run_id) {
            return None;
        }
        // Stale seq check: discard if a newer launch_seq was already registered.
        let current = self.current_launch_seq(run_id);
        if launch_seq < current {
            return None;
        }
        // Record activation — highest seq seen for this run.
        let entry = self
            .bus_activated_seqs
            .entry(run_id.to_string())
            .or_insert(0);
        *entry = (*entry).max(launch_seq);
        // Mark as in-flight so the degraded-mode path can detect a lost confirm.
        self.bus_in_flight_workers
            .insert((run_id.to_string(), launch_seq));

        if is_acp {
            // The actor inserted this pair before the first execution. A retry after a
            // transient completion-publication failure re-inserts the same pair here.
            self.active_workers.insert((run_id.to_string(), launch_seq));
            Some(self.next_epoch(run_id))
        } else {
            Some(0)
        }
    }
}

/// Compute the largest byte offset ≤ `max_bytes` that is still a valid UTF-8
/// boundary. Avoids splitting multi-byte codepoints.
fn msg_floor_at(s: &str, max_bytes: usize) -> usize {
    if s.len() <= max_bytes {
        return s.len();
    }
    let mut floor = max_bytes;
    while floor > 0 && !s.is_char_boundary(floor) {
        floor -= 1;
    }
    floor
}

// ── EpochCleanup RAII guard (DES-002 T4) ─────────────────────────────────────

/// RAII guard that fires `cleanup_run` when an `exec_turn_acp` invocation exits
/// (via normal return, early return on error, or panic).
///
/// It is the **sole caller** of `ElicitationMaps::cleanup_run` — a design invariant
/// enforced by the spec ("Never do: Call `cleanup_run` from `on_run_complete`").
///
/// # `bus_in_flight_deferred` flag
///
/// `exec_turn` on the bus path sets `bus_in_flight_deferred = true` BEFORE returning
/// (after the `confirm_task_completed` call). On that path, `Drop` SKIPS the
/// `clear_bus_in_flight` call — the bus consumer will call it after the ack-gated
/// cursor advance. On the panic/cancel path the flag stays `false` and `Drop` clears
/// the in-flight marker immediately so the in-flight `HashSet` does not leak.
pub struct EpochCleanup {
    pub maps: Arc<Mutex<ElicitationMaps>>,
    pub run_id: String,
    pub epoch: u64,
    pub launch_seq: u64,
    /// When `true`, `Drop` skips `clear_bus_in_flight` (bus consumer owns the clear).
    /// When `false` (default), `Drop` clears it immediately.
    pub bus_in_flight_deferred: bool,
    /// Relay channel to emit `ElicitationResolved` when a resolution was in progress
    /// at the time the guard fires.
    pub tx: std::sync::mpsc::Sender<Command>,
    /// Set when an elicitation was in-flight (resolved but not yet emitted) at guard
    /// fire time so `Drop` can emit the `ElicitationResolved` event.
    pub in_flight_id: Option<String>,
    pub in_flight_action: Option<String>,
    pub in_flight_reason: Option<String>,
}

impl Drop for EpochCleanup {
    fn drop(&mut self) {
        // Step 1: clear bus in-flight unless the bus consumer owns the clear.
        if !self.bus_in_flight_deferred {
            if let Ok(mut m) = self.maps.lock() {
                m.clear_bus_in_flight(&self.run_id, self.launch_seq);
            }
        }
        // Step 2: emit ElicitationResolved if a resolution is pending.
        if let Some(ref id) = self.in_flight_id {
            let _ = self.tx.send(Command::EmitEvent(
                crate::event::CoreEvent::ElicitationResolved {
                    session: self.run_id.clone(),
                    elicitation_id: id.clone(),
                    action: self.in_flight_action.clone().unwrap_or_default(),
                    reason: self.in_flight_reason.clone().unwrap_or_default(),
                },
            ));
        }
        // Step 3: call cleanup_run — the one and only call site.
        if let Ok(mut m) = self.maps.lock() {
            m.cleanup_run(&self.run_id, self.epoch, self.launch_seq);
        }
    }
}

// ── ACP child process ─────────────────────────────────────────────────────────

struct AcpProcess {
    /// Shared with the actor's teardown registry so cancellation can interrupt a
    /// turn that is blocked waiting for ordinary ACP output (not only elicitation).
    kill_handle: Arc<KillHandle>,
    write_lock: Arc<Mutex<()>>,
    stdin: BufWriter<ChildStdin>,
    /// Lines arriving from the ACP server's stdout, fed by the reader thread.
    /// Unbounded so the reader never blocks the child on a full pipe.
    line_rx: std::sync::mpsc::Receiver<String>,
    _reader: std::thread::JoinHandle<()>,
    /// Bounded tail of the bridge's stderr, kept for the life of the session so a turn-level
    /// failure can report what the bridge said rather than only that it stopped answering.
    stderr_tail: StderrTail,
    _stderr_reader: Option<std::thread::JoinHandle<()>>,
    session_id: String,
    next_id: u64,
    /// Whether this session's `initialize` advertised the elicitation capability.
    /// Computed exactly once in `start_acp_process` (from the adapter BINARY via
    /// `elicitation_verified_adapter`) and read by `exec_turn_acp`'s turn-time gate,
    /// so advertisement and turn-time behavior are the same decision by construction —
    /// a seat can never advertise elicitation and then auto-cancel it, nor serve what
    /// it never advertised (core#341: the old turn-time gate keyed on the registry
    /// `cli_key`, so the stock `claude` seat — key `claude`, binary `claude-agent-acp` —
    /// advertised the capability and then cancelled every `elicitation/create`).
    elicitation_advertised: bool,
    /// Whether `AcpConfig::verified_version` (if any) matched the ACTUAL resolved binary this
    /// process was spawned from, computed ONCE in `start_acp_process` before spawn — never
    /// re-probed per turn, since this field describes a fact about the running process, not
    /// about any later turn. `true` when the seat carries no version pin at all. Consulted at the
    /// turn-time gate alongside the registry's static `acp_input_governance`: a seat can be
    /// statically admitted yet have this `false` for one particular process instance (e.g. an
    /// opencode Homebrew auto-update landed mid-session) — that instance is treated as
    /// disclosed-ungoverned regardless of what the registry says (DES-INPUT-GOV-006 §3.4).
    governance_verified: bool,
    /// (DES-GOV-008 Boundary 1 / A1) `Some((level, reason))` when `os_sandbox` was requested but the
    /// kernel WRITE-containment floor could NOT arm (no launcher on PATH, firejail-only, or a
    /// canonicalize failure), so this session spawned uncontained. Computed ONCE at spawn — the
    /// session is cached and reused across turns — and read by the caller to emit exactly one
    /// `SandboxUnenforced` disclosure per spawn. `None` when the floor armed OR the flag was OFF.
    sandbox_downgrade: Option<(String, String)>,
    /// (core#396) The skills snapshot this session was OPENED with — the plugin the bridge loaded
    /// at `session/new` — bound to the process for its lifetime. Every later turn of the cached
    /// session prompts, admits, read-widens and reports against THIS generation, never against a
    /// re-resolved `current`: the process holds the plugin it loaded, and a snapshot resolved
    /// mid-session would describe a generation this bridge never saw. `None` when no snapshot was
    /// handed (a chat session, a non-claude seat, or no root on the ladder).
    skills: Option<crate::skills_snapshot::SkillsSnapshot>,
    /// (v3.1 §3) This session's own configuration directory —
    /// `<worker home>/sessions/<run>-<cli>-<pid>-<seq>/`, holding the `settings.json` the bridge
    /// was handed in `session/new` — or `None` when the launch carried none (a chat session, a
    /// non-Claude bridge, the inherit escape hatch). OWNED by this process and by nothing else
    /// (codex round 3): the name is unique per spawn (`write_session_settings`), so no racing
    /// launch shares or deletes it, and it is reaped exactly once — here, on drop, by its owner.
    session_dir: Option<std::path::PathBuf>,
    /// F-036 (adversarial review on #414): whether this process was opened for a NO-CODE phase
    /// (`executes_code: false`). Like the PTY carrier's `PtySession.no_code`, a process serves only
    /// turns of its own posture — a creator's process is never handed to the evaluator that follows
    /// (it could background a writer past the guard's final snapshot), and a no-code phase's
    /// process is killed, group and all, the moment its unit ends.
    no_code: bool,
    /// A CHAT session's filesystem boundary (core#410, review): the scratch root writable, the
    /// scoped repository roots read-only. Judged on every `session/request_permission` of a turn
    /// that carries no governance gate (`acp_permission::chat_boundary_result`); `None` for unit
    /// sessions, whose boundary rides their `AcpGate`.
    chat_boundary: Option<crate::gate_hook::BoundaryCtx>,
}

impl Drop for AcpProcess {
    fn drop(&mut self) {
        // crew#290 instrumentation: every drop KILLS the bridge, and a drop while the bridge
        // is mid-turn is the leading hypothesis for the field's silent exit-0 deaths. Say so,
        // with the session id, so the daemon log carries the ordering evidence — which engine
        // path released the process relative to the turn's own error lines.
        eprintln!(
            "[wicked-core] dropping ACP bridge (session {}): about to send the kill signal — if a turn was in \
             flight, its failure lines should appear adjacent to this one",
            self.session_id
        );
        self.kill_handle.signal();
        // The owner cleans ONLY its own per-session settings directory (v3.1 §3, codex round 3):
        // the bridge read the file at start, and no other process can hold this path — its name
        // carries this process's pid and a per-process counter. No-follow; a failure is SAID
        // (codex round 9), never swallowed — the directory is left behind, and nothing else can
        // mistake it for its own.
        if let Some(dir) = &self.session_dir {
            if let Err(e) = remove_entry_no_follow(dir) {
                eprintln!(
                    "[wicked-core] acp.warn could not remove the per-session settings directory {} \
                     ({e}); it is left behind",
                    dir.display()
                );
            }
        }
    }
}

// ── Handshake budgets and start concurrency ───────────────────────────────────
//
// The two handshake calls are budgeted SEPARATELY because they have different cost profiles.
// Sweeping bridge-startup concurrency directly (K bridges released simultaneously on a barrier,
// same host, same binary):
//
//   K   initialize (id=1)   session/new (id=2) med / max   trips a 10s budget
//   1   0.31 – 0.69s        1.67s  /  1.67s                0/1
//   2   0.31 – 0.69s        1.74s  /  1.75s                0/2
//   4   0.31 – 0.69s        3.41s  /  5.31s                0/4
//   8   0.31 – 0.69s        7.12s  / 11.57s                2/8
//
// `initialize` is flat at every K; `session/new` scales ~linearly past K=2. A single constant
// applied to both under-budgets exactly the call that contends — and every ACP timeout observed
// in the field names id=2. Host load does NOT predict this: K=1 returned 1.67s at load average
// 37.86, the highest load and the fastest sample in the same experiment.
//
// This is a governance defect, not a latency one. A handshake timeout does not fail the unit; it
// silently downgrades it to the single-shot wrapped-CLI path. So under-budgeting trades the
// governed execution path for latency without saying so, and does it most often exactly when
// parallelism is highest — which is when governance matters most. (FINDING-022)

/// Budget for `initialize`. Generous relative to the 0.69s worst case above because the FIRST
/// spawn after daemon start is cold and was measured at 9.83s.
const INIT_DEFAULT_SECS: u64 = 60;

/// Budget for `session/new` — the call whose cost scales with concurrency. ~7x the median measured
/// in the slow regime and ~5x its worst case.
const SESSION_NEW_DEFAULT_SECS: u64 = 60;

/// How many ACP bridges may be inside `initialize` + `session/new` at once. 2 is the highest
/// concurrency measured with no degradation. What is bounded is contention, not useful work: a
/// queued handshake succeeds where a contended one silently downgrades its unit to ungoverned
/// execution. Set the env override to a large number to disable the gate.
const START_PERMITS_DEFAULT: usize = 2;

/// How long a start waits for a permit before giving up and proceeding contended.
///
/// Deliberately NOT [`session_new_budget`]. That wait is pure overhead spent before the bridge is
/// even spawned, and the waiter still needs its full budget *after* admission — so tying the two
/// together makes them compound, and raising the budget to fix slow handshakes would lengthen the
/// queue in front of them.
///
/// 30s comes from the drain rate the gate itself enforces: held to 2 concurrent starts a handshake
/// costs ~1.75s (K=2 in the table above), so a fan-out of roughly 34 simultaneous units is still
/// admitted inside the bound — well past any run this platform dispatches. Beyond that the gate is
/// no longer the thing helping (a permit holder is stuck near its own budget, or arrivals far
/// exceed what serialising can absorb) and proceeding contended beats waiting longer.
const START_WAIT: Duration = Duration::from_secs(30);

/// Parses a seconds override, falling back to `default`.
///
/// Split from the env lookup so the defaults are testable without the process environment: a test
/// that asserted on [`initialize_budget`] would fail on any host that legitimately sets the
/// override, which is a supported configuration and not a defect.
fn parse_secs(raw: Option<String>, default: u64) -> Duration {
    let secs = raw
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(default);
    Duration::from_secs(secs)
}

fn env_secs(key: &str, default: u64) -> Duration {
    parse_secs(std::env::var(key).ok(), default)
}

fn initialize_budget() -> Duration {
    env_secs("WICKED_ACP_INIT_SECS", INIT_DEFAULT_SECS)
}

fn session_new_budget() -> Duration {
    env_secs("WICKED_ACP_SESSION_NEW_SECS", SESSION_NEW_DEFAULT_SECS)
}

/// Parses the start-concurrency override. Pure, for the same reason as [`parse_secs`].
fn parse_permits(raw: Option<String>) -> usize {
    raw.and_then(|s| s.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(START_PERMITS_DEFAULT)
}

fn start_permits() -> usize {
    parse_permits(std::env::var("WICKED_ACP_START_CONCURRENCY").ok())
}

struct StartGate {
    available: Mutex<usize>,
    released: std::sync::Condvar,
}

impl StartGate {
    fn new(permits: usize) -> Self {
        Self {
            available: Mutex::new(permits),
            released: std::sync::Condvar::new(),
        }
    }

    /// Waits for a permit, but only up to `wait`. On timeout it returns `None` and the caller
    /// proceeds ANYWAY: the gate reduces contention, it is not a correctness barrier, and a
    /// contended handshake that might still succeed beats a queue that looks like a hang. Without
    /// the cap, two bridges stuck for their full budget would stall every other start behind them.
    fn acquire(&'static self, wait: Duration) -> Option<StartPermit> {
        let guard = self.available.lock().unwrap_or_else(|p| p.into_inner());
        let (mut n, _) = self
            .released
            .wait_timeout_while(guard, wait, |n| *n == 0)
            .unwrap_or_else(|p| p.into_inner());
        // Decide on the permit count, NOT on `WaitTimeoutResult::timed_out()`. The two agree here
        // — `wait_timeout_while` only reports a timeout with its predicate still true, and the lock
        // is held from that check to the return — but the count is what actually decides, and
        // reading it directly means this cannot start refusing an available permit if that detail
        // of the std implementation ever shifts.
        if *n == 0 {
            return None;
        }
        *n -= 1;
        Some(StartPermit { gate: self })
    }
}

/// The process-wide gate. Tests build their own [`StartGate`] instead of exhausting this one,
/// so a test that deliberately holds every permit cannot stall a concurrent one.
fn start_gate() -> &'static StartGate {
    static GATE: std::sync::OnceLock<StartGate> = std::sync::OnceLock::new();
    GATE.get_or_init(|| StartGate::new(start_permits()))
}

/// A permit to run a handshake. Released on drop, so an early return or a panic mid-handshake
/// cannot leak one.
struct StartPermit {
    gate: &'static StartGate,
}

impl Drop for StartPermit {
    fn drop(&mut self) {
        let mut n = self
            .gate
            .available
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        *n += 1;
        self.gate.released.notify_one();
    }
}

/// The tail of a bridge's stderr, kept so a failed handshake can report what the bridge itself
/// said. Bounded: a chatty bridge must not grow memory in a runner that lives as long as the
/// daemon. Previously stderr was `Stdio::null()`, which is why every failure in this path — a
/// contended handshake, a missing binary, an auth hang — collapsed to one opaque string
/// containing a raw JSON-RPC id.
type StderrTail = Arc<Mutex<std::collections::VecDeque<String>>>;

const STDERR_TAIL_LINES: usize = 20;

/// Per-line byte cap. A line count alone does not bound anything: one bridge writing a single
/// megabyte-long line without a newline would sit in the tail whole. Both bounds together make the
/// rendered tail small enough to append to a capped output without argument.
const STDERR_TAIL_LINE_BYTES: usize = 512;

/// Truncates on a char boundary and SAYS it truncated — a silently clipped line reads as a bridge
/// that stopped mid-sentence, which is a different diagnosis than one that said too much.
fn clip_stderr_line(line: String) -> String {
    if line.len() <= STDERR_TAIL_LINE_BYTES {
        return line;
    }
    let mut cut = STDERR_TAIL_LINE_BYTES;
    while cut > 0 && !line.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…(+{} bytes)", &line[..cut], line.len() - cut)
}

fn drain_stderr(stderr: std::process::ChildStderr) -> (StderrTail, std::thread::JoinHandle<()>) {
    let tail: StderrTail = Arc::new(Mutex::new(std::collections::VecDeque::new()));
    let sink = tail.clone();
    let handle = std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if line.trim().is_empty() {
                continue;
            }
            let mut t = sink.lock().unwrap_or_else(|p| p.into_inner());
            if t.len() == STDERR_TAIL_LINES {
                t.pop_front();
            }
            t.push_back(clip_stderr_line(line));
        }
    });
    (tail, handle)
}

/// Appends `note` to `output` while keeping `output` within `max_out`, trimming the OLDER text to
/// make room. `handle_update` holds streamed output to that cap and appending must not quietly
/// break it — but the note outranks what it displaces: by the time one is written, `output` is
/// already a truncated fragment of a turn that failed, and the note is the only account of why.
fn append_within_cap(output: &mut String, note: &str, max_out: usize) {
    if output.len() + note.len() > max_out {
        let mut cut = max_out.saturating_sub(note.len()).min(output.len());
        while cut > 0 && !output.is_char_boundary(cut) {
            cut -= 1;
        }
        output.truncate(cut);
    }
    output.push_str(note);
}

/// Renders the tail for an error message, or a note that the bridge said nothing — which is
/// itself diagnostic: silence points at contention or a hang, output points at the bridge.
fn stderr_context(tail: &StderrTail) -> String {
    let t = tail.lock().unwrap_or_else(|p| p.into_inner());
    if t.is_empty() {
        return "; bridge stderr: (silent)".to_string();
    }
    format!(
        "; bridge stderr (last {} of {}): {}",
        t.len(),
        STDERR_TAIL_LINES,
        t.iter().cloned().collect::<Vec<_>>().join(" | ")
    )
}

/// The full post-mortem note for a died bridge (crew#267): stderr tail (existing), the child's
/// exit status when knowable, and the last stdout lines still queued in the reader channel —
/// a bridge that dies mid-write leaves its final words there, unread by any turn. Two silent
/// deaths in the field carried "(silent)" stderr; exit + stdout are the next discriminators.
fn death_context(proc: &AcpProcess) -> String {
    death_context_with(proc, proc.kill_handle.try_exit_status())
}

/// [`death_context`] with the exit status the CALLER already fetched — `try_exit_status`
/// reaps, and a concurrent `signal()` can take the child between two calls, so a probe that
/// observed the status once must pass it through rather than ask again and read `None`.
fn death_context_with(proc: &AcpProcess, status: Option<std::process::ExitStatus>) -> String {
    let mut note = stderr_context(&proc.stderr_tail);
    match status {
        Some(status) => note.push_str(&format!("; bridge exit: {status}")),
        None => note.push_str("; bridge exit: unknown (not yet reaped)"),
    }
    // crew#290: 5 lines was too little post-mortem — the two field deaths carried "(silent)"
    // stderr, so the queued stdout frames are the only account of the bridge's last moments.
    let mut tail: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    while let Ok(line) = proc.line_rx.try_recv() {
        if tail.len() == 20 {
            tail.pop_front();
        }
        tail.push_back(line.chars().take(240).collect());
    }
    if tail.is_empty() {
        note.push_str("; bridge stdout tail: (empty)");
    } else {
        note.push_str(&format!(
            "; bridge stdout tail: {}",
            tail.into_iter().collect::<Vec<_>>().join(" | ")
        ));
    }
    note
}

// ── Session startup ───────────────────────────────────────────────────────────

/// The env var claude's CLI and Agent SDK resolve their per-user configuration directory from —
/// user-scope settings, hooks, plugins, memory. The ACP bridge hands its own environment to the
/// SDK it drives in-process (`CLAUDE_CONFIG_DIR = process.env.CLAUDE_CONFIG_DIR ?? homedir()`),
/// so this variable decides WHOSE configuration a worker runs under. It is the carrier the
/// bridge honours where argv is not: flags the bridge does not parse are discarded, which is how
/// FINDING-060 happened. Spelled once, below this crate, so the council ballot spawn sets the
/// SAME variable from the SAME resolver (F-030).
pub(crate) const CLAUDE_CONFIG_DIR_ENV: &str = wicked_apps_core::spawn::CLAUDE_CONFIG_DIR_ENV;

/// Decide the [`CLAUDE_CONFIG_DIR_ENV`] override for an ACP worker spawn — `None` means inherit
/// the operator's own configuration (the explicit escape hatch only).
///
/// FINDING-061: FINDING-047 (a worker that inherits the operator's CLI configuration changes
/// what it does — their hooks fire, their permission defaults apply, an operator on `dontAsk`
/// gets workers whose every file write reroutes through Bash) was fixed on the wrapped path
/// only, because `inject_isolation_flags` rides argv and the ACP bridge does not read argv. The
/// bridge DOES honour [`CLAUDE_CONFIG_DIR_ENV`], so the ACP spawn points it at an engine-minted
/// directory instead — the same boundary, carried on the seam this path actually has.
///
/// `inherit_operator` is [`crate::execute_wrapped::INHERIT_OPERATOR_CONFIG_ENV`]'s presence,
/// read at the call site: the SAME opt-in escape hatch as the wrapped path, because two
/// opt-outs for one boundary is how one of them silently stops working. Parameterised so both
/// branches are testable without mutating the test process's environment. The home is
/// launch-independent (v3.1 §3): the settings a particular launch needs — its fence, its
/// snapshot — ride that session's own configuration ([`SessionOptions`]), never this shared dir.
///
/// TEST-ONLY since core#410: the spawn resolves EVERY seat through
/// `wicked_apps_core::spawn::seat_config_for` (whose claude arm is this same hatch + dir) and
/// ensures the claude home right there; this stays as the claude-only view the agreement tests
/// compare against the ballot's and the roster's spellings.
#[cfg(test)]
fn worker_claude_config_dir(
    inherit_operator: bool,
    operational_home: Option<&std::path::Path>,
) -> Option<anyhow::Result<std::path::PathBuf>> {
    if inherit_operator {
        return None;
    }
    Some(ensure_worker_config_home(operational_home))
}

/// Mint a fresh, engine-owned config directory for ONE ACP spawn.
///
/// The PERSISTENT, engine-owned config home for ACP claude workers (crew#267, operator
/// decision: "option 3"). One stable directory per operator, NOT per spawn.
///
/// History: this used to mint a fresh temp dir per spawn (FINDING-061) — which also severed
/// the CLI's login state, so every governed ACP claude session failed its first prompt with
/// `-32000 Authentication required` and fell back single-shot. The chosen fix: a stable
/// worker home the operator logs in ONCE (their own browser OAuth — the engine never reads,
/// copies, or holds credentials), combined with per-spawn RE-SANITIZATION of every mutation
/// vector FINDING-047/061 named:
///
///  - `settings.json` is OVERWRITTEN with the deny fence on every spawn — a worker that edits
///    it changes nothing for the next worker;
///  - `hooks/`, `plugins/`, `commands/`, `agents/`, `settings.local.json`,
///    `managed-settings.json` are REMOVED on every spawn — no executable-config carryover;
///  - login/session state (`.claude.json`, `.credentials.json`, todos) PERSISTS — that is the
///    point.
///
/// Location: `~/.wicked-worker/claude` — deliberately NOT under `~/.wicked-crew` (the deny
/// fence blocks worker tools from that whole tree, which would break the worker's own
/// tool-mediated memory writes) and NOT under `~/.config/wicked-core` (the gate-pin tree,
/// where writes are boundary-FATAL). The home-dir location also retires the temp-dir
/// pre-creation attack the old exclusive-create defended against ($HOME is not
/// world-writable); a symlink planted at either path component is still refused below.
///
/// [`wicked_apps_core::spawn::WORKER_HOME_ENV`] overrides the BASE dir. Every TEST binary arms it
/// pre-main at a per-process temp base (via `emit::hermetic_test_spool`, core#311-class): a real
/// start reached by a test re-sanitizes the resolved home, which must never be the operator's
/// real `~/.wicked-worker`.
///
/// The PATH is the shared resolver's (`wicked_apps_core::spawn::worker_claude_config_dir`) — the
/// same one the council ballot spawn sets and the roster's claude sign-in command names (F-030 /
/// F-013). This crate owns only what happens AT that path: creation, permissions, symlink refusal
/// and per-spawn re-sanitization ([`ensure_worker_config_home`]).
fn worker_config_home() -> anyhow::Result<std::path::PathBuf> {
    wicked_apps_core::spawn::worker_claude_config_dir()
}

/// Filesystem entries re-sanitized out of the worker home at EVERY spawn — the exact
/// executable-config vectors FINDING-047/061 named. Login/session state is not listed.
const WORKER_HOME_SANITIZED: &[&str] = &[
    "hooks",
    "plugins",
    "commands",
    "agents",
    "settings.local.json",
    "managed-settings.json",
];

/// Ensure the persistent worker home exists, is private, is not a planted symlink, and has
/// been re-sanitized for THIS spawn. Returns the home. Fail closed on anything odd.
///
/// SERIALIZED process-wide: the start gate admits 2 concurrent handshakes, and two ensures
/// racing on the same home can interleave remove/write on `settings.json` into a spurious
/// NotFound failure (caught by the 8-simultaneous-starts gate test on macOS CI). The critical
/// section is a handful of fs ops; contention is bounded by the start gate anyway.
///
/// LAUNCH-INDEPENDENT by construction (v3.1 §3): the shared `settings.json` carries only what
/// every launch agrees on — the fence over every directory EXCEPT the state home, plus the Bash
/// verbs (`execute_wrapped::shared_deny_rules`). The state-home rule differs per launch (the
/// registry when the snapshot sits in its read slot, the blanket otherwise), and deny rules MERGE
/// across settings layers, so a blanket here would deny every concurrent session's snapshot
/// whatever its own settings say; it rides each session's `session/new` options and per-session
/// settings file instead ([`write_session_settings`]). Written atomically (tmp + rename) so a
/// concurrent spawn's CLI never reads a torn file.
fn ensure_worker_config_home(
    // The engine's operational state home (codex round 8): kept OUT of the shared file's blanket
    // like every state-home candidate (its fence rides each session's own configuration).
    operational_home: Option<&std::path::Path>,
) -> anyhow::Result<std::path::PathBuf> {
    static ENSURE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _g = ENSURE.lock().unwrap_or_else(|p| p.into_inner());
    let dir = worker_config_home()?;
    refuse_symlinked_home(&dir)?;
    if !dir.is_dir() {
        let mut b = std::fs::DirBuilder::new();
        b.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            b.mode(0o700);
        }
        use anyhow::Context;
        b.create(&dir)
            .with_context(|| format!("could not create worker config home {}", dir.display()))?;
    }
    #[cfg(unix)]
    {
        // Private, always — an existing dir may predate this build or have been loosened.
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    }
    // TOCTTOU narrowing (Copilot, PR#277): re-verify no component became a symlink between
    // the pre-create probe and the mutating block below. The window that remains — a same-uid
    // process swapping the path in the microseconds before each write — cannot be fully closed
    // without dirfd/O_NOFOLLOW traversal, and a same-uid attacker (the only principal that can
    // write under $HOME) already holds strictly stronger levers than this directory. The probe
    // pair + the process-wide ENSURE mutex reduce the practical surface to that residual.
    refuse_symlinked_home(&dir)?;
    // RE-SANITIZE: executable-config vectors go; login/session state stays. Judged on
    // symlink_metadata, never a following stat: a prior worker could plant
    // `hooks -> ~/.ssh` or `settings.json -> <victim>` and a following remove/write would
    // act OUTSIDE the home (Copilot, PR#277). A symlink entry is removed AS a link.
    for entry in WORKER_HOME_SANITIZED {
        remove_entry_no_follow(&dir.join(entry))?;
    }
    // settings.json is re-written every spawn — REPLACED, never unlinked first (codex round 5):
    // `write_atomic` writes the pid/seq temp and `rename`s it over the target, which replaces
    // atomically and does not follow a symlinked target (a planted link is replaced AS a link),
    // so a concurrent reader — another engine process's spawn, a CLI already running on this
    // home — sees the previous valid file or the new one and never NO file, and a write that
    // fails (temp create, write, sync, rename) leaves the previous valid file in place. Round 4
    // unlinked the valid file before the write: that gap was observable across processes, which
    // the process-local ENSURE mutex above does not cover. The one thing cleared beforehand is an
    // entry that is NOT a file or a link — a planted DIRECTORY named `settings.json` — which
    // rename cannot replace and which was never a valid settings file to keep. Then the temp
    // files THIS process's earlier spawns left behind (a crash between create and rename) are
    // swept. Only this process's: the worker home is shared by every engine process on the host,
    // and another process's temp file is its in-flight write, which its own rename is about to
    // consume — unlinking it under that process fails ITS isolation setup (codex round 4).
    let settings_path = dir.join(SETTINGS_FILENAME);
    if let Ok(m) = std::fs::symlink_metadata(&settings_path) {
        if !m.is_file() && !m.file_type().is_symlink() {
            remove_entry_no_follow(&settings_path)?;
        }
    }
    sweep_own_settings_temps(&dir)?;
    // (codex round 9) an unspellable fenced directory refuses the spawn — the shared file is never
    // written with a hole in it.
    let shared =
        crate::execute_wrapped::shared_deny_rules(operational_home).map_err(anyhow::Error::msg)?;
    let settings = json!({ "permissions": { "deny": shared } });
    write_atomic(&dir, &settings_path, &serde_json::to_vec(&settings)?)?;
    Ok(dir)
}

/// The per-session configuration directories under the worker home (v3.1 §3). NOT in
/// [`WORKER_HOME_SANITIZED`]: concurrent sessions live here, and a spawn must never delete
/// another session's settings.
const SESSIONS_DIRNAME: &str = "sessions";
const SETTINGS_FILENAME: &str = "settings.json";
/// The suffix of an atomic writer's temp file: `<name>.<pid>.<seq>.tmp` ([`settings_temp_name`]).
const SETTINGS_TMP_SUFFIX: &str = ".tmp";

/// The temp file [`write_atomic`] writes `file` through: `<file>.<pid>.<seq>.tmp` — the pid so
/// concurrent engine processes sharing a directory never collide AND so a sweep can tell its own
/// leftovers from another process's in-flight write ([`sweep_own_settings_temps`]); the sequence
/// for this process's own concurrent writes.
fn settings_temp_name(file: &str, pid: u32, seq: u64) -> String {
    format!("{file}.{pid}.{seq}{SETTINGS_TMP_SUFFIX}")
}

/// The pid a temp name carries when it has exactly the shape [`settings_temp_name`] produces for
/// `file`; `None` for every other name (the final file itself, a foreign or legacy shape).
fn settings_temp_pid(name: &str, file: &str) -> Option<u32> {
    let rest = name
        .strip_prefix(file)?
        .strip_prefix('.')?
        .strip_suffix(SETTINGS_TMP_SUFFIX)?;
    let (pid, seq) = rest.split_once('.')?;
    if seq.is_empty() || !seq.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    pid.parse().ok()
}

/// Remove the `settings.json` temp files in `dir` that THIS process left behind (its pid in the
/// name) — never another process's, whose temp is an in-flight write its rename is about to
/// consume. A directory that cannot be listed, or an entry that cannot be read while listing, is
/// an ERROR carrying the cause (codex round 9 — round 8 returned `Ok` on an unlistable directory
/// and dropped per-entry errors): the spawn that needs this directory refuses rather than
/// proceeding on a worker home it could not inspect. A name that is not UTF-8 is never one of ours
/// (ours are ASCII) and is left alone.
fn sweep_own_settings_temps(dir: &std::path::Path) -> anyhow::Result<()> {
    let listing = |e: std::io::Error| {
        anyhow::anyhow!(
            "cannot list the worker home {} to sweep this process's settings temps ({e})",
            dir.display()
        )
    };
    let entries = std::fs::read_dir(dir).map_err(listing)?;
    let me = std::process::id();
    for entry in entries {
        let entry = entry.map_err(listing)?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if settings_temp_pid(name, SETTINGS_FILENAME) == Some(me) {
            remove_entry_no_follow(&entry.path())?;
        }
    }
    Ok(())
}

/// (v3.1 §3) Write THIS session's Claude settings — the full fence for this launch
/// (`execute_wrapped::deny_rules`: the state-home registry when the snapshot sits in its read
/// slot, the blanket otherwise) — into a per-PROCESS directory
/// `<worker home>/sessions/<run>-<cli>-<pid>-<seq>/settings.json`, minted with an exclusive
/// `create_dir` (an existing name is retried with the next suffix) and written atomically (tmp +
/// rename). COLLISION-FREE by construction (codex round 3): pass 2 named the directory
/// `<run>-<cli>` and removed-then-recreated it on every start, so two concurrent starts for the
/// same key could delete each other's file (rename cannot protect a file whose parent is being
/// removed), and two distinct ids that sanitize alike (`campaign:one`, `campaign_one`) shared one
/// path. Now no launch ever removes a directory it did not create: the name carries this
/// process's pid and a per-process counter, and the owning [`AcpProcess`] reaps it on drop.
/// Returns the settings path; the bridge is handed it in `session/new` (`SessionOptions::settings`,
/// the SDK's `settings` = `--settings`) and the same rules as `disallowedTools`.
fn write_session_settings(
    home: &std::path::Path,
    run_id: &str,
    cli_key: &str,
    deny: &[String],
) -> anyhow::Result<std::path::PathBuf> {
    let sessions = home.join(SESSIONS_DIRNAME);
    match std::fs::symlink_metadata(&sessions) {
        Ok(m) if m.file_type().is_symlink() => anyhow::bail!(
            "refusing per-session settings: {} is a symlink",
            sessions.display()
        ),
        Ok(m) if !m.is_dir() => anyhow::bail!(
            "refusing per-session settings: {} is not a directory",
            sessions.display()
        ),
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Two concurrent first starts may both see NotFound: the loser's create fails
            // AlreadyExists, which is the same directory — not an error.
            match private_dir(&sessions) {
                Ok(()) => {}
                Err(e)
                    if e.downcast_ref::<std::io::Error>()
                        .is_some_and(|io| io.kind() == std::io::ErrorKind::AlreadyExists) => {}
                Err(e) => return Err(e),
            }
        }
        Err(e) => anyhow::bail!("cannot stat {} ({e})", sessions.display()),
    }
    let dir = create_session_dir(
        &sessions,
        &session_dir_stem(run_id, cli_key),
        &mut session_suffix,
    )?;
    let settings = json!({ "permissions": { "deny": deny } });
    let path = dir.join("settings.json");
    write_atomic(&dir, &path, &serde_json::to_vec(&settings)?)?;
    Ok(path)
}

/// Create `<sessions>/<stem>-<suffix>` EXCLUSIVELY, minting a fresh suffix while the name is
/// taken (`AlreadyExists`) — bounded, so a directory that keeps reappearing under our feet fails
/// loudly rather than spinning. Never removes anything: a colliding name belongs to another
/// live launch (or a crashed one's leftover) and is not ours to delete.
fn create_session_dir(
    sessions: &std::path::Path,
    stem: &str,
    mint: &mut dyn FnMut() -> String,
) -> anyhow::Result<std::path::PathBuf> {
    const ATTEMPTS: usize = 64;
    for _ in 0..ATTEMPTS {
        let dir = sessions.join(format!("{stem}-{}", mint()));
        match private_dir_builder().create(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => anyhow::bail!("could not create {} ({e})", dir.display()),
        }
    }
    anyhow::bail!(
        "could not mint a unique per-session settings directory under {} for `{stem}` in \
         {ATTEMPTS} attempts",
        sessions.display()
    )
}

/// `<pid>-<seq>`: unique across concurrent processes sharing the worker home (the pid) and
/// across this process's own launches (a monotonic counter).
fn session_suffix() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    format!(
        "{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// `<run_id>-<cli_key>`, each component reduced to `[A-Za-z0-9._-]` (a campaign run id carries
/// `:`; a registry key is free text) so the directory name is one path component everywhere.
/// A STEM only — two distinct ids may sanitize alike, and the unique suffix keeps them apart.
fn session_dir_stem(run_id: &str, cli_key: &str) -> String {
    format!(
        "{}-{}",
        sanitize_component(run_id),
        sanitize_component(cli_key)
    )
}

fn sanitize_component(s: &str) -> String {
    let out: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() || out.trim_matches('.').is_empty() {
        "_".to_string()
    } else {
        out
    }
}

/// A non-recursive `DirBuilder` for directories private to the user: mode `0o700` on unix; the
/// platform default elsewhere (the worker home's ACLs are the user's own). Two `cfg` bodies
/// rather than one with a `cfg`-gated mutation, so the non-unix build has no `let mut` whose
/// only mutation is compiled out (`unused_mut` under `-D warnings` on the Windows job).
#[cfg(unix)]
fn private_dir_builder() -> std::fs::DirBuilder {
    use std::os::unix::fs::DirBuilderExt;
    let mut b = std::fs::DirBuilder::new();
    b.mode(0o700);
    b
}

#[cfg(not(unix))]
fn private_dir_builder() -> std::fs::DirBuilder {
    std::fs::DirBuilder::new()
}

/// Create `dir` (non-recursively — its parent is ours already) private to the user.
fn private_dir(dir: &std::path::Path) -> anyhow::Result<()> {
    use anyhow::Context;
    private_dir_builder()
        .create(dir)
        .with_context(|| format!("could not create {}", dir.display()))
}

/// Write `bytes` to `path` atomically: an exclusively-created private temp file in the same
/// directory (`<name>.<pid>.<seq>.tmp`, [`settings_temp_name`]), then `rename` over `path` — a
/// reader sees the old file or the new one, never a torn one, and a planted link at `path` is
/// replaced as a link (rename does not follow its target).
fn write_atomic(dir: &std::path::Path, path: &std::path::Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write as _;
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let file = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(SETTINGS_FILENAME);
    let tmp = dir.join(settings_temp_name(
        file,
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed),
    ));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let result = (|| -> std::io::Result<()> {
        let mut f = opts.open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result.map_err(|e| anyhow::anyhow!("could not write {} atomically ({e})", path.display()))
}

/// Refuse a worker home whose leaf or parent is a symlink — a redirect here re-aims every
/// write the worker's CLI makes at a path the operator never chose. FAIL CLOSED on any stat
/// error other than not-found (a PermissionDenied probe must not read as "not a symlink").
fn refuse_symlinked_home(dir: &std::path::Path) -> anyhow::Result<()> {
    // The ONE no-follow check, shared with the council ballot spawn (PR#413): a link at the home
    // or its parent re-aims every write AND every credential read.
    wicked_apps_core::spawn::refuse_symlinked_home(dir)
}

/// Remove a worker-home entry WITHOUT following symlinks: a link is deleted as a link
/// (`remove_file` — std's `remove_dir_all` also refuses to traverse links, but routing links
/// away explicitly keeps the property visible and covers link-to-file too). Missing → Ok.
fn remove_entry_no_follow(p: &std::path::Path) -> anyhow::Result<()> {
    let meta = match std::fs::symlink_metadata(p) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => anyhow::bail!("cannot stat {} while sanitizing ({e})", p.display()),
    };
    if meta.file_type().is_dir() {
        std::fs::remove_dir_all(p)?;
    } else {
        std::fs::remove_file(p)?;
    }
    Ok(())
}

/// Spawn the ACP binary and complete the `initialize` + `session/new` handshake — with an
/// `authenticate` step between the two whenever `initialize` advertises `authMethods`
/// (FINDING-015; methodId from [`AcpConfig::auth_method`], else the agent's first advertised).
/// Returns `Err` if the binary is not on PATH, the process fails to start, a handshake call
/// exceeds its budget (see [`initialize_budget`] / [`session_new_budget`]), or the agent still
/// refuses `session/new` as unauthenticated (the named error from [`unauthenticated_error`]).
///
/// This takes no governance argument. It used to accept one and translate it into `--settings
/// <path>` plus the gate-hook's env vars; the env vars arrived, the flag did not (the bridge does
/// not parse it), so the hook had everything it needed except the instruction to run. Governed units
/// take the wrapped path now — see the fail-closed return in `run_unit_streaming` and FINDING-060.
/// `code_graph_db` is the run's repo-local estate graph (engine-resolved:
/// `<state home>/repo-graphs/<key>/estate.db`, never inside the checkout — see `code_graph.rs`,
/// core#406), or `None` for an ungoverned / repo-less session. When present, the worker's `session/new` advertises the
/// estate MCP server scoped to that store (FINDING-122) — the ACP-array twin of the wrapped path's
/// `settings.json` injection. `None` ⇒ no estate server (never the daemon store; see FINDING-067).
/// Bounded budget for the spawn-time version-pin probe (`AcpConfig::verified_version`,
/// DES-INPUT-GOV-006 §3.4). Generous, not tuned: a `--version` probe is a trivial command and
/// this only bounds an ALREADY-degraded path (a pathological binary hanging here downgrades this
/// spawn to disclosed-ungoverned, per [`resolved_binary_version_matches`] — it does not block the
/// spawn itself).
const VERSION_PIN_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Whether the ACTUAL binary about to be spawned for this seat (`config.binary` — the same
/// string `start_acp_process` passes to `Command::new`) reports exactly `expected` from
/// `--version`, trimmed. Reuses wicked-council's own bounded subprocess runner rather than
/// re-implementing a watcher loop.
///
/// `false` on ANY probe failure — spawn error, timeout, non-zero exit, or a mismatched value.
/// A probe that cannot PROVE the match must not be treated as one (the same fail-closed posture
/// `worker_claude_config_dir` already documents for FINDING-061): this seat's `acp_governance_env`
/// forcing function was proven against one specific pinned build (opencode's Homebrew tap
/// auto-updates with no lockfile), not a range, so an unreadable or different version must
/// downgrade this spawn's governance claim rather than assume it still holds.
fn resolved_binary_version_matches(binary: &str, expected: &str) -> bool {
    // The version pin must probe the SAME binary the spawn path will actually launch. On Windows,
    // npm shims install as `<name>.cmd` and a bare-name spawn returns NotFound (the exact case the
    // spawn below retries with an explicit `.cmd`). Without the same retry here, `run_bounded`
    // fails to resolve the shim → this returns false → governance is downgraded to
    // disclosed-ungoverned even though the bridge spawns fine. That is fail-SAFE (it discloses
    // rather than falsely claiming governance) but it defeats admission for every npm-shim ACP
    // adapter on Windows (#377 review). Mirror the spawn's retry so the check matches the spawn.
    let probe = |b: &str| {
        wicked_council::probe::run_bounded(b, &["--version".to_string()], VERSION_PIN_PROBE_TIMEOUT)
    };
    let matches = |combined: &str| combined.lines().next().map(str::trim) == Some(expected);
    match probe(binary) {
        Ok((true, combined)) => matches(&combined),
        Err(wicked_council::probe::ProbeError::Spawn)
            if cfg!(windows) && std::path::Path::new(binary).extension().is_none() =>
        {
            matches!(probe(&format!("{binary}.cmd")), Ok((true, c)) if matches(&c))
        }
        _ => false,
    }
}

/// The shape the spawn tests drive (TEST-ONLY since core#410: the unit path and the chat path
/// both call [`start_acp_process_with_write_roots`] directly — the unit with its governance
/// facts, the chat with its recorded scope). `unix` like its only callers, the shell-script
/// stub wrappers — on Windows it would be dead code under `-D warnings`.
#[cfg(all(test, unix))]
fn start_acp_process(
    config: &AcpConfig,
    cwd: &std::path::Path,
    code_graph_db: Option<&str>,
    // `Some` ⇒ point the worker's platform temp env (`TMPDIR`/`TMP`/`TEMP`) at this dir
    // (core#264) so scratch lands inside the unit boundary instead of tripping (advisory)
    // denies in the system temp. UNIT sessions pass `<cwd>/tmp`; CHAT sessions pass `None` —
    // dropping a `tmp/` dir into a chat's scratch root is pointless (the root IS scratch).
    scratch_tmp: Option<&std::path::Path>,
    // The CLI the seat this bridge carries runs (`seat_cli_of`, core#410): decides the per-seat
    // configuration root — claude's engine-owned worker home, codex/pi/copilot/opencode's own roots
    // under the same base — and which foreign seat variables are STRIPPED.
    seat_cli: wicked_apps_core::spawn::SeatCli,
    // The engine's own operational state home (codex round 8), fenced on chat sessions too.
    operational_home: Option<&std::path::Path>,
) -> anyhow::Result<AcpProcess> {
    // No skills delivery and no per-session settings dir: the tests that use this shape are not
    // run units — the skills handoff belongs to the unit path, which calls the chokepoint
    // directly (as does the chat path, which adds its scope). The fence still rides the frame
    // (`SessionOptions::deny`).
    start_acp_process_with_write_roots(
        config,
        cwd,
        code_graph_db,
        scratch_tmp,
        &[],
        &[],
        &[],
        &crate::skills_snapshot::SkillsDelivery::None,
        seat_cli,
        None,
        operational_home,
    )
}

/// The `OPENCODE_CONFIG_CONTENT` an opencode launch composes its skills paths INTO: the seat's
/// registry value when its `[cli.acp] acp_governance_env` names that variable, else whatever the
/// daemon's own environment carries (the operator's content), else nothing (a bare document is
/// composed). One resolution shared by the pre-spawn admission in `exec_turn_inner` and the
/// spawn chokepoint, so the two cannot judge different values.
fn opencode_existing_config(config: &AcpConfig) -> Option<String> {
    config
        .acp_governance_env
        .as_ref()
        .filter(|(k, _)| k == crate::skills_snapshot::OPENCODE_CONFIG_ENV)
        .map(|(_, v)| v.clone())
        .or_else(|| std::env::var(crate::skills_snapshot::OPENCODE_CONFIG_ENV).ok())
}

/// The per-session Claude configuration carried in `session/new` under
/// `_meta.claudeCode.options` — the object the bridge spreads into the Agent SDK's `Options`
/// (`@agentclientprotocol/claude-agent-acp` 0.73.0, acp-agent.js `userProvidedOptions` :5209,
/// `...userProvidedOptions` :5312) — the ACP analog of the wrapped path's argv, field by field:
///
/// - `skills_plugin` → `plugins: [{type: "local", path}]` (SDK `SdkPluginConfig`), the snapshot
///   — `--plugin-dir` on the wrapped path (core#396);
/// - `deny` → `disallowedTools`, which the bridge MERGES with its own (:5356) — THIS launch's
///   fence, `--disallowedTools` on the wrapped path (v3.1 §3);
/// - `settings` → the SDK's `settings?: string` ("equivalent to the `--settings` CLI flag"), the
///   per-session settings file carrying the same fence.
///
/// Every field is per SESSION: nothing generation- or launch-dependent rides a file two launches
/// share. A non-Claude bridge ignores the `claudeCode` extension (unit paths hand it nothing).
pub(crate) struct SessionOptions<'a> {
    pub skills_plugin: Option<&'a std::path::Path>,
    pub deny: &'a [String],
    pub settings: Option<&'a std::path::Path>,
    /// `additionalDirectories` — the SDK's "directories Claude may also access": a CHAT's scoped
    /// repository roots (core#410 / crew#502), so a seat whose cwd is the chat's scratch root can
    /// read the repos it was pointed at without a permission round-trip per file. Appended to an
    /// existing list (never replacing one). Empty for unit sessions — a unit's read roots are the
    /// in-process boundary's business (`assemble_read_roots`), not the SDK's.
    pub additional_directories: &'a [String],
    /// `settingSources` — the ACP analog of the wrapped path's `--setting-sources project,local`
    /// (codex round 8): the engine OWNS the scope selection, so this is SET (not merged — a list
    /// that kept an existing `user` would defeat the isolation) when present; `None` under the
    /// inherit-config hatch, where the bridge's own default (`user,project,local`) applies.
    pub setting_sources: Option<&'a [&'a str]>,
}

/// The scopes a worker session reads its settings from — never the operator's `user` scope.
pub(crate) const ENGINE_SETTING_SOURCES: &[&str] = &["project", "local"];

#[cfg(test)]
impl SessionOptions<'_> {
    pub(crate) const NONE: SessionOptions<'static> = SessionOptions {
        skills_plugin: None,
        deny: &[],
        settings: None,
        additional_directories: &[],
        setting_sources: None,
    };
}

/// The `session/new` params: the spec fields every ACP agent reads (`cwd`, `mcpServers`) plus
/// the Claude bridge's `_meta.claudeCode.options` extension carrying this session's
/// [`SessionOptions`]. Pure, so the frame the bridge receives is pinned by a test rather than
/// inferred from a live handshake.
fn session_new_params(
    cwd: &std::path::Path,
    mcp_servers: Value,
    options: &SessionOptions<'_>,
) -> Value {
    let mut params = json!({
        "cwd": cwd.to_string_lossy().as_ref(),
        "mcpServers": mcp_servers
    });
    attach_session_options(&mut params, options);
    params
}

/// MERGE `options` into `params._meta.claudeCode.options` — see [`SessionOptions`]. Merge, never
/// replace: every object on the way down is created only where absent, sibling keys
/// (`settingSources`, a future permission option) are kept, an existing `plugins` or
/// `disallowedTools` list gains our entries rather than losing its own. Nothing is attached for
/// an empty option, so a frame with no options carries no `_meta` at all.
fn attach_session_options(params: &mut Value, options: &SessionOptions<'_>) {
    if let Some(root) = options.skills_plugin {
        attach_skills_plugin(params, root);
    }
    if !options.deny.is_empty() {
        let node = claude_code_options(params);
        let ours = options.deny.iter().map(|r| Value::String(r.clone()));
        match node
            .get_mut("disallowedTools")
            .and_then(Value::as_array_mut)
        {
            Some(list) => {
                for rule in ours {
                    if !list.contains(&rule) {
                        list.push(rule);
                    }
                }
            }
            None => node["disallowedTools"] = Value::Array(ours.collect()),
        }
    }
    if let Some(path) = options.settings {
        claude_code_options(params)["settings"] = json!(path.to_string_lossy().as_ref());
    }
    if !options.additional_directories.is_empty() {
        let node = claude_code_options(params);
        let ours = options
            .additional_directories
            .iter()
            .map(|d| Value::String(d.clone()));
        match node
            .get_mut("additionalDirectories")
            .and_then(Value::as_array_mut)
        {
            Some(list) => {
                for dir in ours {
                    if !list.contains(&dir) {
                        list.push(dir);
                    }
                }
            }
            None => node["additionalDirectories"] = Value::Array(ours.collect()),
        }
    }
    if let Some(sources) = options.setting_sources {
        claude_code_options(params)["settingSources"] = json!(sources);
    }
}

/// `params._meta.claudeCode.options`, created where absent. A non-object where an object is
/// needed is a shape this engine never produced; it is replaced so the option lands rather than
/// being silently dropped.
fn claude_code_options(params: &mut Value) -> &mut Value {
    let mut node = params;
    for key in ["_meta", "claudeCode", "options"] {
        if !node.is_object() {
            *node = json!({});
        }
        node = node
            .as_object_mut()
            .expect("made an object just above")
            .entry(key)
            .or_insert_with(|| json!({}));
    }
    if !node.is_object() {
        *node = json!({});
    }
    node
}

/// MERGE the skills snapshot into `params._meta.claudeCode.options.plugins` as the Agent SDK's
/// own `SdkPluginConfig { type: "local", path }`. The bridge spreads that `options` object into
/// its session options, so it is the one channel it honours for plugins (argv it does not parse
/// is discarded — FINDING-060 — and the worker home's `plugins/` is re-sanitized on every spawn).
///
/// DE-DUPLICATED by canonical path (Copilot, review pass 7): a `plugins` list that already carries
/// this snapshot as a local plugin — an upstream layer, a re-attached options object — gains no
/// second entry, so the bridge never loads one generation twice (the ACP twin of the wrapped path's
/// single `--plugin-dir`).
fn attach_skills_plugin(params: &mut Value, root: &std::path::Path) {
    let node = claude_code_options(params);
    let entry = json!({ "type": "local", "path": root.to_string_lossy().as_ref() });
    match node.get_mut("plugins").and_then(Value::as_array_mut) {
        Some(plugins) => {
            let already = plugins.iter().any(|p| {
                p.get("type").and_then(Value::as_str) == Some("local")
                    && p.get("path")
                        .and_then(Value::as_str)
                        .is_some_and(|existing| same_plugin_path(existing, root))
            });
            if !already {
                plugins.push(entry);
            }
        }
        None => node["plugins"] = json!([entry]),
    }
}

/// Do `existing` (a plugin path already in the frame) and `root` name the same directory — spelled
/// identically, or resolving to the same real path (`/snap/.` and `/snap`; a symlinked spelling)?
/// A spelling that cannot be resolved is compared as spelled only.
fn same_plugin_path(existing: &str, root: &std::path::Path) -> bool {
    let spelled = std::path::Path::new(existing);
    if spelled == root {
        return true;
    }
    match (std::fs::canonicalize(spelled), std::fs::canonicalize(root)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// The actual ACP spawn chokepoint. `extra_write_roots` comes from the same launch-validated
/// governance context used to arm `WICKED_WRITE_ROOTS`; Boundary 1 is WRITE containment only,
/// not exfiltration protection, a read jail, or a replacement for ACP governance.
///
/// Ten parameters, each a distinct launch fact documented inline below; bundling them into a
/// struct would touch every test spawn in this file for no gain in clarity.
#[allow(clippy::too_many_arguments)]
fn start_acp_process_with_write_roots(
    config: &AcpConfig,
    cwd: &std::path::Path,
    code_graph_db: Option<&str>,
    scratch_tmp: Option<&std::path::Path>,
    extra_write_roots: &[String],
    // core#410 / crew#502: a CHAT's scoped repository roots, advertised to a claude seat as the
    // SDK's `additionalDirectories` (`SessionOptions`). Empty for unit sessions, whose read roots
    // are the in-process boundary's (`assemble_read_roots`), never the SDK's.
    additional_read_roots: &[String],
    // Ordered `(name, value)` provenance for the estate MCP the worker's `session/new` advertises
    // (`execute_wrapped::estate_provenance_env`) — stamped onto its `proposal.submit`s. Empty for a
    // repo-less session (no estate server is advertised at all) or an ungoverned/chat caller.
    estate_provenance: &[(String, String)],
    // core#396 / v3.2: what this session is handed, in its lever's shape — the snapshot as a LOCAL
    // PLUGIN for the Claude bridge (`session/new` `_meta.claudeCode.options.plugins`, the one
    // channel it honours for plugins: argv it does not parse is discarded, FINDING-060, and the
    // worker home's `plugins/` is re-sanitized on every spawn); `--no-skills --skill …` /
    // `--add-dir …` appended to the bridge argv when the bridge IS pi / copilot; opencode's
    // `skills.paths` composed into `OPENCODE_CONFIG_CONTENT`; or nothing.
    delivery: &crate::skills_snapshot::SkillsDelivery,
    // The CLI the seat this bridge carries runs, as `seat_cli_of` judges it (the merged registry
    // record's `binary` — never the bridge). Decides the per-seat configuration below (core#410):
    // a claude carrier gets the engine-owned worker home (created and re-sanitized here, or
    // inherited under the hatch); codex / pi / copilot / opencode get their OWN roots under the
    // same base through the CLI's own configuration-home variable; every foreign seat variable is
    // STRIPPED — no ambient configuration path of another CLI, and no ensuring (creating,
    // re-sanitizing) claude's home on a foreign seat's account (codex review, PR#413).
    seat_cli: wicked_apps_core::spawn::SeatCli,
    // `Some((run_id, cli_key))` for a UNIT session: names its per-session settings directory
    // (v3.1 §3). `None` for chat sessions.
    session: Option<(&str, &str)>,
    // The engine's OWN operational state home (codex round 8) — fenced on every launch.
    operational_home: Option<&std::path::Path>,
) -> anyhow::Result<AcpProcess> {
    use crate::skills_snapshot::SkillsDelivery;
    // FINDING-061: decided BEFORE the spawn closure so both spawn attempts (the bare binary and
    // the Windows `.cmd` retry) carry the same isolation. Fail CLOSED on a mint failure: a spawn
    // that proceeded without the override would run under the operator's own configuration,
    // which is the exact leak being fixed — and the caller's fallback is the wrapped path, which
    // carries its own isolation.
    // core#410: EVERY seat's configuration decision from the ONE resolver
    // (`wicked_apps_core::spawn::seat_config_for`) — claude keeps the FINDING-061 worker home
    // (created, made private and RE-SANITIZED right here, as before); codex / pi / copilot /
    // opencode get their own roots under the same base (created private); every foreign seat
    // variable is stripped. Fail CLOSED on a resolver error, for every CLI.
    let seat_config = wicked_apps_core::spawn::seat_config_for(seat_cli).map_err(|e| {
        anyhow::anyhow!(
            "ACP worker config isolation failed ({e}); refusing to start an ACP worker under the \
             operator's own CLI configuration (FINDING-061 / core#410)"
        )
    })?;
    let worker_config_dir: Option<std::path::PathBuf> = match (&seat_config, seat_cli) {
        (
            wicked_apps_core::spawn::SeatConfig::Isolated { .. },
            wicked_apps_core::spawn::SeatCli::Claude,
        ) => {
            let dir = ensure_worker_config_home(operational_home).map_err(|e| {
                anyhow::anyhow!(
                    "ACP worker config isolation failed ({e}); refusing to start an ACP worker \
                     under the operator's own CLI configuration (FINDING-061)"
                )
            })?;
            // One resolver: the ensured dir IS the dir the decision sets.
            debug_assert_eq!(Some(dir.as_path()), seat_config.claude_dir());
            Some(dir)
        }
        (wicked_apps_core::spawn::SeatConfig::Isolated { .. }, _) => {
            seat_config.ensure_dirs().map_err(|e| {
                anyhow::anyhow!(
                    "ACP seat config root could not be prepared ({e}); refusing to start '{}' \
                     under the operator's own CLI configuration (core#410)",
                    config.binary
                )
            })?;
            None
        }
        (wicked_apps_core::spawn::SeatConfig::Inherit, _) => None,
    };
    let skills_plugin: Option<&std::path::Path> = match delivery {
        SkillsDelivery::ClaudePlugin(root) => Some(root.as_path()),
        _ => None,
    };
    // v3.1 §3: THIS launch's fence — the state-home registry when the snapshot sits in the read
    // slot, the blanket otherwise (`execute_wrapped::deny_rules`), the engine's own operational
    // home included (codex round 8) — computed once here and carried on this session's own
    // configuration only: the `session/new` options and, for a unit session, its per-session
    // settings file under the worker home. Nothing launch-dependent touches the shared worker
    // home. ALWAYS injected — the inherit escape hatch inherits the operator's scopes (no
    // engine-minted worker home, no `settingSources` override), never the fence (codex round 8;
    // the wrapped path does the same). Attached for every bridge: only the Claude bridge reads
    // `_meta.claudeCode.options`, and the others ignore the extension — one code path, exercised
    // by every stub the tests drive.
    let inherit = crate::execute_wrapped::inherits_operator_config();
    // (codex round 9) a fenced directory the rule syntax cannot spell refuses the spawn — the
    // frame is never sent with a hole in its fence.
    let mut deny: Vec<String> = crate::execute_wrapped::deny_rules(skills_plugin, operational_home)
        .map_err(anyhow::Error::msg)?;
    // A CHAT's scoped repository roots are READ-ONLY (core#410, review): the claude seat's own
    // fence says so — `Edit`/`Write`/`NotebookEdit` under each root are denied in the session's
    // `disallowedTools` (the SDK honours them without a permission round-trip; the boundary on
    // `session/request_permission` covers every other seat and every other tool). A root the rule
    // syntax cannot spell refuses the spawn, like every other fenced directory.
    for root in additional_read_roots {
        let p = crate::execute_wrapped::rule_path(std::path::Path::new(root)).ok_or_else(|| {
            anyhow::anyhow!(
                "refusing to advertise read root {root}: it cannot be spelled as a permission rule"
            )
        })?;
        for tool in ["Edit", "Write", "NotebookEdit"] {
            deny.push(format!("{tool}({p}/**)"));
        }
    }
    let session_settings: Option<std::path::PathBuf> = match (session, &worker_config_dir) {
        (Some((run_id, cli_key)), Some(home)) => {
            Some(write_session_settings(home, run_id, cli_key, &deny)?)
        }
        _ => None,
    };
    let session_options = SessionOptions {
        skills_plugin,
        deny: &deny,
        settings: session_settings.as_deref(),
        additional_directories: additional_read_roots,
        setting_sources: if inherit {
            None
        } else {
            Some(ENGINE_SETTING_SOURCES)
        },
    };
    // v3.2: opencode's lever rides the SAME variable its governance content does — composed,
    // never replaced. The seat's registry value is the base when it names that variable; else
    // whatever the daemon's own environment carries (the operator's content), else a bare doc.
    // A malformed base FAILS the spawn (codex round 3) — `exec_turn_inner` refuses the unit
    // before reaching here; this is the chokepoint's own guarantee for every other caller.
    let opencode_config: Option<String> = delivery
        .opencode_config(opencode_existing_config(config).as_deref())
        .map_err(|why| {
            anyhow::anyhow!(
                "refusing to start '{}': {why}; the seat's governance content is never replaced \
                 with defaults",
                config.binary
            )
        })?;
    let delivery_flags = delivery.argv_flags();
    // Computed ONCE per spawn — not re-probed per turn — because the session this spawn starts
    // is cached and reused across every turn of its lifetime (`probe_cached_session`); "the same
    // resolved ACP binary that is spawned" means the binary this exact process came from, so the
    // check belongs here, at the one point the spawn decision is made, not scattered across
    // later per-turn governance lookups that cannot see which binary actually started this
    // process.
    let governance_verified = match &config.verified_version {
        None => true,
        Some(expected) => resolved_binary_version_matches(&config.binary, expected),
    };
    // The graph's key dir is recognised against THIS daemon's repo-graph root — derived from the
    // same state home the fence is built from (core#406) — and joins the WRITE roots for a UNIT
    // session only (an indexing phase writes it); a CHAT (no unit session) is grounded on its graph
    // READ-ONLY — the MCP runs `--readonly` and the boundary treats the graph as read-only — so its
    // key dir never becomes a kernel-floor write root (Copilot, #426).
    let graph_write = if session.is_some() {
        crate::execute_wrapped::graph_write_dir(code_graph_db, operational_home)
    } else {
        None
    };
    let worker_write_roots = crate::execute_wrapped::armed_write_root_paths(
        cwd,
        extra_write_roots,
        graph_write.as_deref(),
    );
    // A1: a requested kernel floor that cannot arm DISCLOSES AND CONTINUES rather than failing the
    // spawn. `detect_worker_sandbox` returns a best-effort (empty-wrapper) sandbox with a downgrade
    // reason on the no-launcher hosts (all of Windows), firejail-only Linux, and a canonicalize
    // failure. We wrap ONLY when the floor actually armed, and surface the gap on the `AcpProcess`
    // so the caller emits exactly ONE `SandboxUnenforced` per spawn (the session is cached and
    // reused across turns, so the disclosure belongs to the spawn, not each turn).
    let (worker_sandbox, sandbox_downgrade) = if config.os_sandbox {
        let ws = crate::validator::detect_worker_sandbox(&worker_write_roots);
        match ws.downgrade_reason {
            Some(reason) => (None, Some((ws.level.as_wire().to_string(), reason))),
            None => (Some(ws), None),
        }
    } else {
        (None, None)
    };
    let worker_write_roots_env = std::env::join_paths(&worker_write_roots)
        .unwrap_or_else(|_| cwd.as_os_str().to_os_string());
    let build_cmd = |binary: &str| {
        let mut cmd = if let Some(sandbox) = &worker_sandbox {
            let mut cmd = std::process::Command::new(&sandbox.wrapper[0]);
            cmd.args(&sandbox.wrapper[1..]);
            cmd.arg(binary);
            cmd
        } else {
            std::process::Command::new(binary)
        };
        // The engine's internal environment is stripped through the one chokepoint (FINDING-067): an
        // agent CLI that inherits `WICKED_ESTATE_DB` has every estate tool it can spawn pointed at the
        // engine's operational store by default. Governed units do not come through here (they take the
        // wrapped path, FINDING-060), but an ungoverned worker in a repo runs the same
        // `wicked-estate index .`. Harden FIRST — anything set below is set deliberately.
        cmd.hardened();
        if config.os_sandbox {
            cmd.env(crate::gate_hook::WRITE_ROOTS_ENV, &worker_write_roots_env);
        }
        // Set AFTER `hardened()`, per the ordering contract in `wicked_apps_core::spawn`: clear
        // to a known slate, then set exactly what this path intends. The seat's OWN
        // configuration-home variable(s) point into its root under the worker home (this also
        // overrides any CLAUDE_CONFIG_DIR / CODEX_HOME / … the daemon itself inherited — the
        // operator's live config dir is frequently exactly that variable); every FOREIGN seat
        // variable is stripped, so a codex/pi/copilot/opencode bridge never carries an ambient
        // claude configuration path (PR#413) and a claude one never carries theirs (core#410).
        // The inherit hatch sets and strips nothing, on purpose.
        seat_config.apply(&mut cmd);
        // UNCONDITIONAL — never gated on whether THIS unit is governed (DES-INPUT-GOV-006 §3.3).
        // A session is spawned once and cached/reused across turns (`probe_cached_session`); a
        // process spawned before a later turn's governance decision would have no way to
        // retroactively gain this env var, so conditioning it on a per-turn check could leave a
        // governed turn running against an unprotected, already-spawned process. Injecting
        // unconditionally costs an ungoverned session nothing beyond extra
        // session/request_permission round-trips — the shared ACP client already answers those
        // with an unconditional allow (`acp_ungoverned_event`'s own "allow_result, unchecked").
        if let Some((k, v)) = &config.acp_governance_env {
            // v3.2: when opencode's skills paths were composed onto this very variable, the
            // composed value below carries the governance content too — set it once.
            if !(opencode_config.is_some() && k == crate::skills_snapshot::OPENCODE_CONFIG_ENV) {
                cmd.env(k, v);
            }
        }
        if let Some(content) = &opencode_config {
            cmd.env(crate::skills_snapshot::OPENCODE_CONFIG_ENV, content);
        }
        // In-boundary scratch for unit sessions (core#264) — tools the bridge spawns inherit
        // this, so `mktemp`/`$TMPDIR` writes land inside the unit instead of the system temp.
        // Set ONLY when the dir really exists as a directory (same rule as the wrapped path):
        // a temp env pointing at nothing breaks tools that consult it; left unset, the worker
        // falls back to the system temp, which the advisory carve-out tolerates (Copilot).
        if let Some(tmp) = scratch_tmp {
            if std::fs::create_dir_all(tmp).is_ok() && tmp.is_dir() {
                cmd.env("TMPDIR", tmp);
                cmd.env("TMP", tmp);
                cmd.env("TEMP", tmp);
            }
        }
        cmd.args(&config.start_args);
        // v3.2: pi's `--no-skills --skill <dir>…` / copilot's `--add-dir <view>` — only when the
        // bridge IS that CLI (`WorkerCli::for_binaries` judged the carrier); empty otherwise.
        cmd.args(&delivery_flags);
        cmd.current_dir(cwd);
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        // Defense-in-depth for crew#290: put the bridge in its OWN process group so a
        // signal delivered to the DAEMON's process group — a Ctrl-C in the terminal the
        // daemon runs in (SIGINT to the foreground group), or a `kill -TERM -<daemon_pgid>`
        // / `pkill -g <daemon_pgid>` — cannot reach an idle cached bridge. Upstream turns
        // SIGTERM/SIGINT into a silent `process.exit(0)`, so a stray group signal was one
        // way an idle bridge could die between units and then be reused blind.
        //
        // `process_group(0)` calls `setpgid(0, 0)` in the child post-fork/pre-exec: the
        // child becomes the leader of a new group whose id equals its own pid. This changes
        // ONLY the process-group id — the parent/child link is untouched (ppid stays the
        // daemon), so `Child::kill`/`wait` (pid-targeted, see `KillHandle::signal`) and the
        // stdin-EOF teardown both still work exactly as before.
        //
        // This is NOT complete protection and core#343's liveness probe stays the PRIMARY
        // recovery: a pid-targeted signal (`kill <pid>`) and, crucially, a name-pattern
        // `pkill claude` / `pkill -f claude-agent-acp` still reach the bridge directly —
        // process-group isolation only blocks GROUP- and terminal-scoped signals, not
        // signals addressed to the process by pid or by name.
        //
        // Windows has no process groups in the POSIX sense; the analogue is spawning with
        // the `CREATE_NEW_PROCESS_GROUP` creation flag (via `CommandExt::creation_flags`),
        // which detaches the child from the console's Ctrl-C/Ctrl-Break group. The bridge
        // path is Unix-shaped today (stdio adapters, the reaper, this whole runner run on
        // Unix in the field), so this is gated `#[cfg(unix)]` rather than guessed at for
        // win32; a Windows port should add the `creation_flags(CREATE_NEW_PROCESS_GROUP)`
        // leg here and verify Ctrl-Break routing before relying on it.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        cmd
    };

    // Held for the spawn and both handshake calls, released when this function returns — or NOT
    // held at all, if no permit came free inside `START_WAIT`. That is the designed outcome, not a
    // failure: the start proceeds either way, contended rather than queued.
    let _permit = start_gate().acquire(START_WAIT);

    let mut child = match build_cmd(&config.binary).spawn() {
        Ok(c) => c,
        // Windows: npm installs launcher shims as `<name>.cmd`, which CreateProcess
        // does not resolve for a bare name — retry with the extension explicit
        // (std special-cases explicit .cmd/.bat since the BatBadBut hardening).
        // Only when the configured binary has no extension of its own: appending
        // to `foo.exe` would produce a nonsensical `foo.exe.cmd`.
        Err(e)
            if cfg!(windows)
                && e.kind() == std::io::ErrorKind::NotFound
                && std::path::Path::new(&config.binary).extension().is_none() =>
        {
            let cmd_name = format!("{}.cmd", config.binary);
            build_cmd(&cmd_name).spawn().map_err(|e2| {
                anyhow::anyhow!(
                    "ACP binary '{}': {e} (also tried '{cmd_name}': {e2})",
                    config.binary
                )
            })?
        }
        Err(e) => return Err(anyhow::anyhow!("ACP binary '{}': {e}", config.binary)),
    };

    // Start draining stderr immediately: a bridge that fails during startup writes its reason
    // there, and a piped stream nobody reads eventually blocks the writer.
    let (stderr_tail, stderr_reader) = match child.stderr.take() {
        Some(s) => {
            let (tail, handle) = drain_stderr(s);
            (tail, Some(handle))
        }
        None => (
            Arc::new(Mutex::new(std::collections::VecDeque::new())),
            None,
        ),
    };

    // Take stdout/stdin before spawning the reader — kill the child if either fails so we
    // don't leak a background process when the child started but didn't expose its pipes.
    let stdout = match child.stdout.take() {
        Some(s) => s,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(anyhow::anyhow!("ACP binary '{}': no stdout", config.binary));
        }
    };
    let mut stdin = BufWriter::new(match child.stdin.take() {
        Some(s) => s,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(anyhow::anyhow!("ACP binary '{}': no stdin", config.binary));
        }
    });

    // Unbounded channel — the reader thread never blocks the child on a full buffer.
    let (tx, rx) = std::sync::mpsc::channel();
    let reader_thread = std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        loop {
            match read_bounded_frame(&mut reader, FRAME_BYTE_CAP) {
                Ok(FrameRead::Frame(line)) => {
                    if !line.is_empty() && tx.send(line).is_err() {
                        break;
                    }
                }
                Ok(FrameRead::Oversized) => {
                    tracing::warn!(
                        frame_byte_cap = FRAME_BYTE_CAP,
                        "dropping oversized ACP stdout frame"
                    );
                }
                Ok(FrameRead::Eof) | Err(_) => break,
            }
        }
    });

    // Helper: kills the child and waits before returning a handshake error so we don't leak
    // the background process when initialize/session-new fails or times out. The bridge's own
    // stderr is appended to every one of these — the failure reason reaches `AcpFallback`, which
    // is the operator's only signal that the governed path was abandoned.
    macro_rules! handshake_err {
        ($child:expr, $e:expr) => {{
            let _ = $child.kill();
            let _ = $child.wait();
            let e: anyhow::Error = $e;
            return Err(anyhow::anyhow!("{e}{}", stderr_context(&stderr_tail)));
        }};
    }

    // Advertise elicitation/form support only to adapters in the verified allow-list
    // (ELICITATION_VERIFIED_ADAPTERS, matched on the binary's file stem — see
    // `elicitation_verified_adapter`). Other adapters receive no elicitation capability so
    // they cannot suspend turns waiting for a human response that will never arrive.
    // This decision is made exactly ONCE per session, here, and stored on the returned
    // `AcpProcess` (`elicitation_advertised`) — turn-time gating reads that stored flag,
    // so advertisement and `elicitation/create` handling cannot diverge (core#341).
    let form_enabled = elicitation_verified_adapter(&config.binary);
    // `permission: true` says this client ANSWERS session/request_permission. Without it the
    // bridge never asks, which is exactly why the ACP path ran ungoverned (FINDING-060/062).
    //
    // `fs: {}` is DELIBERATELY EMPTY and stays empty (core#293 review point). ACP's
    // `FileSystemCapability` is `{readTextFile: bool, writeTextFile: bool}`, both defaulting to
    // false — so `{}` advertises NO filesystem capability, and a spec-conforming agent never sends
    // `fs/read_text_file` or `fs/write_text_file`. That is why this client has no handler for
    // them: it never claimed them. The alternative (implementing the two methods) would hand the
    // agent an ungoverned read/write channel that bypasses the permission gate above — the
    // opposite of what this path is for. A non-conforming agent that asks anyway now receives a
    // JSON-RPC `Method not found` from the dispatcher's catch-all instead of being left blocked.
    let client_caps = if form_enabled {
        json!({"fs": {}, "terminal": false, "permission": true, "elicitation": {"form": {}}})
    } else {
        json!({"fs": {}, "terminal": false, "permission": true})
    };
    if let Err(e) = rpc_send(
        &mut stdin,
        1,
        "initialize",
        json!({
            "protocolVersion": 1,
            "clientCapabilities": client_caps,
            "clientInfo": {"name": "wicked-core", "version": env!("CARGO_PKG_VERSION")}
        }),
    ) {
        handshake_err!(child, e);
    }
    // FINDING-015: this result used to be discarded (`if let Err(e) = rpc_expect(...)`), so the
    // `authMethods` the agent advertised were never read and `authenticate` was never sent — an
    // auth-requiring agent then stalled or errored on `session/new` with nothing naming the
    // actual problem. Capture it, and run the ACP auth step below when the agent asks for one.
    let init = match rpc_expect(&rx, &mut stdin, 1, initialize_budget()) {
        Ok(v) => v,
        Err(e) => handshake_err!(child, e),
    };
    let auth_methods: Vec<String> = init["result"]["authMethods"]
        .as_array()
        .map(|methods| {
            methods
                .iter()
                .filter_map(|m| m.get("id").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    let mut next_id: u64 = 2;
    // `(methodId sent, authenticate's own failure if it had one)` — carried into the named error
    // below so a refused session tells the operator what was already tried.
    let mut auth_attempt: Option<(String, Option<anyhow::Error>)> = None;
    if !auth_methods.is_empty() {
        // The operator's explicit choice wins; otherwise the FIRST advertised method — the ACP
        // contract puts the agent's preferred method first, and guessing differently here would
        // encode one agent's auth surface into every agent's startup.
        let method_id = config
            .auth_method
            .clone()
            .unwrap_or_else(|| auth_methods[0].clone());
        let id = next_id;
        next_id += 1;
        let outcome = rpc_send(
            &mut stdin,
            id,
            "authenticate",
            json!({ "methodId": method_id }),
        )
        .and_then(|()| rpc_expect(&rx, &mut stdin, id, initialize_budget()).map(|_| ()));
        // A failed `authenticate` is NOT fatal on its own: agents advertise methods even while
        // their stored credentials are already valid, and some reject `authenticate` outright in
        // that state (claude-agent-acp@0.62 throws "Method not implemented." for its terminal
        // methods). The authority on whether auth is satisfied is `session/new` below; the
        // failure is kept so the named error can carry it if it turns out to matter.
        auth_attempt = Some((method_id, outcome.err()));
    }

    // `mcpServers` is required by the ACP spec — native ACP agents (copilot --acp)
    // reject session/new with -32602 when it is absent; bridges ignore it. When the run has a code
    // graph the engine vouched for — its repo's own, or its project's (`actor::run_code_graph_db`)
    // — advertise the estate MCP server over it (FINDING-122) — the ACP stdio-server shape
    // ({name,command,args,env}) of the same parts the wrapped path writes into settings.json. A
    // repo-less session keeps the empty array exactly as before.
    let mcp_servers = crate::execute_wrapped::repo_estate_mcp_parts(code_graph_db)
        .map(|(command, args)| {
            // Server-side provenance for the estate MCP's `proposal.submit` (DES-MEM-FACETED-001
            // follow-on): the ACP `env` is the spec's `{name,value}` ARRAY (vs the wrapped carrier's
            // object) — same pairs, formatted for this carrier so a proposal from an ACP worker carries
            // the run/unit/agent that produced it.
            let env: Vec<serde_json::Value> = estate_provenance
                .iter()
                .map(|(name, value)| json!({ "name": name, "value": value }))
                .collect();
            json!([{
                "name": "wicked-estate",
                "command": command,
                "args": args,
                "env": env
            }])
        })
        .unwrap_or_else(|| json!([]));
    let session_new = session_new_params(cwd, mcp_servers, &session_options);
    let session_new_id = next_id;
    next_id += 1;
    if let Err(e) = rpc_send(&mut stdin, session_new_id, "session/new", session_new) {
        handshake_err!(child, e);
    }
    let resp = match rpc_expect(&rx, &mut stdin, session_new_id, session_new_budget()) {
        Ok(v) => v,
        Err(e) => {
            // FINDING-015, the fail-fast half: an `auth_required` refusal gets the NAMED error —
            // the operator's fix is credentials (or `auth_method` in the registry), not retries,
            // and a bare "ACP server error: {code:-32000}" says neither. Matched on the code the
            // agent sent, not on its message text.
            let still_unauth = e
                .downcast_ref::<RpcServerError>()
                .is_some_and(|se| se.code == Some(AUTH_REQUIRED_CODE));
            if still_unauth {
                handshake_err!(
                    child,
                    unauthenticated_error(&config.binary, &auth_methods, auth_attempt.as_ref(), &e)
                );
            }
            handshake_err!(child, e)
        }
    };
    let session_id = match resp["result"]["sessionId"].as_str() {
        Some(s) => s.to_string(),
        None => handshake_err!(
            child,
            anyhow::anyhow!("ACP session/new: missing sessionId in response")
        ),
    };
    // core#274: a pi unit reported its worktree "completely empty" while the diff sat exactly
    // where session/new's cwd pointed — whether the bridge honoured the param was undecidable
    // because the response was never recorded. Log it (bounded) with the cwd we REQUESTED, so
    // the next such report pins the divergence to the bridge, not the spawn.
    {
        let resp_note: String = resp["result"].to_string().chars().take(600).collect();
        eprintln!(
            "[wicked-core] ACP session/new for '{}': requested cwd={} → {resp_note}",
            config.binary,
            cwd.display()
        );
    }

    Ok(AcpProcess {
        kill_handle: Arc::new(KillHandle::new(child)),
        no_code: false,
        write_lock: Arc::new(Mutex::new(())),
        stdin,
        line_rx: rx,
        _reader: reader_thread,
        stderr_tail,
        _stderr_reader: stderr_reader,
        session_id,
        next_id,
        elicitation_advertised: form_enabled,
        governance_verified,
        sandbox_downgrade,
        // Bound by the unit runner right after the spawn (it holds the admitted snapshot; this
        // chokepoint only knows the delivery it put in the handshake).
        skills: None,
        chat_boundary: None,
        session_dir: session_settings
            .as_deref()
            .and_then(std::path::Path::parent)
            .map(std::path::Path::to_path_buf),
    })
}

/// The named failure for FINDING-015: `session/new` was refused with [`AUTH_REQUIRED_CODE`].
/// Which variant fires depends on what the auth step already tried, so the message states what
/// happened, what was attempted, and what the operator can change — never just the raw code.
fn unauthenticated_error(
    binary: &str,
    advertised: &[String],
    attempt: Option<&(String, Option<anyhow::Error>)>,
    refusal: &anyhow::Error,
) -> anyhow::Error {
    match attempt {
        Some((method_id, Some(auth_err))) => anyhow::anyhow!(
            "ACP agent '{binary}' requires authentication: `authenticate` (methodId \
             '{method_id}') failed ({auth_err}), then session/new was refused as \
             unauthenticated ({refusal}). Advertised authMethods: {advertised:?} — set \
             `auth_method` in this CLI's [cli.acp] registry entry to one of them, or \
             authenticate the agent out of band"
        ),
        Some((method_id, None)) => anyhow::anyhow!(
            "ACP agent '{binary}' is still unauthenticated after `authenticate` (methodId \
             '{method_id}') succeeded: session/new was refused ({refusal}). Advertised \
             authMethods: {advertised:?}"
        ),
        None => anyhow::anyhow!(
            "ACP agent '{binary}' requires authentication but advertised no authMethods at \
             initialize; session/new was refused ({refusal})"
        ),
    }
}

// ── JSON-RPC helpers ──────────────────────────────────────────────────────────

fn rpc_send(
    stdin: &mut BufWriter<ChildStdin>,
    id: u64,
    method: &str,
    params: Value,
) -> anyhow::Result<()> {
    let msg = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
    writeln!(stdin, "{msg}")?;
    stdin.flush()?;
    Ok(())
}

/// Send a JSON-RPC 2.0 response to a `request_id` (which may be a string, a number, or — see
/// below — `null`). The `id` field is echoed VERBATIM from the incoming request — NOT cast
/// to u64 — so string-typed request ids (common in ACP adapters) round-trip
/// correctly. `result` is the response payload.
///
/// There is deliberately NO "null id ⇒ stay silent" guard here (Copilot review, core#293). A
/// NOTIFICATION is a frame whose `id` member is ABSENT (JSON-RPC 2.0 §4.1); an EXPLICIT
/// `"id": null` is a legal request id — the spec's own parse-error responses carry it — and its
/// sender blocks until answered. Notifications are filtered structurally by the dispatchers via
/// [`is_notification`], so every id that reaches this function belongs to a real request and is
/// echoed as-is. A guard here would silently re-introduce the drop this PR exists to remove.
fn rpc_respond<W: Write>(writer: &mut W, request_id: &Value, result: Value) -> anyhow::Result<()> {
    let msg = json!({
        "jsonrpc": "2.0",
        "id": request_id,
        "result": result,
    });
    writeln!(writer, "{msg}")?;
    // Flush immediately — the adapter's stdin is typically a pipe; a response left in the
    // BufWriter buffer deadlocks the adapter's `r()` / `readline()` call indefinitely.
    // `flush()` is a no-op for `Vec<u8>` (unit tests), so this is safe in all contexts.
    writer.flush()?;
    Ok(())
}

/// JSON-RPC 2.0 `Method not found`. Sent to any agent-originated REQUEST this client has no
/// handler for, so the agent gets an answer instead of blocking forever (core#293).
const METHOD_NOT_FOUND_CODE: i64 = -32601;

/// Send a JSON-RPC 2.0 ERROR response to `request_id`.
///
/// The reason this exists (core#293): an inbound request whose `method` matches no arm used to be
/// silently DROPPED. A JSON-RPC request blocks its sender until it is answered, so a dropped one
/// wedges the agent for the whole turn timeout with no diagnostic anywhere. Answering with an
/// error is the protocol-correct "I don't implement that" and lets the agent proceed. Any future
/// ACP method therefore degrades to a refusal rather than a hang.
///
/// Like [`rpc_respond`], this carries no null-id guard: `"id": null` is a request id, not a
/// notification marker (see that function's docs), and JSON-RPC 2.0 §5.1 in fact REQUIRES an
/// error response to echo a null id when the id could not be determined.
fn rpc_respond_error<W: Write>(
    writer: &mut W,
    request_id: &Value,
    code: i64,
    message: &str,
) -> anyhow::Result<()> {
    let msg = json!({
        "jsonrpc": "2.0",
        "id": request_id,
        "error": {"code": code, "message": message},
    });
    writeln!(writer, "{msg}")?;
    writer.flush()?;
    Ok(())
}

/// Whether `v` is a JSON-RPC RESPONSE to the outbound request `id` we are waiting on.
///
/// THE `method` CHECK IS THE POINT (core#293). Matching on `id` alone conflates two opposite
/// frame kinds: our own response (id, `result`/`error`, NO `method`) and an agent-originated
/// REQUEST (id, `method`, `params`). The two id spaces are independent — the client counts
/// `AcpProcess::next_id` from 2 and never resets it per turn, the bridge SDK counts its own
/// requests from 0 — so they eventually cross. On a crossing, `session/request_permission` was
/// consumed as the prompt RESULT: no `result.stopReason` → `unwrap_or("end_turn")` → the turn was
/// declared complete while the agent sat blocked on a permission nobody would ever answer, and
/// the NEXT prompt went to an agent that was not listening (0 tools, 0 hooks, idle until the
/// turn timeout).
///
/// A frame the agent ORIGINATED is never a response, whatever its id says.
fn is_response_to(v: &Value, id: u64) -> bool {
    agent_method(v).is_none() && v.get("id").and_then(Value::as_u64) == Some(id)
}

/// The `method` of a frame the AGENT originated — a request or a notification — or `None` when
/// the frame is a response and must be matched on id instead.
///
/// The primary test is the presence of `method`: a JSON-RPC response never carries one. The
/// `result`/`error` half is belt-and-braces against an adapter that sloppily ECHOES the method
/// back on its own response. Without it, this fix would classify such a response as an unknown
/// request, answer it with `Method not found`, and hang the very handshake it exists to protect —
/// trading one wedge for another. A response MUST carry `result` or `error`; a request MUST NOT.
/// When a frame contradicts itself, "it answers something" wins.
///
/// Deliberately NOT strengthened to "a response must carry result/error": a malformed bare
/// `{"id":n}` has always been treated as a (useless) response and terminated the wait. Requiring
/// `result`/`error` would turn that into a silent 2-hour hang — the failure mode this issue is
/// about — so the loose reading is kept for frames that at least do not claim to be requests.
fn agent_method(v: &Value) -> Option<&str> {
    if v.get("result").is_some() || v.get("error").is_some() {
        return None;
    }
    v.get("method").and_then(Value::as_str)
}

/// Whether `v` is a JSON-RPC 2.0 NOTIFICATION: agent-originated (it carries a `method`) AND its
/// `id` member is ABSENT.
///
/// ABSENCE is the whole test (Copilot review, core#293). "A Notification is a Request object
/// without an `id` member" (§4.1) — it is not "a request whose id is null". `"id": null` is a
/// permitted request id, and the dispatchers used to derive ids with
/// `v.get("id").cloned().unwrap_or(Value::Null)`, which flattened the two into one value: an
/// agent that sent a real request with an explicit null id was classified as a notification,
/// never answered, and left blocked for the whole turn timeout — the exact class of silent drop
/// this issue removes. Testing the MEMBER instead of its value keeps them apart, so a
/// notification draws no response and an explicit-null-id request draws one echoing `null`.
fn is_notification(v: &Value) -> bool {
    agent_method(v).is_some() && v.get("id").is_none()
}

/// The `id` an agent-originated frame must be ANSWERED on, or `None` when it is a notification
/// and must not be answered at all.
///
/// The returned `Value` may itself be `Value::Null` — that is a request with an explicit null id,
/// and it gets a response echoing `null`. Callers must NOT re-test the returned value for
/// nullness; presence is the entire question and [`is_notification`] has already settled it.
fn answerable_id(v: &Value) -> Option<&Value> {
    if is_notification(v) {
        return None;
    }
    v.get("id")
}

/// Answer an agent REQUEST and, when the write fails, SAY SO in the turn output instead of
/// discarding the error (Copilot review, core#293).
///
/// A lost response leaves the agent blocked on that request until the turn times out, and the io
/// error is the only thing that explains the stall — swallowing it turns a broken pipe into "the
/// model was slow". `what` names the request in the note, e.g. "a permission request".
fn respond_or_note<W: Write>(
    stdin: &mut W,
    write_lock: &Mutex<()>,
    request_id: &Value,
    result: Value,
    what: &str,
    output: &mut String,
    max_out: usize,
) {
    let respond_err = {
        let _wl = write_lock.lock().unwrap_or_else(|p| p.into_inner());
        rpc_respond(stdin, request_id, result).err()
    };
    note_write_failure(respond_err, what, output, max_out);
}

/// [`respond_or_note`] for the refusal path: send `Method not found` and surface a failed write
/// rather than dropping it. `what` names the refused request.
fn refuse_or_note<W: Write>(
    stdin: &mut W,
    write_lock: &Mutex<()>,
    request_id: &Value,
    message: &str,
    what: &str,
    output: &mut String,
    max_out: usize,
) {
    let respond_err = {
        let _wl = write_lock.lock().unwrap_or_else(|p| p.into_inner());
        rpc_respond_error(stdin, request_id, METHOD_NOT_FOUND_CODE, message).err()
    };
    note_write_failure(respond_err, what, output, max_out);
}

/// Append the "we could not answer, the agent is stuck" note for a failed response write.
fn note_write_failure(
    respond_err: Option<anyhow::Error>,
    what: &str,
    output: &mut String,
    max_out: usize,
) {
    if let Some(e) = respond_err {
        let note = format!(
            "\n[wicked-core] could not answer {what}: {e}. The agent is blocked on it and this \
             turn will time out."
        );
        append_within_cap(output, &note, max_out);
    }
}

/// ACP adapters verified to correctly serialize tool execution across the
/// `elicitation/create` suspension boundary (OQ-R-6). Entries are adapter BINARY
/// names (never registry `cli_key`s — keys diverge from binaries: the stock `claude`
/// seat runs the `claude-agent-acp` bridge), matched by [`elicitation_verified_adapter`].
/// The one consumer is `start_acp_process`, which both advertises the capability and
/// stores the decision on `AcpProcess::elicitation_advertised` for the turn-time gate
/// in `exec_turn_acp` — one predicate, one evaluation, both sites (core#341).
///
/// Adding a new adapter REQUIRES a verifiable artifact (link to passing integration
/// test run or source-code audit in the PR description) — self-assertion alone is
/// insufficient (spec §Ask first).
const ELICITATION_VERIFIED_ADAPTERS: &[&str] = &["claude-agent-acp", "codex-acp"];

/// Whether `binary` is one of the [`ELICITATION_VERIFIED_ADAPTERS`], classified by the
/// binary's file STEM — the same rationale as `binary_is_claude` (core#341): registry
/// records and clis.toml overlays may point at an absolute path (`/opt/bin/claude-agent-acp`)
/// or a platform-suffixed name (`claude-agent-acp.cmd` — the Windows spawn retry), and all
/// of those run the same verified bridge code. Registry `cli_key`s (e.g. `claude`,
/// `codex`, or an aliased `claude-eval`) never reach this predicate — the capability is a
/// property of the adapter program, not of the seat name.
fn elicitation_verified_adapter(binary: &str) -> bool {
    std::path::Path::new(binary)
        .file_stem()
        .and_then(|s| s.to_str())
        .map(|stem| ELICITATION_VERIFIED_ADAPTERS.contains(&stem))
        .unwrap_or(false)
}

/// Dual-poll interval for the `'elicit` loop: check the resolution channel AND
/// drain stdout every 50 ms to prevent the ACP adapter's stdout buffer from filling
/// (a full buffer deadlocks the adapter's stdin writes).
const ELICITATION_POLL_MS: u64 = 50;

/// Cap on bytes read from a single stdout frame. Prevents a runaway adapter from
/// growing the output buffer beyond MAX_OUT * 7 (56 MB).
const FRAME_BYTE_CAP: usize = 8 * 1024 * 1024 * 7;

enum FrameRead {
    Frame(String),
    Oversized,
    Eof,
}

/// Read one newline-delimited frame without ever allocating beyond `cap` bytes.
/// Oversized frames are drained through their newline and reported to the caller so
/// the following well-formed frame remains parseable.
fn read_bounded_frame<R: BufRead>(reader: &mut R, cap: usize) -> std::io::Result<FrameRead> {
    let mut bytes = Vec::new();
    let mut oversized = false;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return if bytes.is_empty() && !oversized {
                Ok(FrameRead::Eof)
            } else if oversized {
                Ok(FrameRead::Oversized)
            } else {
                Ok(FrameRead::Frame(
                    String::from_utf8_lossy(&bytes).into_owned(),
                ))
            };
        }

        let newline = available.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(available.len(), |index| index + 1);
        let payload_len = newline.unwrap_or(available.len());
        if !oversized {
            if bytes.len().saturating_add(payload_len) > cap {
                oversized = true;
                bytes.clear();
            } else {
                bytes.extend_from_slice(&available[..payload_len]);
            }
        }
        reader.consume(consumed);

        if newline.is_some() {
            if oversized {
                return Ok(FrameRead::Oversized);
            }
            if bytes.last() == Some(&b'\r') {
                bytes.pop();
            }
            return Ok(FrameRead::Frame(
                String::from_utf8_lossy(&bytes).into_owned(),
            ));
        }
    }
}

/// Wait for the JSON-RPC response whose `"id"` matches `id`, skipping both
/// notifications and non-JSON startup banners/logs. Returns `Err` on timeout,
/// channel disconnect, or a server-side `"error"` field.
/// The JSON-RPC `error.code` the ACP spec assigns to "authentication required": the agent
/// refuses the call until `authenticate` succeeds. Matched structurally on the code the agent
/// sent (via [`RpcServerError`]), never by pattern-matching a rendered message.
const AUTH_REQUIRED_CODE: i64 = -32000;

/// Whether a turn error is the bridge's `-32000 Authentication required` refusal (crew#267) —
/// matched on the CODE via downcast, never on display text. Pure so the classification is
/// testable without a live bridge.
fn is_auth_required_error(e: &anyhow::Error) -> bool {
    e.downcast_ref::<RpcServerError>()
        .is_some_and(|se| se.code == Some(AUTH_REQUIRED_CODE))
}

/// A JSON-RPC error frame from the agent, kept structured so a caller can react to the CODE
/// (e.g. [`AUTH_REQUIRED_CODE`]) with a `downcast_ref` instead of grepping the display string.
/// Renders exactly the message [`rpc_expect`] always produced, so nothing operator-visible
/// changed when this type was introduced.
#[derive(Debug)]
struct RpcServerError {
    code: Option<i64>,
    raw: String,
}

impl std::fmt::Display for RpcServerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ACP server error: {}", self.raw)
    }
}

impl std::error::Error for RpcServerError {}

///
/// During the handshake phase an `elicitation/create` notification may arrive from
/// an adapter that races the handshake. The guard immediately responds with
/// `action:"cancel"` via `stdin` so the adapter does not stall waiting for a
/// resolution that will never come during startup.
fn rpc_expect<W: Write>(
    rx: &std::sync::mpsc::Receiver<String>,
    stdin: &mut W,
    id: u64,
    timeout: Duration,
) -> anyhow::Result<Value> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or_default();
        if remaining.is_zero() {
            return Err(anyhow::anyhow!("ACP timeout waiting for response id={id}"));
        }
        match rx.recv_timeout(remaining) {
            Ok(line) => {
                // Skip non-JSON lines (startup banners, log output, etc.) — consistent
                // with exec_turn_acp which also silently skips non-JSON noise.
                let v: Value = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                // Anything carrying a `method` is agent-originated — a REQUEST or a
                // notification — and is dispatched HERE, before the id comparison below
                // (core#293). It is never the response we are waiting on, however its id
                // happens to collide with ours.
                if let Some(method) = agent_method(&v) {
                    // A true NOTIFICATION (the `id` member is ABSENT) expects no answer — skip
                    // it. Anything else is a REQUEST and gets answered below, INCLUDING one
                    // whose id is an explicit `null`: that is a legal id, not a notification
                    // marker, and its sender blocks until we reply (Copilot review, core#293).
                    let Some(req_id) = answerable_id(&v).cloned() else {
                        continue;
                    };
                    // A failed write during the handshake is fatal: the agent stays blocked on
                    // this request, our own `initialize`/`session/new` will never be answered,
                    // and the wait would expire as a bare "ACP timeout" naming nothing. Propagate
                    // instead, so the handshake fails immediately with the io error and the
                    // method that could not be answered (Copilot review).
                    let written = if method == "elicitation/create" {
                        // Elicitation guard: a stray `elicitation/create` during handshake is
                        // immediately cancelled — it cannot be resolved (no maps context here)
                        // and must not block the handshake.
                        rpc_respond(stdin, &req_id, json!({"action":"cancel"}))
                    } else {
                        // Any OTHER inbound request during the handshake: refuse it explicitly.
                        // There is no session yet and no gate context, so it cannot be served —
                        // but dropping it would leave the agent blocked and stall the handshake
                        // into a timeout that names nothing.
                        rpc_respond_error(
                            stdin,
                            &req_id,
                            METHOD_NOT_FOUND_CODE,
                            &format!("wicked-core does not handle `{method}` during the handshake"),
                        )
                    };
                    written.map_err(|e| {
                        anyhow::anyhow!(
                            "ACP handshake could not answer the agent's `{method}` request \
                             (id={req_id}) while waiting for response id={id}: {e}"
                        )
                    })?;
                    continue;
                }
                if is_response_to(&v, id) {
                    if let Some(err) = v.get("error") {
                        return Err(anyhow::Error::new(RpcServerError {
                            code: err.get("code").and_then(Value::as_i64),
                            raw: err.to_string(),
                        }));
                    }
                    return Ok(v);
                }
                // A response to some OTHER outbound id (e.g. a call this loop already gave up
                // on) — skip it silently; it blocks nobody.
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err(anyhow::anyhow!("ACP process exited during handshake"));
            }
        }
    }
}

// ── Turn execution ────────────────────────────────────────────────────────────

struct TurnResult {
    output: String,
    status: StepStatus,
    usage: Option<Usage>,
    files: Vec<String>,
    /// Tool NAMES invoked this turn (FINDING-046). Empty on the ACP path today: unlike the
    /// stream-json path (`tool_use.name` → Read/Bash/Edit), ACP reports tool activity on
    /// `tool_call`/`tool_call_update` notifications as `kind`/`title`, a different identity that
    /// must be pinned against a live frame before it can be emitted without misleading an operator.
    /// Carried as a field now so the `ToolInvoked` event is uniform across runners; populating it
    /// from ACP frames is the scoped follow-up.
    tools: Vec<String>,
}

impl TurnResult {
    /// Construct a default-failed `TurnResult` with empty output. Used as the
    /// starting state before a turn executes; callers overwrite it on success.
    #[allow(dead_code)]
    fn default_failed() -> Self {
        Self {
            output: String::new(),
            status: StepStatus::Failed,
            usage: None,
            files: Vec::new(),
            tools: Vec::new(),
        }
    }
}

/// Send one `session/prompt` request and collect `session/update` notifications until
/// the response arrives (or `timeout` elapses). Streams text deltas through `emit`.
///
/// `prior_outputs` are injected as leading ACP prompt blocks so the agent sees the work this turn is
/// supposed to build on — a peer CLI's output, or (FINDING-024) the output of a phase this one
/// declared `depends_on`. Each block is prefixed with its label so the agent can attribute the
/// contribution, and a contract header precedes them stating that they are the subject of the task.
/// When the slice is empty the prompt stays a single text block exactly as before — no header.
/// Validate an `elicitation/create` `requestedSchema` and, if valid, return
/// `(prop_name, prop_type)`. Returns `None` when the schema has more than one
/// property or the single property's `type` is not `"string"`.
///
/// The guard is deliberately restrictive (OQ-R-5): ACP elicitation is intended
/// for short confirmations, not for general-purpose forms with rich types. A
/// multi-property or non-string schema is immediately cancelled so the adapter
/// cannot stall waiting for a response that wicked-core will never provide.
fn validate_elicitation_schema(schema: &Value) -> Option<(String, Option<String>)> {
    let props = schema.get("properties").and_then(Value::as_object)?;
    if props.len() != 1 {
        return None; // zero or >1 properties → cancel
    }
    let (prop_name, prop_schema) = props.iter().next()?;
    let prop_type = prop_schema.get("type").and_then(Value::as_str);
    if prop_type.is_some_and(|t| t != "string") {
        return None; // non-string type → cancel
    }
    Some((prop_name.clone(), prop_type.map(|s| s.to_string())))
}

/// A stream-aware banner gate over one turn's `agent_message_chunk` deltas (core#410, F-068):
/// pi's RPC-mode startup banner arrives as the FIRST agent text of a seat's first turn — 4.8 KB
/// listing every skill and extension the seat loaded — and used to stream straight into the
/// chat transcript as the answer's opening (`ChatDelta`), where the UI turned its file names into
/// artifact chips. `strip_pi_banner` cleans the ASSEMBLED output; this gate cleans the STREAM.
///
/// Loss-averse by construction: text is HELD only while it is still a plausible banner head
/// (`pi v<digit>…` then `---`, up to the closing `---`), released the moment it diverges, and
/// flushed at turn end either way; a complete banner is removed through `strip_pi_banner` (the
/// one pattern), so the gate can never remove what the assembly seam would keep. Bounded: a
/// banner-shaped head that never closes is released after [`BannerGate::HOLD_CAP`] bytes.
#[derive(Debug, Default)]
pub(crate) struct BannerGate {
    held: String,
    passthrough: bool,
}

impl BannerGate {
    /// The most banner-shaped text held back before it is released as content.
    const HOLD_CAP: usize = 64 * 1024;

    /// Feed one delta; the text to deliver now, if any.
    pub(crate) fn push(&mut self, delta: &str) -> Option<String> {
        if self.passthrough {
            return Some(delta.to_string());
        }
        // The bound is judged BEFORE retaining (Copilot, #426): one oversized banner-shaped chunk
        // must not sit in `held` past the cap even for the length of this call. Past it, the head
        // is content — release what is held plus this delta (a complete banner at the head is
        // still stripped on the way out).
        if self.held.len() + delta.len() > Self::HOLD_CAP {
            self.passthrough = true;
            let mut out = std::mem::take(&mut self.held);
            out.push_str(delta);
            return Some(strip_pi_banner(&out).to_string());
        }
        self.held.push_str(delta);
        // Remove every COMPLETE banner at the head (observed twice in one capture, core#268).
        let stripped = strip_pi_banner(&self.held);
        if stripped.len() != self.held.len() {
            self.held = stripped.to_string();
        }
        let head = self.held.trim_start_matches(['\n', '\r']);
        if head.is_empty() {
            // Nothing but stripped banners and line breaks so far — keep waiting for content.
            return None;
        }
        if banner_head_could_follow(head) && self.held.len() <= Self::HOLD_CAP {
            return None;
        }
        self.passthrough = true;
        Some(std::mem::take(&mut self.held))
    }

    /// The turn ended: release whatever is held (a complete banner is stripped; an incomplete
    /// banner-shaped head is content and is delivered — loss-averse).
    pub(crate) fn finish(&mut self) -> Option<String> {
        self.passthrough = true;
        let rest = strip_pi_banner(&self.held).to_string();
        self.held.clear();
        (!rest.is_empty()).then_some(rest)
    }
}

/// Could `head` (line-break-trimmed) still grow into a pi banner — `pi v<digit>…\n---\n…`? True
/// for a strict prefix of that shape, false the moment a byte diverges from it.
fn banner_head_could_follow(head: &str) -> bool {
    const LEAD: &str = "pi v";
    if head.len() < LEAD.len() {
        return LEAD.starts_with(head);
    }
    let Some(after_lead) = head.strip_prefix(LEAD) else {
        return false;
    };
    let Some(first) = after_lead.chars().next() else {
        return true;
    };
    if !first.is_ascii_digit() {
        return false;
    }
    // Past the version line, the next line must be a bare `---` (or a prefix of one so far).
    match after_lead.find('\n') {
        None => true,
        Some(i) => {
            let second = &after_lead[i + 1..];
            match second.find('\n') {
                None => "---".starts_with(second.trim_end_matches('\r')),
                Some(_) => second.lines().next().unwrap_or("").trim_end() == "---",
            }
        }
    }
}

/// Owned convenience over [`strip_pi_banner`]: returns the ORIGINAL `String` untouched when no
/// banner was present (equal-length subslice ⇒ no copy), and clones only the stripped remainder
/// otherwise. Exists so call sites stay ONE line — the exit-0 arm in `execute_wrapped` is under
/// source-scan audit windows (FINDING-101) that a multi-line insertion overflows.
pub(crate) fn strip_pi_banner_owned(text: String) -> String {
    let stripped = strip_pi_banner(&text);
    // Same POINTER and length ⇒ provably the untouched original — a same-length check alone
    // would wrongly skip the clone for any future same-length subslice (Copilot, #271).
    if std::ptr::eq(stripped.as_ptr(), text.as_ptr()) && stripped.len() == text.len() {
        text
    } else {
        stripped.to_string()
    }
}

/// Strip pi's RPC-mode startup banner from captured text (core#268).
///
/// The pi bridge spawns `pi --mode rpc`, whose FIRST payload is the startup banner — version
/// line, `---`, a `## Skills`/`## Extensions` listing, `---`, and an optional "New version
/// available…" line — and pi's `quietStartup` setting does not silence the rpc path (verified
/// empirically: setting on, banner still streamed). The banner then pollutes every captured
/// unit output, compounds through prior-output context injection, and opens every chat reply.
/// Stripping at CAPTURE (here) cleans all three consumers at one seam.
///
/// Pattern-gated and loss-averse: only fires when the text head is a `pi v<digit…>` line
/// followed by a `---` line, and only removes through the MATCHING closing `---` (plus the
/// optional version-notice line). Anything else — including legitimate `---` inside real
/// content — is left byte-identical. Loops because the banner has been observed twice in one
/// capture.
pub(crate) fn strip_pi_banner(text: &str) -> &str {
    let mut rest = text;
    loop {
        let t = rest.trim_start_matches(['\n', '\r']);
        let mut lines = t.split_inclusive('\n');
        // Head must be `pi v<digit>…` and the next line a bare `---`, else not a banner.
        let Some(head) = lines.next() else {
            return rest;
        };
        if !(head.starts_with("pi v") && head[4..].starts_with(|c: char| c.is_ascii_digit())) {
            return rest;
        }
        let Some(open) = lines.next() else {
            return rest;
        };
        if open.trim_end() != "---" {
            return rest;
        }
        // Scan to the CLOSING bare `---`; refuse to strip when it never comes (loss-averse).
        let mut consumed = head.len() + open.len();
        let mut closed = false;
        for line in lines {
            consumed += line.len();
            if line.trim_end() == "---" {
                closed = true;
                break;
            }
        }
        if !closed {
            return rest;
        }
        let mut after = &t[consumed..];
        // Optional single-line update notice directly after the banner.
        let trimmed = after.trim_start_matches(['\n', '\r']);
        if trimmed.starts_with("New version available") {
            after = match trimmed.find('\n') {
                Some(i) => &trimmed[i + 1..],
                None => "",
            };
        }
        rest = after;
    }
}

#[allow(clippy::too_many_arguments)]
fn exec_turn_acp(
    proc: &mut AcpProcess,
    prompt: &str,
    prior_outputs: &[PriorUnitOutput],
    emit: &DeltaSink,
    timeout: Duration,
    elicitation_maps: Arc<Mutex<ElicitationMaps>>,
    run_id: &str,
    epoch: u64,
    tx: &std::sync::mpsc::Sender<Command>,
    gate: Option<&crate::acp_permission::AcpGate<'_>>,
) -> anyhow::Result<TurnResult> {
    let id = proc.next_id;
    proc.next_id += 1;

    // Clone the write_lock Arc so we can hold it around each proc.stdin write without
    // borrowing proc for the whole function. shared_run_terminal's try_lock() must see
    // this held to detect an in-flight write (FINDING-254 / core#254).
    let write_lock = Arc::clone(&proc.write_lock);

    // Elicitation is gated on a non-zero epoch AND on what this session actually
    // ADVERTISED at initialize (OQ-R-6, core#341). `elicitation_advertised` is the one
    // decision `start_acp_process` made from the adapter binary — gating on it here means
    // a seat can never advertise the capability and then auto-cancel `elicitation/create`
    // at turn time (the old gate keyed on the registry `cli_key`, which diverges from the
    // binary for every stock seat). Chat turns always pass epoch=0 and are never suspended.
    let elicitation_enabled = epoch > 0 && proc.elicitation_advertised;

    // Build the prompt block array: a contract header, the prior outputs, then the work prompt.
    let mut blocks: Vec<Value> = Vec::new();
    if !prior_outputs.is_empty() {
        // FINDING-024 (3): STATE the contract; do not let the phase name imply it. Labelled blobs
        // alone were read as background — an `adversarial-review` phase handed the build's output
        // still re-solved the original task, because nothing told it the blob was the subject. The
        // phase name is not an instruction, so this says plainly what the blocks are and what to do
        // with them. Only emitted when there IS prior context, so single-CLI runs with no declared
        // dependency keep the exact prompt they had before.
        blocks.push(json!({
            "type": "text",
            "text": "CONTEXT (prior phases of this run): the block(s) below are the verbatim output \
of earlier phases in this same workflow run. Blocks marked `depends_on` are the artifacts your \
phase explicitly declared it consumes — treat them as the SUBJECT of your task, not as background. \
Build on this work; do not re-solve the original problem from scratch, and do not choose a different \
target than the one the prior phase worked on. If your phase reviews, tests, or revises, it is that \
prior output you are reviewing, testing, or revising."
        }));
    }
    blocks.extend(prior_outputs.iter().map(|p| {
        json!({
            "type": "text",
            "text": format!("{}\n{}", p.label, p.output)
        })
    }));
    blocks.push(json!({"type": "text", "text": prompt}));

    {
        let _wl = write_lock.lock().unwrap_or_else(|p| p.into_inner());
        rpc_send(
            &mut proc.stdin,
            id,
            "session/prompt",
            json!({
                "sessionId": proc.session_id,
                "prompt": blocks
            }),
        )?;
    }

    let mut output = String::new();
    let mut usage: Option<Usage> = None;
    let mut files: Vec<String> = Vec::new();
    const MAX_OUT: usize = 8 * 1024 * 1024;

    let deadline = Instant::now() + timeout;

    // State variables for this turn.
    let (mut found, mut timed_out) = (false, false);
    // A JSON-RPC error frame answering THIS turn's id, kept structured (crew#267).
    let mut rpc_error: Option<RpcServerError> = None;
    // Set when the turn is suspended on elicitation and the suspend deadline expires without a
    // human response.
    let mut elicitation_timed_out = false;
    // A human or teardown cancellation is terminal for the unit. `decline` is not:
    // the adapter may continue the turn after being told it cannot obtain the value.
    let mut elicitation_cancelled = false;
    // Set when stdin closes mid-turn (write_failed during `rpc_respond` inside `'elicit`).
    let mut write_failed_terminal = false;
    // Set when `line_rx` disconnects inside the `'elicit` poll loop (adapter died mid-suspend).
    let mut dead_session = false;

    'exec: loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or_default();
        if remaining.is_zero() {
            timed_out = true;
            break 'exec;
        }
        match proc.line_rx.recv_timeout(remaining) {
            Ok(line) => {
                let v: Value = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    Err(_) => continue 'exec,
                };

                // ── elicitation/create arm ─────────────────────────────────────────────
                if agent_method(&v) == Some("elicitation/create") {
                    // `elicitation/create` is a REQUEST — the agent blocks on the answer. If the
                    // `id` member is ABSENT the frame is a notification and there is nobody to
                    // answer, so raising a human prompt for it would only strand the human; skip
                    // it. An explicit `"id": null` IS a request and is served normally
                    // (Copilot review, core#293).
                    let Some(request_id) = answerable_id(&v).cloned() else {
                        continue 'exec;
                    };
                    let schema = &v["params"]["requestedSchema"];
                    let message = v["params"]["message"].as_str().unwrap_or("");

                    // Guard 1: elicitation disabled for this epoch/adapter → immediate cancel.
                    if !elicitation_enabled {
                        // The turn CONTINUES after this cancel, so a lost write is not
                        // best-effort: it leaves the agent blocked on an elicitation nobody will
                        // ever answer. Surface it (Copilot review, core#293).
                        respond_or_note(
                            &mut proc.stdin,
                            &write_lock,
                            &request_id,
                            json!({"action":"cancel"}),
                            "an elicitation this adapter is not allowed to raise",
                            &mut output,
                            MAX_OUT,
                        );
                        continue 'exec;
                    }

                    // Guard 2: schema must have exactly one string-typed property.
                    let (prop_name, prop_type) = match validate_elicitation_schema(schema) {
                        Some(v) => v,
                        None => {
                            respond_or_note(
                                &mut proc.stdin,
                                &write_lock,
                                &request_id,
                                json!({"action":"cancel"}),
                                "an elicitation with an unsupported schema",
                                &mut output,
                                MAX_OUT,
                            );
                            continue 'exec;
                        }
                    };

                    // Extract enum options from the property schema if present.
                    let options = schema["properties"][&prop_name]["enum"]
                        .as_array()
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|v| v.as_str().map(str::to_string))
                                .collect::<Vec<_>>()
                        });

                    // Mint a unique elicitation id and register in the maps.
                    let elicitation_id = uuid::Uuid::new_v4().to_string();
                    let registration = {
                        let mut m = elicitation_maps.lock().unwrap_or_else(|p| p.into_inner());
                        m.register(run_id, epoch, &elicitation_id, message, options, &prop_name)
                    };
                    let (deliver_rx, capped_msg, filtered_opts, prop_key) = match registration {
                        Some(r) => r,
                        None => {
                            // Epoch was cancelled (suppressed creation) — cancel and continue.
                            // The turn continues, so a lost write blocks the agent: surface it.
                            respond_or_note(
                                &mut proc.stdin,
                                &write_lock,
                                &request_id,
                                json!({"action":"cancel"}),
                                "an elicitation raised on a cancelled epoch",
                                &mut output,
                                MAX_OUT,
                            );
                            continue 'exec;
                        }
                    };

                    // Announce the elicitation so the UI can show the question.
                    let _ = tx.send(Command::EmitEvent(
                        crate::event::CoreEvent::ElicitationCreated {
                            session: run_id.to_string(),
                            epoch,
                            elicitation_id: elicitation_id.clone(),
                            message: capped_msg,
                            options: filtered_opts,
                            prop_type,
                        },
                    ));

                    // ── 'elicit: dual-poll loop ────────────────────────────────────────
                    // Keep draining stdout (prevents buffer full / deadlock) while also
                    // checking the resolution channel every ELICITATION_POLL_MS.
                    let mut elicit_action = String::new();
                    // Assigned on every `break 'elicit` path before the post-loop read; declared
                    // without an initializer so the dead `String::new()` doesn't trip
                    // `-D unused-assignments` (unlike `elicit_action`, whose initial empty value
                    // is read on the `session_prompt` break paths).
                    let mut elicit_reason: String;

                    'elicit: loop {
                        let remaining = deadline
                            .checked_duration_since(Instant::now())
                            .unwrap_or_default();
                        if remaining.is_zero() {
                            // Outer turn deadline expired while suspended — cancel the elicitation.
                            elicitation_timed_out = true;
                            elicit_action = "cancel".to_string();
                            elicit_reason = "timeout".to_string();
                            // `let _ =` is CORRECT here (Copilot review, core#293): the turn
                            // deadline has already expired and this path unwinds into
                            // `timed_out` regardless. The cancel is a courtesy to an adapter we
                            // are about to abandon — a failed write changes no outcome and has
                            // no reader, since the turn is already reported as a timeout.
                            let _wl = write_lock.lock().unwrap_or_else(|p| p.into_inner());
                            let _ = rpc_respond(
                                &mut proc.stdin,
                                &request_id,
                                json!({"action":"cancel"}),
                            );
                            break 'elicit;
                        }

                        // Check shutdown flag (actor is draining).
                        {
                            let m = elicitation_maps.lock().unwrap_or_else(|p| p.into_inner());
                            if m.is_shutdown() {
                                elicitation_timed_out = true;
                                elicit_action = "cancel".to_string();
                                elicit_reason = "teardown".to_string();
                                drop(m);
                                // Best-effort by design (Copilot review, core#293): the actor is
                                // draining and this session is being torn down, so the adapter
                                // is going away whether or not the cancel lands. Nothing would
                                // read a surfaced error — the turn ends as "teardown".
                                let _wl = write_lock.lock().unwrap_or_else(|p| p.into_inner());
                                let _ = rpc_respond(
                                    &mut proc.stdin,
                                    &request_id,
                                    json!({"action":"cancel"}),
                                );
                                break 'elicit;
                            }
                        }

                        // Try resolution channel (non-blocking).
                        match deliver_rx.try_recv() {
                            Ok(result) => {
                                // Remove from maps before responding.
                                {
                                    let mut m =
                                        elicitation_maps.lock().unwrap_or_else(|p| p.into_inner());
                                    m.remove(run_id, &elicitation_id);
                                }
                                let epoch_cancelled = {
                                    let m =
                                        elicitation_maps.lock().unwrap_or_else(|p| p.into_inner());
                                    m.is_epoch_cancelled(run_id, epoch)
                                };
                                elicit_action = if epoch_cancelled {
                                    "cancel".to_string()
                                } else {
                                    result.action.clone()
                                };
                                let response_payload = match elicit_action.as_str() {
                                    "accept" => match result.response {
                                        Some(resp_val) => {
                                            json!({"action":"accept","content":{&prop_key: resp_val}})
                                        }
                                        None => {
                                            elicit_action = "cancel".to_string();
                                            json!({"action":"cancel"})
                                        }
                                    },
                                    "decline" => json!({"action":"decline"}),
                                    _ => json!({"action":"cancel"}),
                                };
                                elicitation_cancelled = elicit_action == "cancel";
                                elicit_reason = if epoch_cancelled {
                                    "teardown".to_string()
                                } else {
                                    "human".to_string()
                                };
                                if {
                                    let _wl = write_lock.lock().unwrap_or_else(|p| p.into_inner());
                                    rpc_respond(&mut proc.stdin, &request_id, response_payload)
                                }
                                .is_err()
                                {
                                    write_failed_terminal = true;
                                    // Phase 3 post-write tombstone gate (test 36):
                                    // If the epoch was deliberately cancelled (teardown) before
                                    // or during the write, the reason is "teardown", not
                                    // "adapter_write_failure". The latter is reserved for
                                    // unexpected transport failures on non-cancelled epochs.
                                    let was_cancelled = {
                                        let m = elicitation_maps
                                            .lock()
                                            .unwrap_or_else(|p| p.into_inner());
                                        m.is_epoch_cancelled(run_id, epoch)
                                    };
                                    elicit_reason = if was_cancelled {
                                        "teardown".to_string()
                                    } else {
                                        "adapter_write_failure".to_string()
                                    };
                                }
                                break 'elicit;
                            }
                            Err(std::sync::mpsc::TryRecvError::Empty) => {}
                            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                                // Channel dropped (EpochCleanup fired) → cancel the adapter.
                                elicitation_timed_out = true;
                                elicit_action = "cancel".to_string();
                                elicit_reason = "teardown".to_string();
                                // Best-effort by design (Copilot review, core#293): the epoch has
                                // already been cleaned up and this path unwinds the turn as
                                // "teardown"; a failed cancel changes nothing downstream.
                                let _wl = write_lock.lock().unwrap_or_else(|p| p.into_inner());
                                let _ = rpc_respond(
                                    &mut proc.stdin,
                                    &request_id,
                                    json!({"action":"cancel"}),
                                );
                                break 'elicit;
                            }
                        }

                        // Poll stdout with a short timeout to drain the pipe.
                        match proc
                            .line_rx
                            .recv_timeout(Duration::from_millis(ELICITATION_POLL_MS))
                        {
                            Ok(inner_line) => {
                                let v2: Value = match serde_json::from_str(&inner_line) {
                                    Ok(v) => v,
                                    Err(_) => continue 'elicit,
                                };

                                // Agent-originated frames are dispatched on `method` FIRST, ahead
                                // of the id comparison below (core#293) — and this sub-loop must
                                // serve the SAME set of methods the main loop does, because a
                                // suspended turn is exactly when the agent keeps working.
                                if let Some(method) = agent_method(&v2) {
                                    match method {
                                        // Second elicitation/create during suspension → cancel.
                                        // Only a REQUEST can be cancelled: an id-less
                                        // notification has no reply address, while an explicit
                                        // `"id": null` is a request and IS answered.
                                        "elicitation/create" => {
                                            if let Some(nested_id) = answerable_id(&v2).cloned() {
                                                // The turn continues after this cancel, so a lost
                                                // write leaves the agent blocked — surface it.
                                                respond_or_note(
                                                    &mut proc.stdin,
                                                    &write_lock,
                                                    &nested_id,
                                                    json!({"action":"cancel"}),
                                                    "a nested elicitation raised during a \
                                                     suspended turn",
                                                    &mut output,
                                                    MAX_OUT,
                                                );
                                            }
                                        }
                                        "session/update" => {
                                            handle_update(
                                                &v2,
                                                emit,
                                                &mut output,
                                                &mut usage,
                                                &mut files,
                                                MAX_OUT,
                                            );
                                        }
                                        // core#293: this arm did not exist. A permission request
                                        // arriving while the turn was suspended on an elicitation
                                        // was silently DROPPED, blocking the agent for the rest of
                                        // the turn. Answered here with the same policy the main
                                        // loop applies, via the same handler.
                                        "session/request_permission" => {
                                            answer_permission_request(
                                                &mut proc.stdin,
                                                &write_lock,
                                                gate,
                                                proc.chat_boundary.as_ref(),
                                                &v2,
                                                &mut output,
                                                MAX_OUT,
                                            );
                                        }
                                        // Unknown request → explicit refusal; unknown
                                        // NOTIFICATION (the `id` member is absent) → ignored.
                                        // An explicit `"id": null` is a request, so it is
                                        // refused rather than dropped (Copilot review).
                                        other => {
                                            if let Some(req_id) = answerable_id(&v2).cloned() {
                                                refuse_or_note(
                                                    &mut proc.stdin,
                                                    &write_lock,
                                                    &req_id,
                                                    &format!(
                                                        "wicked-core does not implement `{other}`"
                                                    ),
                                                    &format!("the unhandled request `{other}`"),
                                                    &mut output,
                                                    MAX_OUT,
                                                );
                                            }
                                        }
                                    }
                                    continue 'elicit;
                                }

                                // The prompt result arrived during the elicitation (the adapter decided
                                // to finish without waiting for the elicitation response).
                                if is_response_to(&v2, id) {
                                    // Remove from maps if still registered (edge: result raced resolution).
                                    {
                                        let mut m = elicitation_maps
                                            .lock()
                                            .unwrap_or_else(|p| p.into_inner());
                                        m.remove(run_id, &elicitation_id);
                                    }
                                    elicit_reason = "session_prompt".to_string();
                                    if v2.get("error").is_some() {
                                        break 'elicit;
                                    }
                                    let stop =
                                        v2["result"]["stopReason"].as_str().unwrap_or("end_turn");
                                    if stop == "cancelled" {
                                        timed_out = true;
                                    } else {
                                        found = true;
                                    }
                                    if let Some(result_usage) =
                                        parse_result_usage(&v2["result"]["usage"])
                                    {
                                        let cost = usage.as_ref().and_then(|u| u.cost_usd);
                                        usage = Some(Usage {
                                            cost_usd: cost.or(result_usage.cost_usd),
                                            ..result_usage
                                        });
                                    }
                                    break 'elicit;
                                }
                                // A response to some OTHER outbound id — ignore it.
                            }
                            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue 'elicit,
                            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                                dead_session = true;
                                elicit_action = "cancel".to_string();
                                elicit_reason = "teardown".to_string();
                                break 'elicit;
                            }
                        }
                    } // 'elicit

                    // Emit the resolution event now that 'elicit has exited with a reason.
                    if !elicit_reason.is_empty() {
                        let _ = tx.send(Command::EmitEvent(
                            crate::event::CoreEvent::ElicitationResolved {
                                session: run_id.to_string(),
                                elicitation_id: elicitation_id.clone(),
                                action: elicit_action.clone(),
                                reason: elicit_reason.clone(),
                            },
                        ));
                    }

                    // After 'elicit: decide whether the outer loop should keep going.
                    if found
                        || timed_out
                        || dead_session
                        || elicitation_timed_out
                        || elicitation_cancelled
                        || write_failed_terminal
                    {
                        break 'exec;
                    }
                    // Otherwise: normal elicitation resolution (adapter continues) → keep looping.
                    continue 'exec;
                }
                // ── end elicitation/create arm ─────────────────────────────────────────

                // ── agent-originated frames: dispatch on `method` BEFORE the id check ───
                //
                // core#293: everything below carries a `method`, which makes it a REQUEST or a
                // notification FROM the agent — never the response to our `session/prompt`. It is
                // handled here, ahead of the id comparison, so a colliding id can no longer make
                // the id check swallow it. (`is_response_to` enforces the same rule from the other
                // side; both are kept so neither alone is load-bearing.)
                if let Some(method) = agent_method(&v) {
                    match method {
                        "session/update" => {
                            handle_update(&v, emit, &mut output, &mut usage, &mut files, MAX_OUT);
                        }
                        // The agent asking permission for a tool call. This is a REQUEST, not a
                        // notification: it carries an `id` and blocks the agent until answered.
                        // Before this arm existed the loop handled only notifications, so an
                        // unanswered request would have hung the turn — which is why the
                        // capability above had to stay off.
                        "session/request_permission" => {
                            answer_permission_request(
                                &mut proc.stdin,
                                &write_lock,
                                gate,
                                proc.chat_boundary.as_ref(),
                                &v,
                                &mut output,
                                MAX_OUT,
                            );
                        }
                        // Catch-all (core#293): a request this client does not implement gets an
                        // explicit JSON-RPC error. Dropping it would block the agent until the
                        // turn timeout with nothing naming why — the precise failure mode this
                        // issue is about. A future ACP method now degrades to a refusal.
                        other => {
                            if let Some(req_id) = answerable_id(&v).cloned() {
                                refuse_or_note(
                                    &mut proc.stdin,
                                    &write_lock,
                                    &req_id,
                                    &format!("wicked-core does not implement `{other}`"),
                                    &format!("the unhandled request `{other}`"),
                                    &mut output,
                                    MAX_OUT,
                                );
                            }
                            // Unknown NOTIFICATIONS — the `id` member ABSENT, per JSON-RPC 2.0
                            // §4.1 — block nobody, so they are ignored. A request carrying an
                            // explicit `"id": null` is NOT one of those and is refused above.
                        }
                    }
                    continue 'exec;
                }

                // ── no `method` ⇒ a RESPONSE. Only the one answering THIS prompt matters. ──
                if is_response_to(&v, id) {
                    if let Some(err) = v.get("error") {
                        // JSON-RPC error response: surface it STRUCTURED so the caller can
                        // classify by CODE (crew#267: the bridge's -32000 auth refusal must
                        // become a named fallback, not a generic failed-turn/"session exited").
                        rpc_error = Some(RpcServerError {
                            code: err.get("code").and_then(Value::as_i64),
                            raw: err.to_string(),
                        });
                        break 'exec;
                    }
                    let stop = v["result"]["stopReason"].as_str().unwrap_or("end_turn");
                    if stop == "cancelled" {
                        timed_out = true;
                    } else {
                        found = true;
                    }
                    // The ecosystem adapters (official claude/codex, pi-acp, native
                    // opencode) report authoritative usage ON THE PROMPT RESULT, not as
                    // inputTokens/outputTokens usage_update notifications — without this
                    // no CliUsage event fires and the studio's Burn panel stays empty.
                    // Prefer it over notification-derived usage; keep any notification
                    // cost (the result shape carries no cost field).
                    if let Some(result_usage) = parse_result_usage(&v["result"]["usage"]) {
                        let cost = usage.as_ref().and_then(|u| u.cost_usd);
                        usage = Some(Usage {
                            cost_usd: cost.or(result_usage.cost_usd),
                            ..result_usage
                        });
                    }
                    break 'exec;
                }
                // A response to some OTHER outbound id — nothing is waiting on it here.
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue 'exec,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break 'exec,
        }
    }

    // No `stopReason` and no timeout means the bridge stopped answering — it died mid-turn. Its
    // stderr is the only account of why, and `StepOutput.output` is where an operator looks, so
    // say it there rather than reporting a Failed unit with an empty reason.
    if !found
        && !timed_out
        && !elicitation_timed_out
        && !elicitation_cancelled
        && !dead_session
        && !write_failed_terminal
    {
        let note = format!(
            "\n[wicked-core] ACP turn ended with no stopReason (the bridge stopped answering){}",
            stderr_context(&proc.stderr_tail)
        );
        append_within_cap(&mut output, &note, MAX_OUT);
    }

    // A structured error frame outranks the flag-derived status: the caller's Err arm
    // classifies by code (auth_required vs session death) and runs the fallback either way.
    if let Some(err) = rpc_error {
        return Err(anyhow::Error::new(err));
    }

    Ok(TurnResult {
        // Banner-strip at the ONE assembly seam (core#268): cleans unit outputs, the prior-
        // output injections derived from them, and chat replies alike. Deltas already streamed
        // raw — cosmetic only; every durable consumer reads this assembled form.
        output: strip_pi_banner(&output).trim_end().to_string(),
        status: if found {
            StepStatus::Ok
        } else if elicitation_timed_out
            || elicitation_cancelled
            || dead_session
            || write_failed_terminal
        {
            // Elicitation-terminal paths: not retriable, bypass FailureTriageReady (spec I-7).
            StepStatus::ElicitationFailed
        } else if timed_out {
            // The engine's own turn ceiling (`WICKED_UNIT_TIMEOUT_SECS`) — NOT an operator
            // cancel, which tears the session down and never reaches this classification.
            StepStatus::TimedOut
        } else {
            StepStatus::Failed
        },
        usage,
        files,
        tools: Vec::new(),
    })
}

/// Answer one `session/request_permission` REQUEST from the agent.
///
/// Factored out for core#293: the `'elicit` sub-loop had no permission arm at all, so a
/// permission request arriving while a turn was suspended on an elicitation was silently dropped
/// and the agent blocked until the turn timed out. One handler now serves both dispatchers so the
/// two cannot drift apart again.
///
/// `gate` present ⇒ governed: the SAME policy and the SAME audit records as the wrapped path's
/// PreToolUse hook. Otherwise a CHAT boundary, when the session carries one (core#410): the scratch
/// root writable, the scoped roots read-only, nothing beyond — judged by the shared pure check.
/// Neither ⇒ permitted, as this path has always behaved — but said out loud rather than left to a
/// capability we quietly withheld.
fn answer_permission_request<W: Write>(
    stdin: &mut W,
    write_lock: &Mutex<()>,
    gate: Option<&crate::acp_permission::AcpGate<'_>>,
    chat_boundary: Option<&crate::gate_hook::BoundaryCtx>,
    frame: &Value,
    output: &mut String,
    max_out: usize,
) {
    // `request_id` — not a raw `get("id")` — so the "notification ⇒ no answer" rule is decided in
    // ONE place: the `id` member being ABSENT means nothing to answer, while an explicit
    // `"id": null` is a real request and is answered with a null-id response (Copilot review).
    let Some(req_id) = answerable_id(frame).cloned() else {
        return; // a permission NOTIFICATION is not a thing; nothing to answer.
    };
    let params = frame.get("params").cloned().unwrap_or(Value::Null);
    let result = match (gate, chat_boundary) {
        (Some(g), _) => crate::acp_permission::permission_result(g, &params).0,
        (None, Some(b)) => crate::acp_permission::chat_boundary_result(b, &params).0,
        (None, None) => crate::acp_permission::allow_result(&params),
    };
    // NOT `let _ =`. A failed write leaves the agent blocked until the turn times out, and the
    // reason is the only thing that explains the stall — dropping it turns a broken pipe into
    // "the model was slow" (review).
    respond_or_note(
        stdin,
        write_lock,
        &req_id,
        result,
        "a permission request",
        output,
        max_out,
    );
}

/// Process one `session/update` notification — extract text chunks and usage.
fn handle_update(
    v: &Value,
    emit: &DeltaSink,
    output: &mut String,
    usage: &mut Option<Usage>,
    files: &mut Vec<String>,
    max_out: usize,
) {
    let update = &v["params"]["update"];
    let kind = update
        .get("sessionUpdate")
        .and_then(Value::as_str)
        .unwrap_or("");
    match kind {
        "agent_message_chunk" => {
            if let Some(text) = update["content"]["text"].as_str() {
                emit(text);
                let used = output.len();
                if used < max_out {
                    // Clamp to remaining capacity at a valid UTF-8 boundary so
                    // a single large chunk never pushes output past max_out.
                    let remaining = max_out - used;
                    let safe = text
                        .char_indices()
                        .take_while(|(i, c)| *i + c.len_utf8() <= remaining)
                        .last()
                        .map(|(i, c)| i + c.len_utf8())
                        .unwrap_or(0);
                    output.push_str(&text[..safe]);
                }
            }
        }
        "usage_update" => {
            let input = update["inputTokens"]
                .as_u64()
                .or_else(|| update["input_tokens"].as_u64())
                .unwrap_or(0);
            let out = update["outputTokens"]
                .as_u64()
                .or_else(|| update["output_tokens"].as_u64())
                .unwrap_or(0);
            // The official claude adapter's usage_update is `{used, size, cost:{amount}}`
            // — no per-direction tokens (those arrive on the prompt result), but cost is
            // ONLY reported here, so lift it even when the token fields are absent.
            let cost = update["cost"]["amount"].as_f64();
            if input > 0 || out > 0 {
                // A usage_update notification carries only totals — no cache split (that arrives on
                // the prompt result, which `parse_result_usage` then supersedes this with). Record 0
                // for the split rather than a guess (FINDING-012).
                *usage = Some(Usage {
                    input_tokens: input,
                    output_tokens: out,
                    cache_read_tokens: 0,
                    cache_creation_tokens: 0,
                    cost_usd: cost.or_else(|| usage.as_ref().and_then(|u| u.cost_usd)),
                });
            } else if let Some(c) = cost {
                let (i, o, cr, cc) = usage
                    .as_ref()
                    .map(|u| {
                        (
                            u.input_tokens,
                            u.output_tokens,
                            u.cache_read_tokens,
                            u.cache_creation_tokens,
                        )
                    })
                    .unwrap_or((0, 0, 0, 0));
                *usage = Some(Usage {
                    input_tokens: i,
                    output_tokens: o,
                    cache_read_tokens: cr,
                    cache_creation_tokens: cc,
                    cost_usd: Some(c),
                });
            }
        }
        "tool_call_update" => {
            // Collect file paths reported by the CLI (e.g. read/edit locations).
            if let Some(locs) = update["locations"].as_array() {
                for loc in locs {
                    if let Some(path) = loc["path"].as_str() {
                        files.push(path.to_string());
                    }
                }
            }
        }
        _ => {}
    }
}

/// Parse the authoritative usage object from a `session/prompt` RESULT. All ecosystem
/// adapters report here (camelCase): official claude/codex, pi-acp, native opencode.
/// Input counts cached reads/writes alongside fresh input, mirroring the historical
/// bridge semantics (total context presented to the model). `None` when absent/empty.
fn parse_result_usage(u: &Value) -> Option<Usage> {
    if !u.is_object() {
        return None;
    }
    let field = |k: &str| u[k].as_u64().unwrap_or(0);
    // Saturating: token counters come from an external process — a malformed or
    // hostile frame must clamp, never wrap into a tiny bogus total.
    let cache_read_tokens = field("cachedReadTokens");
    let cache_creation_tokens = field("cachedWriteTokens");
    let input = field("inputTokens")
        .saturating_add(cache_read_tokens)
        .saturating_add(cache_creation_tokens);
    let output = field("outputTokens");
    if input == 0 && output == 0 {
        return None;
    }
    Some(Usage {
        input_tokens: input,
        output_tokens: output,
        cache_read_tokens,
        cache_creation_tokens,
        cost_usd: None,
    })
}

// ── Fallback helpers ──────────────────────────────────────────────────────────

/// FINDING #5. A single transient CLI/connection blip must not kill a whole governed run. A
/// governed unit's single-shot `claude -p` that exits NONZERO has almost always hit an
/// API/connection error — a task judgment surfaces as exit-0 plus a DOWNSTREAM gate deny, never as a
/// nonzero CLI exit (a governance tool-deny is handled inside claude, not as a process failure) — so
/// the nonzero-exit / could-not-run failure is retried a bounded number of times before the unit
/// fails closed. Governed phases are idempotent (estate annotations upsert; a re-run re-derives), so
/// a retry is safe.
const MAX_TRANSIENT_RETRIES: u32 = 2;

/// Whether a FAILED single-shot output looks like an infrastructural/transient CLI failure (worth a
/// retry) rather than a deterministic one. Pure, so the policy is falsifiable without a `StepInput`
/// fixture. Matches the wrapped runner's own nonzero-exit / could-not-run messages
/// (`execute_wrapped.rs`) plus the network signatures a `claude -p` prints on an API/connection drop.
///
/// A missing declared deliverable (FINDING-101) used to need an explicit substring exclusion here,
/// because the wrapped runner reported it as a synthetic `StepStatus::Failed` carrying an English
/// sentence, and retrying a deterministic incompleteness burns budget to fail identically. core#297
/// removed the need: the floor moved to the runner-independent fold in `actor::apply_step_result`,
/// which rejects the unit DIRECTLY and never produces a failed `StepOutput` for any classifier to
/// read. Structural, not string-sniffed — and it closes the small hole where a worker printing that
/// sentence into its own transcript could reclassify its own failure.
pub(crate) fn is_transient_cli_failure(output: &str) -> bool {
    let o = output.to_ascii_lowercase();
    o.contains("exited") // the wrapped runner's "(cli `x` exited N) …" nonzero-exit message
        || o.contains("could not run")
        || o.contains("connection")
        || o.contains("closed")
        || o.contains("reset")
        || o.contains("network")
        || o.contains("stream error")
        || o.contains("overloaded")
        || o.contains("rate limit")
        || o.contains("502")
        || o.contains("503")
}

/// Whether a FAILED output is WORKER-ORIGINATED — the CLI process itself failed (nonzero exit,
/// spawn failure, connection drop, or the harness killed it at a deadline) rather than the WORK
/// being judged bad (a judged rejection surfaces as exit-0 plus a downstream gate deny, never as
/// one of these shapes). The FINDING-101 missing-deliverable case no longer needs the substring
/// exclusion it once carried here — see [`is_transient_cli_failure`] for why core#297 made it
/// structural.
///
/// Superset of [`is_transient_cli_failure`], adding the TIMEOUT signatures. A timeout is
/// deliberately NOT in the transient set: a same-seat in-runner retry would silently burn another
/// full unit budget on a seat that just proved it cannot finish. But it IS a seat-health signal
/// the actor's failover ladder must act on by moving to the NEXT seat — core#282: seat `agy`
/// timed out twice on the same unit because the timeout shape never entered the ladder, so the
/// engine re-dispatched the same seat until the run died.
pub(crate) fn is_worker_originated_failure(output: &str) -> bool {
    if is_transient_cli_failure(output) {
        return true;
    }
    let o = output.to_ascii_lowercase();
    o.contains("exceeded the timeout") // execute_wrapped's bounded-run kill message
        || o.contains("timed out")
        || o.contains("acp timeout") // acp_runner's rpc/turn deadline
}

/// Whether to retry the single-shot worker after an outcome. Pure + exhaustively unit-tested — the
/// loop in [`fallback_with_warning`] is a trivial application of this policy.
///
/// `governed` GATES the retry: only a GOVERNED unit is retried. The idempotency argument (a re-run
/// re-derives; estate annotations upsert) and the "nonzero exit ⇒ infrastructural" argument are
/// properties of the governed campaign phases. The engine's OWN ungoverned `claude` calls (the
/// internal agent-judge / validator-authoring invocations) are NOT retried — they are not campaign
/// phases and their re-run safety is not established (Copilot review on #216). `retries_done` is how
/// many retries have already run (0 on the first outcome).
fn should_retry_worker(
    governed: bool,
    status: StepStatus,
    output: &str,
    retries_done: u32,
) -> bool {
    governed
        && status == StepStatus::Failed
        && retries_done < MAX_TRANSIENT_RETRIES
        && is_transient_cli_failure(output)
}

/// Run the single-shot fallback, prepending `warning` to the output so it appears in
/// both the streaming view and the persisted `StepOutput.output` (visible in studio). Retries a
/// TRANSIENT worker failure up to [`MAX_TRANSIENT_RETRIES`] times (FINDING #5) so a single API blip
/// in a long GOVERNED phase does not fail the whole run. The retry notice is folded into the
/// PERSISTED output (not just streamed) so an operator/Studio can see a unit succeeded after retries.
fn fallback_with_warning(
    warning: String,
    input: &StepInput,
    emit: &DeltaSink,
    fallback: &WrappedCliStepRunner,
) -> StepOutput {
    emit(&format!("{warning}\n"));
    // Only governed campaign units are retried — see `should_retry_worker`.
    let governed = input.governance.is_some();
    let mut result = fallback.run_unit_streaming(input, emit);
    // Retry decision reads the RAW runner output (the "(cli … exited N)" message), before the
    // warning is prepended below.
    let mut retries_done = 0u32;
    while should_retry_worker(governed, result.status, &result.output, retries_done) {
        retries_done += 1;
        emit(&format!(
            "[wicked-core] worker hit a transient CLI/connection failure; retrying \
             ({retries_done}/{MAX_TRANSIENT_RETRIES})\n"
        ));
        result = fallback.run_unit_streaming(input, emit);
    }
    // Persist the retry notice (not just `emit`, which rides the excluded delta stream): the final
    // outcome — success or a fail-closed after exhausting retries — must show it in the durable output.
    let retry_note = if retries_done > 0 {
        let outcome = if result.status == StepStatus::Failed {
            "still failed after"
        } else {
            "succeeded after"
        };
        format!("[wicked-core] worker {outcome} {retries_done} transient-failure retry(ies)\n")
    } else {
        String::new()
    };
    let warning = format!("{warning}\n{retry_note}");
    let warning = warning.trim_end().to_string();
    result.output = if result.output.is_empty() {
        warning
    } else {
        format!("{warning}\n{}", result.output)
    };
    result
}

// ── ACP input governance ──────────────────────────────────────────────────────

// ── ACP input governance (removed) ────────────────────────────────────────────
//
// `arm_acp_governance`, `AcpGovArmed` and `quote_exe_for_hook` lived here. They wrote a per-unit
// settings file (PreToolUse gate-hook + `permissions.deny`) and handed it to the bridge as
// `--settings <path>`. The bridge never read that flag, so the whole mechanism was ceremony: armed,
// announced, never applied (FINDING-060). They are deleted rather than kept behind a feature flag
// because a governance mechanism that compiles but cannot fire is worse than an absent one — it
// reads as coverage. Governed claude units now take the wrapped path, which arms the same hook via
// `execute_wrapped::arm_input_governance` on argv the CLI does read.
//
// Restoring governance to the ACP path means finding a channel the bridge honours — see
// FINDING-062. Whatever that channel turns out to be, the arming code should be written against a
// verified carrier, not resurrected from here.

// ── AcpStepRunner ─────────────────────────────────────────────────────────────

// `None` entries cache a failed startup so subsequent units for the same
// `(run_id, cli_key)` fall back immediately without re-attempting spawn.
type SessionMap = Arc<Mutex<HashMap<(String, String), Option<Arc<Mutex<AcpProcess>>>>>>;

/// What the session cache holds for a `(run_id, cli_key)` — after the crew#340 liveness
/// probe has had its say. A cached bridge that already EXITED (a `kill -9` between turns,
/// an OOM kill, a crash) is a husk: writing `session/prompt` into it can only produce a
/// broken pipe, which used to degrade the unit to single-shot AND leave the map poisoned
/// for every later unit of the run. The probe purges the husk so the caller spawns fresh.
enum SessionProbe {
    /// A cached session whose bridge has not been observed dead — reuse it.
    Live(Arc<Mutex<AcpProcess>>),
    /// A previous startup for this key failed; fall back single-shot without re-spawning.
    FailedStartup,
    /// Nothing cached (or a dead husk was just purged) — start a fresh session.
    Vacant,
}

/// A [`StepRunner`] that drives ACP multi-turn sessions for all registered CLIs.
///
/// Sessions are keyed by `(run_id, cli_key)` — each CLI in a multi-CLI run gets its own
/// persistent ACP process so units are never mis-routed to the wrong agent.
///
/// Falls back to [`WrappedCliStepRunner`] (single-shot) when:
/// - the unit is governed and runs claude — governance only holds on the wrapped path (FINDING-060)
/// - the CLI has no ACP config in the registry
/// - the ACP binary is not on PATH
/// - the handshake fails or the session dies mid-run
///
/// All fallbacks prepend a `[wicked-core] ACP …` warning to `StepOutput.output` so
/// the degradation is visible in both streaming output and persisted logs.
/// Stable `fallback_kind` slugs carried on [`CoreEvent::AcpFallback`] for UI dispatch.
pub(crate) mod fallback_kind {
    pub const BINARY_UNAVAILABLE: &str = "binary_unavailable";
    pub const SESSION_DIED: &str = "session_died";
    /// The bridge answered the turn with `-32000 Authentication required` (crew#267): the
    /// engine-minted `CLAUDE_CONFIG_DIR` (FINDING-061) severs the CLI's logged-in state, so a
    /// governed ACP claude session fails its FIRST prompt by construction. Named so the seat
    /// health surface and operators see AUTH, not a generic session death.
    pub const AUTH_REQUIRED: &str = "auth_required";
    pub const HTTP_UNIMPLEMENTED: &str = "http_unimplemented";
    /// A governed claude unit, routed to the wrapped path on purpose. Not a failure — nothing broke
    /// — but it IS a behaviour change the operator has to be able to see: the unit runs single-shot
    /// instead of multi-turn, and the reason is that the ACP bridge cannot carry input governance.
    /// Emitting nothing here would make "governed units are slower" an unexplained mystery, which is
    /// how the ungoverned ACP path went unnoticed in the first place.
    pub const GOVERNANCE_REQUIRES_WRAPPED: &str = "governance_requires_wrapped";
    // RETIRED: `handshake_failed`. Its only emitter was the governed-ACP branch removed with
    // FINDING-060. The shared session path reports every startup failure — spawn or handshake — as
    // `binary_unavailable`, so the slug is no longer produced; a consumer still switching on it is
    // waiting for an event that cannot arrive.
}

/// Operator messages queued per run for next-turn delivery: `(original target, message)`.
type InjectQueue = Arc<Mutex<HashMap<String, Vec<(crate::command::InjectTarget, String)>>>>;

pub struct AcpStepRunner {
    /// Back-channel to the actor's single emit point (relay via `Command::EmitEvent`).
    tx: std::sync::mpsc::Sender<Command>,
    /// Keyed by `(run_id, cli_key)` — one process per CLI per run.
    sessions: SessionMap,
    /// Operator messages queued for delivery on the run's next matching unit prompt
    /// (the ACP inject path — there is no PTY to write into mid-turn). Keyed by run_id;
    /// drained in [`AcpStepRunner::exec_turn`], pruned with the run's sessions.
    pending_injects: InjectQueue,
    /// Last activity per CHAT id — set on open, on every ensure, and on every turn.
    ///
    /// Idleness is a property of the chat, not of a seat: `chat_close` reaps a whole chat, so that
    /// is the granularity a reaper can act on. Kept beside the pool rather than inside it because
    /// the pool is keyed per seat and a chat with zero warm seats still needs a last-touch (it may
    /// be mid-`chat_open`, warming its first seat).
    chat_activity: Arc<Mutex<HashMap<String, Instant>>>,
    /// Each open chat's [`ChatScope`] (core#410 / crew#502) — the cwd, graph and read roots its
    /// seats run against, recorded at `chat_open` so a seat re-warmed after an eviction lands in
    /// the same scope — stamped with the OPEN GENERATION that recorded it, so an older open still
    /// finishing cannot drop a newer open's record (Copilot, #426). Removed with the chat.
    chat_scopes: Arc<Mutex<HashMap<String, RecordedScope>>>,
    /// Monotonic open counter behind [`RecordedScope::gen`].
    chat_open_seq: Arc<std::sync::atomic::AtomicU64>,
    fallback: WrappedCliStepRunner,
    timeout: Duration,
    /// The engine's OWN operational state home — the canonical parent of the database it was
    /// spawned on (codex round 8) — fenced on every launch, snapshot or not. `None` outside an
    /// engine spawn (`new`, the tests).
    operational_home: Option<std::path::PathBuf>,
    /// Shared elicitation coordination state (DES-002). One Arc per Core instance; also held
    /// by the actor for `Command::ResolveElicitation` dispatch.
    pub elicitation_maps: Arc<Mutex<ElicitationMaps>>,
    /// Write-lock session registry shared with the actor.
    /// Key: `(run_id, session_key, launch_seq)`. Value: `(write_lock, kill_handle)`.
    /// Created in `spawn_with_acp_sessions`; PTY and injected runners hold an empty registry.
    pub write_reg: WriteReg,
}

/// Why a chat's warm sessions were released — carried on `ChatClosed` so an operator can tell a
/// chat they ended from one the daemon reclaimed underneath them (FINDING-027).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatCloseReason {
    /// An explicit close from the operator or the UI.
    Requested,
    /// Reclaimed after `WICKED_CHAT_IDLE_SECS` with no turn.
    Idle,
    /// Evicted as the least-recently-used chat when the pool reached `WICKED_CHAT_POOL_MAX`.
    PoolCap,
}

impl ChatCloseReason {
    /// The wire token. Stable — consumers branch on it.
    pub fn as_str(self) -> &'static str {
        match self {
            ChatCloseReason::Requested => "requested",
            ChatCloseReason::Idle => "idle",
            ChatCloseReason::PoolCap => "pool_cap",
        }
    }
}

/// `chat_open`'s per-seat outcomes: `(cli_key, Ok(()) | Err(reason))`, in the order asked.
pub type ChatOpenOutcomes = Vec<(String, Result<(), String>)>;

/// One live chat, for the enumerate surface. A leak nobody can list is a leak nobody can reclaim
/// (FINDING-027 gap 4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatInfo {
    pub chat_id: String,
    /// The seats currently warm, sorted.
    pub seats: Vec<String>,
    /// Seconds since the last open/ensure/turn on this chat.
    pub idle_secs: u64,
    /// What the seats run against (core#410 / crew#502); `None` for a pool entry whose scope was
    /// never recorded (a chat mid-close).
    pub scope: Option<ChatScope>,
}

/// What a chat's seats run against (core#410 / crew#502, F-067): the scratch directory they run
/// IN, the code graph they are grounded ON, and the repository roots they may READ. Recorded at
/// `chat_open` and reused by every later ensure — a seat evicted mid-chat re-warms into the SAME
/// scope. Never the daemon's own working directory: a chat opened without one runs in a private
/// scratch directory of its own ([`ChatScope::scratch_for`]).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChatScope {
    /// The seats' working directory — the chat's scratch root. Created on the first ensure.
    pub cwd: std::path::PathBuf,
    /// The estate graph the seats' READ-ONLY estate MCP is bound to (DES-GROUNDING-001 — the same
    /// grounding governed workers get): the project's co-located graph, or a repo's own. `None`
    /// ⇒ no estate MCP is advertised.
    pub code_graph_db: Option<String>,
    /// The repository roots in scope, absolute. Advertised to a claude seat as the SDK's
    /// `additionalDirectories`; recorded for every seat (the enumerate surface reports them).
    pub read_roots: Vec<String>,
}

/// Create a chat's scratch root for its seats — ABSOLUTE, PRIVATE (0700 on unix, enforced on an
/// existing directory too) and never through a planted link (Copilot, #426): the default root sits
/// under the system temp directory, where another local user can pre-place a symlink under a
/// predictable name, and `create_dir_all` alone would follow it and hand the seats that target as
/// their cwd; a relative root would resolve against the daemon's cwd — the F-067 leak by another
/// spelling. One shared helper with the seat config roots (`ensure_private_dir`).
fn ensure_chat_scratch_root(cwd: &std::path::Path) -> Result<(), String> {
    wicked_apps_core::spawn::ensure_private_dir(cwd)
        .map_err(|e| format!("refusing scratch root {} ({e})", cwd.display()))
}

/// The real path of `p` even when `p` does not exist yet: its longest EXISTING ancestor is
/// canonicalized and the remaining segments re-appended. A lexical `resolve` would compare a path
/// under a symlinked temp dir (macOS `/var/folders/…` → `/private/var/folders/…`) unequal to one
/// that was resolved through the link.
fn canonical_ish(p: &std::path::Path) -> std::path::PathBuf {
    let mut cur = p.to_path_buf();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if let Ok(real) = std::fs::canonicalize(&cur) {
            let mut out = real;
            for seg in tail.iter().rev() {
                out.push(seg);
            }
            return out;
        }
        match (cur.file_name(), cur.parent()) {
            (Some(name), Some(parent)) => {
                tail.push(name.to_os_string());
                cur = parent.to_path_buf();
            }
            _ => return p.to_path_buf(),
        }
    }
}

/// Is `file` the SAME file (device + inode) as any top-level entry of `dir` — the operational store
/// or one of its sidecars reached through a hard link or a symlink elsewhere? Unix only: on other
/// platforms the canonical-path comparison covers symlinks and junctions, while a HARD link's
/// identity needs `MetadataExt::file_index` (unstable) or a crate this workspace does not carry —
/// a documented limitation (Copilot, #426): the chat opener is the daemon, and a hard link to the
/// operational store would have to be planted by the operator's own account on that host.
fn same_file_as_a_top_level_entry_of(file: &std::path::Path, dir: &std::path::Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let Ok(target) = std::fs::metadata(file) else {
            return false;
        };
        let Ok(entries) = std::fs::read_dir(dir) else {
            return false;
        };
        entries.flatten().any(|e| {
            std::fs::metadata(e.path())
                .map(|m| m.is_file() && m.dev() == target.dev() && m.ino() == target.ino())
                .unwrap_or(false)
        })
    }
    #[cfg(not(unix))]
    {
        let _ = (file, dir);
        false
    }
}

/// A recorded chat scope and the open generation that recorded it (see `AcpStepRunner::chat_scopes`).
#[derive(Debug, Clone, PartialEq, Eq)]
struct RecordedScope {
    gen: u64,
    scope: ChatScope,
}

/// May `cli_key` join a chat in `scope` (Copilot, #426)? An UNSCOPED chat (no roots, no graph)
/// promises nothing beyond its scratch root and admits every seat. A SCOPED chat promises that
/// its roots are read-only and that the seats see nothing else — a promise this engine can keep
/// only through a channel the seat actually passes through: the chat boundary on
/// `session/request_permission` (an adapter admitted to input governance asks for every tool
/// call — claude and opencode, as registered; copilot's `--acp` is NOT admitted) or the kernel
/// write floor (`os_sandbox` armed on the seat's record). An adapter that asks no permissions and
/// runs under no floor (pi-acp, codex-acp, copilot --acp, agy-acp as registered) would run
/// unbounded behind a read-only statement, so it is refused BY NAME for
/// scoped chats — the open still succeeds for the seats that can be held, and the per-seat
/// outcome says why this one cannot. Read containment for such adapters needs a read jail this
/// platform does not have; the residual is stated rather than hidden.
fn scoped_seat_admission(
    cli_key: &str,
    scope: &ChatScope,
    config: &AcpConfig,
) -> Result<(), String> {
    let scoped = scope.code_graph_db.is_some() || !scope.read_roots.is_empty();
    if !scoped || config.acp_input_governance || config.os_sandbox {
        return Ok(());
    }
    Err(format!(
        "seat '{cli_key}' cannot join a SCOPED chat: its ACP adapter '{}' asks no permissions (the \
         chat's read-only boundary never sees its tool calls) and its record arms no OS sandbox, so \
         the scoped repositories could not be held read-only; open the chat unscoped for this seat, \
         or set `os_sandbox = true` on its [cli.acp] record",
        config.binary
    ))
}

/// The boundary a chat's seats are judged against (core#410, review): the scratch root is the ONE
/// write root (and the cwd), the scoped repository roots are read-only, `HOME` and — for a claude
/// seat — its worker config dir get the same carve-outs the governed boundary applies. No phase
/// scopes: a chat has no phases.
fn chat_boundary(
    scope: &ChatScope,
    seat_cli: wicked_apps_core::spawn::SeatCli,
) -> crate::gate_hook::BoundaryCtx {
    crate::gate_hook::BoundaryCtx {
        roots: crate::path_policy::AllowedRoots {
            write: vec![scope.cwd.clone()],
            read: scope
                .read_roots
                .iter()
                .map(std::path::PathBuf::from)
                .collect(),
        },
        cwd: scope.cwd.clone(),
        home: std::env::var_os("HOME").map(std::path::PathBuf::from),
        claude_config_dir: wicked_apps_core::spawn::seat_config_for(seat_cli)
            .ok()
            .and_then(|c| c.claude_dir().map(std::path::Path::to_path_buf)),
        pre_build_scope: false,
        no_code_scope: false,
    }
}

impl ChatScope {
    /// The private scratch root a chat runs in when its opener names none:
    /// `<system temp>/wicked-core-chat-<safe prefix>-<fnv1a64 of the full id>`. NEVER the daemon's
    /// cwd (F-067: a daemon started from `$HOME` gave every chat seat the operator's home directory
    /// to explore). The readable prefix is the id reduced to `[A-Za-z0-9._-]` (so an arbitrary
    /// client-minted id cannot spell a path); the hash of the FULL id keeps distinct ids apart —
    /// `a/b` and `a_b`, `.` and `` — so two chats never share a root (Copilot, #426).
    pub fn scratch_for(chat_id: &str) -> std::path::PathBuf {
        let safe: String = chat_id
            .chars()
            .take(48)
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '_' | '-') {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        // FNV-1a 64: stable across builds and platforms (a `DefaultHasher` is neither), no
        // dependency, and collision-resistant enough for ids one daemon mints.
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in chat_id.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        let prefix = if safe.is_empty() {
            "chat"
        } else {
            safe.as_str()
        };
        std::env::temp_dir().join(format!("wicked-core-chat-{prefix}-{hash:016x}"))
    }
}

impl AcpStepRunner {
    pub(crate) fn new(tx: std::sync::mpsc::Sender<Command>) -> Self {
        let maps = Arc::new(Mutex::new(ElicitationMaps::new()));
        let write_reg: WriteReg = Arc::new(Mutex::new(HashMap::new()));
        Self::new_with_maps(tx, maps, write_reg)
    }

    /// [`new`](Self::new) for the runner the engine spawns on a STORE (codex round 8): `db_path`
    /// is the database `Core::spawn` was given; its canonical parent is the daemon's operational
    /// state home (`state_home::operational_home_of_db`), fenced on every launch this runner and
    /// its wrapped fallback make — snapshot or not.
    pub(crate) fn new_for_store(tx: std::sync::mpsc::Sender<Command>, db_path: &str) -> Self {
        let mut runner = Self::new(tx.clone());
        runner.operational_home = crate::state_home::operational_home_of_db(db_path);
        runner.fallback = WrappedCliStepRunner::with_tx_for_store(tx, db_path);
        runner
    }

    /// Construct with explicitly-provided `ElicitationMaps` and `WriteReg` Arcs.
    ///
    /// Used by `spawn_with_acp_sessions` so the actor and the runner share the same
    /// `ElicitationMaps` instance. The caller verifies `Arc::ptr_eq` after construction.
    pub(crate) fn new_with_maps(
        tx: std::sync::mpsc::Sender<Command>,
        elicitation_maps: Arc<Mutex<ElicitationMaps>>,
        write_reg: WriteReg,
    ) -> Self {
        let secs = std::env::var("WICKED_UNIT_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(7200);
        Self {
            // Give the fallback runner the same tx so it can relay GovernanceContextArmed
            // events (EVT-016 "wrapped_cli" path) when ACP falls back to the wrapped-CLI runner.
            fallback: WrappedCliStepRunner::with_tx(tx.clone()),
            tx,
            sessions: Arc::new(Mutex::new(HashMap::new())),
            pending_injects: Arc::new(Mutex::new(HashMap::new())),
            chat_activity: Arc::new(Mutex::new(HashMap::new())),
            chat_scopes: Arc::new(Mutex::new(HashMap::new())),
            chat_open_seq: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            timeout: Duration::from_secs(secs),
            operational_home: None,
            elicitation_maps,
            write_reg,
        }
    }

    /// Accessor for the shared `ElicitationMaps` arc (used by `spawn_with_acp_sessions`
    /// to `Arc::ptr_eq`-verify that the actor and runner share the same instance).
    pub fn elicitation_maps(&self) -> &Arc<Mutex<ElicitationMaps>> {
        &self.elicitation_maps
    }

    fn emit_event(&self, ev: CoreEvent) {
        let _ = self.tx.send(Command::EmitEvent(ev));
    }

    // ── Chat sessions (crew#165 / core#13) ──────────────────────────────────────
    //
    // A chat reuses the SAME session pool as runs, keyed `("chat:<id>", cli)`. Turns
    // are RAW conversation — no governance arming, no council, no unit machinery, no
    // wrapped-CLI fallback (a dead seat is reported honestly and re-warmed on the
    // next ensure, never silently downgraded to one-shot).

    fn chat_pool_key(chat_id: &str) -> String {
        format!("chat:{chat_id}")
    }

    fn chat_timeout() -> Duration {
        let secs = std::env::var("WICKED_CHAT_TURN_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(300);
        Duration::from_secs(secs)
    }

    /// How long a chat may sit with no turn before the reaper reclaims it.
    ///
    /// This is NOT [`Self::chat_timeout`]: that is a per-turn response budget and never fires on a
    /// chat nobody is talking to. Idle eviction is the only reclamation path that covers a chat
    /// orphaned by a closed or crashed tab, which no client-side teardown can reach (FINDING-027).
    ///
    /// 30 minutes by default: a warm seat costs ~520 MB resident, and a chat untouched for half an
    /// hour is far more likely abandoned than mid-thought. Re-warming is a few seconds.
    pub fn chat_idle_ttl() -> Duration {
        let secs = std::env::var("WICKED_CHAT_IDLE_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(1800);
        Duration::from_secs(secs)
    }

    /// The most chats that may hold warm seats at once. A backstop for the case the TTL cannot
    /// cover: many chats opened faster than the idle window retires them.
    ///
    /// Floored at 1 — a cap of 0 would evict the chat being opened, which is not a smaller pool but
    /// a broken one.
    pub fn chat_pool_cap() -> usize {
        std::env::var("WICKED_CHAT_POOL_MAX")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(8)
            .max(1)
    }

    /// Mark a chat as active NOW. Called on open, ensure, and turn — anything that proves someone
    /// is still using it.
    fn chat_touch(&self, chat_id: &str) {
        let mut guard = self.chat_activity.lock().unwrap_or_else(|p| p.into_inner());
        guard.insert(chat_id.to_string(), Instant::now());
    }

    /// Every chat holding pool entries, with how long it has been idle.
    ///
    /// Chats are enumerated from the SESSION POOL, not from the activity map: the pool is what
    /// actually pins processes, so this can never report a chat that costs nothing while missing
    /// one that does.
    ///
    /// A chat with a pool entry but no WARM seat still lists, with `seats` empty. That differs from
    /// [`Self::chat_seats`] on purpose: `seats` answers "who can take a turn", this answers "what
    /// is holding pool state". Anything in the map is something only a close removes, so anything
    /// in the map has to be listable and reapable — an entry no surface reports is the shape of the
    /// leak this whole mechanism exists to end.
    pub fn chat_list(&self) -> Vec<ChatInfo> {
        let mut by_chat: HashMap<String, Vec<String>> = HashMap::new();
        {
            let guard = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
            for ((rid, cli), slot) in guard.iter() {
                let Some(chat_id) = rid.strip_prefix("chat:") else {
                    continue;
                };
                let seats = by_chat.entry(chat_id.to_string()).or_default();
                if slot.is_some() {
                    seats.push(cli.clone());
                }
            }
        }
        let now = Instant::now();
        let activity = self
            .chat_activity
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        let scopes: HashMap<String, ChatScope> = self
            .chat_scopes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .map(|(id, r)| (id.clone(), r.scope.clone()))
            .collect();
        let mut out: Vec<ChatInfo> = by_chat
            .into_iter()
            .map(|(chat_id, mut seats)| {
                seats.sort();
                // A warm chat with no recorded activity is treated as idle-since-forever rather
                // than as fresh: the conservative reading reclaims it, and the alternative would
                // let any gap in touch-recording pin memory permanently — the exact defect here.
                let idle_secs = activity
                    .get(&chat_id)
                    .map(|t| now.saturating_duration_since(*t).as_secs())
                    .unwrap_or(u64::MAX);
                ChatInfo {
                    scope: scopes.get(&chat_id).cloned(),
                    chat_id,
                    seats,
                    idle_secs,
                }
            })
            .collect();
        out.sort_by(|a, b| a.chat_id.cmp(&b.chat_id));
        out
    }

    /// Close every chat idle longer than `ttl`. Returns the ids reaped, oldest first.
    pub fn chat_reap_idle(&self, ttl: Duration) -> Vec<String> {
        let ttl_secs = ttl.as_secs();
        let mut victims: Vec<(u64, String)> = self
            .chat_list()
            .into_iter()
            .filter(|c| c.idle_secs >= ttl_secs)
            .map(|c| (c.idle_secs, c.chat_id))
            .collect();
        // Oldest first, so a caller reading the returned list sees them in the order they aged out.
        victims.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        let reaped: Vec<String> = victims
            .into_iter()
            .map(|(_, id)| {
                self.chat_close(&id, ChatCloseReason::Idle);
                id
            })
            .collect();
        self.prune_orphan_activity(ttl);
        reaped
    }

    /// Drop activity entries with no pool entry behind them.
    ///
    /// `chat_close` prunes the chat it closes, but a turn outliving the TTL re-touches on the way
    /// out — after the reaper already closed the chat — leaving an entry nothing else collects.
    /// Tiny individually; unbounded over a long-lived daemon, which is the shape of bug this whole
    /// change exists to end.
    ///
    /// Only entries older than `ttl` are dropped. `chat_ensure` touches BEFORE inserting its pool
    /// entry, so a chat mid-open is briefly touched-but-unpooled; pruning on absence alone would
    /// race with it, erase its timestamp, and get it reaped on the next sweep as
    /// idle-since-forever. A just-touched entry is never old enough to qualify.
    fn prune_orphan_activity(&self, ttl: Duration) {
        let pooled: std::collections::HashSet<String> =
            self.chat_list().into_iter().map(|c| c.chat_id).collect();
        let now = Instant::now();
        let mut activity = self.chat_activity.lock().unwrap_or_else(|p| p.into_inner());
        activity.retain(|id, t| pooled.contains(id) || now.saturating_duration_since(*t) < ttl);
    }

    /// Evict least-recently-used chats until at most `cap` remain. Returns the ids evicted.
    pub fn chat_enforce_cap(&self, cap: usize) -> Vec<String> {
        let cap = cap.max(1);
        let mut live = self.chat_list();
        let Some(excess) = live.len().checked_sub(cap).filter(|n| *n > 0) else {
            return Vec::new();
        };
        // Most idle first. The caller touches the chat it is opening BEFORE calling this, so that
        // chat sorts last and opening a chat can never evict the chat being opened.
        live.sort_by(|a, b| {
            b.idle_secs
                .cmp(&a.idle_secs)
                .then_with(|| a.chat_id.cmp(&b.chat_id))
        });
        live.into_iter()
            .take(excess)
            .map(|c| {
                self.chat_close(&c.chat_id, ChatCloseReason::PoolCap);
                c.chat_id
            })
            .collect()
    }

    /// Warm (or return the existing) ACP session for one chat seat. Unlike the run
    /// path, a failed start is NOT cached as poisoned — chats are interactive, so
    /// every ensure retries and the operator sees each failure.
    fn chat_ensure(&self, chat_id: &str, cli_key: &str) -> Result<Arc<Mutex<AcpProcess>>, String> {
        // Touch FIRST, and unconditionally: a chat whose seat is warming is in use, and recording
        // that only on success would leave a chat mid-`chat_open` looking idle-since-forever to a
        // reaper running concurrently.
        self.chat_touch(chat_id);
        let key = (Self::chat_pool_key(chat_id), cli_key.to_string());
        {
            let guard = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(Some(arc)) = guard.get(&key) {
                return Ok(arc.clone());
            }
        }
        // ONE registry read for this launch: the transport config AND the seat identity come off
        // the same record (codex r2, PR#413).
        let (config, seat_cli) =
            acp_launch_facts(cli_key).ok_or_else(|| format!("no ACP config for '{cli_key}'"))?;
        if config.transport == AcpTransport::Http {
            return Err(format!(
                "ACP HTTP transport not supported for chat ('{cli_key}')"
            ));
        }
        // The scope recorded at open (core#410 / crew#502). A chat with none is a caller bug and
        // is refused — never a fallback to the daemon's own cwd (F-067).
        let scope = self
            .chat_scopes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(chat_id)
            .map(|r| r.scope.clone())
            .ok_or_else(|| format!("chat '{chat_id}' has no scope recorded — open it first"))?;
        // A SCOPED chat promises read-only roots: only a seat this engine can actually hold to
        // that — its ACP adapter asks permissions (the chat boundary sees every tool call) or it
        // runs under the kernel write floor — may join one (Copilot, #426).
        scoped_seat_admission(cli_key, &scope, &config)?;
        ensure_chat_scratch_root(&scope.cwd).map_err(|e| format!("chat '{chat_id}': {e}"))?;
        // Grounded on the scope's graph — the READ-ONLY estate MCP, the same seam governed
        // workers get (DES-GROUNDING-001; formerly "chat is repo-less exploration → no estate
        // MCP", FINDING-122) — in its scratch cwd, with the scoped repository roots advertised.
        // No skills delivery, no per-session settings dir, no unit provenance: a chat is not a
        // run unit.
        let mut proc = start_acp_process_with_write_roots(
            &config,
            &scope.cwd,
            scope.code_graph_db.as_deref(),
            None,
            &[],
            &scope.read_roots,
            &[],
            &crate::skills_snapshot::SkillsDelivery::None,
            seat_cli,
            None,
            self.operational_home.as_deref(),
        )
        .map_err(|e| e.to_string())?;
        // The chat's filesystem boundary, judged on every permission request of every turn on
        // this session (core#410, review): write = the scratch root; read = the scoped roots.
        proc.chat_boundary = Some(chat_boundary(&scope, seat_cli));
        let arc = Arc::new(Mutex::new(proc));
        // Insert under BOTH locks, scopes then sessions (the order `chat_open` takes them): the
        // scope this process was warmed in must still be the recorded one (Copilot, #426 — a
        // re-open with a new scope while this seat was warming would otherwise land an old-scope
        // process under a key the new scope now owns); a stale process is dropped, not inserted.
        let scopes = self.chat_scopes.lock().unwrap_or_else(|p| p.into_inner());
        if scopes.get(chat_id).map(|r| &r.scope) != Some(&scope) {
            drop(arc);
            return Err(format!(
                "chat '{chat_id}': its scope changed while seat '{cli_key}' was warming — retry"
            ));
        }
        let mut guard = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
        // A racing ensure (same scope, by the check above) may have inserted first — reuse theirs.
        if let Some(Some(existing)) = guard.get(&key) {
            return Ok(existing.clone());
        }
        guard.insert(key, Some(arc.clone()));
        Ok(arc)
    }

    /// Eagerly warm one session per seat in `scope`; per-seat outcome, `ChatSessionReady`/
    /// `ChatSessionFailed` emitted for each. The scope is RECORDED first (core#410 / crew#502) so
    /// every later ensure — a re-warm after an eviction, a turn on a seat that was never warm —
    /// lands in the same cwd, on the same graph, with the same read roots.
    ///
    /// Re-opening a chat that already holds a DIFFERENT scope EVICTS its warm seats first (the
    /// pool entries are dropped, no `ChatClosed` — the chat is not closing), so they re-warm in
    /// the new scope here rather than keep running in the old cwd while the record says
    /// otherwise (Copilot, #426). A re-open with the SAME scope is a plain ensure. When no seat
    /// is warm afterwards (every start failed, or `clis` was empty) the scope is DROPPED again:
    /// the pool is what the reaper and the enumerate surface see, so a scope with no pool entry
    /// would be held by nothing that can reclaim it — the caller opens again.
    pub fn chat_open(
        &self,
        chat_id: &str,
        clis: &[String],
        scope: ChatScope,
    ) -> Result<ChatOpenOutcomes, String> {
        self.validate_chat_scope(&scope)?;
        let gen = self
            .chat_open_seq
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        {
            let mut scopes = self.chat_scopes.lock().unwrap_or_else(|p| p.into_inner());
            let changed = scopes
                .get(chat_id)
                .is_some_and(|recorded| recorded.scope != scope);
            if changed {
                let prefix = Self::chat_pool_key(chat_id);
                self.sessions
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .retain(|(rid, _), _| rid != &prefix);
            }
            scopes.insert(chat_id.to_string(), RecordedScope { gen, scope });
        }
        let opened: ChatOpenOutcomes = clis
            .iter()
            .map(|cli| {
                let outcome = self.chat_ensure(chat_id, cli).map(|_| ());
                match &outcome {
                    Ok(()) => self.emit_event(CoreEvent::ChatSessionReady {
                        chat: chat_id.to_string(),
                        cli_key: cli.clone(),
                    }),
                    Err(reason) => self.emit_event(CoreEvent::ChatSessionFailed {
                        chat: chat_id.to_string(),
                        cli_key: cli.clone(),
                        reason: reason.clone(),
                    }),
                }
                (cli.clone(), outcome)
            })
            .collect();
        if self.chat_seats(chat_id).is_empty() {
            // Nothing warmed: hold no scope (and no activity stamp) for a chat the pool does not
            // know — see the doc above. Only THIS open's record, though (Copilot, #426): a newer
            // open may have recorded its own scope meanwhile, and that one is its to keep.
            self.drop_scope_if_gen(chat_id, gen);
        }
        // Enforce the cap only AFTER the new chat is warm and touched, so it is the freshest entry
        // and therefore the last possible victim. Doing it first would let a full pool evict a
        // chat, warm the new one, and leave the pool at the cap anyway — same memory, one more
        // reap. Cap breaches are rare, so paying the reap on the open path costs nothing typical.
        //
        // Evictions are not logged here: each one emits `ChatClosed { reason: "pool_cap" }`, which
        // is the surface an operator actually watches. A second, log-only channel would be the one
        // that goes stale.
        self.chat_enforce_cap(Self::chat_pool_cap());
        Ok(opened)
    }

    /// Drop `chat_id`'s recorded scope and activity stamp — only if the record is still the one
    /// open generation `gen` made (Copilot, #426: an older open's cleanup must never remove a
    /// newer open's record).
    fn drop_scope_if_gen(&self, chat_id: &str, gen: u64) {
        let mut scopes = self.chat_scopes.lock().unwrap_or_else(|p| p.into_inner());
        if scopes.get(chat_id).is_some_and(|r| r.gen == gen) {
            scopes.remove(chat_id);
            self.chat_activity
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(chat_id);
        }
    }

    /// Refuse a scope this engine must not run a chat in (core#410, review) — judged BEFORE it is
    /// recorded, so nothing downstream ever sees an invalid one. The N-API caller is the daemon,
    /// but the run path validates what IT is handed and the chat path is held to the same bar:
    ///  - every read root is absolute and passes `validate_extra_read_roots` (the same exclusions
    ///    a run's launch-declared read roots get: never the engine's pin/config trees);
    ///  - the scratch root is absolute (a relative one would resolve against the daemon's cwd);
    ///  - the graph, when named, is an absolute path to an EXISTING file and never a top-level
    ///    file of the engine's own state home — that is where the operational store and its
    ///    sidecars live, and a chat's estate MCP over the operational store is FINDING-067.
    fn validate_chat_scope(&self, scope: &ChatScope) -> Result<(), String> {
        // The scratch root: absolute, and a STRICT descendant of the system temp directory
        // (Copilot, #426 — an absolute-only rule let a caller name `/`, `$HOME` or the state home
        // as the writable root, which the ensure would then have made 0700 and the boundary made
        // writable). Judged on real paths through the longest existing ancestor, so a symlinked
        // temp dir (macOS `/var` → `/private/var`) and a not-yet-created leaf compare correctly.
        if !scope.cwd.is_absolute() {
            return Err(format!(
                "chat scope: cwd {} is not absolute",
                scope.cwd.display()
            ));
        }
        let cwd = canonical_ish(&scope.cwd);
        // The temp bases: Rust's `temp_dir()` (unix: `TMPDIR`; Windows: `TMP` then `TEMP`), `/tmp`
        // on unix, AND `TMP`/`TEMP` wherever set — Node's `os.tmpdir()`, which the daemon derives
        // its base from, consults `TMPDIR`, `TMP`, `TEMP` in that order on unix and `TEMP` before
        // `TMP` on Windows, so the two runtimes can disagree on which variable wins (independent
        // review, C5); every spelling a caller could legitimately have used is accepted.
        let temps: Vec<std::path::PathBuf> = {
            let mut t = vec![canonical_ish(&std::env::temp_dir())];
            if cfg!(unix) {
                t.push(canonical_ish(std::path::Path::new("/tmp")));
            }
            for var in ["TMP", "TEMP"] {
                if let Some(v) = std::env::var_os(var).filter(|v| !v.is_empty()) {
                    let p = std::path::PathBuf::from(v);
                    if p.is_absolute() {
                        t.push(canonical_ish(&p));
                    }
                }
            }
            t
        };
        if !temps.iter().any(|t| cwd.starts_with(t) && cwd != *t) {
            return Err(format!(
                "chat scope: cwd {} must be a directory of its own under the system temp directory \
                 ({}), never a repository, a home or the engine's state",
                scope.cwd.display(),
                std::env::temp_dir().display()
            ));
        }
        let op_home = self.operational_home.as_deref().map(canonical_ish);
        if let Some(op) = &op_home {
            if cwd.starts_with(op) {
                return Err(format!(
                    "chat scope: cwd {} lies inside the engine's own state home",
                    scope.cwd.display()
                ));
            }
        }
        // The read roots: absolute; the run path's exclusions; never the engine's state home or
        // anything under it (Copilot, #426 — `validate_extra_read_roots` fences the config tree
        // only); never equal to / containing / inside the scratch root (the claude deny rules over
        // a root that contained the cwd would make the writable root unwritable — the two layers
        // must agree).
        for root in &scope.read_roots {
            let p = std::path::Path::new(root);
            if !p.is_absolute() {
                return Err(format!("chat scope: read root {root:?} is not absolute"));
            }
            let r = canonical_ish(p);
            if let Some(op) = &op_home {
                if r.starts_with(op) {
                    return Err(format!(
                        "chat scope: read root {root} lies inside the engine's own state home \
                         (FINDING-067)"
                    ));
                }
            }
            if r.starts_with(&cwd) || cwd.starts_with(&r) {
                return Err(format!(
                    "chat scope: read root {root} overlaps the scratch root {} — a root can neither \
                     contain nor sit inside the chat's writable directory",
                    scope.cwd.display()
                ));
            }
        }
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(std::path::PathBuf::from);
        crate::path_policy::validate_extra_read_roots(&scope.read_roots, home.as_deref())
            .map_err(|e| format!("chat scope: {e}"))?;
        // The graph: absolute, an EXISTING file, and never the engine's own store — judged on the
        // RESOLVED file (Copilot, #426: a symlink or hard link outside the state home that resolves
        // to `core.db` or a sidecar passed a spelling-based check): its canonical parent must not
        // be the state home, and on unix its (device, inode) must match no top-level entry there.
        if let Some(db) = scope.code_graph_db.as_deref() {
            let db_path = std::path::Path::new(db);
            if !db_path.is_absolute() {
                return Err(format!("chat scope: code graph {db:?} is not absolute"));
            }
            if !db_path.is_file() {
                return Err(format!(
                    "chat scope: no code graph at {db} (the estate MCP would answer for nothing)"
                ));
            }
            if let Some(op) = self.operational_home.as_deref() {
                let resolved =
                    std::fs::canonicalize(db_path).unwrap_or_else(|_| db_path.to_path_buf());
                let op_real = canonical_ish(op);
                if resolved
                    .parent()
                    .is_some_and(|parent| crate::state_home::same_dir(parent, &op_real))
                    || same_file_as_a_top_level_entry_of(db_path, op)
                {
                    return Err(format!(
                        "chat scope: {db} is (or resolves to) a top-level file of the engine's own \
                         state home — the operational store is never a chat's graph (FINDING-067)"
                    ));
                }
            }
        }
        Ok(())
    }

    /// One seat's turn on a chat message. Streams deltas via `ChatDelta`, returns the
    /// completed reply text. On failure the seat's session is EVICTED (next ensure
    /// re-warms, into the chat's recorded scope) and the error is returned — never floored,
    /// never faked.
    pub fn chat_turn(&self, chat_id: &str, cli_key: &str, text: &str) -> Result<String, String> {
        let arc = self.chat_ensure(chat_id, cli_key)?;
        let tx = self.tx.clone();
        let (chat_ev, cli_ev) = (chat_id.to_string(), cli_key.to_string());
        // F-068 (core#410): a seat's startup banner never reaches the transcript. The gate holds
        // the stream only while it is still banner-shaped and releases everything else at once;
        // the assembled reply below is banner-stripped at its own seam (`strip_pi_banner`).
        let gate = Arc::new(Mutex::new(BannerGate::default()));
        let deliver = {
            let tx = tx.clone();
            move |text: String| {
                let _ = tx.send(Command::EmitEvent(CoreEvent::ChatDelta {
                    chat: chat_ev.clone(),
                    cli_key: cli_ev.clone(),
                    text,
                }));
            }
        };
        let emit: Box<crate::workflow::DeltaSink> = {
            let gate = Arc::clone(&gate);
            let deliver = deliver.clone();
            Box::new(move |delta: &str| {
                let released = gate.lock().unwrap_or_else(|p| p.into_inner()).push(delta);
                if let Some(text) = released {
                    deliver(text);
                }
            })
        };
        let result = {
            let mut proc = arc.lock().unwrap_or_else(|p| p.into_inner());
            // Chat turns never run in a governed epoch — epoch=0 disables elicitation.
            exec_turn_acp(
                &mut proc,
                text,
                &[],
                &emit,
                Self::chat_timeout(),
                Arc::clone(&self.elicitation_maps),
                "",
                0,
                &self.tx,
                None,
            )
        };
        // Whatever the gate still holds at turn end is content (or a banner it strips) — deliver.
        if let Some(text) = gate.lock().unwrap_or_else(|p| p.into_inner()).finish() {
            deliver(text);
        }
        // Touch again on the way out. `chat_ensure` touched on the way in, but a long turn would
        // then be counted as idle for its whole duration — a 40-minute agent turn would be reaped
        // out from under the operator the moment it finished.
        self.chat_touch(chat_id);
        match result {
            Ok(turn) if turn.status == StepStatus::Ok => Ok(turn.output),
            Ok(turn) => {
                self.chat_evict(chat_id, cli_key, &arc);
                let msg = format!(
                    "seat '{cli_key}' turn ended {:?}: {}",
                    turn.status, turn.output
                );
                // Daemon-log the eviction (crew#267) — but SUMMARIZED: turn.output can be up
                // to the 8MB cap and carry user/model content; the log gets status + size,
                // the caller (and thus the ChatReply the user sees) keeps the full text
                // (Copilot).
                eprintln!(
                    "[wicked-core] chat '{chat_id}' evicting seat '{cli_key}': turn ended {:?} ({} output bytes)",
                    turn.status,
                    turn.output.len()
                );
                Err(msg)
            }
            Err(e) => {
                self.chat_evict(chat_id, cli_key, &arc);
                let msg = format!("seat '{cli_key}' session error: {e}");
                eprintln!("[wicked-core] chat '{chat_id}' evicting {msg}");
                Err(msg)
            }
        }
    }

    /// Drop the seat's pool entry — but only if it is still THIS process (Copilot, #426): a
    /// re-open with a new scope may have evicted the old process and warmed a replacement under
    /// the same key while a turn on the old one was still in flight; that turn's failure must not
    /// remove the replacement.
    fn chat_evict(&self, chat_id: &str, cli_key: &str, this: &Arc<Mutex<AcpProcess>>) {
        let key = (Self::chat_pool_key(chat_id), cli_key.to_string());
        let mut guard = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
        if matches!(guard.get(&key), Some(Some(existing)) if Arc::ptr_eq(existing, this)) {
            guard.remove(&key);
        }
    }

    /// The seats currently warm for a chat (fan-out default for `targets: None`).
    pub fn chat_seats(&self, chat_id: &str) -> Vec<String> {
        let prefix = Self::chat_pool_key(chat_id);
        let guard = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
        let mut seats: Vec<String> = guard
            .iter()
            .filter(|((rid, _), slot)| rid == &prefix && slot.is_some())
            .map(|((_, cli), _)| cli.clone())
            .collect();
        seats.sort();
        seats
    }

    /// Close a chat's warm sessions and reap their processes. Idempotent.
    ///
    /// `reason` reaches the operator on `ChatClosed`: a chat that vanished because the daemon
    /// reclaimed it is a different event from one the operator ended, and a UI that cannot tell
    /// them apart reports a reclaim as a mystery.
    pub fn chat_close(&self, chat_id: &str, reason: ChatCloseReason) {
        let prefix = Self::chat_pool_key(chat_id);
        {
            let mut guard = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
            guard.retain(|(rid, _), _| rid != &prefix);
        }
        // Drop the activity entry too. It is small, but it is keyed by an unbounded stream of
        // client-minted chat ids — leaving it behind trades a 520 MB leak for a slower one.
        self.chat_activity
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(chat_id);
        // And the recorded scope (core#410): a closed chat holds no cwd/graph/roots either.
        self.chat_scopes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(chat_id);
        self.emit_event(CoreEvent::ChatClosed {
            chat: chat_id.to_string(),
            reason: reason.as_str().to_string(),
        });
    }

    /// Close all ACP sessions for `run_id` and kill their child processes. Idempotent.
    /// Call this after the last unit of a run completes (mirrors
    /// [`PersistentStepRunner::drop_session`]).
    /// Drop ONE `(run_id, cli_key)` session — the process is killed (group and all) and reaped
    /// when the last `Arc` lets go, which for a caller that has released its guard is right here.
    /// The run's other seats keep their sessions.
    fn drop_session_key(&self, key: &(String, String)) {
        let removed = {
            let mut guard = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
            guard.remove(key)
        };
        drop(removed);
        self.write_reg
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|(rid, skey, _), _| !(rid == &key.0 && skey == &key.1));
    }

    pub fn drop_session(&self, run_id: &str) {
        let mut guard = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
        // v3.1 §3: each session's per-process settings directory goes with its `AcpProcess` —
        // reaped by the owner on drop (a turn still holding the `Arc` reaps when it lets go),
        // never by name from here.
        guard.retain(|(rid, _), _| rid != run_id);
        drop(guard);
        self.write_reg
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|(rid, _, _), _| rid != run_id);
        let mut injects = self
            .pending_injects
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        injects.remove(run_id);
    }

    /// Drain queued operator messages matching `(run_id, cli_key)` — `All`-targeted and
    /// exact-CLI-targeted entries deliver; entries for other CLIs stay queued. Each entry
    /// is returned as `(original target string, prompt-ready block)` so the delivery event
    /// carries the INJECTION target ("all" or the cli_key the operator named), matching
    /// the PTY path's event contract.
    fn drain_operator_messages(
        &self,
        run_id: &str,
        cli_key: &str,
    ) -> Vec<(String, PriorUnitOutput)> {
        use crate::command::InjectTarget;
        let mut guard = self
            .pending_injects
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let Some(queue) = guard.get_mut(run_id) else {
            return Vec::new();
        };
        let mut delivered = Vec::new();
        queue.retain(|(target, message)| {
            let (matches, target_str) = match target {
                InjectTarget::All => (true, "all".to_string()),
                InjectTarget::Cli(k) => (k == cli_key, k.clone()),
            };
            if matches {
                delivered.push((
                    target_str,
                    PriorUnitOutput {
                        label: "[operator message]".to_string(),
                        output: message.clone(),
                    },
                ));
            }
            !matches
        });
        if queue.is_empty() {
            guard.remove(run_id);
        }
        delivered
    }

    /// Probe the session cache for `session_key`, purging a dead husk (crew#340).
    ///
    /// `kill -9` on a cached bridge between turns leaves `Some(arc)` in the map pointing at
    /// an exited process. The next `session/prompt` write EPIPEs, the unit degrades to the
    /// single-shot fallback, and — because the husk was only evicted per-turn — every later
    /// unit of the run repeated the dance. Worse, in the field the daemon's ACP client looked
    /// wedged until restart. The fix: check `KillHandle::try_exit_status` (non-blocking, never
    /// killing) BEFORE reuse; a bridge that already exited is purged from the session map and
    /// the write registry, and the caller starts a FRESH `session/new` instead of degrading.
    ///
    /// A session whose proc mutex is HELD (a concurrent turn in flight) cannot be probed
    /// without blocking behind that turn — it is reported `Live` and the caller blocks on the
    /// lock exactly as before; the in-flight turn's own error path handles a mid-turn death.
    fn probe_cached_session(&self, session_key: &(String, String)) -> SessionProbe {
        let slot = {
            let guard = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
            guard.get(session_key).cloned()
        };
        let arc = match slot {
            None => return SessionProbe::Vacant,
            Some(None) => return SessionProbe::FailedStartup,
            Some(Some(arc)) => arc,
        };
        // The post-mortem note carries the exit status, the stderr tail, and any queued
        // stdout — everything crew#267/#289 taught this file to keep about a dead bridge.
        // The kill-handle Arc rides along as the HUSK'S IDENTITY: the registry purge below
        // must match this exact process, never a racing replacement's entries.
        let post_mortem = match arc.try_lock() {
            Ok(proc) => proc.kill_handle.try_exit_status().map(|st| {
                (
                    death_context_with(&proc, Some(st)),
                    Arc::clone(&proc.kill_handle),
                )
            }),
            // Held by an in-flight turn — alive enough; that turn's own paths report death.
            Err(std::sync::TryLockError::WouldBlock) => None,
            Err(std::sync::TryLockError::Poisoned(p)) => {
                let proc = p.into_inner();
                proc.kill_handle.try_exit_status().map(|st| {
                    (
                        death_context_with(&proc, Some(st)),
                        Arc::clone(&proc.kill_handle),
                    )
                })
            }
        };
        let Some((note, husk_kill)) = post_mortem else {
            return SessionProbe::Live(arc);
        };
        // The husk is dead. Purge exactly THIS husk (never a racing replacement) from the
        // session map, and drop its write-registry handles so teardown stops signalling it.
        {
            let mut guard = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
            if guard
                .get(session_key)
                .is_some_and(|slot| slot.as_ref().is_some_and(|cur| Arc::ptr_eq(cur, &arc)))
            {
                guard.remove(session_key);
            }
        }
        // Registry entries are matched by the husk's own kill-handle identity — a concurrent
        // replacement for the same (run_id, cli_key) inserted between the map removal above
        // and this purge keeps its handles (its key differs by launch_seq, its value by Arc).
        self.write_reg
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|(rid, key, _), (_, kill)| {
                rid != &session_key.0 || key != &session_key.1 || !Arc::ptr_eq(kill, &husk_kill)
            });
        // LOUD (crew#340): the operator's daemon log must say the cached bridge was found
        // dead and a fresh session is being started — the old symptom was a silent
        // degradation that read as "ACP wedged until daemon restart".
        eprintln!(
            "[wicked-core] cached ACP session for '{}' (run {}) found dead before reuse \
             (crew#340) — purging the husk and starting a fresh session instead of \
             degrading to single-shot{note}",
            session_key.1, session_key.0
        );
        SessionProbe::Vacant
    }

    fn exec_turn(&self, input: &StepInput, emit: &DeltaSink) -> StepOutput {
        // The actor allocates one epoch for every ACP unit before it leaves the actor
        // thread. Keep the cleanup guard outside the implementation so every return path
        // (including wrapped fallback and a panic) releases that epoch exactly once.
        let mut epoch_guard = (input.elicitation_epoch > 0).then(|| EpochCleanup {
            maps: Arc::clone(&self.elicitation_maps),
            run_id: input.run_id.clone(),
            epoch: input.elicitation_epoch,
            launch_seq: input.launch_seq,
            bus_in_flight_deferred: false,
            tx: self.tx.clone(),
            in_flight_id: None,
            in_flight_action: None,
            in_flight_reason: None,
        });

        let output = self.exec_turn_inner(input, emit);

        // On a normal bus-worker return, publication owns the in-flight marker until
        // `task.completed` is durable. A panic never reaches this assignment, so Drop
        // clears the marker and the degraded recovery path can terminalize the task.
        if let Some(guard) = epoch_guard.as_mut() {
            let maps = self
                .elicitation_maps
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            guard.bus_in_flight_deferred =
                maps.is_bus_worker_in_flight(&input.run_id, input.launch_seq);
        }
        output
    }

    fn exec_turn_inner(&self, input: &StepInput, emit: &DeltaSink) -> StepOutput {
        let run_id = input.run_id.clone();
        let cli_key = input
            .unit
            .assigned_cli
            .as_deref()
            .unwrap_or("claude")
            .to_string();

        // The seat's MERGED registry record, read by key ONCE for this turn: its `binary` decides
        // whether this is a claude seat (the only one the plugin handshake and the Claude directive
        // form apply to), its `[cli.acp]` bridge is the carrier, and its `[cli.acp]` table decides
        // the transport and admission below. The identity is judged off THIS record through the
        // ONE function seat selection also reads (`seat_identity_of` / `acp_seat_identity`; #402
        // review pass 2), so a unit is never routed to a seat this carrier would then refuse. The
        // wrapped fallback below judges the CLI on its own terms.
        let seat = registry_record(&cli_key);
        let worker_cli = seat_identity_of(seat.as_ref(), &cli_key);
        // Judged off THIS record — the same one whose `[cli.acp]` decides the transport below
        // (`acp_cfg_probe`) — never a second registry read (codex r2, PR#413: a clis.toml edit
        // between two reads would pair one record's bridge with another's identity). The per-seat
        // configuration decision (core#410) reads the same record's `binary`.
        let seat_cli = seat_cli_of(seat.as_ref(), &cli_key);
        // core#396 / v3.1 §4 — admission, BEFORE the operator messages below are consumed
        // (at-most-once) and before any session is opened, with the CACHED SESSION FIRST and ONE
        // policy for cached and fresh alike (`admit_turn`, codex round 4): a session this run
        // already holds for this seat is admitted against the snapshot it was OPENED with
        // (`proc.skills`), never against a re-resolved `current` — the bridge holds the plugin it
        // loaded, and a `current` that has since moved to a generation dropping a skill the pinned
        // one still has must not refuse a turn that can succeed. A cached session opened with
        // NOTHING pinned (no root anywhere at the time) is judged as exactly that (codex round 5,
        // `Turn::Cached(None)`): the plugin is handed at `session/new` and never afterwards, so a
        // skill-bearing turn on it is REFUSED naming the skills — the ambient configuration is not
        // re-resolved for a session that cannot receive what it now holds, and no directive is
        // ever generated for a skill the session never loaded. Only a FRESH launch (`Turn::Fresh`)
        // resolves the ladder + the fence check. The run's whole skill set must EXIST (plan-wide,
        // transitive mandates included) and what THIS seat invokes must be deliverable to it, or
        // the unit is REFUSED by name.
        let session_key = (run_id.clone(), cli_key.clone());
        let probe = self.probe_cached_session(&session_key);
        // F-036 (adversarial review on #414): a cached process is reused ONLY when its posture
        // matches this unit's. A no-code phase reaching the creator's (write-posture) process
        // closes it — group killed, bounded reap — and opens a fresh one; a code phase never
        // inherits a read-only process either. Same rule as the PTY carrier.
        let wants_no_code = crate::worktree_guard::applies_to(&input.unit);
        let probe =
            match probe {
                SessionProbe::Live(arc)
                    if arc.lock().unwrap_or_else(|p| p.into_inner()).no_code != wants_no_code =>
                {
                    eprintln!(
                    "wicked-core: run {run_id} unit {} needs a {} ACP process for `{cli_key}` but \
                     the cached one was opened {} — closing it and starting fresh (F-036)",
                    input.unit.ord,
                    if wants_no_code { "read-only" } else { "write-posture" },
                    if wants_no_code { "with write posture" } else { "read-only" }
                );
                    drop(arc);
                    self.drop_session_key(&session_key);
                    SessionProbe::Vacant
                }
                other => other,
            };
        let turn = match &probe {
            SessionProbe::Live(arc) => {
                let proc = arc.lock().unwrap_or_else(|p| p.into_inner());
                crate::skills_snapshot::Turn::Cached(proc.skills.clone())
            }
            SessionProbe::Vacant | SessionProbe::FailedStartup => {
                crate::skills_snapshot::Turn::Fresh
            }
        };
        let skills = match crate::skills_snapshot::admit_turn(
            turn,
            input,
            &worker_cli,
            self.operational_home.as_deref(),
        ) {
            Ok(s) => s,
            Err(e) => return crate::execute_wrapped::skills_refusal(input, &e),
        };
        // Handed to real work units; an engine-internal judge/triage session only when it names a
        // skill (the same rule as the wrapped runner — a plugin's whole catalog in a byte-exact
        // verdict session costs context for a method it never invokes).
        let handed = skills.as_ref().filter(|_| {
            !crate::execute_wrapped::is_engine_internal(&input.unit)
                || input.unit.skill_ref.is_some()
        });
        let skill_form = SkillForm::for_cli(&worker_cli);
        // What a NEW session on this carrier is handed, in its lever's shape (v3.2 §2).
        let delivery = handed
            .map(|s| s.delivery(&worker_cli))
            .unwrap_or(crate::skills_snapshot::SkillsDelivery::None);
        let delivers = !matches!(delivery, crate::skills_snapshot::SkillsDelivery::None);

        // Deliver queued operator messages on this turn (the inject path for ACP runs):
        // appended AFTER the cross-CLI context blocks so they read as the most recent
        // guidance. Consumed here even if the turn later falls back to the wrapped path —
        // the same at-most-once posture as the cross-CLI context those paths also drop.
        let operator_msgs = self.drain_operator_messages(&run_id, &cli_key);
        for (orig_target, block) in &operator_msgs {
            self.emit_event(CoreEvent::WorkerMessageInjected {
                session: run_id.clone(),
                message: block.output.clone(),
                target: orig_target.clone(),
            });
        }
        let prior_with_operator: Vec<PriorUnitOutput>;
        let prior_outputs: &[PriorUnitOutput] = if operator_msgs.is_empty() {
            &input.prior_outputs
        } else {
            prior_with_operator = input
                .prior_outputs
                .iter()
                .cloned()
                .chain(operator_msgs.into_iter().map(|(_, block)| block))
                .collect();
            &prior_with_operator
        };

        // GOVERNED UNITS DO NOT RUN ON ACP — they take the wrapped-CLI path, which is the only
        // path where input governance is measured to hold (FINDING-060 / FINDING-061).
        //
        // What this replaced: an "armed" ACP path that wrote a per-unit settings file carrying the
        // PreToolUse gate-hook and the `permissions.deny` fence, passed it to the bridge as
        // `--settings <path>`, and emitted `GovernanceContextArmed { path: "acp" }`. The bridge does
        // not read that flag. `@agentclientprotocol/claude-agent-acp@0.62` inspects argv for exactly
        // four things — `--cli`, `--version`, `-v`, `--hide-claude-auth` — and `--settings` is
        // accepted as unknown argv and discarded; the `claude` it then spawns carries no `--settings`
        // of its own. Measured on one live run whose units split across both paths, sharing a single
        // decisions log: 33 gate-hook firings across the two wrapped-fallback units, 0 across the two
        // ACP units, while all four were announced as armed. The ACP units were not idle — one burned
        // 4.1M input tokens editing files. Every one of those tool calls was ungoverned, and the
        // engine recorded `governed: true` for them.
        //
        // The bridge also hardcodes `settingSources: ["user", "project", "local"]` and resolves the
        // permission mode from `permissions.defaultMode` in those settings, so an ACP worker used to
        // inherit the operator's user scope — the leak FINDING-047 closed on the wrapped path only
        // (`inject_isolation_flags` rides argv, which the bridge does not read). An operator whose
        // settings said `dontAsk` got workers with Read/Edit/Write denied; observed consequence was
        // every file mutation rerouted through Bash, which no file-tool deny rule can see, and one
        // unit that silently applied nothing and still reported done. Closed here as FINDING-061:
        // `start_acp_process` now points CLAUDE_CONFIG_DIR at an engine-minted per-spawn directory
        // (see `worker_claude_config_dir`), so the worker's user scope is engine-owned, not the
        // operator's.
        //
        // Falling back is the same decision the HTTP-transport branch below already makes for the
        // same reason ("--settings cannot be injected"). The stdio branch assumed injection worked
        // because the flag was accepted. Accepted is not applied.
        //
        // The cost is real: governed units lose multi-turn ACP and run single-shot. That is the
        // deliberate trade — the engine's contract is that a governed unit MUST NOT run ungoverned,
        // and multi-turn is a performance property. Restoring it needs a channel the bridge actually
        // honours (`_meta.claudeCode.options`, or answering `session/request_permission` from the
        // gate); filed as FINDING-062, and it is a capability addition, not a precondition for this.
        //
        // Non-claude CLIs are unaffected: input arming was always claude-only, so their governed
        // units keep the shared ACP session path below exactly as before.
        // GOVERNED UNITS RUN HERE NOW. This used to reroute to the wrapped path — single-shot —
        // because the ACP bridge discards `--settings` and there was no other way to carry input
        // governance. There is: the bridge asks the CLIENT for permission on every tool call, and
        // we now answer with the same policy and the same audit records as the hook
        // (`acp_permission`, FINDING-060/062). The cost that reroute paid — a governed unit gets
        // one turn — is what made domain-extraction unable to finish on a real repo (FINDING-100).
        // The unit's working directory, decided ONCE for both the ACP process spawn and the
        // boundary base (core#260): the worktree when the run targets a repo, else the SAME
        // per-run sandbox the wrapped path uses — never the daemon's own cwd, which is where
        // repo-less ACP units used to run (and where relative tool-call paths resolved).
        let unit_cwd = input
            .workdir
            .clone()
            .unwrap_or_else(|| crate::execute_wrapped::sandbox_for(input));
        // Read ONCE and reused below — both to decide the unadmitted-disclosure scope and (past
        // the match) to decide the ACP-vs-fallback route for this same seat. Admission is derived
        // from THIS SAME resolved value (`acp_cfg_probe.is_some_and(...)`), not a second,
        // independent `registry_record` lookup: once the admission predicate asserts a fact about
        // a specific launched executable (DES-INPUT-GOV-006 §3.4/§3.5 — `verified_version` pins
        // opencode to one build), a second reload of the disk-backed merged registry could in
        // principle race a concurrent clis.toml edit and diverge from the config that actually
        // gets spawned a few lines below. Binding both to one resolution closes that — the same
        // `seat` record whose binary decided the skill form above.
        let acp_cfg_probe = seat.and_then(|c| c.acp);
        let acp_admitted = acp_cfg_probe
            .as_ref()
            .is_some_and(|c| c.acp_input_governance);
        let mut gate_ctx = match (&input.governance, acp_admitted) {
            (Some(g), true) => {
                let scope =
                    crate::scope::resolve_scope(input.entity_mode, &input.run_id, &input.unit.id);
                let phase = crate::scope::unit_phase(input.unit.ord);
                let decisions_path =
                    crate::gate_hook::decisions_path_for(&input.run_id, input.attempt);
                let decisions_path = decisions_path.to_string_lossy().into_owned();
                // The ARMED marker before the first tool call, exactly as the wrapped path writes
                // it: the fold uses its presence to tell a governed unit that legitimately made no
                // tool calls from one whose hook never fired. Without it, a clean governed ACP run
                // would be denied for looking bypassed.
                if let Err(e) = crate::gate_hook::write_armed_marker(
                    std::path::Path::new(&decisions_path),
                    &phase,
                ) {
                    // Fail CLOSED: unable to arm means unable to prove the gate ran.
                    let reason = crate::diagnostic::with_cause(
                        "[wicked-core] could not arm governance for this ACP unit",
                        &e,
                    );
                    self.emit_event(CoreEvent::AcpFallback {
                        session: run_id.clone(),
                        cli_key: cli_key.clone(),
                        reason: reason.clone(),
                        fallback_kind: fallback_kind::GOVERNANCE_REQUIRES_WRAPPED.to_string(),
                    });
                    return fallback_with_warning(reason, input, emit, &self.fallback);
                }
                // The unit's filesystem boundary, mirroring what the wrapped launcher arms by
                // env (core#260): WRITE = unit cwd + the launch-validated extra roots; READ =
                // the shared assembly (skills dir + repo root + the launch-validated
                // extra_read_roots, core#294). Built HERE, on the runner with the governance
                // context in hand — the in-process evaluation cannot read it from any env.
                let boundary = crate::gate_hook::BoundaryCtx {
                    roots: crate::path_policy::AllowedRoots {
                        // WRITE mirrors the wrapped carrier's `armed_write_roots`: cwd, the
                        // launch-validated extras, and that graph's own key dir under THIS
                        // daemon's repo-graph root (WAL/journal; `graph_write_dir` is
                        // engine-resolved and per-key precise — `None` for anything not in that
                        // shape, an in-tree path included; core#406).
                        write: std::iter::once(unit_cwd.clone())
                            .chain(g.extra_write_roots.iter().map(std::path::PathBuf::from))
                            .chain(crate::execute_wrapped::graph_write_dir(
                                g.code_graph_db.as_deref(),
                                self.operational_home.as_deref(),
                            ))
                            .collect(),
                        // READ = the shared assembly: evidence-derived roots + the
                        // launch-validated `extra_read_roots` (core#294) + the skills snapshot
                        // handed to this unit (core#396) — read-only, so the widening never
                        // touches the write list above.
                        read: crate::execute_wrapped::assemble_read_roots(
                            g.code_graph_db.as_deref(),
                            self.operational_home.as_deref(),
                            &g.extra_read_roots,
                            handed.map(|s| s.root.as_path()),
                        ),
                    },
                    cwd: unit_cwd.clone(),
                    // The same HOME the worker subprocess inherits — captured once here so the
                    // in-process judgement's `~` expansion and `~/.claude` carve-out cannot
                    // diverge from the wrapped carrier's (Copilot).
                    home: std::env::var_os("HOME").map(std::path::PathBuf::from),
                    // The operator's alternate agent-state home (core#272) — but ONLY when the
                    // FINDING-061 escape hatch says the worker actually inherits the operator's
                    // configuration. In the default mode `start_acp_process` points the worker
                    // at an engine-minted config dir (under the OS temp, which the core#264
                    // carve-out already tolerates); carving out the DAEMON's CLAUDE_CONFIG_DIR
                    // there would downgrade writes into a tree the worker has no business in
                    // (Copilot). Validated: an empty/relative/root value must not steer an
                    // advisory carve-out.
                    claude_config_dir: std::env::var_os(
                        crate::execute_wrapped::INHERIT_OPERATOR_CONFIG_ENV,
                    )
                    .is_some()
                    .then(|| std::env::var_os(CLAUDE_CONFIG_DIR_ENV))
                    .flatten()
                    .and_then(|v| crate::gate_hook::valid_config_home(&v)),
                    // The unit's PHASE SCOPE (core#296), straight off the unit — the wrapped
                    // carrier reads the same fact from `PRE_BUILD_SCOPE_ENV`, which this carrier
                    // has no access to (the daemon's env is the daemon's, core#260).
                    pre_build_scope: input.unit.pre_build_scope,
                    // F-036: the NO-CODE scope, same route — the ACP carrier answers the
                    // seat's permission requests in-process, so the flag rides the boundary.
                    no_code_scope: crate::worktree_guard::applies_to(&input.unit),
                };
                Some((scope, phase, decisions_path, g.db_path.clone(), boundary))
            }
            // An ACP adapter that has not passed the evidence proof must remain usable for
            // non-governance-required work, but it must never look governed: disclose the
            // permissive ACP response path on the audit wire before it can execute a turn.
            //
            // Only when the seat actually HAS an ACP config: a seat with none never reaches
            // `allow_result` at all (it falls straight to `self.fallback` below), so disclosing
            // "answered by allow_result, unchecked" here would be a false claim about a unit the
            // wrapped path may be governing correctly via its own, independent gate-hook check.
            (Some(_), false) => {
                if acp_unadmitted_but_configured(acp_cfg_probe.as_ref()) {
                    if let Some(event) = acp_ungoverned_event(input, &cli_key) {
                        self.emit_event(event);
                    }
                }
                None
            }
            (None, _) => None,
        };

        // SHARED ACP SESSION PATH — ungoverned units of every CLI, plus governed units whose
        // adapter is not yet admitted. The latter remain explicitly disclosed as ungoverned.
        // Reuses `acp_cfg_probe` from the `gate_ctx` match above rather than re-reading the
        // (disk-backed) merged registry a second time for the same seat.
        let acp_config = match acp_cfg_probe {
            Some(c) => c,
            None => return self.fallback.run_unit_streaming(input, emit),
        };

        // v3.2 × codex round 3: an opencode seat whose governance content cannot take the skills
        // paths (not a JSON object) is a LAUNCH ERROR here — a refused unit naming the variable
        // — not a spawn failure that would fall back to the wrapped carrier and fail there with
        // the same defect. Judged on the same resolved value the spawn composes into.
        if let Err(why) = delivery.opencode_config(opencode_existing_config(&acp_config).as_deref())
        {
            return crate::execute_wrapped::skills_refusal(
                input,
                &crate::skills_snapshot::SkillsError::LeverConfig {
                    cli: cli_key.clone(),
                    var: crate::skills_snapshot::OPENCODE_CONFIG_ENV,
                    why,
                },
            );
        }

        if acp_config.transport == AcpTransport::Http {
            let reason = format!(
                "[wicked-core] ACP HTTP transport not yet implemented for '{cli_key}'; \
                 using single-shot fallback"
            );
            self.emit_event(CoreEvent::AcpFallback {
                session: run_id.clone(),
                cli_key: cli_key.clone(),
                reason: reason.clone(),
                fallback_kind: fallback_kind::HTTP_UNIMPLEMENTED.to_string(),
            });
            return fallback_with_warning(reason, input, emit, &self.fallback);
        }

        // Lazily open a session for (run_id, cli_key). The global map lock is held only
        // for the brief map lookup/insert — not across the blocking spawn + handshake.
        // `FailedStartup` means a previous startup for this key failed; fall back
        // immediately without re-attempting spawn (avoids repeated warnings per run).
        // The cached session was liveness-probed ABOVE, before admission (crew#340 + v3.1 §4):
        // a bridge SIGKILLed between turns is purged and replaced by a fresh spawn, never
        // written into. `reused` ⇒ the session was opened by an earlier turn and carries the
        // snapshot it was opened with (core#396) — see the binding below the match.
        let (proc_arc, reused): (Arc<Mutex<AcpProcess>>, bool) = match probe {
            SessionProbe::FailedStartup => {
                return self.fallback.run_unit_streaming(input, emit);
            }
            SessionProbe::Live(arc) => (arc, true),
            SessionProbe::Vacant => {
                // The SAME cwd the boundary was built from (core#260) — worktree, else the
                // per-run sandbox. The old `current_dir()` fallback ran repo-less units in the
                // DAEMON's own directory.
                let cwd = unit_cwd.clone();
                // Fail CLOSED if the unit's directory cannot exist (permissions, bad path):
                // proceeding would spawn the agent somewhere else and fail later with a less
                // specific error, without marking this session slot failed (Copilot). The
                // single-shot fallback resolves the same cwd and reports its own spawn error.
                if let Err(e) = std::fs::create_dir_all(&cwd) {
                    let reason = format!(
                        "[wicked-core] cannot create unit workdir {} ({e}); \
                         using single-shot fallback",
                        cwd.display()
                    );
                    {
                        let mut guard = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
                        guard.entry(session_key.clone()).or_insert(None);
                    }
                    self.emit_event(CoreEvent::AcpFallback {
                        session: run_id.clone(),
                        cli_key: cli_key.clone(),
                        reason: reason.clone(),
                        fallback_kind: fallback_kind::BINARY_UNAVAILABLE.to_string(),
                    });
                    return fallback_with_warning(reason, input, emit, &self.fallback);
                }
                // Scope the worker's estate MCP server to THIS run's repo graph (FINDING-122). The
                // session is cached per (run_id, cli_key), so the repo is stable for its lifetime.
                let code_graph_db = input
                    .governance
                    .as_ref()
                    .and_then(|g| g.code_graph_db.as_deref());
                let extra_write_roots = input
                    .governance
                    .as_ref()
                    .map_or(&[][..], |g| g.extra_write_roots.as_slice());
                // Provenance for the estate MCP's `proposal.submit`, stamped from the run/unit/agent
                // that owns this session (DES-MEM-FACETED-001 follow-on) — same source fields and helper
                // as the wrapped carrier, formatted into the ACP `env` array by the spawn.
                let estate_provenance = crate::execute_wrapped::estate_provenance_env(
                    &input.run_id,
                    input.unit.ord,
                    input.unit.assigned_cli.as_deref(),
                );
                match start_acp_process_with_write_roots(
                    &acp_config,
                    &cwd,
                    code_graph_db,
                    Some(&cwd.join("tmp")),
                    extra_write_roots,
                    &[],
                    &estate_provenance,
                    &delivery,
                    seat_cli,
                    Some((run_id.as_str(), cli_key.as_str())),
                    self.operational_home.as_deref(),
                ) {
                    Ok(mut proc) => {
                        // core#396: BIND the admitted snapshot to the process it was handed to.
                        // From here on this session's turns are judged against it, not against
                        // whatever `current` resolves to later.
                        proc.skills = handed.cloned();
                        proc.no_code = wants_no_code;
                        let acp_session_id = proc.session_id.clone();
                        // A1: captured before `proc` moves into the Arc so the once-per-spawn
                        // `SandboxUnenforced` disclosure can be emitted in the `did_insert` arm.
                        let sandbox_downgrade = proc.sandbox_downgrade.clone();
                        let arc = Arc::new(Mutex::new(proc));
                        let session_handles = {
                            let proc = arc.lock().unwrap_or_else(|p| p.into_inner());
                            (Arc::clone(&proc.write_lock), Arc::clone(&proc.kill_handle))
                        };
                        let mut guard = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
                        use std::collections::hash_map::Entry;
                        let (result, did_insert) = match guard.entry(session_key.clone()) {
                            Entry::Vacant(v) => {
                                let slot = v.insert(Some(arc.clone()));
                                (slot.as_ref().unwrap().clone(), true)
                            }
                            Entry::Occupied(mut o) => {
                                let existing = o.get().as_ref().cloned();
                                match existing {
                                    Some(existing) => (existing, false),
                                    None => {
                                        o.insert(Some(Arc::clone(&arc)));
                                        (arc.clone(), true)
                                    }
                                }
                            }
                        };
                        drop(guard);
                        if did_insert {
                            self.write_reg
                                .lock()
                                .unwrap_or_else(|p| p.into_inner())
                                .insert(
                                    (run_id.clone(), cli_key.clone(), input.launch_seq),
                                    session_handles,
                                );
                            self.emit_event(CoreEvent::AcpSessionStarted {
                                session: run_id.clone(),
                                cli_key: cli_key.clone(),
                                acp_session_id,
                            });
                            // core#396: the generation this session was handed, once per spawn
                            // (the session is cached and reused across the run's turns) — the log
                            // line for the operator and the event crew consults before reaping an
                            // old generation.
                            if let (Some(s), true) = (handed, delivers) {
                                s.report(&format!("path=acp run={run_id} cli={cli_key}"));
                                self.emit_event(s.handed_event(
                                    &run_id,
                                    input.unit.ord,
                                    input.attempt,
                                    "acp",
                                    &cli_key,
                                ));
                            }
                            // A1: this seat requested the kernel WRITE-containment floor but it
                            // could not arm, so the bridge spawned uncontained. Disclose it exactly
                            // once per spawn (the ACP-path convention names the registry seat key as
                            // `cli`, like `GovernanceUnenforced`) — the WRITE-containment sibling of
                            // `GovernanceUnenforced`, never a silent gap. Suppressed when the floor
                            // armed or `os_sandbox` was OFF (`sandbox_downgrade` is then `None`).
                            if let Some((level, reason)) = &sandbox_downgrade {
                                self.emit_event(CoreEvent::SandboxUnenforced {
                                    session: run_id.clone(),
                                    ord: input.unit.ord,
                                    attempt: input.attempt,
                                    cli: cli_key.clone(),
                                    level: level.clone(),
                                    reason: reason.clone(),
                                });
                            }
                        }
                        // A racing turn may have inserted its own process first (`did_insert`
                        // false): that session is the one reused, with ITS bound snapshot.
                        (result, !did_insert)
                    }
                    Err(e) => {
                        let reason = format!(
                            "[wicked-core] ACP unavailable for '{cli_key}' ({e}); \
                             using single-shot fallback"
                        );
                        {
                            let mut guard = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
                            guard.entry(session_key.clone()).or_insert(None);
                        } // release sessions lock before the blocking fallback call
                        self.emit_event(CoreEvent::AcpFallback {
                            session: run_id.clone(),
                            cli_key: cli_key.clone(),
                            reason: reason.clone(),
                            fallback_kind: fallback_kind::BINARY_UNAVAILABLE.to_string(),
                        });
                        return fallback_with_warning(reason, input, emit, &self.fallback);
                    }
                }
            }
        };

        let mut proc = proc_arc.lock().unwrap_or_else(|p| p.into_inner());
        // core#396: the snapshot THIS SESSION was opened with is the one every turn of it uses —
        // for the prompt, the read boundary, admission and generation reporting. The bridge holds
        // the plugin it loaded at `session/new`; a `current` re-resolved mid-session would describe
        // a generation this process never loaded, so a REUSED session is re-admitted against its
        // own snapshot (the run may have advanced to a unit needing a skill that generation lacks
        // — refuse it by name rather than prompt for it), the governance boundary's read roots are
        // rebuilt from the same root, and the generation is reported as the one in use.
        let bound: Option<crate::skills_snapshot::SkillsSnapshot> = if reused {
            proc.skills.clone()
        } else {
            handed.cloned()
        };
        if reused {
            // Already judged above for a session the probe found; this catches the race where a
            // concurrent turn opened the session first (`did_insert` false) — same policy
            // (`admit_turn`), the CACHED turn against ITS snapshot (`None` included: a session the
            // racing turn opened without one is refused for a skill-bearing turn here too, never
            // admitted off the ambient root — codex round 5). Idempotent.
            if let Err(e) = crate::skills_snapshot::admit_turn(
                crate::skills_snapshot::Turn::Cached(bound.clone()),
                input,
                &worker_cli,
                self.operational_home.as_deref(),
            ) {
                drop(proc);
                return crate::execute_wrapped::skills_refusal(input, &e);
            }
            if let (Some((_, _, _, _, boundary)), Some(g)) =
                (gate_ctx.as_mut(), input.governance.as_ref())
            {
                boundary.roots.read = crate::execute_wrapped::assemble_read_roots(
                    g.code_graph_db.as_deref(),
                    self.operational_home.as_deref(),
                    &g.extra_read_roots,
                    bound.as_ref().map(|s| s.root.as_path()),
                );
            }
            if let Some(s) = bound.as_ref().filter(|s| {
                !matches!(
                    s.delivery(&worker_cli),
                    crate::skills_snapshot::SkillsDelivery::None
                )
            }) {
                s.report(&format!(
                    "path=acp run={run_id} cli={cli_key} unit={} reused=true",
                    input.unit.ord
                ));
            }
        }
        let prompt = unit_prompt(input, skill_form, bound.as_ref());

        // A statically-admitted seat (`gate_ctx.is_some()`) can still fail its per-process
        // version pin (`AcpProcess::governance_verified`, computed once at spawn — DES-INPUT-
        // GOV-006 §3.4): an auto-updating distribution (opencode's Homebrew tap has no lockfile)
        // may have changed underneath the pinned proof between admission and this spawn. Downgrade
        // to disclosed-ungoverned for THIS process rather than trust a registry claim this
        // specific binary did not prove — and say so on the audit wire, the same way an
        // unadmitted-but-configured seat already discloses (`acp_ungoverned_event`), so this
        // never silently reads as "governed" when it is not.
        if gate_ctx.is_some() && !proc.governance_verified {
            self.emit_event(CoreEvent::GovernanceUnenforced {
                session: run_id.clone(),
                ord: input.unit.ord,
                attempt: input.attempt,
                cli: cli_key.clone(),
                reason: format!(
                    "unit is governed and '{cli_key}' is admitted to input governance, but the \
                     resolved ACP binary did not match its pinned verified_version at spawn \
                     time; this session is treated as unadmitted (answered by allow_result, \
                     unchecked) until a fresh admission re-proves the currently-installed build"
                ),
            });
        }
        let gate = gate_ctx.as_ref().filter(|_| proc.governance_verified).map(
            |(scope, phase, decisions_path, db, boundary)| {
                crate::acp_permission::AcpGate {
                    scope,
                    phase,
                    phase_alias: None,
                    db: Some(db.as_str()),
                    decisions_path,
                    // Clone rather than borrow: BoundaryCtx owns its PathBufs and the gate is
                    // rebuilt per turn; the roots are a handful of paths.
                    boundary: Some(crate::gate_hook::BoundaryCtx {
                        roots: crate::path_policy::AllowedRoots {
                            write: boundary.roots.write.clone(),
                            read: boundary.roots.read.clone(),
                        },
                        cwd: boundary.cwd.clone(),
                        home: boundary.home.clone(),
                        claude_config_dir: boundary.claude_config_dir.clone(),
                        pre_build_scope: boundary.pre_build_scope,
                        no_code_scope: boundary.no_code_scope,
                    }),
                }
            },
        );
        let turn = exec_turn_acp(
            &mut proc,
            &prompt,
            prior_outputs,
            emit,
            self.timeout,
            Arc::clone(&self.elicitation_maps),
            &run_id,
            input.elicitation_epoch,
            &self.tx,
            gate.as_ref(),
        );
        let superseded = {
            let maps = self
                .elicitation_maps
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            maps.shutdown_flag()
                || maps.is_epoch_cancelled(&run_id, input.elicitation_epoch)
                || maps.current_launch_seq(&run_id) > input.launch_seq
        };
        if superseded {
            let (output, usage, files) = match turn {
                Ok(result) => (result.output, result.usage, result.files),
                Err(error) => (
                    format!("[wicked-core] superseded ACP turn stopped: {error}"),
                    None,
                    Vec::new(),
                ),
            };
            drop(proc);
            return StepOutput {
                run_id: input.run_id.clone(),
                unit_ix: input.unit_ix,
                attempt: input.attempt,
                output,
                status: StepStatus::ElicitationFailed,
                usage,
                files,
                tools: Vec::new(),
                governed: gate.is_some(),
            };
        }

        match turn {
            Ok(result) if result.status == StepStatus::Ok => {
                if wants_no_code {
                    // F-036 QUIESCE (adversarial review on #414): a NO-CODE unit's process — the
                    // bridge, the CLI it wraps and anything either backgrounded — dies with the
                    // unit, group and all, BEFORE this returns and the worker thread takes the
                    // guard's final snapshot. Never reused anyway (the next phase either writes,
                    // or is another no-code phase that opens its own).
                    drop(proc);
                    self.drop_session_key(&session_key);
                    drop(proc_arc);
                }
                StepOutput {
                    run_id: input.run_id.clone(),
                    unit_ix: input.unit_ix,
                    attempt: input.attempt,
                    output: result.output,
                    status: StepStatus::Ok,
                    usage: result.usage,
                    files: result.files,
                    tools: result.tools,
                    governed: gate.is_some(),
                }
            }
            Ok(result) if matches!(result.status, StepStatus::Cancelled | StepStatus::TimedOut) => {
                // Turn ceiling (TimedOut) or an external cancel — drop the session either way:
                // the reader thread may wedge on a full pipe if we leave the ACP process running
                // while no longer consuming its output. The status is FORWARDED, not collapsed —
                // the fold's consumers separate the engine's own timeout from an operator cancel.
                drop(proc);
                self.drop_session(&run_id);
                StepOutput {
                    run_id: input.run_id.clone(),
                    unit_ix: input.unit_ix,
                    attempt: input.attempt,
                    output: result.output,
                    status: result.status,
                    usage: result.usage,
                    files: result.files,
                    tools: result.tools,
                    governed: gate.is_some(),
                }
            }
            Ok(result) if result.status == StepStatus::ElicitationFailed => {
                // Elicitation terminal — non-retriable; drop the session so a hung adapter
                // does not pin the slot. The actor routes ElicitationFailed directly to the
                // run-terminal path (spec I-7), bypassing FailureTriageReady/Retry.
                drop(proc);
                self.drop_session(&run_id);
                StepOutput {
                    run_id: input.run_id.clone(),
                    unit_ix: input.unit_ix,
                    attempt: input.attempt,
                    output: result.output,
                    status: StepStatus::ElicitationFailed,
                    usage: result.usage,
                    files: result.files,
                    tools: result.tools,
                    governed: gate.is_some(),
                }
            }
            Ok(_) => {
                // The tail is the only account of WHY the bridge died — surface it in the
                // fallback reason and the daemon log, or the death is invisible to operators
                // (crew#267: a live seat death left a 619-line daemon log with zero errors).
                let stderr_note = death_context(&proc);
                drop(proc);
                self.drop_session(&run_id);
                let reason = format!(
                    "[wicked-core] ACP session exited for '{cli_key}'; \
                     using single-shot fallback{stderr_note}"
                );
                eprintln!("{reason}");
                self.emit_event(CoreEvent::AcpFallback {
                    session: run_id.clone(),
                    cli_key: cli_key.clone(),
                    reason: reason.clone(),
                    fallback_kind: fallback_kind::SESSION_DIED.to_string(),
                });
                fallback_with_warning(reason, input, emit, &self.fallback)
            }
            Err(e) => {
                let stderr_note = death_context(&proc);
                drop(proc);
                self.drop_session(&run_id);
                // crew#267: an auth refusal is NOT a session death — name it, so the operator's
                // fix (restore worker auth) is visible instead of a generic bridge post-mortem.
                let auth_required = is_auth_required_error(&e);
                let (reason, kind) = if auth_required {
                    let home_hint = worker_config_home()
                        .map(|d| d.display().to_string())
                        .unwrap_or_else(|_| "~/.wicked-worker/claude".to_string());
                    (
                        format!(
                            "[wicked-core] ACP worker for '{cli_key}' is NOT AUTHENTICATED \
                             (crew#267). One-time fix: run \
                             `CLAUDE_CONFIG_DIR=\"{home_hint}\" claude login` yourself, then \
                             every worker stays logged in. Using single-shot fallback meanwhile — \
                             it runs under the SAME worker home, so it needs the same sign-in"
                        ),
                        fallback_kind::AUTH_REQUIRED,
                    )
                } else {
                    (
                        format!(
                            "[wicked-core] ACP error for '{cli_key}' ({e}); \
                             using single-shot fallback{stderr_note}"
                        ),
                        fallback_kind::SESSION_DIED,
                    )
                };
                eprintln!("{reason}");
                self.emit_event(CoreEvent::AcpFallback {
                    session: run_id.clone(),
                    cli_key: cli_key.clone(),
                    reason: reason.clone(),
                    fallback_kind: kind.to_string(),
                });
                fallback_with_warning(reason, input, emit, &self.fallback)
            }
        }
    }
}

impl Default for AcpStepRunner {
    fn default() -> Self {
        let (tx, _rx) = std::sync::mpsc::channel();
        Self::new(tx)
    }
}

impl StepRunner for AcpStepRunner {
    fn queue_operator_message(
        &self,
        run_id: &str,
        target: &crate::command::InjectTarget,
        message: &str,
    ) -> bool {
        let mut guard = self
            .pending_injects
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        guard
            .entry(run_id.to_string())
            .or_default()
            .push((target.clone(), message.to_string()));
        true
    }

    fn run_unit(&self, input: &StepInput) -> StepOutput {
        let noop = |_: &str| {};
        self.exec_turn(input, &noop)
    }

    fn run_unit_streaming(&self, input: &StepInput, emit: &DeltaSink) -> StepOutput {
        self.exec_turn(input, emit)
    }

    /// Close all ACP sessions for `run_id` so Claude processes don't leak after a run ends.
    ///
    /// Runs cleanup on a background thread — `on_run_complete` is called from the actor thread
    /// (via `finalize_run`/`fail_run`/`cancel_run`). Dropping `AcpProcess` calls `kill()` +
    /// `wait()` on the child process, which blocks. Doing that on the actor thread would stall
    /// the entire actor while waiting for the subprocess to exit.
    fn on_run_complete(&self, run_id: &str) {
        // crew#277: in-flight WRAPPED workers (the fallback path every non-ACP CLI takes) must
        // die with the run too — a canceled run's hung `copilot -p` survived ~90 minutes because
        // only ACP sessions had kill handles.
        self.fallback.cancel_run_workers(run_id);
        // Defensive cancel: shared_run_terminal does the primary cancel_epoch before calling
        // on_run_complete, but if it was skipped (e.g. non-ACP path or future code path),
        // this ensures no in-flight elicitations are left dangling. Guarded by has_active_run
        // so PTY runs and tool_cmd units (epoch 0) never insert a stale tombstone.
        {
            let mut maps = self
                .elicitation_maps
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if maps.has_active_run(run_id) {
                let epoch = maps.current_epoch(run_id);
                maps.cancel_epoch(run_id, epoch);
            }
        }
        let sessions = self.sessions.clone();
        let pending_injects = self.pending_injects.clone();
        let write_reg = self.write_reg.clone();
        let run_id = run_id.to_string();
        std::thread::spawn(move || {
            let mut guard = sessions.lock().unwrap_or_else(|p| p.into_inner());
            guard.retain(|(rid, _), _| *rid != run_id);
            drop(guard);
            write_reg
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .retain(|(rid, _, _), _| *rid != run_id);
            let mut injects = pending_injects.lock().unwrap_or_else(|p| p.into_inner());
            injects.remove(&run_id);
        });
    }

    /// ACP can fall back to a one-shot wrapped CLI when its bridge is unavailable. That child has
    /// no ACP kill handle, so forward ReassignUnit's exact old-turn identity to the fallback.
    fn cancel_reassigned_worker(&self, run_id: &str, epoch: u64, launch_seq: u64) {
        self.fallback
            .cancel_reassigned_worker(run_id, epoch, launch_seq);
    }

    /// Close a single ACP session for `(run_id, cli_key)` — called by `ReassignUnit` before
    /// re-dispatching to a different CLI. Registry/session removal is synchronous so the
    /// replacement cannot race with cleanup; kill/wait remains on a background thread.
    fn close_cli_session(&self, run_id: &str, cli_key: &str) {
        let run_id = run_id.to_string();
        let cli_key = cli_key.to_string();
        let kill_handles: Vec<Arc<KillHandle>> = {
            let mut registry = self.write_reg.lock().unwrap_or_else(|p| p.into_inner());
            let handles = registry
                .iter()
                .filter(|((rid, key, _), _)| rid == &run_id && key == &cli_key)
                .map(|(_, (_, kill))| Arc::clone(kill))
                .collect();
            registry.retain(|(rid, key, _), _| rid != &run_id || key != &cli_key);
            handles
        };
        let removed = self
            .sessions
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&(run_id, cli_key));
        std::thread::spawn(move || {
            for kill in kill_handles {
                kill.signal();
            }
            drop(removed);
        });
    }
}

// ── Registry helper ───────────────────────────────────────────────────────────

/// The merged-registry record for `cli_key` (built-ins + user overlay). Deliberately
/// NOT `registry_roster()`: that filters to `enabled_for_council` seats (a seat disabled
/// for voting can still execute units over ACP) and swallows load errors. A malformed
/// overlay falls back to built-ins instead of stripping every ACP config.
fn registry_record(cli_key: &str) -> Option<wicked_council::AgenticCli> {
    let user = wicked_council::registry::default_user_path();
    wicked_council::registry::load(user.as_deref())
        .unwrap_or_else(|_| wicked_council::registry::builtin())
        .into_iter()
        .find(|c| c.key == cli_key)
}

/// The seat identity the ACP carrier judges for ONE launch of `cli_key` (core#396), as the
/// [`WorkerCli`](crate::skills_snapshot::WorkerCli) the skills admission and the Claude-only
/// handshake key off: the MERGED registry record read by key — the operator's `clis.toml`
/// overriding a built-in wholesale — whose `binary` decides whether this is a claude seat
/// (`binary_is_claude`, the same test the wrapped runner applies to its template) and whose
/// `[cli.acp]` bridge is the CARRIER this path spawns (v3.2): a bridge that is a separate program
/// (`pi-acp`, `codex-acp`) forwards no CLI flags and so has no skills lever even where the CLI
/// itself does; a bridge that IS the CLI (`copilot --acp`, `opencode acp`) keeps it. An
/// unregistered key is its own binary, exactly as `resolve_invocation` treats it.
///
/// ONE function for the runner (`exec_turn_inner`) and for seat selection
/// (`distribute::seat_is_claude`, #402 review pass 2): routing eligibility is read off the same
/// resolution this carrier will execute — never off the launch roster's own record, which the
/// registry may override — so a seat that passes routing as claude cannot execute here as
/// anything else.
pub(crate) fn acp_seat_identity(cli_key: &str) -> crate::skills_snapshot::WorkerCli {
    seat_identity_of(registry_record(cli_key).as_ref(), cli_key)
}

/// [`acp_seat_identity`] on an already-read registry record (`None` ⇒ unregistered: the key is
/// its own binary) — the runner reads the record once per turn for the transport too, and judges
/// the identity off THAT record through this function, so it and the routing cannot diverge.
pub(crate) fn seat_identity_of(
    seat: Option<&wicked_council::AgenticCli>,
    cli_key: &str,
) -> crate::skills_snapshot::WorkerCli {
    let seat_binary = seat.map_or(cli_key, |c| c.binary.as_str()).to_string();
    let carrier_binary = seat
        .and_then(|c| c.acp.as_ref())
        .map_or(seat_binary.clone(), |a| a.binary.clone());
    crate::skills_snapshot::WorkerCli::for_binaries(&seat_binary, &carrier_binary, cli_key)
}

/// ONE registry read for ONE ACP launch: the seat's `[cli.acp]` transport config AND whether the
/// seat is claude (`seat_identity_of` on the SAME record), or `None` when the seat has no ACP
/// config. The MERGED registry, not `builtin()`: a user record replaces its built-in wholesale,
/// so its `[cli.acp]` table (or its absence) must decide the transport here exactly as it does
/// everywhere else — and its `binary` decides the identity off that same record, never a second,
/// independent read that a concurrent `clis.toml` edit could make disagree (codex r2, PR#413).
fn acp_launch_facts(cli_key: &str) -> Option<(AcpConfig, wicked_apps_core::spawn::SeatCli)> {
    let record = registry_record(cli_key);
    let config = record.as_ref().and_then(|c| c.acp.clone())?;
    Some((config, seat_cli_of(record.as_ref(), cli_key)))
}

/// The CLI a seat RUNS, for the per-seat configuration decision (core#410) — judged off the SAME
/// record as [`seat_identity_of`]: the record's `binary` (the CLI, never its `[cli.acp]` bridge —
/// `pi-acp` carries pi, `codex-acp` carries codex), the key itself for an unregistered seat. Its
/// claude arm is exactly `seat_identity_of`'s (`binary_is_claude` IS `SeatCli::from_binary ==
/// Claude`), so the skills admission and the configuration root can never disagree on a seat.
pub(crate) fn seat_cli_of(
    seat: Option<&wicked_council::AgenticCli>,
    cli_key: &str,
) -> wicked_apps_core::spawn::SeatCli {
    wicked_apps_core::spawn::SeatCli::from_binary(seat.map_or(cli_key, |c| c.binary.as_str()))
}

/// Make the wire-visible disclosure for a governed unit using an ACP adapter that has not passed
/// admission. Returning `None` for an empty key keeps the audit vocabulary actionable.
///
/// Callers must have already established that `cli_key` actually HAS an ACP config: the wording
/// below claims the unit's tool calls are "answered by `allow_result`", which is only true for a
/// seat that takes the ACP session path at all. Guard with [`acp_unadmitted_but_configured`] at
/// the `gate_ctx` call site.
fn acp_ungoverned_event(input: &StepInput, cli_key: &str) -> Option<CoreEvent> {
    (!cli_key.is_empty()).then(|| CoreEvent::GovernanceUnenforced {
        session: input.run_id.clone(),
        ord: input.unit.ord,
        attempt: input.attempt,
        cli: cli_key.to_string(),
        reason: format!(
            "unit is governed but the ACP adapter for '{cli_key}' is not admitted to input \
             governance (acp_input_governance=false); its tool calls are answered by \
             allow_result, unchecked"
        ),
    })
}

/// The `gate_ctx` disclosure gate: true only when the seat's resolved ACP config exists but its
/// capability is off. A `None` config means the seat has no `[cli.acp]` at all — it never enters
/// the ACP session path (`acp_config_for` returning `None` a few lines below `gate_ctx` sends it
/// straight to `self.fallback.run_unit_streaming`, which answers tool calls through the
/// wrapped-CLI gate-hook, or its own independent `GovernanceUnenforced`, never `allow_result`).
/// Disclosing the ACP-unadmitted reason for such a seat would be false: it never touched the path
/// that reason describes. Takes the already-resolved config rather than re-reading the (disk-
/// backed) merged registry, which also keeps this predicate pure and independent of ambient
/// registry state for testing.
fn acp_unadmitted_but_configured(acp_cfg: Option<&AcpConfig>) -> bool {
    matches!(acp_cfg, Some(cfg) if !cfg.acp_input_governance)
}

#[cfg(test)]
mod tests {
    use super::{
        is_transient_cli_failure, is_worker_originated_failure, should_retry_worker,
        MAX_TRANSIENT_RETRIES,
    };
    use crate::workflow::StepStatus;

    // Guards process-global env (WICKED_WORKER_HOME, HOME, WICKED_SKILLS_SNAPSHOT) — cargo runs
    // tests in one process, in parallel. The CRATE-WIDE lock (`crate::test_env`), shared with
    // execute_wrapped's, actor's, validator's and gate_hook's env-mutating tests: tests that
    // MUTATE a variable hold `write()`; tests that drive a REAL `start_acp_process` hold `read()`
    // across the start, because the spawn resolves the ambient variables mid-call
    // (`ensure_worker_config_home`, the skills ladder). Without the read side, a start landing
    // inside a mutator's window resolves the MUTATOR's fixture home — the symlink-refusal
    // fixture, in the flake that motivated this (core#285) — and trips the FINDING-061 guard.
    // Lock order everywhere: ENV_LOCK before REAL_STARTS.
    use crate::test_env::ENV_LOCK;

    /// The spawn and worker-home entry points with NO operational state home (codex round 8 —
    /// these tests fence nothing but the defaults and the handed snapshot's home); shadow the glob
    /// imports. The operational-home cases live in `execute_wrapped::tests` against the fence
    /// builders themselves. `#[cfg(unix)]`: its only callers drive shell-script stubs (Unix-only),
    /// so on Windows it would be dead code under `-D warnings` (the round-9 windows job failed
    /// exactly here).
    #[cfg(unix)]
    fn start_acp_process(
        config: &AcpConfig,
        cwd: &std::path::Path,
        code_graph_db: Option<&str>,
        scratch_tmp: Option<&std::path::Path>,
    ) -> anyhow::Result<AcpProcess> {
        // The stubs below stand in for the CLAUDE bridge unless a test says otherwise.
        super::start_acp_process(
            config,
            cwd,
            code_graph_db,
            scratch_tmp,
            wicked_apps_core::spawn::SeatCli::Claude,
            None,
        )
    }
    /// [`start_acp_process`] for a stub standing in for a NON-claude bridge of an UNKNOWN CLI
    /// (no configuration-home variable of its own — every seat variable stripped).
    #[cfg(unix)]
    fn start_non_claude_acp_process(
        config: &AcpConfig,
        cwd: &std::path::Path,
    ) -> anyhow::Result<AcpProcess> {
        super::start_acp_process(
            config,
            cwd,
            None,
            None,
            wicked_apps_core::spawn::SeatCli::Other,
            None,
        )
    }
    /// `#[cfg(unix)]`: its only callers drive shell-script stubs (Unix-only), so on Windows it
    /// would be dead code under `-D warnings`.
    #[cfg(unix)]
    #[allow(clippy::too_many_arguments)]
    fn start_acp_process_with_write_roots(
        config: &AcpConfig,
        cwd: &std::path::Path,
        code_graph_db: Option<&str>,
        scratch_tmp: Option<&std::path::Path>,
        extra_write_roots: &[String],
        estate_provenance: &[(String, String)],
        delivery: &crate::skills_snapshot::SkillsDelivery,
        session: Option<(&str, &str)>,
    ) -> anyhow::Result<AcpProcess> {
        super::start_acp_process_with_write_roots(
            config,
            cwd,
            code_graph_db,
            scratch_tmp,
            extra_write_roots,
            &[],
            estate_provenance,
            delivery,
            wicked_apps_core::spawn::SeatCli::Claude,
            session,
            None,
        )
    }
    fn ensure_worker_config_home() -> anyhow::Result<std::path::PathBuf> {
        super::ensure_worker_config_home(None)
    }
    fn worker_claude_config_dir(inherit: bool) -> Option<anyhow::Result<std::path::PathBuf>> {
        super::worker_claude_config_dir(inherit, None)
    }

    /// A fresh base dir for a worker-home fixture. Keyed by test name + pid + a process-wide
    /// counter — NEVER by `ThreadId` (core#285): the harness pools test threads, so a
    /// ThreadId-keyed name repeats across tests (and across processes, after a killed run
    /// strands its dir in the temp root), letting one test inherit another's poisoned fixture.
    /// Same idiom as `scratch(name)`; the counter keeps repeated mints inside one test
    /// disjoint. Callers remove the dir at test end (best-effort); the pre-clean here also
    /// sweeps any same-named leftover from a crashed earlier run.
    fn worker_home_base(name: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "wworker-{name}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// Fixture cleanup for tests that re-aim `WICKED_WORKER_HOME` at their own scratch base:
    /// RESTORE the pre-main hermetic arming (core#311-class) instead of `remove_var`. An unset
    /// window would hand the next real start in this binary the DEFAULT resolution — the
    /// operator's REAL `~/.wicked-worker/claude`, which `ensure_worker_config_home` REWRITES
    /// (settings.json) and SANITIZES (deletes hooks/, plugins/, …) on every start. Callers hold
    /// `ENV_LOCK` write, same as the mutation they are cleaning up after.
    fn restore_hermetic_worker_home() {
        std::env::set_var(
            wicked_apps_core::spawn::WORKER_HOME_ENV,
            wicked_apps_core::spawn::hermetic_test_worker_home(),
        );
    }

    // ── FINDING #5: transient single-shot failures are retried; deterministic ones are not ────────

    /// crew#267 — the bridge's auth refusal is classified by CODE (downcast), never by display
    /// text, and only -32000 qualifies: a generic io error or another RPC code stays a session
    /// death (fallback still fires either way; only the NAME changes).
    #[test]
    fn an_auth_refusal_is_classified_by_code_not_text() {
        let auth = anyhow::Error::new(RpcServerError {
            code: Some(AUTH_REQUIRED_CODE),
            raw: "{\"code\":-32000,\"message\":\"Authentication required\"}".into(),
        });
        assert!(is_auth_required_error(&auth));

        let other_code = anyhow::Error::new(RpcServerError {
            code: Some(-32603),
            raw: "internal".into(),
        });
        assert!(!is_auth_required_error(&other_code));

        // Text that MENTIONS auth without the code must not match — the classification is a
        // protocol fact, not a grep.
        let text_only = anyhow::anyhow!("Authentication required (but plain text)");
        assert!(!is_auth_required_error(&text_only));
    }

    #[test]
    fn transient_cli_failures_are_recognized_and_deterministic_ones_are_not() {
        // The wrapped runner's nonzero-exit + could-not-run messages, and network signatures.
        for t in [
            "(cli `claude` exited 1) Connection closed mid-response",
            "(cli `claude` exited 143)",
            "(could not run `claude`: No such file or directory)",
            "stream error: the server reset the connection",
            "Error: overloaded_error (503)",
            "rate limit exceeded",
        ] {
            assert!(is_transient_cli_failure(t), "should be transient: {t:?}");
        }
        // A plain evaluator-style rejection with no infrastructural marker is not retried.
        assert!(!is_transient_cli_failure(
            "the requirement claim is content-free"
        ));
    }

    /// core#297 — the FINDING-101 missing-deliverable case is kept out of BOTH classifiers
    /// STRUCTURALLY, not by the substring exclusion they used to carry. A missing deliverable is a
    /// deterministic incompleteness: neither an in-runner retry nor a different seat can conjure
    /// the artifact, so it must reach neither ladder. The floor now rejects the unit directly at
    /// the fold (`actor::apply_step_result`) and no runner ever produces a failed `StepOutput` for
    /// it, so there is nothing for these classifiers to misread — which also closes the hole where
    /// a WORKER printing that sentence into its own transcript could reclassify its own failure.
    #[test]
    fn the_deliverable_floor_never_reaches_the_retry_or_failover_classifiers() {
        // No runner constructs a missing-deliverable failure any more — the audit that keeps it
        // that way lives in `actor::deliverable_floor_tests`. Here: prove this file holds no
        // deliverable-shaped special case, so nobody re-adds one instead of keeping the floor at
        // the fold. Both needles are built by CONCATENATION so this test's own text cannot satisfy
        // the search — which is what lets the scan cover the WHOLE file rather than a
        // `#[cfg(test)]`-truncated prefix (this file interleaves test modules with production
        // code, so a prefix scan would leave thousands of production lines unread).
        let needle = format!("did not produce its {}", "declared deliverable");
        assert!(
            !include_str!("acp_runner.rs").contains(needle.as_str()),
            "a substring carve-out for the deliverable floor is back in acp_runner — the floor \
             belongs at the fold, where it is a STATUS, not a sentence to grep out of \
             worker-controlled output"
        );
        // And the classifiers judge the INFRASTRUCTURAL shape only: prose that merely mentions a
        // deliverable is neither transient nor worker-originated, with or without a carve-out.
        let prose = format!("unit u3 reported done but {needle}(s): rg.json");
        assert!(!is_transient_cli_failure(&prose));
        assert!(!is_worker_originated_failure(&prose));
    }

    /// core#282 — the failover ladder's classifier. Timeouts are WORKER-originated (the seat
    /// proved it cannot finish → move to the NEXT seat) but deliberately NOT transient (a
    /// same-seat retry would burn another full unit budget); every transient shape is also
    /// worker-originated; judged/deterministic failures are neither.
    #[test]
    fn timeouts_are_worker_originated_but_not_transient() {
        for t in [
            "(cli `agy` exceeded the timeout and was killed)",
            "ACP timeout waiting for response id=42",
            "the bridge request timed out after 120s",
        ] {
            assert!(
                is_worker_originated_failure(t),
                "a timeout is a seat-health signal the failover ladder must act on: {t:?}"
            );
            assert!(
                !is_transient_cli_failure(t),
                "a timeout must never earn a same-seat in-runner retry: {t:?}"
            );
        }
        // Every transient shape (exit-nonzero, spawn failure, network) is also worker-originated.
        for t in [
            "(cli `claude` exited 1) connection reset",
            "(could not run `claude`: No such file or directory)",
        ] {
            assert!(is_worker_originated_failure(t), "transient ⊂ worker: {t:?}");
        }
        // A judged, work-level rejection has no worker signature at all.
        assert!(!is_worker_originated_failure(
            "the requirement claim is content-free"
        ));
    }

    #[test]
    fn should_retry_only_a_transient_failure_and_only_within_the_bound() {
        let transient = "(cli `claude` exited 1) connection reset";
        // Retry a GOVERNED transient FAILED while retries remain…
        assert!(should_retry_worker(true, StepStatus::Failed, transient, 0));
        assert!(should_retry_worker(
            true,
            StepStatus::Failed,
            transient,
            MAX_TRANSIENT_RETRIES - 1
        ));
        // …but STOP at the bound (no unbounded retry — a persistent transient still fails closed).
        assert!(!should_retry_worker(
            true,
            StepStatus::Failed,
            transient,
            MAX_TRANSIENT_RETRIES
        ));
        // NEVER retry an UNGOVERNED unit — the idempotency/infra argument is governed-only (an
        // engine-internal judge/validator claude call must not be silently re-run).
        assert!(!should_retry_worker(
            false,
            StepStatus::Failed,
            transient,
            0
        ));
        // Never retry a success, an external cancel, our own turn ceiling, or a non-transient failure.
        assert!(!should_retry_worker(true, StepStatus::Ok, transient, 0));
        assert!(!should_retry_worker(
            true,
            StepStatus::Cancelled,
            transient,
            0
        ));
        assert!(!should_retry_worker(
            true,
            StepStatus::TimedOut,
            transient,
            0
        ));
        assert!(!should_retry_worker(
            true,
            StepStatus::Failed,
            "the requirement claim is content-free",
            0
        ));
    }

    /// crew#267 option 3 — the worker home is STABLE across spawns (one login persists), and
    /// every spawn RE-SANITIZES the executable-config vectors while PRESERVING login state.
    #[test]
    fn the_worker_home_is_stable_sanitized_per_spawn_and_preserves_login_state() {
        let _g = ENV_LOCK.write().unwrap_or_else(|p| p.into_inner());
        let base = worker_home_base("stable");
        std::env::set_var("WICKED_WORKER_HOME", &base);

        let a = ensure_worker_config_home().expect("first ensure");
        let b = ensure_worker_config_home().expect("second ensure");
        assert_eq!(
            a, b,
            "one persistent home, not per-spawn dirs — the login must stick"
        );

        // A prior worker's mutations: rogue settings, a hooks dir, a local-settings file…
        std::fs::create_dir_all(a.join("hooks")).unwrap();
        std::fs::write(a.join("hooks/evil.sh"), "#!/bin/sh\n").unwrap();
        std::fs::write(a.join("settings.local.json"), "{}").unwrap();
        std::fs::write(
            a.join("settings.json"),
            "{\"permissions\":{\"allow\":[\"*\"]}}",
        )
        .unwrap();
        // …and the operator's login state, which must survive.
        std::fs::write(a.join(".credentials.json"), "{\"token\":\"keep-me\"}").unwrap();
        std::fs::write(a.join(".claude.json"), "{\"oauthAccount\":{}}").unwrap();

        let c = ensure_worker_config_home().expect("re-ensure sanitizes");
        assert_eq!(c, a);
        assert!(
            !a.join("hooks").exists(),
            "hooks/ must be re-sanitized away"
        );
        assert!(
            !a.join("settings.local.json").exists(),
            "settings.local.json must be re-sanitized away"
        );
        let settings: Value =
            serde_json::from_slice(&std::fs::read(a.join("settings.json")).unwrap()).unwrap();
        assert!(
            settings["permissions"]["allow"].is_null(),
            "a worker-written settings.json must be OVERWRITTEN with the fence"
        );
        assert_eq!(
            std::fs::read_to_string(a.join(".credentials.json")).unwrap(),
            "{\"token\":\"keep-me\"}",
            "login state must persist across spawns — that is the point of option 3"
        );
        assert!(a.join(".claude.json").exists());

        restore_hermetic_worker_home();
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A prior worker planting SYMLINKS inside the home (hooks -> victim-dir,
    /// settings.json -> victim-file) must have the LINKS removed — never the targets touched,
    /// and never a write through the link (Copilot, PR#277).
    #[cfg(unix)]
    #[test]
    fn sanitize_removes_planted_symlinks_without_following_them() {
        let _g = ENV_LOCK.write().unwrap_or_else(|p| p.into_inner());
        let base = worker_home_base("plant");
        std::env::set_var("WICKED_WORKER_HOME", &base);
        let home = ensure_worker_config_home().expect("first ensure");

        // The victims a malicious worker would aim at.
        let victim_dir = base.join("victim-dir");
        std::fs::create_dir_all(&victim_dir).unwrap();
        std::fs::write(victim_dir.join("precious.txt"), "keep").unwrap();
        let victim_file = base.join("victim.json");
        std::fs::write(&victim_file, "{\"untouched\":true}").unwrap();

        // The plants.
        std::os::unix::fs::symlink(&victim_dir, home.join("hooks")).unwrap();
        std::fs::remove_file(home.join("settings.json")).unwrap();
        std::os::unix::fs::symlink(&victim_file, home.join("settings.json")).unwrap();

        ensure_worker_config_home().expect("re-ensure sanitizes the plants");

        assert!(
            victim_dir.join("precious.txt").exists(),
            "sanitize must remove the LINK, never the target directory's contents"
        );
        assert_eq!(
            std::fs::read_to_string(&victim_file).unwrap(),
            "{\"untouched\":true}",
            "the settings re-write must never travel through a planted link"
        );
        assert!(
            std::fs::symlink_metadata(home.join("hooks")).is_err(),
            "the planted hooks link itself must be gone"
        );
        let settings: Value =
            serde_json::from_slice(&std::fs::read(home.join("settings.json")).unwrap()).unwrap();
        assert!(settings["permissions"]["deny"].is_array());

        restore_hermetic_worker_home();
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A symlink planted at the home (or its parent) re-aims every CLI write — refused.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_worker_home_is_refused() {
        let _g = ENV_LOCK.write().unwrap_or_else(|p| p.into_inner());
        let base = worker_home_base("ln");
        std::fs::create_dir_all(&base).unwrap();
        let target = base.join("elsewhere");
        std::fs::create_dir_all(&target).unwrap();
        std::os::unix::fs::symlink(&target, base.join("claude")).unwrap();
        std::env::set_var("WICKED_WORKER_HOME", &base);
        let err = ensure_worker_config_home().expect_err("symlinked home must be refused");
        assert!(err.to_string().contains("symlink"), "{err}");
        restore_hermetic_worker_home();
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A RELATIVE `WICKED_WORKER_HOME` is refused by the ACP path before any spawn — at the shared
    /// resolver, so the ballot path and the sign-in command refuse the same value the same way
    /// (codex, PR#413: a relative dir would otherwise be resolved against three different working
    /// directories). The empty spelling is covered at the pure resolver
    /// (`wicked_apps_core::spawn::tests`): Windows deletes a variable set to "" so it cannot be
    /// pinned through the environment.
    #[test]
    fn a_relative_worker_home_is_refused_before_any_spawn() {
        let _g = ENV_LOCK.write().unwrap_or_else(|p| p.into_inner());
        for bad in ["relative/worker", "worker", "./worker"] {
            std::env::set_var("WICKED_WORKER_HOME", bad);
            let err = ensure_worker_config_home().expect_err(bad);
            assert!(err.to_string().contains("absolute"), "{bad}: {err}");
            // The spawn's own decision (no hatch) is the same refusal, so `start_acp_process`
            // fails closed before spawning — never a worker on `relative/worker/claude`.
            let decision = worker_claude_config_dir(false).expect("not the hatch");
            assert!(decision.is_err(), "{bad}: the ACP spawn must refuse too");
            assert!(
                wicked_apps_core::spawn::seat_claude_config_dir()
                    .expect("not the hatch")
                    .is_err(),
                "{bad}: the ballot spawn must refuse too"
            );
        }
        restore_hermetic_worker_home();
    }

    /// codex r2, PR#413: the ACP launch's transport config and its seat identity are read off ONE
    /// registry record (`acp_launch_facts`), so an operator override that changes a seat's `binary`
    /// changes its identity in the SAME resolution that hands out its bridge — never one record's
    /// bridge paired with another's identity. Pins HOME so the override is the only `clis.toml`.
    #[test]
    fn acp_launch_facts_couple_the_bridge_and_the_identity_from_one_record() {
        let _g = ENV_LOCK.write().unwrap_or_else(|p| p.into_inner());
        let home = worker_home_base("launch-facts-home");
        let council = home.join(".config").join("wicked-council");
        std::fs::create_dir_all(&council).unwrap();
        std::fs::write(
            council.join("clis.toml"),
            r#"
[[cli]]
key = "claude"
display_name = "Not actually claude"
binary = "some-other-cli"
headless_invocation = "some-other-cli -p \"{PROMPT}\""

[cli.acp]
binary = "/opt/overridden/bridge-a"
transport = "stdio"

[[cli]]
key = "skills-seat"
display_name = "Claude under another key"
binary = "claude"
headless_invocation = "claude -p \"{PROMPT}\""

[cli.acp]
binary = "/opt/overridden/bridge-b"
transport = "stdio"
"#,
        )
        .unwrap();
        let prior_home = std::env::var_os("HOME");
        std::env::set_var("HOME", &home);
        let a = acp_launch_facts("claude");
        let b = acp_launch_facts("skills-seat");
        let none = acp_launch_facts("no-such-seat");
        match prior_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
        let (cfg_a, cli_a) = a.expect("the override has an ACP table");
        assert_eq!(
            cfg_a.binary, "/opt/overridden/bridge-a",
            "the bridge off THAT record"
        );
        assert_ne!(
            cli_a,
            wicked_apps_core::spawn::SeatCli::Claude,
            "the key says claude but THAT record's binary does not — identity follows the record"
        );
        let (cfg_b, cli_b) = b.expect("the override has an ACP table");
        assert_eq!(cfg_b.binary, "/opt/overridden/bridge-b");
        assert_eq!(
            cli_b,
            wicked_apps_core::spawn::SeatCli::Claude,
            "a claude binary under another key IS a claude seat"
        );
        assert!(none.is_none(), "no record, no launch facts");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// codex r3, PR#413: the ACP seat identity — judged off the record's `binary` through the same
    /// carrier test as the ballot and the wrapped runner — follows the OS's executable lookup: a
    /// record spelling its binary `CLAUDE.EXE` / `Claude.cmd` is a claude seat on Windows (so the
    /// bridge gets the worker home, never a stripped variable), and not one on a case-sensitive
    /// filesystem.
    #[test]
    fn a_case_variant_claude_binary_is_a_claude_seat_exactly_where_the_os_launches_it_as_one() {
        use crate::skills_snapshot::WorkerCli;
        for spelled in ["CLAUDE.EXE", "Claude.cmd", r"C:\Tools\CLAUDE.exe"] {
            let identity = WorkerCli::for_binaries(spelled, "claude-agent-acp", "claude");
            assert_eq!(
                matches!(identity, WorkerCli::Claude),
                cfg!(windows),
                "{spelled}: {identity:?}"
            );
        }
        assert!(matches!(
            WorkerCli::for_binaries("claude", "claude-agent-acp", "claude"),
            WorkerCli::Claude
        ));
    }

    /// A NON-claude bridge (codex, pi, copilot, opencode) gets no ambient claude configuration
    /// path: the daemon's own `CLAUDE_CONFIG_DIR` is stripped, not forwarded, and the engine-owned
    /// claude home is neither handed to it nor ensured on its account (codex, PR#413).
    #[test]
    #[cfg(unix)]
    fn a_non_claude_acp_worker_gets_no_ambient_claude_config_dir() {
        let _env = ENV_LOCK.write().unwrap_or_else(|p| p.into_inner());
        let _serial = REAL_STARTS.lock().unwrap_or_else(|p| p.into_inner());
        let dir = scratch("non-claude-config");
        std::env::set_var("WICKED_WORKER_HOME", &dir);
        // The daemon's own claude config dir — what a non-claude bridge must NOT see.
        let decoy = dir.join("daemon-config-dir");
        std::fs::create_dir_all(&decoy).unwrap();
        let _decoy = EnvPin::set(CLAUDE_CONFIG_DIR_ENV, &decoy);
        let ledger = dir.join("seen-config-dir.txt");
        let script = write_stub(
            &dir,
            &format!(
                r#"#!/bin/sh
printf '%s\n' "${{CLAUDE_CONFIG_DIR:-UNSET}}" > "{ledger}"
read _init
printf '%s\n' '{{"jsonrpc":"2.0","id":1,"result":{{}}}}'
read _new
printf '%s\n' '{{"jsonrpc":"2.0","id":2,"result":{{"sessionId":"nc"}}}}'
sleep 30
"#,
                ledger = ledger.display()
            ),
        );
        let proc = start_non_claude_acp_process(&stub_config(&script, None), &dir).expect("start");
        let seen = std::fs::read_to_string(&ledger).unwrap().trim().to_string();
        assert_eq!(
            seen, "UNSET",
            "a non-claude bridge must see NO claude config dir — neither the daemon's nor the \
             worker home's"
        );
        assert!(
            !dir.join("claude").exists(),
            "claude's worker home must not be ensured on a non-claude bridge's account"
        );
        drop(proc);
        restore_hermetic_worker_home();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// F-030 / F-013: ONE resolver for "the claude seat's config dir". Compared through the two
    /// spawn-facing APIs EXACTLY as the spawns call them — the ACP worker path's
    /// `worker_claude_config_dir(inherits_operator_config(), ..)` (what `start_acp_process` sets
    /// `CLAUDE_CONFIG_DIR` to) and the council ballot path's
    /// `wicked_apps_core::spawn::seat_claude_config_dir()` (what `run_in_isolation` sets), hatch
    /// included — plus the roster's claude `login_invocation` (what the studio tells the operator
    /// to sign in). All three must agree under `WICKED_WORKER_HOME`: the finding was three
    /// spellings that agreed only on the default laptop layout. (Copilot, PR#413: comparing the
    /// hatch-unaware `worker_claude_config_dir()` resolver instead would not catch the ballot API
    /// diverging from it.)
    #[test]
    fn the_worker_home_the_ballots_and_the_sign_in_command_resolve_to_one_dir() {
        let _g = ENV_LOCK.write().unwrap_or_else(|p| p.into_inner());
        let base = worker_home_base("one-resolver");
        std::env::set_var("WICKED_WORKER_HOME", &base);
        // Pin HOME so `registry_roster` reads NO developer `~/.config/wicked-council/clis.toml`
        // (which could override or disable the claude seat) — the built-in roster is under test.
        let prior_home = std::env::var_os("HOME");
        std::env::set_var("HOME", &base);

        let inherit = crate::execute_wrapped::inherits_operator_config();
        // The ACP spawn's decision, as `start_acp_process_with_write_roots` makes it.
        let worker = worker_claude_config_dir(inherit).map(|r| r.expect("worker home resolves"));
        // The ballot spawn's decision, as `run_in_isolation` makes it.
        let ballot = wicked_apps_core::spawn::seat_claude_config_dir()
            .map(|r| r.expect("seat dir resolves"));
        let claude = crate::registry_roster()
            .into_iter()
            .find(|c| c.key == "claude")
            .expect("the built-in roster seats claude");
        let login = claude
            .login_invocation
            .expect("claude has a sign-in command");
        match (&worker, &ballot) {
            (None, None) => {
                // The operator's inherit hatch: BOTH spawns run on the operator's own config
                // (one hatch, not two), so that is where the sign-in goes.
                assert!(inherit, "only the hatch may make a spawn inherit");
                assert_eq!(login, "claude");
            }
            (Some(worker), Some(ballot)) => {
                assert!(!inherit, "without the hatch every spawn sets the dir");
                assert_eq!(*worker, base.join("claude"));
                assert_eq!(
                    worker, ballot,
                    "the ACP worker and the ballot spawn disagree on the seat dir"
                );
                assert_eq!(
                    login,
                    format!("CLAUDE_CONFIG_DIR=\"{}\" claude", ballot.display()),
                    "the sign-in command must name the dir the seats actually run under"
                );
                assert!(
                    !login.contains("$HOME"),
                    "resolved, never the hard-coded default spelling (F-013): {login}"
                );
            }
            (w, b) => panic!(
                "the two spawn paths disagree on WHETHER to set the seat dir (worker={w:?}, \
                 ballot={b:?}) — the hatch is read in two places"
            ),
        }

        match prior_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
        restore_hermetic_worker_home();
        let _ = std::fs::remove_dir_all(&base);
    }

    /// core#311 (adjacent organ): with NO test-local override, a real start's home resolution must
    /// land in the pre-main-armed per-process temp base — never the operator's real
    /// `~/.wicked-worker/claude`, which `ensure_worker_config_home` REWRITES (settings.json) and
    /// SANITIZES (deletes hooks/, plugins/, …). Before the arming, every unscoped real-start test
    /// in this binary (`an_auth_requiring_agent…`, `eight_simultaneous_starts…`, the mock-ACP
    /// suite) did exactly that on every `cargo test` run. Falsified by removing the
    /// `hermetic_test_worker_home()` call from `emit::hermetic_test_spool`: the resolution below
    /// then lands under `$HOME` and the containment asserts fail.
    #[test]
    fn worker_home_resolution_lands_in_the_armed_temp_base_never_the_real_home() {
        // Read side: fixture tests above re-aim the variable under the write lock and RESTORE the
        // armed value; under the read lock we always observe the armed (or restored) state.
        let _env = ENV_LOCK.read().unwrap_or_else(|p| p.into_inner());
        let armed = wicked_apps_core::spawn::hermetic_test_worker_home();
        assert_eq!(
            std::env::var_os(wicked_apps_core::spawn::WORKER_HOME_ENV)
                .map(std::path::PathBuf::from),
            Some(armed.clone()),
            "the pre-main ctor must have armed the worker-home override (and every fixture must \
             RESTORE it, never remove it)"
        );
        let resolved = ensure_worker_config_home().expect("ensure resolves the armed base");
        assert_eq!(resolved, armed.join("claude"));
        assert!(
            resolved.starts_with(std::env::temp_dir()),
            "the armed worker home must live under the system temp dir, got {}",
            resolved.display()
        );
        if let Some(home) = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(std::path::PathBuf::from)
        {
            assert!(
                !resolved.starts_with(home.join(".wicked-worker")),
                "a test resolution must never reach the operator's real ~/.wicked-worker"
            );
        }
    }
    use super::*;
    use crate::command::InjectTarget;

    fn runner() -> AcpStepRunner {
        let (tx, _rx) = std::sync::mpsc::channel();
        AcpStepRunner::new(tx)
    }

    // ── FINDING-022: handshake budgets, start gate, stderr capture ──────────────

    #[test]
    fn the_two_handshake_calls_have_separate_budgets_and_neither_is_the_old_10s_constant() {
        // The defect was ONE constant covering two calls with different cost profiles, at a value
        // that sat inside the measured spread of the slower one. Pinning both to >10s is what
        // stops a reviewer reinstating a single shared constant at the old value.
        //
        // Asserted on the defaults, not on `initialize_budget()` / `session_new_budget()`: those
        // read the process env, and an override is a supported configuration — a host that sets
        // one must not fail a test about what the code ships with.
        assert!(parse_secs(None, INIT_DEFAULT_SECS) > Duration::from_secs(10));
        assert!(parse_secs(None, SESSION_NEW_DEFAULT_SECS) > Duration::from_secs(10));
    }

    #[test]
    fn a_budget_override_must_be_a_positive_number_or_the_default_stands() {
        // `parse_secs` runs per handshake, so a typo'd or zero override must not silently become
        // an instant timeout — that would fail EVERY handshake open and downgrade every unit to
        // ungoverned execution, which is the exact failure this whole change exists to stop.
        let default = Duration::from_secs(60);
        assert_eq!(parse_secs(None, 60), default, "unset");
        assert_eq!(parse_secs(Some("0".into()), 60), default, "zero");
        assert_eq!(parse_secs(Some("".into()), 60), default, "empty");
        assert_eq!(parse_secs(Some("ninety".into()), 60), default, "garbage");
        assert_eq!(parse_secs(Some("-5".into()), 60), default, "negative");
        assert_eq!(
            parse_secs(Some("90".into()), 60),
            Duration::from_secs(90),
            "valid"
        );
    }

    #[test]
    fn the_permit_wait_is_not_tied_to_the_budget_of_the_call_it_guards() {
        // Coupling them compounds: the wait is spent BEFORE the bridge is spawned and the waiter
        // still needs its full budget after admission, so raising the budget to fix slow
        // handshakes would also lengthen the queue in front of them.
        assert!(
            START_WAIT < parse_secs(None, SESSION_NEW_DEFAULT_SECS),
            "the permit wait must stay strictly under the budget of the call it guards"
        );
    }

    /// A gate of its own per test — exhausting the process-wide one would stall any concurrent
    /// test that starts a bridge, which is the very failure mode these tests exist to prevent.
    fn test_gate(permits: usize) -> &'static StartGate {
        Box::leak(Box::new(StartGate::new(permits)))
    }

    #[test]
    fn the_start_gate_hands_out_a_bounded_number_of_permits_and_reclaims_them_on_drop() {
        let gate = test_gate(2);
        let held: Vec<StartPermit> = (0..2)
            .map(|_| gate.acquire(Duration::from_secs(5)).expect("a free permit"))
            .collect();
        // Exhausted: the next caller waits rather than piling onto the contended handshake.
        assert!(
            gate.acquire(Duration::from_millis(50)).is_none(),
            "the gate handed out more than its 2 permits"
        );
        drop(held);
        // Reclaimed on drop — an early return or a panic mid-handshake cannot leak a permit.
        assert!(
            gate.acquire(Duration::from_millis(500)).is_some(),
            "dropping a permit did not return it to the gate"
        );
    }

    #[test]
    fn waiting_for_a_permit_times_out_rather_than_blocking_forever() {
        // The gate is a contention reducer, not a correctness barrier: if every permit is held by
        // a stuck bridge, callers must proceed anyway rather than queue behind it indefinitely.
        let gate = test_gate(1);
        let _held = gate.acquire(Duration::from_secs(5)).unwrap();
        let t0 = Instant::now();
        assert!(gate.acquire(Duration::from_millis(100)).is_none());
        assert!(
            t0.elapsed() < Duration::from_secs(2),
            "acquire() blocked past its wait bound"
        );
    }

    #[test]
    fn the_default_start_concurrency_is_bounded_and_non_zero() {
        // Zero would make every start wait out START_WAIT before proceeding contended anyway —
        // pure dead time, since the gate is not a correctness barrier and nothing is gained by
        // the wait. Unbounded would reinstate the contention that makes `session/new` outrun its
        // budget. Asserted on `parse_permits(None)` rather than `start_permits()` so a host that
        // sets the (supported) override does not fail a test about the shipped default.
        let n = parse_permits(None);
        assert!(
            n > 0 && n <= 8,
            "default start concurrency {n} is out of range"
        );
    }

    #[test]
    fn a_start_concurrency_override_must_be_a_positive_number_or_the_default_stands() {
        // Zero is the dangerous one: a gate with no permits would make every start wait out
        // START_WAIT before proceeding contended anyway — 30s of dead time per unit.
        assert_eq!(
            parse_permits(Some("0".into())),
            START_PERMITS_DEFAULT,
            "zero"
        );
        assert_eq!(
            parse_permits(Some("two".into())),
            START_PERMITS_DEFAULT,
            "garbage"
        );
        assert_eq!(parse_permits(Some("6".into())), 6, "valid");
    }

    /// Writes a stub ACP bridge: answers `initialize` and `session/new`, and brackets its own
    /// handshake window with `+` / `-` in a shared file so the test can reconstruct how many
    /// bridges were genuinely inside a handshake at the same moment.
    #[cfg(unix)]
    fn stub_bridge(dir: &std::path::Path, ledger: &std::path::Path) -> std::path::PathBuf {
        let script = dir.join("stub-acp-bridge.sh");
        std::fs::write(
            &script,
            format!(
                r#"#!/bin/sh
printf '+' >> "{ledger}"
read _init
printf '%s\n' '{{"jsonrpc":"2.0","id":1,"result":{{}}}}'
read _new
# Hold the window open so genuinely-concurrent starts overlap in the ledger.
sleep 0.4
printf '%s\n' '{{"jsonrpc":"2.0","id":2,"result":{{"sessionId":"stub"}}}}'
printf -- '-' >> "{ledger}"
sleep 30
"#,
                ledger = ledger.display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    /// [`stub_bridge`], plus one line: before the handshake, dump the named env var's value to
    /// `capture_file` (OUTSIDE the unit cwd, so this observation step itself cannot be mistaken
    /// for the thing under test — DES-INPUT-GOV-006's `acp_governance_env` injection). Proves the
    /// env var the engine sets actually reaches the child process, independent of whether
    /// anything landed in the cwd.
    #[cfg(unix)]
    fn stub_bridge_capturing_env(
        dir: &std::path::Path,
        env_var: &str,
        capture_file: &std::path::Path,
    ) -> std::path::PathBuf {
        let script = dir.join("stub-acp-bridge-capture-env.sh");
        std::fs::write(
            &script,
            format!(
                r#"#!/bin/sh
printf '%s' "${env_var}" > "{capture}"
read _init
printf '%s\n' '{{"jsonrpc":"2.0","id":1,"result":{{}}}}'
read _new
printf '%s\n' '{{"jsonrpc":"2.0","id":2,"result":{{"sessionId":"stub"}}}}'
sleep 30
"#,
                capture = capture_file.display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    #[cfg(unix)]
    #[test]
    fn acp_spawn_kernel_denies_an_outside_write_when_os_sandbox_is_enabled() {
        let _env = ENV_LOCK.read().unwrap_or_else(|p| p.into_inner());
        let base =
            std::env::temp_dir().join(format!("wicked-acp-os-sandbox-{}", std::process::id()));
        let cwd = base.join("worktree");
        let outside = base.join("outside");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        if crate::validator::detect_worker_sandbox(std::slice::from_ref(&cwd)).level
            != crate::validator::SandboxLevel::Sandboxed
        {
            let _ = std::fs::remove_dir_all(&base);
            return;
        }
        let script = base.join("sandbox-probe-acp-bridge.sh");
        std::fs::write(
            &script,
            r#"#!/bin/sh
printf x > "$WICKED_TEST_OUTSIDE"
printf '%s' "$?" > acp-outside-write-status
read _init
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{}}'
read _new
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"stub"}}'
sleep 30
"#,
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let outside_file = outside.join("pwned");
        let config = AcpConfig {
            binary: script.to_string_lossy().into_owned(),
            start_args: vec![],
            transport: AcpTransport::Stdio,
            auth_method: None,
            acp_input_governance: false,
            os_sandbox: true,
            acp_governance_env: Some((
                "WICKED_TEST_OUTSIDE".to_string(),
                outside_file.to_string_lossy().into_owned(),
            )),
            verified_version: None,
        };
        let proc = match start_acp_process(&config, &cwd, None, Some(&cwd.join("tmp"))) {
            Ok(proc) => proc,
            // A launcher may be installed but disabled by the outer CI/container sandbox. Its
            // refusal means the real child never ran; production surfaces this startup error
            // rather than silently falling back, and this kernel test skips that host honestly.
            Err(err) if err.to_string().contains("Operation not permitted") => {
                let _ = std::fs::remove_dir_all(&base);
                return;
            }
            Err(err) => panic!("the sandbox-wrapped ACP bridge must complete its handshake: {err}"),
        };
        // Non-zero exit, not literal "1": permission-denied redirect codes vary across `/bin/sh`
        // (Copilot #384). Containment is proven by the file-absence check below.
        let acp_status = std::fs::read_to_string(cwd.join("acp-outside-write-status")).unwrap();
        let acp_code = acp_status.trim();
        assert!(
            !acp_code.is_empty() && acp_code != "0",
            "the ACP child must observe a non-zero (OS-denied) exit for its outside write, got {acp_status:?}"
        );
        assert!(
            !outside_file.exists(),
            "the ACP child cannot create outside files"
        );
        drop(proc);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The task's own "prove with a test" requirement for the zero-file-write claim
    /// (DES-INPUT-GOV-006 §1, §5 condition 1): `acp_governance_env` is an environment variable,
    /// never a file drop, so a governed seat's unit cwd — the same directory a real repo's
    /// TRACKED, committed files live in — must come out of a spawn byte-for-byte identical to how
    /// it went in. Also proves the env var actually reaches the child (a test that only checked
    /// "no new file" but never confirmed injection happened would pass just as well with the
    /// injection code deleted).
    #[test]
    #[cfg(unix)]
    fn governance_env_injection_reaches_the_child_and_writes_nothing_into_the_unit_cwd() {
        let _env = ENV_LOCK.read().unwrap_or_else(|p| p.into_inner());
        let base = std::env::temp_dir().join(format!(
            "wicked-acp-governance-env-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();

        // The "tracked fixture": a unit cwd standing in for a real repo worktree, seeded with a
        // file that would be a repo-committed tracked file in the real thing.
        let fixture_cwd = base.join("fixture");
        std::fs::create_dir_all(&fixture_cwd).unwrap();
        let tracked_file = fixture_cwd.join("tracked.txt");
        std::fs::write(&tracked_file, "committed content\n").unwrap();
        let before: std::collections::BTreeSet<String> = std::fs::read_dir(&fixture_cwd)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();

        let capture_file = base.join("observed-env.txt");
        let script = stub_bridge_capturing_env(&base, "WICKED_TEST_GOVERNANCE_ENV", &capture_file);

        let config = AcpConfig {
            binary: script.to_string_lossy().to_string(),
            start_args: vec![],
            transport: AcpTransport::default(),
            auth_method: None,
            acp_input_governance: true,
            os_sandbox: false,
            acp_governance_env: Some((
                "WICKED_TEST_GOVERNANCE_ENV".into(),
                "governance-forcing-value".into(),
            )),
            verified_version: None,
        };

        let proc =
            start_acp_process(&config, &fixture_cwd, None, None).expect("stub bridge must start");

        // 1. The injection actually reached the child process.
        let observed = std::fs::read_to_string(&capture_file).unwrap_or_default();
        assert_eq!(
            observed, "governance-forcing-value",
            "the child must observe the governance-env value the engine set, not an empty/\
             inherited one"
        );

        // 2. The tracked fixture directory is UNCHANGED: same file set, same content — no
        // opencode.json or any other file was ever dropped into it.
        let after: std::collections::BTreeSet<String> = std::fs::read_dir(&fixture_cwd)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            before, after,
            "acp_governance_env must never add or remove a file in the unit's tracked cwd"
        );
        assert_eq!(
            std::fs::read_to_string(&tracked_file).unwrap(),
            "committed content\n",
            "a pre-existing tracked file must be byte-for-byte unchanged"
        );

        drop(proc);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The collision/precedence proof AND the regression guard for the crew#434 tracked-file-leak
    /// class, in one test: a git-tracked, permissive `opencode.json` — a target repo actively
    /// shipping its own config, exactly like oq-opencode-acp-002's collision fixtures — run through
    /// the REAL `start_acp_process` spawn path with `acp_governance_env` set, then asserted clean
    /// by `git diff --exit-code` and `git status --porcelain`, not merely a directory listing.
    /// `acp_governance_env` is an environment variable, never a file drop, so this must pass
    /// without any restore-before-deliver step — that fallback clause the task named never
    /// triggers because there is nothing to restore.
    #[test]
    #[cfg(unix)]
    fn provisioned_governance_env_leaves_a_tracked_permissive_config_git_clean() {
        let _env = ENV_LOCK.read().unwrap_or_else(|p| p.into_inner());
        let base = std::env::temp_dir().join(format!(
            "wicked-acp-governance-env-git-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();

        let fixture_cwd = base.join("fixture");
        std::fs::create_dir_all(&fixture_cwd).unwrap();
        let run_git = |args: &[&str]| {
            let mut cmd = std::process::Command::new("git");
            cmd.hardened();
            let out = cmd
                .args(args)
                .current_dir(&fixture_cwd)
                .output()
                .expect("git must be on PATH for this test");
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            out
        };
        run_git(&["init", "-q"]);
        run_git(&["config", "user.email", "wicked-test@example.invalid"]);
        run_git(&["config", "user.name", "wicked-test"]);
        // The tracked, permissive config a target repo might ship — the SAME collision shape
        // oq-opencode-acp-002's capture-cc-collision-*.ndjson proves the env var wins against.
        std::fs::write(
            fixture_cwd.join("opencode.json"),
            r#"{"$schema":"https://opencode.ai/config.json","permission":{"read":"allow","edit":"allow","bash":"allow"}}"#,
        )
        .unwrap();
        run_git(&["add", "-A"]);
        run_git(&["commit", "-q", "-m", "seed with permissive tracked config"]);

        let capture_file = base.join("observed-env.txt");
        let script = stub_bridge_capturing_env(&base, "WICKED_TEST_GOVERNANCE_ENV", &capture_file);
        let config = AcpConfig {
            binary: script.to_string_lossy().to_string(),
            start_args: vec![],
            transport: AcpTransport::default(),
            auth_method: None,
            acp_input_governance: true,
            os_sandbox: false,
            acp_governance_env: Some((
                "WICKED_TEST_GOVERNANCE_ENV".into(),
                "governance-forcing-value".into(),
            )),
            verified_version: None,
        };

        let proc =
            start_acp_process(&config, &fixture_cwd, None, None).expect("stub bridge must start");
        assert_eq!(
            std::fs::read_to_string(&capture_file).unwrap_or_default(),
            "governance-forcing-value",
            "the injection must reach the child even with a tracked opencode.json present"
        );
        drop(proc);

        // The proof this test adds beyond the plain-directory-listing check: `git` itself, not a
        // hand-rolled comparison, confirms the tracked file is unmodified and the tree carries no
        // untracked additions.
        let diff = run_git(&["diff", "--exit-code"]);
        assert!(diff.status.success());
        let status = run_git(&["status", "--porcelain"]);
        assert!(
            status.stdout.is_empty(),
            "git status must be clean after a governed spawn: {}",
            String::from_utf8_lossy(&status.stdout)
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// A stub bridge that answers `--version` distinctly from the ACP handshake (branching on
    /// `$1`), so a single script doubles as both the spawned ACP binary and the thing the
    /// spawn-time version-pin probe (`resolved_binary_version_matches`) runs `--version` against —
    /// exactly how `AcpConfig::verified_version` checks the SAME resolved binary that gets spawned.
    #[cfg(unix)]
    fn stub_bridge_with_version(dir: &std::path::Path, version: &str) -> std::path::PathBuf {
        let script = dir.join("stub-acp-bridge-versioned.sh");
        std::fs::write(
            &script,
            format!(
                r#"#!/bin/sh
if [ "$1" = "--version" ]; then
  echo '{version}'
  exit 0
fi
read _init
printf '%s\n' '{{"jsonrpc":"2.0","id":1,"result":{{}}}}'
read _new
printf '%s\n' '{{"jsonrpc":"2.0","id":2,"result":{{"sessionId":"stub"}}}}'
sleep 30
"#
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    #[test]
    #[cfg(unix)]
    fn resolved_binary_version_matches_the_exact_pinned_string_only() {
        let dir = std::env::temp_dir().join(format!(
            "wicked-version-pin-probe-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let script = stub_bridge_with_version(&dir, "1.17.18");
        let bin = script.to_string_lossy().to_string();

        assert!(resolved_binary_version_matches(&bin, "1.17.18"));
        assert!(
            !resolved_binary_version_matches(&bin, "1.18.21"),
            "a different reported version must not match a stale pin"
        );
        assert!(
            !resolved_binary_version_matches("wicked-no-such-binary-xyzzy", "1.17.18"),
            "a probe that cannot even spawn must not be treated as a match"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// End-to-end version-pin downgrade: a seat whose `verified_version` does NOT match the
    /// actual resolved binary must have `AcpProcess::governance_verified == false` even though the
    /// spawn itself succeeds — the unit still runs, only the governance CLAIM is downgraded
    /// (DES-INPUT-GOV-006 §3.4). A matching pin, and no pin at all, must both leave it `true`.
    #[test]
    #[cfg(unix)]
    fn version_pin_mismatch_downgrades_governance_verified_without_failing_the_spawn() {
        let _env = ENV_LOCK.read().unwrap_or_else(|p| p.into_inner());
        let dir = std::env::temp_dir().join(format!(
            "wicked-version-pin-e2e-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let script = stub_bridge_with_version(&dir, "1.17.18");
        let bin = script.to_string_lossy().to_string();

        let base_config = AcpConfig {
            binary: bin,
            start_args: vec![],
            transport: AcpTransport::default(),
            auth_method: None,
            acp_input_governance: true,
            os_sandbox: false,
            acp_governance_env: None,
            verified_version: None,
        };

        // No pin at all: always verified.
        let proc = start_acp_process(&base_config, &dir, None, None).unwrap();
        assert!(proc.governance_verified);
        drop(proc);

        // Matching pin: verified.
        let matching = AcpConfig {
            verified_version: Some("1.17.18".into()),
            ..base_config.clone()
        };
        let proc = start_acp_process(&matching, &dir, None, None).unwrap();
        assert!(proc.governance_verified);
        drop(proc);

        // Mismatched pin: the spawn still succeeds, but governance is downgraded for this process.
        let mismatched = AcpConfig {
            verified_version: Some("1.18.21".into()),
            ..base_config
        };
        let proc = start_acp_process(&mismatched, &dir, None, None).unwrap();
        assert!(
            !proc.governance_verified,
            "a resolved binary reporting a different version must not be trusted as admitted"
        );
        drop(proc);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The fix that matters most. `session/new` cost scales with how many bridges start at once
    /// (1.67s at K=1 → 7.12s median / 11.57s max at K=8), and a handshake that outruns its budget
    /// does not fail the unit — it silently downgrades it to ungoverned execution. Bounding the
    /// overlap is what keeps that cost off the curve.
    ///
    /// Measured through the real `start_acp_process`, not the gate in isolation, so deleting the
    /// permit acquisition fails this test rather than leaving a passing unit test behind.
    #[test]
    #[cfg(unix)]
    fn eight_simultaneous_starts_never_exceed_the_gate_in_flight() {
        // ENV read side (core#285): every real start below resolves WICKED_WORKER_HOME mid-call,
        // so hold the read lock against the fixture tests that re-aim the variable at a
        // symlink-refusal home. One parent-held guard covers the spawned starts — readers never
        // block readers. ENV_LOCK before REAL_STARTS (same order everywhere).
        let _env = ENV_LOCK.read().unwrap_or_else(|p| p.into_inner());
        let _serial = REAL_STARTS.lock().unwrap_or_else(|p| p.into_inner());
        let dir = std::env::temp_dir().join(format!("wicked-acp-gate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ledger = dir.join("overlap.txt");
        std::fs::write(&ledger, "").unwrap();
        let script = stub_bridge(&dir, &ledger);

        let config = AcpConfig {
            binary: script.to_string_lossy().to_string(),
            start_args: vec![],
            transport: AcpTransport::default(),
            auth_method: None,
            acp_input_governance: false,
            os_sandbox: false,
            acp_governance_env: None,
            verified_version: None,
        };

        let handles: Vec<_> = (0..8)
            .map(|_| {
                let config = config.clone();
                let cwd = dir.clone();
                std::thread::spawn(move || start_acp_process(&config, &cwd, None, None))
            })
            .collect();
        let procs: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        // Every start must still SUCCEED. A gate that bounded contention by failing starts would
        // trade one silent downgrade for another.
        for p in &procs {
            assert!(p.is_ok(), "a gated start failed: {:?}", p.as_ref().err());
        }

        // Replay the ledger for the peak number of simultaneous handshakes.
        let marks = std::fs::read_to_string(&ledger).unwrap();
        let (mut cur, mut peak) = (0i32, 0i32);
        for c in marks.chars() {
            match c {
                '+' => {
                    cur += 1;
                    peak = peak.max(cur);
                }
                '-' => cur -= 1,
                _ => {}
            }
        }
        assert_eq!(marks.matches('+').count(), 8, "all 8 bridges ran: {marks}");
        assert!(
            peak <= start_permits() as i32,
            "peak concurrent handshakes {peak} exceeded the gate's {} permits (ledger: {marks})",
            start_permits()
        );

        drop(procs); // kills the stub children
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(unix)]
    fn a_bridge_that_writes_to_stderr_has_its_last_lines_kept_and_older_ones_dropped() {
        // spawn-audit: test-only — a shell writing 50 stderr lines, to prove the ring buffer keeps the last ones.
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg("i=1; while [ $i -le 50 ]; do echo line$i >&2; i=$((i+1)); done")
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn sh");
        let (tail, handle) = drain_stderr(child.stderr.take().unwrap());
        handle.join().unwrap();
        let _ = child.wait();

        let ctx = stderr_context(&tail);
        assert!(
            ctx.contains("line50"),
            "the most recent stderr line must survive: {ctx}"
        );
        assert!(
            !ctx.contains("line1 "),
            "the tail must be bounded, not the whole stream: {ctx}"
        );
        assert_eq!(
            tail.lock().unwrap().len(),
            STDERR_TAIL_LINES,
            "the tail is capped at STDERR_TAIL_LINES"
        );
    }

    #[test]
    fn appending_the_died_mid_turn_note_keeps_output_inside_its_cap() {
        // The streaming path caps `output` at MAX_OUT; this append used to run after that cap and
        // straight past it. It must fit — and the note, not the truncated stream it displaces, is
        // what an operator needs when a bridge dies mid-turn.
        let note = "\n[wicked-core] died".to_string();
        let mut at_cap = "a".repeat(100);
        append_within_cap(&mut at_cap, &note, 100);
        assert_eq!(at_cap.len(), 100, "the cap holds");
        assert!(at_cap.ends_with(&note), "the note survives the trim");

        // Room to spare: nothing is trimmed.
        let mut small = "abc".to_string();
        append_within_cap(&mut small, &note, 10_000);
        assert_eq!(small, format!("abc{note}"));

        // A multi-byte boundary at the cut point must not panic or corrupt.
        let mut wide = "é".repeat(50);
        append_within_cap(&mut wide, &note, 60);
        assert!(wide.len() <= 60);
        assert!(wide.ends_with(&note));
    }

    #[test]
    fn one_enormous_stderr_line_cannot_grow_the_tail_without_bound() {
        // A line COUNT bounds nothing on its own: a bridge that writes a megabyte and no newline
        // would sit in the tail whole, in a runner that lives as long as the daemon — and the tail
        // is appended to a capped `output`, so an unbounded line escapes that cap too.
        let huge = "x".repeat(100_000);
        let clipped = clip_stderr_line(huge);
        assert!(
            clipped.len() < STDERR_TAIL_LINE_BYTES + 64,
            "clipped to {}",
            clipped.len()
        );
        assert!(
            clipped.contains("+99488 bytes"),
            "a clipped line must say it was clipped: {clipped}"
        );

        // Multi-byte input must not be cut mid-character — `clip_stderr_line` returns a String, so
        // a bad boundary would panic rather than corrupt.
        let wide = "é".repeat(1_000);
        assert!(clip_stderr_line(wide).len() < STDERR_TAIL_LINE_BYTES + 64);

        // A line at the limit is passed through untouched.
        let small = "y".repeat(STDERR_TAIL_LINE_BYTES);
        assert_eq!(clip_stderr_line(small.clone()), small);
    }

    #[test]
    fn a_silent_bridge_is_reported_as_silent_rather_than_as_no_information() {
        // Silence is itself diagnostic — it points at contention or a hang rather than at the
        // bridge rejecting something — so it must be stated, not rendered as an empty string.
        let empty: StderrTail = Arc::new(Mutex::new(std::collections::VecDeque::new()));
        assert!(stderr_context(&empty).contains("silent"));
    }

    // ── FINDING-015: the ACP client authenticates when the agent asks it to ─────

    /// Every test that drives a REAL `start_acp_process` serialises here.
    /// `start_acp_process` acquires the process-wide start gate, and
    /// `eight_simultaneous_starts_never_exceed_the_gate_in_flight` asserts a concurrency peak
    /// measured against that same gate — a permit held by a concurrent test forces one of its
    /// 8 starts past `START_WAIT` into a contended start, and the peak assertion becomes a race.
    #[cfg(unix)]
    static REAL_STARTS: Mutex<()> = Mutex::new(());

    /// A fresh scratch dir per test — these stubs run concurrently under `cargo test`, so a
    /// shared dir would interleave ledgers. Platform-independent (codex round 4 / Windows CI):
    /// the per-session settings and temp-sweep tests run on every platform and use it too.
    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("wicked-acp-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(unix)]
    fn write_stub(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
        let script = dir.join("stub-bridge.sh");
        std::fs::write(&script, body).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    #[cfg(unix)]
    fn stub_config(script: &std::path::Path, auth_method: Option<&str>) -> AcpConfig {
        AcpConfig {
            binary: script.to_string_lossy().to_string(),
            start_args: vec![],
            transport: AcpTransport::default(),
            auth_method: auth_method.map(str::to_string),
            acp_input_governance: false,
            os_sandbox: false,
            acp_governance_env: None,
            verified_version: None,
        }
    }

    /// A stub agent that REQUIRES authentication: `initialize` advertises two authMethods, and
    /// the next frame decides the outcome — an `authenticate` frame is appended to `ledger` and
    /// the session is granted; anything else (i.e. an unauthenticated `session/new`) is refused
    /// with the ACP `auth_required` code, which is exactly what the pre-fix client provoked.
    #[cfg(unix)]
    fn stub_auth_requiring_bridge(
        dir: &std::path::Path,
        ledger: &std::path::Path,
    ) -> std::path::PathBuf {
        write_stub(
            dir,
            &format!(
                r#"#!/bin/sh
read _init
printf '%s\n' '{{"jsonrpc":"2.0","id":1,"result":{{"authMethods":[{{"id":"method-a","name":"A"}},{{"id":"method-b","name":"B"}}]}}}}'
read second
case "$second" in
*'"method":"authenticate"'*)
  printf '%s\n' "$second" >> "{ledger}"
  printf '%s\n' '{{"jsonrpc":"2.0","id":2,"result":null}}'
  read _new
  printf '%s\n' '{{"jsonrpc":"2.0","id":3,"result":{{"sessionId":"authed-session"}}}}'
  ;;
*)
  printf '%s\n' '{{"jsonrpc":"2.0","id":2,"error":{{"code":-32000,"message":"auth required"}}}}'
  ;;
esac
sleep 30
"#,
                ledger = ledger.display()
            ),
        )
    }

    /// FINDING-015 end-to-end, through the real `start_acp_process`: an agent that advertises
    /// `authMethods` and refuses unauthenticated sessions gets `authenticate` between
    /// `initialize` and `session/new`, and the handshake succeeds. The pre-fix client discarded
    /// the initialize result and never authenticated — against this exact stub that path gets
    /// the -32000 refusal, so reverting the fix fails this test at the `expect`.
    #[test]
    #[cfg(unix)]
    fn an_auth_requiring_agent_is_authenticated_before_session_new() {
        let _env = ENV_LOCK.read().unwrap_or_else(|p| p.into_inner()); // real start reads env (core#285)
        let _serial = REAL_STARTS.lock().unwrap_or_else(|p| p.into_inner());
        let dir = scratch("auth-default");
        let ledger = dir.join("auth-frames.txt");
        std::fs::write(&ledger, "").unwrap();
        let script = stub_auth_requiring_bridge(&dir, &ledger);

        let proc = start_acp_process(&stub_config(&script, None), &dir, None, None)
            .expect("an auth-requiring agent must start once the client authenticates");
        assert_eq!(proc.session_id, "authed-session");
        // initialize=1, authenticate=2, session/new=3 — the first turn must not reuse an id.
        assert_eq!(proc.next_id, 4);

        let frames = std::fs::read_to_string(&ledger).unwrap();
        assert!(
            frames.contains(r#""methodId":"method-a""#),
            "with no auth_method configured, the FIRST advertised method is used: {frames}"
        );
        drop(proc);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── crew#340: liveness probe before cached-session reuse ────────────────────

    /// Handshakes normally, then sleeps — a stand-in for a warm bridge between turns.
    #[cfg(unix)]
    fn stub_idle_bridge(dir: &std::path::Path) -> std::path::PathBuf {
        write_stub(
            dir,
            r#"#!/bin/sh
read _init
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{}}'
read _new
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"probe-stub"}}'
sleep 30
"#,
        )
    }

    /// The child pid behind an `AcpProcess`'s kill handle — test-only, for the external
    /// `kill -9` that models an operator (or the OOM killer) taking a worker down.
    #[cfg(unix)]
    fn bridge_pid(arc: &Arc<Mutex<AcpProcess>>) -> u32 {
        let proc = arc.lock().unwrap_or_else(|p| p.into_inner());
        let guard = proc
            .kill_handle
            .inner
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        guard.as_ref().expect("child not yet taken").id()
    }

    /// A runner with one REAL cached session for `(run, cli)`, plus the arc for direct
    /// inspection. The write registry carries the session's handles AND an unrelated seat's
    /// no-op handles, so purge tests can prove eviction is per-`(run_id, cli_key)`.
    #[cfg(unix)]
    fn runner_with_cached_session(
        dir: &std::path::Path,
        run: &str,
        cli: &str,
    ) -> (AcpStepRunner, Arc<Mutex<AcpProcess>>) {
        let script = stub_idle_bridge(dir);
        let proc = start_acp_process(&stub_config(&script, None), dir, None, None)
            .expect("the idle stub must handshake");
        let handles = (Arc::clone(&proc.write_lock), Arc::clone(&proc.kill_handle));
        let arc = Arc::new(Mutex::new(proc));

        let (tx, rx) = std::sync::mpsc::channel();
        // The probe never emits Commands today, but leak the receiver so any future emit
        // in this path cannot silently fail in tests.
        std::mem::forget(rx);
        let runner = AcpStepRunner::new(tx);
        runner
            .sessions
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert((run.to_string(), cli.to_string()), Some(Arc::clone(&arc)));
        let mut reg = runner.write_reg.lock().unwrap_or_else(|p| p.into_inner());
        reg.insert((run.to_string(), cli.to_string(), 1), handles);
        reg.insert(
            (run.to_string(), "other-cli".to_string(), 1),
            (Arc::new(Mutex::new(())), Arc::new(KillHandle::noop())),
        );
        drop(reg);
        (runner, arc)
    }

    /// crew#340, the wedge itself: `kill -9` on a cached bridge between turns must NOT leave
    /// the husk in the session map. The probe reports Vacant (so the caller starts a fresh
    /// `session/new` instead of writing into a broken pipe and degrading to single-shot),
    /// purges the map entry, and drops the write-registry handles for exactly that seat.
    #[test]
    #[cfg(unix)]
    fn a_sigkilled_cached_session_is_purged_and_the_next_unit_starts_fresh() {
        let _env = ENV_LOCK.read().unwrap_or_else(|p| p.into_inner()); // real start reads env (core#285)
        let _serial = REAL_STARTS.lock().unwrap_or_else(|p| p.into_inner());
        let dir = scratch("probe-killed");
        let (runner, arc) = runner_with_cached_session(&dir, "run-340", "stub-cli");
        let key = ("run-340".to_string(), "stub-cli".to_string());

        let pid = bridge_pid(&arc);
        // The real kill: SIGKILL from outside, exactly like an operator's `kill -9`.
        // spawn-audit: test-only — /bin/kill delivering a signal; it runs no job of its own.
        let status = std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .status()
            .expect("spawn kill");
        assert!(status.success(), "kill -9 {pid} must be delivered");

        // Wait until the exit is observable (try_wait), else the probe legitimately says Live.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let exited = {
                let proc = arc.lock().unwrap_or_else(|p| p.into_inner());
                proc.kill_handle.try_exit_status().is_some()
            };
            if exited {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the SIGKILLed bridge never became observably dead"
            );
            std::thread::sleep(Duration::from_millis(50));
        }

        assert!(
            matches!(runner.probe_cached_session(&key), SessionProbe::Vacant),
            "a dead cached session must probe Vacant so the caller spawns fresh"
        );
        assert!(
            !runner
                .sessions
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .contains_key(&key),
            "the husk must be purged from the session map"
        );
        {
            let reg = runner.write_reg.lock().unwrap_or_else(|p| p.into_inner());
            assert!(
                !reg.keys()
                    .any(|(r, c, _)| r == "run-340" && c == "stub-cli"),
                "the dead seat's write-registry handles must be dropped"
            );
            assert!(
                reg.keys()
                    .any(|(r, c, _)| r == "run-340" && c == "other-cli"),
                "an unrelated seat's handles must survive the purge"
            );
        }
        // Idempotent: the husk is gone, so the next probe is a plain miss.
        assert!(matches!(
            runner.probe_cached_session(&key),
            SessionProbe::Vacant
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The probe must never evict a HEALTHY warm session — reuse is the entire point of the
    /// session cache, and core#13 measured cold starts in seconds.
    #[test]
    #[cfg(unix)]
    fn a_live_cached_session_is_reported_live_and_left_cached() {
        let _env = ENV_LOCK.read().unwrap_or_else(|p| p.into_inner()); // real start reads env (core#285)
        let _serial = REAL_STARTS.lock().unwrap_or_else(|p| p.into_inner());
        let dir = scratch("probe-live");
        let (runner, arc) = runner_with_cached_session(&dir, "run-340", "stub-cli");
        let key = ("run-340".to_string(), "stub-cli".to_string());

        match runner.probe_cached_session(&key) {
            SessionProbe::Live(probed) => assert!(
                Arc::ptr_eq(&probed, &arc),
                "the probe must hand back the cached session, not a copy"
            ),
            _ => panic!("a live cached session must probe Live"),
        }
        assert!(
            runner
                .sessions
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .contains_key(&key),
            "a live session must stay cached"
        );
        drop(runner); // drops the map → kills the stub
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── crew#290 defense-in-depth: bridge spawns in its own process group ────────

    /// The process-group id of `pid`, read via the `getpgid(2)` syscall — the same minimal libc
    /// FFI `wicked-council`'s dispatch teardown uses (no `ps` shell-out, so it can't break on a
    /// BusyBox/minimal image). `-1` on failure (e.g. the child already reaped). Test-only.
    #[cfg(unix)]
    fn pgid_of(pid: u32) -> i64 {
        extern "C" {
            fn getpgid(pid: i32) -> i32;
        }
        // SAFETY: getpgid is a pure read of a kernel process attribute; no memory is touched.
        i64::from(unsafe { getpgid(pid as i32) })
    }

    /// crew#290 defense-in-depth: `start_acp_process` must put the bridge in its OWN process
    /// group, isolated from the daemon's. `process_group(0)` makes the child a group leader
    /// whose pgid equals its own pid and differs from the caller's group — so a signal
    /// addressed to the DAEMON's process group (a terminal Ctrl-C → SIGINT to the foreground
    /// group, `kill -TERM -<daemon_pgid>`, `pkill -g <daemon_pgid>`) cannot include an idle
    /// cached bridge. The positive control fires a group-scoped SIGTERM at the child's OWN
    /// group and proves it lands (the mechanism is real); because that group id is provably
    /// different from the daemon's, the same class of signal aimed at the daemon's group
    /// excludes the bridge — and we prove that WITHOUT ever signalling the test runner's own
    /// group, which would kill the harness. Reverting `cmd.process_group(0)` leaves the child
    /// in the test process's group, failing both the leader assertion and the difference
    /// assertion. This does NOT defend against a pid-targeted or name-pattern `pkill claude`
    /// — that is why core#343's liveness probe stays primary (see the two probe tests above).
    #[test]
    #[cfg(unix)]
    fn the_bridge_spawns_in_its_own_process_group_isolated_from_the_daemon() {
        let _env = ENV_LOCK.read().unwrap_or_else(|p| p.into_inner()); // real start reads env (core#285)
        let _serial = REAL_STARTS.lock().unwrap_or_else(|p| p.into_inner());
        let dir = scratch("pgroup-isolation");
        let script = stub_idle_bridge(&dir);
        let proc = start_acp_process(&stub_config(&script, None), &dir, None, None)
            .expect("the idle stub must handshake");
        let arc = Arc::new(Mutex::new(proc));

        let pid = bridge_pid(&arc);
        let daemon_pgid = pgid_of(std::process::id());
        let child_pgid = pgid_of(pid);

        assert_eq!(
            child_pgid, pid as i64,
            "process_group(0) must make the bridge its own group leader (pgid == its pid)"
        );
        assert_ne!(
            child_pgid, daemon_pgid,
            "the bridge must NOT share the daemon's process group — else a group- or \
             terminal-scoped signal to the daemon reaches the idle cached bridge (crew#290)"
        );

        // Direct negative control — SAFE (the test runner's own group is never signalled): a
        // decoy child in ITS OWN fresh process group. A group-scoped SIGTERM aimed at the DECOY's
        // group kills the decoy but leaves the bridge (a DIFFERENT own group) ALIVE — proving
        // group signals do not cross group boundaries, so the daemon's-group signal
        // (child_pgid != daemon_pgid, asserted above) likewise cannot reach the bridge.
        {
            use std::os::unix::process::CommandExt as _;
            // spawn-audit: test-only — a `sleep` decoy that runs no job; it exists only to receive
            // a group signal and prove cross-group non-delivery.
            let mut decoy = std::process::Command::new("sleep")
                .arg("30")
                .process_group(0)
                .spawn()
                .expect("spawn decoy");
            let decoy_pgid = pgid_of(decoy.id());
            assert_ne!(
                decoy_pgid, child_pgid,
                "decoy and bridge must be in different groups"
            );
            assert_ne!(
                decoy_pgid, daemon_pgid,
                "decoy must not share the daemon's group"
            );
            // spawn-audit: test-only — /bin/kill delivering a signal to the DECOY's group only.
            let _ = std::process::Command::new("kill")
                .args(["-TERM", "--", &format!("-{decoy_pgid}")])
                .status();
            let deadline = Instant::now() + Duration::from_secs(10);
            while decoy.try_wait().ok().flatten().is_none() {
                assert!(
                    Instant::now() < deadline,
                    "decoy never died from its own group's SIGTERM"
                );
                std::thread::sleep(Duration::from_millis(50));
            }
            // The bridge, in a DIFFERENT group, is untouched by the decoy-group signal.
            let p = arc.lock().unwrap_or_else(|p| p.into_inner());
            assert!(
                p.kill_handle.try_exit_status().is_none(),
                "the bridge in its own group must survive a group signal aimed at another group"
            );
        }

        // Positive control: a group-scoped SIGTERM aimed at the child's OWN group is delivered.
        // `-- -<pgid>`: the leading `--` ends option parsing so the negative pgid is taken as a
        // target, not a flag. This exercises group-signal delivery against the bridge's group
        // only; the test process's own group is never touched.
        // spawn-audit: test-only — /bin/kill delivering a signal; it runs no job of its own.
        let status = std::process::Command::new("kill")
            .args(["-TERM", "--", &format!("-{child_pgid}")])
            .status()
            .expect("spawn kill");
        assert!(
            status.success(),
            "group SIGTERM to the child's own group ({child_pgid}) must be delivered"
        );

        // The child must actually die from that group signal — proof the group is real and the
        // bridge is a member of it (and therefore of NO OTHER group, the daemon's included).
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let exited = {
                let p = arc.lock().unwrap_or_else(|p| p.into_inner());
                p.kill_handle.try_exit_status().is_some()
            };
            if exited {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the bridge never died from the group SIGTERM aimed at its own group"
            );
            std::thread::sleep(Duration::from_millis(50));
        }

        // drop → AcpProcess::drop still calls kill_handle.signal(), which takes the Child and
        // best-effort kill()/wait()s it; on an already-reaped child those errors are ignored.
        drop(arc);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A `None` slot means "startup already failed once this run" — the probe must keep the
    /// tombstone (fall back immediately) rather than reporting Vacant and re-attempting a
    /// spawn that would fail again with a fresh warning per unit.
    #[test]
    fn a_failed_startup_tombstone_probes_failed_startup_and_is_kept() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let runner = AcpStepRunner::new(tx);
        let key = ("run-340".to_string(), "stub-cli".to_string());
        runner
            .sessions
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(key.clone(), None);

        assert!(matches!(
            runner.probe_cached_session(&key),
            SessionProbe::FailedStartup
        ));
        assert!(
            matches!(
                runner
                    .sessions
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .get(&key),
                Some(None)
            ),
            "the tombstone must survive the probe"
        );
    }

    /// The operator's `auth_method` (the new serde-default field on `AcpConfig`) overrides the
    /// agent's advertised order — a gateway-authed seat must not be logged in with the agent's
    /// preferred interactive method just because it is listed first.
    #[test]
    #[cfg(unix)]
    fn a_configured_auth_method_overrides_the_agents_first_advertised() {
        let _env = ENV_LOCK.read().unwrap_or_else(|p| p.into_inner()); // real start reads env (core#285)
        let _serial = REAL_STARTS.lock().unwrap_or_else(|p| p.into_inner());
        let dir = scratch("auth-configured");
        let ledger = dir.join("auth-frames.txt");
        std::fs::write(&ledger, "").unwrap();
        let script = stub_auth_requiring_bridge(&dir, &ledger);

        let proc = start_acp_process(&stub_config(&script, Some("method-b")), &dir, None, None)
            .expect("configured-method authentication must start the session");

        let frames = std::fs::read_to_string(&ledger).unwrap();
        assert!(
            frames.contains(r#""methodId":"method-b""#),
            "the configured method must be the one sent: {frames}"
        );
        assert!(
            !frames.contains(r#""methodId":"method-a""#),
            "the agent's first method must NOT be sent when the operator chose one: {frames}"
        );
        drop(proc);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The fail-fast half of FINDING-015: `authenticate` is accepted and `session/new` is STILL
    /// refused as unauthenticated. That must produce the named error — one that says what was
    /// tried and what the operator can change — not the bare server error, and not a hang.
    /// Reverting the code-matched branch in `start_acp_process` fails the message assertions.
    #[test]
    #[cfg(unix)]
    fn still_unauthenticated_after_authenticate_fails_with_the_named_error() {
        let _env = ENV_LOCK.read().unwrap_or_else(|p| p.into_inner()); // real start reads env (core#285)
        let _serial = REAL_STARTS.lock().unwrap_or_else(|p| p.into_inner());
        let dir = scratch("auth-never");
        let script = write_stub(
            &dir,
            r#"#!/bin/sh
read _init
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"authMethods":[{"id":"method-a","name":"A"}]}}'
read _auth
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":null}'
read _new
printf '%s\n' '{"jsonrpc":"2.0","id":3,"error":{"code":-32000,"message":"credentials rejected"}}'
sleep 30
"#,
        );

        let err = match start_acp_process(&stub_config(&script, None), &dir, None, None) {
            Err(e) => e,
            Ok(_) => panic!("an agent that refuses every session must fail the start"),
        };
        let msg = err.to_string();
        assert!(
            msg.contains("still unauthenticated after"),
            "the refusal must be NAMED as an auth failure, not rendered as a bare server error: {msg}"
        );
        assert!(
            msg.contains("method-a"),
            "the named error must say which method was already tried: {msg}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The shape claude-agent-acp@0.62 actually has: it advertises terminal auth methods while
    /// already logged in, and its `authenticate` throws "Method not implemented." for them. An
    /// `authenticate` failure therefore must NOT be fatal on its own — `session/new` is the
    /// authority on whether auth is satisfied. Making the failure fatal breaks the one bridge
    /// this runner ships as its primary seat.
    #[test]
    #[cfg(unix)]
    fn an_agent_that_rejects_authenticate_but_grants_sessions_still_starts() {
        let _env = ENV_LOCK.read().unwrap_or_else(|p| p.into_inner()); // real start reads env (core#285)
        let _serial = REAL_STARTS.lock().unwrap_or_else(|p| p.into_inner());
        let dir = scratch("auth-already");
        let script = write_stub(
            &dir,
            r#"#!/bin/sh
read _init
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"authMethods":[{"id":"method-a","name":"A"}]}}'
read _auth
printf '%s\n' '{"jsonrpc":"2.0","id":2,"error":{"code":-32601,"message":"Method not implemented."}}'
read _new
printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"sessionId":"already-authed"}}'
sleep 30
"#,
        );

        let proc = start_acp_process(&stub_config(&script, None), &dir, None, None)
            .expect("a rejected authenticate must not fail a start the agent is willing to grant");
        assert_eq!(proc.session_id, "already-authed");
        drop(proc);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── FINDING-061: an ACP worker must not run under the operator's CLI config ─

    /// End-to-end through the real `start_acp_process`: the child the spawn actually produces
    /// must see an engine-minted CLAUDE_CONFIG_DIR, not the daemon's inherited one and not the
    /// implicit `~/.claude`. The stub echoes the variable it received, so deleting the
    /// `cmd.env(...)` line in `start_acp_process` — the reachability this test exists to prove —
    /// fails the prefix assertion below.
    #[test]
    #[cfg(unix)]
    fn an_acp_worker_does_not_inherit_the_operators_claude_config_dir() {
        // The escape hatch is a supported configuration: a host that sets it runs workers under
        // the operator's config ON PURPOSE, and must not fail a test about the default boundary
        // (same convention as the budget tests asserting on `parse_secs(None, ..)`).
        if std::env::var_os(crate::execute_wrapped::INHERIT_OPERATOR_CONFIG_ENV).is_some() {
            return;
        }
        // ENV_LOCK first (same order everywhere). Write side: this test MUTATES
        // WICKED_WORKER_HOME, like the sanitize/fence tests.
        let _env = ENV_LOCK.write().unwrap_or_else(|p| p.into_inner());
        let _serial = REAL_STARTS.lock().unwrap_or_else(|p| p.into_inner());
        let dir = scratch("config-iso");
        // Scratch-scope the worker home: without this the spawn would ensure (and re-write
        // settings in) the DEVELOPER's real ~/.wicked-worker/claude (Copilot, PR#277).
        std::env::set_var("WICKED_WORKER_HOME", &dir);
        let ledger = dir.join("seen-config-dir.txt");
        let script = write_stub(
            &dir,
            &format!(
                r#"#!/bin/sh
printf '%s\n' "${{CLAUDE_CONFIG_DIR:-UNSET}}" > "{ledger}"
read _init
printf '%s\n' '{{"jsonrpc":"2.0","id":1,"result":{{}}}}'
read _new
printf '%s\n' '{{"jsonrpc":"2.0","id":2,"result":{{"sessionId":"iso"}}}}'
sleep 30
"#,
                ledger = ledger.display()
            ),
        );

        let proc = start_acp_process(&stub_config(&script, None), &dir, None, None).expect("start");
        let seen = std::fs::read_to_string(&ledger).unwrap().trim().to_string();
        assert_ne!(
            seen, "UNSET",
            "the spawn must SET the config dir: merely not-inheriting one leaves the bridge on \
             its homedir() fallback, which is the operator's ~/.claude"
        );
        let seen_dir = std::path::PathBuf::from(&seen);
        // crew#267 option 3: the engine-owned scope is the PERSISTENT worker home now (one
        // login sticks), never the operator's own ~/.claude / CLAUDE_CONFIG_DIR.
        assert_eq!(
            seen_dir,
            worker_config_home().expect("home resolvable"),
            "the worker's config dir must be the engine-owned worker home, not inherited: {seen}"
        );
        // Substance, not presence: the minted scope actually carries the deny fence.
        let settings: Value =
            serde_json::from_slice(&std::fs::read(seen_dir.join("settings.json")).unwrap())
                .unwrap();
        assert!(
            !settings["permissions"]["deny"]
                .as_array()
                .expect("seeded settings carry permissions.deny")
                .is_empty(),
            "the seeded user scope must fence the worker, not just exist: {settings}"
        );
        drop(proc);
        restore_hermetic_worker_home();
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&seen_dir);
    }

    /// core#410 (F-010 / F-068), through the REAL spawn: a bridge carrying a codex / pi /
    /// copilot / opencode seat is handed THAT CLI's configuration-home variable pointing at its
    /// own root under the worker home — created private — and NONE of the other seats' variables,
    /// however many decoys the daemon carries. A fake bridge records the environment it was
    /// spawned with. Deleting `seat_config.apply(&mut cmd)` in the spawn fails the "own variable"
    /// assertion for every seat; deleting a `strip` entry fails the decoy assertion.
    #[test]
    #[cfg(unix)]
    fn every_non_claude_bridge_gets_its_own_config_root_and_no_foreign_seat_variable() {
        if std::env::var_os(crate::execute_wrapped::INHERIT_OPERATOR_CONFIG_ENV).is_some() {
            return;
        }
        use wicked_apps_core::spawn::{
            SeatCli, CLAUDE_CONFIG_DIR_ENV, CODEX_HOME_ENV, COPILOT_HOME_ENV,
            OPENCODE_AUTH_CONTENT_ENV, OPENCODE_CONFIG_CONTENT_ENV, OPENCODE_CONFIG_DIR_ENV,
            OPENCODE_CONFIG_FILE_ENV, PI_AGENT_DIR_ENV, XDG_CONFIG_HOME_ENV, XDG_DATA_HOME_ENV,
            XDG_STATE_HOME_ENV,
        };
        // ONE spelling of opencode's inline-config variable across the two crates (the strip in
        // apps-core, the composition here).
        assert_eq!(
            OPENCODE_CONFIG_CONTENT_ENV,
            crate::skills_snapshot::OPENCODE_CONFIG_ENV
        );
        let _env = ENV_LOCK.write().unwrap_or_else(|p| p.into_inner());
        let _serial = REAL_STARTS.lock().unwrap_or_else(|p| p.into_inner());
        let dir = scratch("seat-roots");
        let worker = dir.join("worker");
        std::env::set_var("WICKED_WORKER_HOME", &worker);
        // The daemon carries a decoy for EVERY seat variable and for a generic XDG base.
        let decoy = dir.join("daemon-decoy");
        std::fs::create_dir_all(&decoy).unwrap();
        let _d1 = EnvPin::set(CLAUDE_CONFIG_DIR_ENV, &decoy);
        let _d2 = EnvPin::set(CODEX_HOME_ENV, &decoy);
        let _d3 = EnvPin::set(PI_AGENT_DIR_ENV, &decoy);
        let _d4 = EnvPin::set(COPILOT_HOME_ENV, &decoy);
        let _d5 = EnvPin::set(OPENCODE_CONFIG_DIR_ENV, &decoy);
        let _d6 = EnvPin::set(XDG_CONFIG_HOME_ENV, &decoy);
        let _d7 = EnvPin::set(XDG_DATA_HOME_ENV, &decoy);
        let _d8 = EnvPin::set(XDG_STATE_HOME_ENV, &decoy);
        // The operator's ambient INLINE opencode config (plugins, permission rules), extra config
        // FILE and inline CREDENTIALS — none may reach an isolated seat, opencode's own included
        // (Copilot, #426; independent review C2).
        let _d9 = EnvPin::set(OPENCODE_CONFIG_CONTENT_ENV, &decoy);
        let _d10 = EnvPin::set(OPENCODE_CONFIG_FILE_ENV, &decoy);
        let _d11 = EnvPin::set(OPENCODE_AUTH_CONTENT_ENV, &decoy);
        let vars = [
            CLAUDE_CONFIG_DIR_ENV,
            CODEX_HOME_ENV,
            PI_AGENT_DIR_ENV,
            COPILOT_HOME_ENV,
            OPENCODE_CONFIG_DIR_ENV,
            OPENCODE_CONFIG_CONTENT_ENV,
            OPENCODE_CONFIG_FILE_ENV,
            OPENCODE_AUTH_CONTENT_ENV,
            XDG_CONFIG_HOME_ENV,
            XDG_DATA_HOME_ENV,
            XDG_STATE_HOME_ENV,
        ];
        let decoy_s = decoy.to_string_lossy().into_owned();
        let cases: Vec<(SeatCli, Vec<(&str, std::path::PathBuf)>)> = vec![
            (SeatCli::Codex, vec![(CODEX_HOME_ENV, worker.join("codex"))]),
            (SeatCli::Pi, vec![(PI_AGENT_DIR_ENV, worker.join("pi"))]),
            (
                SeatCli::Copilot,
                vec![(COPILOT_HOME_ENV, worker.join("copilot"))],
            ),
            (
                SeatCli::Opencode,
                vec![
                    (XDG_CONFIG_HOME_ENV, worker.join("opencode").join("config")),
                    (XDG_DATA_HOME_ENV, worker.join("opencode").join("data")),
                    (XDG_STATE_HOME_ENV, worker.join("opencode").join("state")),
                ],
            ),
        ];
        for (seat_cli, own) in &cases {
            let ledger = dir.join(format!("env-{seat_cli:?}.txt"));
            let record: String = vars
                .iter()
                .map(|v| format!("printf '{v}=%s\\n' \"${{{v}:-UNSET}}\""))
                .collect::<Vec<_>>()
                .join("; ");
            let script = write_stub(
                &dir,
                &format!(
                    r#"#!/bin/sh
{{ {record}; }} > "{ledger}"
read _init
printf '%s\n' '{{"jsonrpc":"2.0","id":1,"result":{{}}}}'
read _new
printf '%s\n' '{{"jsonrpc":"2.0","id":2,"result":{{"sessionId":"roots"}}}}'
sleep 30
"#,
                    ledger = ledger.display()
                ),
            );
            let proc = super::start_acp_process(
                &stub_config(&script, None),
                &dir,
                None,
                None,
                *seat_cli,
                None,
            )
            .unwrap_or_else(|e| panic!("{seat_cli:?}: start: {e}"));
            let seen: std::collections::HashMap<String, String> = std::fs::read_to_string(&ledger)
                .unwrap()
                .lines()
                .filter_map(|l| l.split_once('='))
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            for (var, root) in own {
                assert_eq!(
                    seen.get(*var).map(String::as_str),
                    Some(root.to_string_lossy().as_ref()),
                    "{seat_cli:?}: its own {var} points at its root under the worker home"
                );
                let meta = std::fs::metadata(root)
                    .unwrap_or_else(|e| panic!("{seat_cli:?}: {} exists: {e}", root.display()));
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(
                    meta.permissions().mode() & 0o777,
                    0o700,
                    "{seat_cli:?}: private"
                );
            }
            let own_names: Vec<&str> = own.iter().map(|(v, _)| *v).collect();
            for var in wicked_apps_core::spawn::SEAT_CONFIG_ENV {
                if own_names.contains(var) {
                    continue;
                }
                assert_eq!(
                    seen.get(*var).map(String::as_str),
                    Some("UNSET"),
                    "{seat_cli:?}: the foreign seat variable {var} is STRIPPED, never the daemon's decoy"
                );
            }
            if *seat_cli != SeatCli::Opencode {
                for xdg in [XDG_CONFIG_HOME_ENV, XDG_DATA_HOME_ENV, XDG_STATE_HOME_ENV] {
                    assert_eq!(
                        seen.get(xdg).map(String::as_str),
                        Some(decoy_s.as_str()),
                        "{seat_cli:?}: a generic XDG base is inherited untouched"
                    );
                }
            }
            drop(proc);
        }
        restore_hermetic_worker_home();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// core#410 / crew#502, end to end through `chat_open` → `chat_turn` on a registry seat whose
    /// bridge is a fake: the seat runs IN the scope's cwd (never this process's), its
    /// `session/new` advertises the READ-ONLY estate MCP over the scope's graph and the scoped
    /// repository roots as `additionalDirectories`, the seat's configuration is the worker home's
    /// — and the pi-shaped startup banner the fake streams as its first chunk never reaches a
    /// `ChatDelta`, while the answer does. `chat_list` reports the scope; `chat_close` drops it.
    #[test]
    #[cfg(unix)]
    fn a_chat_seat_runs_in_its_scope_grounded_read_only_and_its_banner_never_reaches_the_stream() {
        if std::env::var_os(crate::execute_wrapped::INHERIT_OPERATOR_CONFIG_ENV).is_some() {
            return;
        }
        let _env = ENV_LOCK.write().unwrap_or_else(|p| p.into_inner());
        let _serial = REAL_STARTS.lock().unwrap_or_else(|p| p.into_inner());
        let dir = scratch("chat-scope");
        let worker = dir.join("worker");
        std::env::set_var("WICKED_WORKER_HOME", &worker);
        let frame_ledger = dir.join("session-new.json");
        let env_ledger = dir.join("seen-config-dir.txt");
        let script = write_stub(
            &dir,
            &format!(
                r#"#!/bin/sh
printf '%s\n' "${{CLAUDE_CONFIG_DIR:-UNSET}}" > "{env_ledger}"
read _init
printf '%s\n' '{{"jsonrpc":"2.0","id":1,"result":{{}}}}'
read new
printf '%s\n' "$new" > "{frame_ledger}"
printf '%s\n' '{{"jsonrpc":"2.0","id":2,"result":{{"sessionId":"scoped"}}}}'
read _prompt
printf '%s\n' '{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"scoped","update":{{"sessionUpdate":"agent_message_chunk","content":{{"type":"text","text":"pi v0.83.0\n---\n\n## Skills\n- /op/.pi/agent/skills/wicked-testing-x/SKILL.md\n\n## Extensions\n- /op/.pi/agent/extensions/wicked-testing.ts\n\n---\n"}}}}}}}}'
printf '%s\n' '{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"scoped","update":{{"sessionUpdate":"agent_message_chunk","content":{{"type":"text","text":"Hello from the scoped seat"}}}}}}}}'
printf '%s\n' '{{"jsonrpc":"2.0","id":3,"result":{{"stopReason":"end_turn"}}}}'
sleep 30
"#,
                env_ledger = env_ledger.display(),
                frame_ledger = frame_ledger.display()
            ),
        );
        // The seat: a CLAUDE seat (so `additionalDirectories` applies) whose bridge is the fake,
        // registered through the operator's clis.toml under a pinned HOME.
        let council = dir.join(".config").join("wicked-council");
        std::fs::create_dir_all(&council).unwrap();
        std::fs::write(
            council.join("clis.toml"),
            format!(
                r#"
[[cli]]
key = "stubchat"
display_name = "Stub chat seat"
binary = "claude"
headless_invocation = "claude -p \"{{PROMPT}}\""

[cli.acp]
binary = "{}"
transport = "stdio"
acp_input_governance = true
"#,
                script.display()
            ),
        )
        .unwrap();
        let prior_home = std::env::var_os("HOME");
        std::env::set_var("HOME", &dir);

        let chat_cwd = dir.join("chats").join("c1");
        let (repo_a, repo_b) = (dir.join("repos").join("a"), dir.join("repos").join("b"));
        std::fs::create_dir_all(&repo_a).unwrap();
        std::fs::create_dir_all(&repo_b).unwrap();
        let graph_db = dir.join("project-graphs").join("p1").join("estate.db");
        std::fs::create_dir_all(graph_db.parent().unwrap()).unwrap();
        std::fs::write(&graph_db, b"").unwrap();
        let scope = ChatScope {
            cwd: chat_cwd.clone(),
            code_graph_db: Some(graph_db.to_string_lossy().into_owned()),
            read_roots: vec![
                repo_a.to_string_lossy().into_owned(),
                repo_b.to_string_lossy().into_owned(),
            ],
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let r = AcpStepRunner::new(tx);
        let opened = r
            .chat_open("c1", &["stubchat".to_string()], scope.clone())
            .expect("a valid scope is accepted");
        let turn = r.chat_turn("c1", "stubchat", "hello");
        let listed = r.chat_list();
        r.chat_close("c1", ChatCloseReason::Requested);
        let after_close = r.chat_list();
        match prior_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
        restore_hermetic_worker_home();

        assert_eq!(opened.len(), 1);
        assert!(
            opened[0].1.is_ok(),
            "the fake bridge warms: {:?}",
            opened[0]
        );
        // (1) The seat runs IN the scope's cwd — created for it — never the daemon's.
        assert!(
            chat_cwd.is_dir(),
            "the scratch root is created on the first ensure"
        );
        let frame: Value =
            serde_json::from_str(&std::fs::read_to_string(&frame_ledger).unwrap()).unwrap();
        assert_eq!(
            frame["params"]["cwd"],
            json!(chat_cwd.to_string_lossy().as_ref()),
            "the seat's cwd is the chat's scratch root: {frame}"
        );
        assert_ne!(
            frame["params"]["cwd"],
            json!(std::env::current_dir().unwrap().to_string_lossy().as_ref()),
            "never the process's own working directory (F-067)"
        );
        // (2) Grounded: the READ-ONLY estate MCP over the scope's graph (DES-GROUNDING-001).
        let servers = frame["params"]["mcpServers"]
            .as_array()
            .expect("mcpServers is an array");
        assert_eq!(servers.len(), 1, "one estate server: {frame}");
        assert_eq!(servers[0]["name"], "wicked-estate");
        let args: Vec<&str> = servers[0]["args"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert!(
            args.contains(&graph_db.to_string_lossy().as_ref()),
            "bound to the scope's graph: {args:?}"
        );
        assert!(args.contains(&"--readonly"), "read-only: {args:?}");
        // (3) The scoped roots are advertised to the claude seat.
        assert_eq!(
            frame["params"]["_meta"]["claudeCode"]["options"]["additionalDirectories"],
            json!(scope.read_roots),
            "{frame}"
        );
        // (4) The seat's configuration is the worker home's (FINDING-061), not the operator's.
        assert_eq!(
            std::path::PathBuf::from(std::fs::read_to_string(&env_ledger).unwrap().trim()),
            worker.join("claude")
        );
        // (5) The assembled reply is the answer; the banner never reached a ChatDelta.
        assert_eq!(
            turn.as_deref(),
            Ok("Hello from the scoped seat"),
            "{turn:?}"
        );
        let deltas: Vec<String> = rx
            .try_iter()
            .filter_map(|c| match c {
                Command::EmitEvent(CoreEvent::ChatDelta { chat, text, .. }) if chat == "c1" => {
                    Some(text)
                }
                _ => None,
            })
            .collect();
        let streamed = deltas.concat();
        assert!(
            !streamed.contains("pi v0.83.0") && !streamed.contains("SKILL.md"),
            "the startup banner must never enter the streamed transcript (F-068): {streamed:?}"
        );
        assert_eq!(streamed, "Hello from the scoped seat", "{deltas:?}");
        // (6) The enumerate surface reports the scope; a closed chat holds none.
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].scope.as_ref(), Some(&scope));
        assert!(after_close.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// FINDING-122, ACP half: a run WITH a repo graph must advertise the estate MCP server on
    /// `session/new`, scoped to that repo's OWN store — the ACP-array twin of the wrapped path's
    /// settings.json injection — so the worker consumes the graph instead of re-deriving it. A
    /// repo-less session (`None`) advertises no server. The stub echoes the `session/new` frame it
    /// received; reverting `mcpServers` to a bare `[]` empties the echo and fails the assertions.
    #[test]
    #[cfg(unix)]
    fn session_new_advertises_the_repo_scoped_estate_mcp_server() {
        let _env = ENV_LOCK.read().unwrap_or_else(|p| p.into_inner()); // real start reads env (core#285)
        let _serial = REAL_STARTS.lock().unwrap_or_else(|p| p.into_inner());
        let dir = scratch("estate-mcp");
        let ledger = dir.join("session-new.json");
        let script = write_stub(
            &dir,
            &format!(
                r#"#!/bin/sh
read _init
printf '%s\n' '{{"jsonrpc":"2.0","id":1,"result":{{}}}}'
read new
printf '%s\n' "$new" > "{ledger}"
printf '%s\n' '{{"jsonrpc":"2.0","id":2,"result":{{"sessionId":"mcp"}}}}'
sleep 30
"#,
                ledger = ledger.display()
            ),
        );

        // WITH a repo graph → the estate server, scoped to that exact db.
        let graph_db = std::path::Path::new("/tmp/wicked-122-repo")
            .join(crate::code_graph::code_graph_rel())
            .to_string_lossy()
            .into_owned();
        let graph_db = graph_db.as_str();
        let proc = start_acp_process(&stub_config(&script, None), &dir, Some(graph_db), None)
            .expect("start");
        let seen = std::fs::read_to_string(&ledger).unwrap();
        assert!(
            seen.contains("\"mcpServers\""),
            "session/new must carry mcpServers: {seen}"
        );
        assert!(
            seen.contains("wicked-estate"),
            "session/new must advertise the estate MCP server (FINDING-122): {seen}"
        );
        assert!(
            seen.contains(graph_db),
            "the estate server must be scoped to the REPO graph, not the daemon store: {seen}"
        );
        drop(proc);

        // WITHOUT a repo graph → no estate server (repo-less parity with the wrapped path).
        let ledger2 = dir.join("session-new-none.json");
        let script2 = write_stub(
            &dir,
            &format!(
                r#"#!/bin/sh
read _init
printf '%s\n' '{{"jsonrpc":"2.0","id":1,"result":{{}}}}'
read new
printf '%s\n' "$new" > "{ledger}"
printf '%s\n' '{{"jsonrpc":"2.0","id":2,"result":{{"sessionId":"none"}}}}'
sleep 30
"#,
                ledger = ledger2.display()
            ),
        );
        let proc2 =
            start_acp_process(&stub_config(&script2, None), &dir, None, None).expect("start");
        let seen2 = std::fs::read_to_string(&ledger2).unwrap();
        // Judged on the `mcpServers` field, not a substring: the session's fence now rides the same
        // frame (`_meta.claudeCode.options.disallowedTools`) and names `~/.wicked-estate` there.
        let frame2: serde_json::Value = serde_json::from_str(&seen2).unwrap();
        assert_eq!(
            frame2["params"]["mcpServers"],
            serde_json::json!([]),
            "a repo-less session must advertise no estate server: {seen2}"
        );
        drop(proc2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// DES-MEM-FACETED-001 follow-on, ACP half: the estate MCP the worker's `session/new` advertises
    /// must carry the run/unit/agent provenance the `proposal.submit` tool server-stamps, formatted as
    /// the ACP `{name,value}` env array. Mirrors the wrapped carrier's `--mcp-config` env object over
    /// the same `estate_provenance_env` pairs. The stub echoes the `session/new` frame it received.
    #[test]
    #[cfg(unix)]
    fn session_new_stamps_estate_mcp_provenance_env() {
        let _env = ENV_LOCK.read().unwrap_or_else(|p| p.into_inner());
        let _serial = REAL_STARTS.lock().unwrap_or_else(|p| p.into_inner());
        let dir = scratch("estate-prov");
        let ledger = dir.join("session-new.json");
        let script = write_stub(
            &dir,
            &format!(
                r#"#!/bin/sh
read _init
printf '%s\n' '{{"jsonrpc":"2.0","id":1,"result":{{}}}}'
read new
printf '%s\n' "$new" > "{ledger}"
printf '%s\n' '{{"jsonrpc":"2.0","id":2,"result":{{"sessionId":"prov"}}}}'
sleep 30
"#,
                ledger = ledger.display()
            ),
        );

        let graph_db = std::path::Path::new("/tmp/wicked-prov-repo")
            .join(crate::code_graph::code_graph_rel())
            .to_string_lossy()
            .into_owned();
        let provenance =
            crate::execute_wrapped::estate_provenance_env("run-prov", 5, Some("codex"));
        let proc = start_acp_process_with_write_roots(
            &stub_config(&script, None),
            &dir,
            Some(graph_db.as_str()),
            None,
            &[],
            &provenance,
            &crate::skills_snapshot::SkillsDelivery::None,
            None,
        )
        .expect("start");
        let seen = std::fs::read_to_string(&ledger).unwrap();
        let frame: serde_json::Value = serde_json::from_str(&seen).unwrap();
        let env = &frame["params"]["mcpServers"][0]["env"];
        // The ACP env is an array of {name, value} — collect it into a lookup for order-independent checks.
        let pairs: std::collections::HashMap<String, String> = env
            .as_array()
            .expect("estate MCP env must be an array on the ACP carrier")
            .iter()
            .map(|e| {
                (
                    e["name"].as_str().unwrap().to_string(),
                    e["value"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        assert_eq!(
            pairs.get("WICKED_RUN_ID").map(String::as_str),
            Some("run-prov"),
            "session/new must stamp the run id: {seen}"
        );
        assert_eq!(
            pairs.get("WICKED_RUN_UNIT").map(String::as_str),
            Some("5"),
            "session/new must stamp the unit ordinal: {seen}"
        );
        assert_eq!(
            pairs.get("WICKED_RUN_AGENT").map(String::as_str),
            Some("codex"),
            "session/new must stamp the assigned CLI: {seen}"
        );
        drop(proc);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// (Copilot, review pass 7) `attach_skills_plugin` never adds the same snapshot twice: a
    /// `plugins` list already naming it — by the identical spelling, or by another spelling of the
    /// same real directory (`<snap>/.`) — gains no second entry; a DIFFERENT plugin path is kept
    /// beside it; and a non-`local` entry with the same path is not mistaken for ours.
    #[test]
    fn attach_skills_plugin_dedupes_the_snapshot_by_canonical_path() {
        let dir = scratch("plugin-dedupe");
        let snap = dir.join("snap");
        std::fs::create_dir_all(&snap).unwrap();
        let other = dir.join("other");
        std::fs::create_dir_all(&other).unwrap();
        let snap_str = snap.to_string_lossy().into_owned();
        // Already present, spelled identically.
        let mut params = json!({
            "cwd": "/wt",
            "_meta": {"claudeCode": {"options": {"plugins": [{"type": "local", "path": snap_str}]}}}
        });
        attach_skills_plugin(&mut params, &snap);
        attach_skills_plugin(&mut params, &snap);
        let plugins = params["_meta"]["claudeCode"]["options"]["plugins"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(
            plugins.len(),
            1,
            "no second entry for the same snapshot: {plugins:?}"
        );
        // Already present under ANOTHER spelling of the same real directory.
        let dotted = snap.join(".").to_string_lossy().into_owned();
        let mut params = json!({
            "_meta": {"claudeCode": {"options": {"plugins": [{"type": "local", "path": dotted}]}}}
        });
        attach_skills_plugin(&mut params, &snap);
        assert_eq!(
            params["_meta"]["claudeCode"]["options"]["plugins"]
                .as_array()
                .unwrap()
                .len(),
            1,
            "the same real directory is one plugin: {params}"
        );
        // A different plugin stays beside ours; a non-local entry with our path is not ours.
        let mut params = json!({
            "_meta": {"claudeCode": {"options": {"plugins": [
                {"type": "local", "path": other.to_string_lossy()},
                {"type": "marketplace", "path": snap.to_string_lossy()}
            ]}}}
        });
        attach_skills_plugin(&mut params, &snap);
        let plugins = params["_meta"]["claudeCode"]["options"]["plugins"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(plugins.len(), 3, "{plugins:?}");
        assert_eq!(
            plugins[2],
            json!({"type": "local", "path": snap.to_string_lossy()})
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// core#396, the ACP lever, pinned on the pure frame builder: the snapshot rides `session/new`
    /// as `_meta.claudeCode.options.plugins = [{type: "local", path}]` — the SDK's own plugin shape
    /// under the extension the bridge spreads into its session options — MERGED into whatever
    /// options are already there (sibling keys such as `settingSources` and an existing `plugins`
    /// list survive), never replacing them; the spec fields every agent reads stay put; and with no
    /// snapshot the frame carries no `_meta` at all. Two generations yield two frames naming their
    /// own roots.
    #[test]
    fn session_new_merges_the_snapshot_into_the_claude_code_options_as_a_local_plugin() {
        let cwd = std::path::Path::new("/wt");
        let servers = serde_json::json!([{"name": "wicked-estate"}]);

        let bare = session_new_params(cwd, servers.clone(), &SessionOptions::NONE);
        assert_eq!(bare["cwd"], "/wt");
        assert_eq!(bare["mcpServers"], servers);
        assert!(
            bare.get("_meta").is_none(),
            "no options ⇒ no extension: {bare}"
        );

        let gen7 = std::path::Path::new("/snapshots/7");
        let with_plugin = |root: &'static std::path::Path| SessionOptions {
            skills_plugin: Some(root),
            deny: &[],
            settings: None,
            additional_directories: &[],
            setting_sources: None,
        };
        let handed = session_new_params(cwd, servers.clone(), &with_plugin(gen7));
        assert_eq!(
            handed["cwd"], "/wt",
            "the spec params are beside the extension, not under it"
        );
        assert_eq!(handed["mcpServers"], servers);
        assert_eq!(
            handed["_meta"]["claudeCode"]["options"]["plugins"],
            serde_json::json!([{"type": "local", "path": "/snapshots/7"}]),
            "{handed}"
        );

        // A second generation names ITS root — nothing is shared between the two frames.
        let gen8 = session_new_params(
            cwd,
            servers.clone(),
            &with_plugin(std::path::Path::new("/snapshots/8")),
        );
        assert_eq!(
            gen8["_meta"]["claudeCode"]["options"]["plugins"][0]["path"],
            "/snapshots/8"
        );
        assert_ne!(handed, gen8);

        // v3.1 §3: the session's fence and settings file ride the same extension — merged into
        // an existing `disallowedTools` (the bridge merges its own on top), `settings` as the
        // SDK's path form — and a frame with no snapshot still carries its fence.
        let deny = vec![
            "Read(/h/.wicked-crew/core.db*)".to_string(),
            "Bash(sudo:*)".to_string(),
        ];
        let fenced = session_new_params(
            cwd,
            servers,
            &SessionOptions {
                skills_plugin: None,
                deny: &deny,
                settings: Some(std::path::Path::new("/wh/sessions/r-claude/settings.json")),
                additional_directories: &[],
                setting_sources: Some(ENGINE_SETTING_SOURCES),
            },
        );
        let options = &fenced["_meta"]["claudeCode"]["options"];
        assert_eq!(options["disallowedTools"], serde_json::json!(deny));
        assert_eq!(options["settings"], "/wh/sessions/r-claude/settings.json");
        // codex round 8 (M2): the production frame carries the engine's scope selection — the
        // ACP analog of `--setting-sources project,local` — and SETS it (an existing `user`
        // kept by a merge would defeat the isolation); `None` (the hatch) leaves it absent.
        assert_eq!(
            options["settingSources"],
            serde_json::json!(["project", "local"]),
            "{fenced}"
        );
        assert!(options.get("plugins").is_none(), "{fenced}");
        let mut params = serde_json::json!({
            "cwd": "/wt", "mcpServers": [],
            "_meta": {"claudeCode": {"options": {"disallowedTools": ["AskUserQuestion", "Bash(sudo:*)"], "settingSources": ["user", "project", "local"]}}}
        });
        attach_session_options(
            &mut params,
            &SessionOptions {
                skills_plugin: None,
                deny: &deny,
                settings: None,
                additional_directories: &[],
                setting_sources: Some(ENGINE_SETTING_SOURCES),
            },
        );
        assert_eq!(
            params["_meta"]["claudeCode"]["options"]["settingSources"],
            serde_json::json!(["project", "local"]),
            "an existing `user` scope is REPLACED, not merged: {params}"
        );
        let mut hatch = serde_json::json!({"cwd": "/wt", "mcpServers": []});
        attach_session_options(
            &mut hatch,
            &SessionOptions {
                skills_plugin: None,
                deny: &deny,
                settings: None,
                additional_directories: &[],
                setting_sources: None,
            },
        );
        assert!(
            hatch["_meta"]["claudeCode"]["options"]
                .get("settingSources")
                .is_none(),
            "under the hatch no scope selection is attached: {hatch}"
        );
        assert_eq!(
            params["_meta"]["claudeCode"]["options"]["disallowedTools"],
            serde_json::json!([
                "AskUserQuestion",
                "Bash(sudo:*)",
                "Read(/h/.wicked-crew/core.db*)"
            ]),
            "existing entries kept, ours appended once: {params}"
        );

        // MERGE: options that already exist keep every sibling key and their own plugin entries.
        let mut params = serde_json::json!({
            "cwd": "/wt",
            "mcpServers": [],
            "_meta": {
                "claudeCode": {
                    "options": {
                        "settingSources": ["project", "local"],
                        "plugins": [{"type": "local", "path": "/some/other/plugin"}]
                    }
                },
                "otherExtension": {"keep": true}
            }
        });
        attach_skills_plugin(&mut params, gen7);
        let options = &params["_meta"]["claudeCode"]["options"];
        assert_eq!(
            options["settingSources"],
            serde_json::json!(["project", "local"]),
            "sibling options survive the merge: {params}"
        );
        assert_eq!(
            options["plugins"],
            serde_json::json!([
                {"type": "local", "path": "/some/other/plugin"},
                {"type": "local", "path": "/snapshots/7"}
            ]),
            "an existing plugins list gains our entry rather than being replaced: {params}"
        );
        assert_eq!(params["_meta"]["otherExtension"]["keep"], true);
        assert_eq!(params["cwd"], "/wt");
    }

    /// core#396 end to end through the real spawn: the frame the bridge RECEIVES carries the
    /// snapshot as the local plugin (positive), the engine-owned worker home holds no `plugins/`
    /// when the bridge starts — a stale hand copy planted there is sanitized away, so the ONLY
    /// skills root a Claude ACP worker can load is the one in the handshake (negative exclusion) —
    /// and the snapshot tree itself is byte-identical afterwards (nothing is copied or written
    /// into it). The stub echoes the `session/new` frame it received.
    #[test]
    #[cfg(unix)]
    fn session_new_hands_the_snapshot_and_the_worker_home_holds_no_stale_plugins() {
        use crate::skills_snapshot::test_support::{snapshot_root, tree_fingerprint};
        let _env = ENV_LOCK.read().unwrap_or_else(|p| p.into_inner());
        let _serial = REAL_STARTS.lock().unwrap_or_else(|p| p.into_inner());
        let dir = scratch("skills-handshake");
        let snapshot = snapshot_root(
            &crate::skills_snapshot::test_support::gen_dir(&dir.join("state"), "12"),
            "12",
            &[
                ("domain", "wicked-garden-domain"),
                ("qe/a11y", "wicked-garden-qe-a11y"),
            ],
        );
        let before = tree_fingerprint(&snapshot);
        // A prior worker (or the operator's stop-gap) left a hand copy in the worker home.
        let home = worker_config_home().expect("the hermetic worker home resolves");
        let stale = home.join("plugins").join("wicked-garden");
        std::fs::create_dir_all(stale.join("skills/domain")).unwrap();
        std::fs::write(
            stale.join("skills/domain/SKILL.md"),
            "---\nname: stale\n---\n",
        )
        .unwrap();

        let ledger = dir.join("session-new.json");
        let script = write_stub(
            &dir,
            &format!(
                r#"#!/bin/sh
read _init
printf '%s\n' '{{"jsonrpc":"2.0","id":1,"result":{{}}}}'
read new
printf '%s\n' "$new" > "{ledger}"
printf '%s\n' '{{"jsonrpc":"2.0","id":2,"result":{{"sessionId":"skills"}}}}'
cat >/dev/null
"#,
                ledger = ledger.display()
            ),
        );
        let proc = start_acp_process_with_write_roots(
            &stub_config(&script, None),
            &dir,
            None,
            None,
            &[],
            &[],
            &crate::skills_snapshot::SkillsDelivery::ClaudePlugin(snapshot.clone()),
            None,
        )
        .expect("start");
        let seen = std::fs::read_to_string(&ledger).unwrap();
        let frame: serde_json::Value = serde_json::from_str(&seen).unwrap();
        assert_eq!(frame["method"], "session/new");
        assert_eq!(
            frame["params"]["_meta"]["claudeCode"]["options"]["plugins"],
            serde_json::json!([{"type": "local", "path": snapshot.to_string_lossy()}]),
            "the bridge must receive the snapshot as the one local plugin: {seen}"
        );
        assert_eq!(
            frame["params"]["cwd"],
            dir.to_string_lossy().as_ref(),
            "the spec params still ride the same frame: {seen}"
        );
        assert!(
            frame["params"]["_meta"]["claudeCode"]["options"]["disallowedTools"]
                .as_array()
                .is_some_and(|d| !d.is_empty()),
            "the plugin is MERGED beside the session's own options (the fence): {seen}"
        );
        assert!(
            std::fs::symlink_metadata(home.join("plugins")).is_err(),
            "the worker home's plugins/ (the stale hand copy) must be sanitized away before the \
             bridge starts — the handshake is the only skills input"
        );
        assert!(
            !seen.contains(&stale.to_string_lossy().to_string()),
            "the handshake must not name the stale copy: {seen}"
        );
        assert_eq!(
            tree_fingerprint(&snapshot),
            before,
            "the snapshot is immutable: the spawn wrote nothing into it"
        );
        drop(proc);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// (codex round 5) The shared worker-home `settings.json` is REPLACED, never unlinked first:
    /// a reader racing `ensure_worker_config_home` — another engine process's spawn, a CLI already
    /// running on this home — sees the previous file or the new one and never NO file. A reader
    /// thread reads the path continuously while this thread re-ensures the home many times; every
    /// read succeeds and parses as JSON. Round 4 removed the file before the atomic write, and a
    /// reader in that window got `NotFound`. Unix-only: on Windows a reader's open handle can make
    /// the replacing rename fail with a sharing violation — a property of the platform's rename,
    /// not of this ordering, and not what this test judges; the Windows job runs the failed-write
    /// test below, which exercises the same writer without a concurrent reader.
    #[test]
    #[cfg(unix)]
    fn a_concurrent_reader_of_the_shared_settings_never_sees_a_missing_file() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let _env = ENV_LOCK.write().unwrap_or_else(|p| p.into_inner());
        let base = worker_home_base("settings-race");
        std::env::set_var(wicked_apps_core::spawn::WORKER_HOME_ENV, &base);
        let dir = ensure_worker_config_home().expect("first ensure");
        let path = dir.join(SETTINGS_FILENAME);
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let reader = {
            let path = path.clone();
            let stop = std::sync::Arc::clone(&stop);
            std::thread::spawn(move || {
                let (mut reads, mut missing, mut torn) = (0u32, 0u32, 0u32);
                while !stop.load(Ordering::Relaxed) {
                    match std::fs::read(&path) {
                        Ok(bytes) => {
                            reads += 1;
                            if serde_json::from_slice::<Value>(&bytes).is_err() {
                                torn += 1;
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => missing += 1,
                        Err(e) => panic!("unexpected read error: {e}"),
                    }
                }
                (reads, missing, torn)
            })
        };
        for _ in 0..120 {
            ensure_worker_config_home().expect("re-ensure");
        }
        stop.store(true, Ordering::Relaxed);
        let (reads, missing, torn) = reader.join().expect("reader thread");
        restore_hermetic_worker_home();
        assert!(reads > 0, "the reader ran alongside the writer");
        assert_eq!(
            missing, 0,
            "a replacement never passes through a missing file ({reads} reads saw one)"
        );
        assert_eq!(torn, 0, "a reader never sees a torn file ({reads} reads)");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// (codex round 5) A settings write that FAILS leaves the previous valid file in place with
    /// its previous content — not a missing file — and no temp of ours behind: `write_atomic`
    /// creates its temp in `dir`, so a `dir` that is not a directory fails the create on every
    /// platform before anything touches the target. (Through `ensure_worker_config_home` the same
    /// writer runs against a home the function itself re-opens to `0o700` first, so a failure
    /// cannot be injected there portably; the ordering it relies on — no unlink before the
    /// rename — is what the concurrent-reader test above observes.)
    #[test]
    fn a_failed_settings_write_leaves_the_previous_file_intact() {
        let dir = scratch("settings-fail");
        let target = dir.join(SETTINGS_FILENAME);
        std::fs::write(&target, b"{\"previous\":true}").unwrap();
        let not_a_dir = dir.join("not-a-dir");
        std::fs::write(&not_a_dir, b"").unwrap();
        let err = write_atomic(&not_a_dir, &target, b"{\"next\":true}")
            .expect_err("the temp cannot be created under a regular file");
        assert!(
            err.to_string().contains("atomically") && err.to_string().contains("settings.json"),
            "{err}"
        );
        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"{\"previous\":true}",
            "the previous valid file is untouched"
        );
        let leftovers: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| settings_temp_pid(n, SETTINGS_FILENAME).is_some())
            .collect();
        assert!(
            leftovers.is_empty(),
            "no temp of ours is left: {leftovers:?}"
        );
        // And a write that SUCCEEDS replaces the content in place, through the same path.
        write_atomic(&dir, &target, b"{\"next\":true}").expect("a writable dir");
        assert_eq!(std::fs::read(&target).unwrap(), b"{\"next\":true}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// RAII pin of one process-global variable, restored on drop. Hold `ENV_LOCK` (write) first
    /// and declare the pin AFTER the lock guard so it restores before the lock releases.
    /// `#[cfg(unix)]` to match its only callers — the recording-bridge tests are Unix-only (the
    /// bridge is a shell script), so on Windows this would otherwise be dead code (`-D warnings`;
    /// the round-4 Windows job failed exactly here).
    #[cfg(unix)]
    struct EnvPin {
        key: &'static str,
        prev: Option<std::ffi::OsString>,
    }
    #[cfg(unix)]
    impl EnvPin {
        fn set(key: &'static str, value: &std::path::Path) -> Self {
            let prev = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, prev }
        }
        /// Pin the variable UNSET (restored on drop) — a scenario that needs "no snapshot on the
        /// ladder" cannot rely on the developer's or CI's environment not carrying one.
        fn unset(key: &'static str) -> Self {
            let prev = std::env::var_os(key);
            std::env::remove_var(key);
            Self { key, prev }
        }
    }
    #[cfg(unix)]
    impl Drop for EnvPin {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }

    /// The JSON lines a recording bridge appended to its ledger (empty when it never ran).
    /// `#[cfg(unix)]` for the same reason as [`EnvPin`]: only the Unix-only bridge tests read one.
    #[cfg(unix)]
    fn ledger_entries(path: &std::path::Path) -> Vec<Value> {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).expect("ledger line is JSON"))
            .collect()
    }

    /// (codex round 4) The shared worker home's temp sweep removes only THIS process's leftover
    /// settings temps — `settings.json.<pid>.<seq>.tmp` carrying our pid — never another engine
    /// process's (its in-flight write, which its own rename is about to consume), never the
    /// settings file itself, and never a file of another shape; and `write_atomic` leaves no temp
    /// of ours behind after its rename.
    #[test]
    fn the_settings_temp_sweep_removes_only_this_process_s_temps() {
        let dir = scratch("tmp-sweep");
        let me = std::process::id();
        let other = if me == u32::MAX { me - 1 } else { me + 1 };
        let ours = dir.join(settings_temp_name(SETTINGS_FILENAME, me, 7));
        let theirs = dir.join(settings_temp_name(SETTINGS_FILENAME, other, 0));
        let legacy = dir.join(".settings.json.tmp-1-2");
        let settings = dir.join(SETTINGS_FILENAME);
        for p in [&ours, &theirs, &legacy, &settings] {
            std::fs::write(p, b"{}").unwrap();
        }
        assert_eq!(
            settings_temp_pid(
                ours.file_name().unwrap().to_str().unwrap(),
                SETTINGS_FILENAME
            ),
            Some(me)
        );
        assert_eq!(
            settings_temp_pid(
                theirs.file_name().unwrap().to_str().unwrap(),
                SETTINGS_FILENAME
            ),
            Some(other)
        );
        for not_ours in [
            "settings.json",
            ".settings.json.tmp-1-2",
            "settings.json.12.x.tmp",
            "settings.json.12.tmp",
            "settings.json.abc.1.tmp",
            "other.json.12.1.tmp",
        ] {
            assert_eq!(
                settings_temp_pid(not_ours, SETTINGS_FILENAME),
                None,
                "{not_ours}"
            );
        }
        sweep_own_settings_temps(&dir).expect("sweep");
        // (codex round 9) a home that cannot be listed is an error carrying the cause, never `Ok`.
        let err = sweep_own_settings_temps(&settings).expect_err("a file cannot be listed");
        assert!(
            err.to_string().contains("cannot list the worker home"),
            "{err}"
        );
        assert!(!ours.exists(), "our leftover is swept");
        assert!(
            theirs.exists(),
            "another process's in-flight temp is never unlinked"
        );
        assert!(legacy.exists(), "a foreign shape is left alone");
        assert!(settings.exists());
        write_atomic(&dir, &settings, b"{\"x\":1}").expect("atomic write");
        assert_eq!(std::fs::read(&settings).unwrap(), b"{\"x\":1}");
        let leftovers: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| settings_temp_pid(n, SETTINGS_FILENAME) == Some(me))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        assert!(theirs.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// (v3.1 §3 × codex round 3) Per-session settings storage is COLLISION-FREE: launches whose
    /// ids sanitize to the SAME stem (`campaign:one` and `campaign_one` both become
    /// `campaign_one`), released together on one barrier, each get their own directory under the
    /// worker home's `sessions/`, each file holds exactly its own launch's rules, and every
    /// directory still exists when all are done — no launch removes another's. The exclusive
    /// create retries a taken name with a fresh suffix, leaving the taken directory alone, and
    /// gives up loudly rather than spinning.
    #[test]
    fn session_settings_dirs_never_collide_across_concurrent_launches_with_the_same_stem() {
        let home = scratch("session-dirs");
        let n = 8usize;
        let barrier = std::sync::Barrier::new(2 * n);
        let outcomes: Vec<(String, std::path::PathBuf)> = std::thread::scope(|s| {
            let mut handles = Vec::with_capacity(2 * n);
            for i in 0..n {
                for (run, tag) in [("campaign:one", "a"), ("campaign_one", "b")] {
                    let (home, barrier) = (home.clone(), &barrier);
                    handles.push(s.spawn(move || {
                        let rule = format!("Read(/{tag}/{i})");
                        barrier.wait();
                        let path = write_session_settings(
                            &home,
                            run,
                            "claude",
                            std::slice::from_ref(&rule),
                        )
                        .expect("each launch writes its own settings");
                        (rule, path)
                    }));
                }
            }
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert_eq!(outcomes.len(), 2 * n);
        let dirs: std::collections::BTreeSet<&std::path::Path> = outcomes
            .iter()
            .map(|(_, p)| p.parent().expect("settings.json has a dir"))
            .collect();
        assert_eq!(
            dirs.len(),
            2 * n,
            "every launch has its own directory: {outcomes:?}"
        );
        for (rule, path) in &outcomes {
            let name = path
                .parent()
                .unwrap()
                .file_name()
                .unwrap()
                .to_str()
                .unwrap();
            assert!(
                name.starts_with("campaign_one-claude-")
                    && name.contains(&format!("-{}-", std::process::id())),
                "the stem collides on purpose and the suffix is per process: {name}"
            );
            let held: Value = serde_json::from_slice(
                &std::fs::read(path).expect("every file survives every other launch"),
            )
            .unwrap();
            assert_eq!(
                held["permissions"]["deny"],
                serde_json::json!([rule]),
                "{}: holds its OWN launch's rules",
                path.display()
            );
        }
        // The exclusive create: a taken name is retried with a fresh suffix and never touched;
        // a name that stays taken fails loudly after a bounded number of attempts.
        let sessions = home.join(SESSIONS_DIRNAME);
        std::fs::create_dir_all(sessions.join("stem-taken")).unwrap();
        std::fs::write(sessions.join("stem-taken").join("settings.json"), b"theirs").unwrap();
        let mut suffixes = vec![
            "fresh".to_string(),
            "taken".to_string(),
            "taken".to_string(),
        ];
        let dir = create_session_dir(&sessions, "stem", &mut || suffixes.pop().unwrap())
            .expect("a fresh suffix is found");
        assert_eq!(dir, sessions.join("stem-fresh"));
        assert_eq!(
            std::fs::read(sessions.join("stem-taken").join("settings.json")).unwrap(),
            b"theirs",
            "a taken directory belongs to someone else and is never touched"
        );
        let err = create_session_dir(&sessions, "stem", &mut || "taken".to_string())
            .expect_err("bounded");
        assert!(err.to_string().contains("64 attempts"), "{err}");
        assert_eq!(
            session_dir_stem("campaign:one", "claude"),
            session_dir_stem("campaign_one", "claude"),
            "the stem alone WOULD collide — which is why the suffix exists"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// The owner — and only the owner — reaps its per-session directory, on drop: two processes
    /// started for the SAME (run, cli) key hold two different directories; dropping the first
    /// removes exactly its own and leaves the second's settings intact for the bridge still
    /// reading them; dropping the second removes the second's.
    #[test]
    #[cfg(unix)]
    fn a_dropped_acp_process_reaps_only_its_own_session_dir() {
        let _env = ENV_LOCK.read().unwrap_or_else(|p| p.into_inner());
        let _serial = REAL_STARTS.lock().unwrap_or_else(|p| p.into_inner());
        let dir = scratch("session-owner");
        let script = write_stub(
            &dir,
            "#!/bin/sh\nread _init\nprintf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}'\nread _new\nprintf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"sessionId\":\"s\"}}'\ncat >/dev/null\n",
        );
        let start = || {
            start_acp_process_with_write_roots(
                &stub_config(&script, None),
                &dir,
                None,
                None,
                &[],
                &[],
                &crate::skills_snapshot::SkillsDelivery::None,
                Some(("run:x", "claude")),
            )
            .expect("start")
        };
        let first = start();
        let second = start();
        let d1 = first
            .session_dir
            .clone()
            .expect("a unit session has its own settings directory");
        let d2 = second.session_dir.clone().unwrap();
        assert_ne!(d1, d2, "the same key, two launches, two directories");
        assert!(d1
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("run_x-claude-"));
        assert!(d1.join("settings.json").is_file() && d2.join("settings.json").is_file());
        drop(first);
        assert!(!d1.exists(), "the owner reaped its own directory on drop");
        assert!(
            d2.join("settings.json").is_file(),
            "the other launch's settings are untouched"
        );
        drop(second);
        assert!(!d2.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// (v3.2 × codex round 3, the ACP carrier's chokepoint) A malformed `OPENCODE_CONFIG_CONTENT`
    /// on the seat FAILS the spawn naming the variable — the bridge is never started — instead of
    /// composing the skills paths onto a bare document; a well-formed object composes, and the
    /// bridge starts with the COMPOSED value in its environment.
    #[test]
    #[cfg(unix)]
    fn a_malformed_opencode_config_fails_the_acp_spawn_before_the_bridge_starts() {
        use crate::skills_snapshot::{SkillsDelivery, OPENCODE_CONFIG_ENV};
        let _env = ENV_LOCK.read().unwrap_or_else(|p| p.into_inner());
        let _serial = REAL_STARTS.lock().unwrap_or_else(|p| p.into_inner());
        let dir = scratch("opencode-acp");
        let marker = dir.join("started");
        let env_dump = dir.join("env.txt");
        let script = write_stub(
            &dir,
            &format!(
                "#!/bin/sh\ntouch \"{marker}\"\nprintf '%s' \"$OPENCODE_CONFIG_CONTENT\" > \"{dump}\"\nread _init\nprintf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{{}}}}'\nread _new\nprintf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{{\"sessionId\":\"s\"}}}}'\ncat >/dev/null\n",
                marker = marker.display(),
                dump = env_dump.display()
            ),
        );
        let skill_dir = dir.join("skills").join("domain");
        let delivery = SkillsDelivery::OpencodeConfig(vec![skill_dir.clone()]);
        let mut config = stub_config(&script, None);
        config.acp_governance_env = Some((OPENCODE_CONFIG_ENV.to_string(), "not json".to_string()));
        let err = start_acp_process_with_write_roots(
            &config,
            &dir,
            None,
            None,
            &[],
            &[],
            &delivery,
            Some(("r", "opencode")),
        )
        .err()
        .expect("a malformed base refuses the spawn");
        let msg = err.to_string();
        assert!(
            msg.contains(OPENCODE_CONFIG_ENV) && msg.contains("not valid JSON"),
            "{msg}"
        );
        assert!(
            !marker.exists(),
            "the bridge must never start on a malformed governance value"
        );
        config.acp_governance_env = Some((
            OPENCODE_CONFIG_ENV.to_string(),
            r#"{"permission":{"read":"ask"}}"#.to_string(),
        ));
        let proc = start_acp_process_with_write_roots(
            &config,
            &dir,
            None,
            None,
            &[],
            &[],
            &delivery,
            Some(("r", "opencode")),
        )
        .expect("a JSON object composes");
        assert!(marker.exists());
        let composed: Value =
            serde_json::from_str(&std::fs::read_to_string(&env_dump).unwrap()).unwrap();
        assert_eq!(
            composed["permission"]["read"], "ask",
            "governance kept: {composed}"
        );
        assert_eq!(
            composed["skills"]["paths"],
            serde_json::json!([skill_dir.to_string_lossy()]),
            "skills composed in: {composed}"
        );
        drop(proc);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// (v3.2 × codex round 3, the ACP carrier through `run_unit`) The malformed value is a LAUNCH
    /// ERROR — the unit is refused naming the variable and the seat, no frame reaches the bridge,
    /// and no wrapped fallback runs — not a spawn failure that would fall back to the wrapped
    /// carrier and fail there with the same defect; with a well-formed value the same unit runs
    /// over ACP. The recording bridge is named `opencode`: the carrier's stem selects the lever.
    #[test]
    #[cfg(unix)]
    fn a_malformed_opencode_config_refuses_the_acp_unit_instead_of_falling_back() {
        use crate::skills_snapshot::test_support::{
            gen_dir, scratch as canonical_scratch, snapshot_root,
        };
        use crate::skills_snapshot::OPENCODE_CONFIG_ENV;
        use crate::workflow::{StepInput, StepRunner};
        let _env = ENV_LOCK.write().unwrap_or_else(|p| p.into_inner());
        let _serial = REAL_STARTS.lock().unwrap_or_else(|p| p.into_inner());
        let home = canonical_scratch("acp-opencode");
        let _home = EnvPin::set("HOME", &home);
        let snapshot = snapshot_root(
            &gen_dir(&home.join(".wicked-crew"), "1"),
            "1",
            &[("domain", "wicked-garden-domain")],
        );
        let _snap = EnvPin::set(crate::skills_snapshot::SKILLS_SNAPSHOT_ENV, &snapshot);
        let ledger = home.join("ledger.ndjson");
        let bridge_src = write_recording_bridge(&home);
        let bin = home.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let bridge = bin.join("opencode");
        std::fs::rename(&bridge_src, &bridge).unwrap();
        let council = home.join(".config").join("wicked-council");
        std::fs::create_dir_all(&council).unwrap();
        std::fs::write(
            council.join("clis.toml"),
            format!(
                r#"
[[cli]]
key = "opencode-seat"
display_name = "opencode seat"
binary = "opencode"
headless_invocation = "opencode run {{PROMPT}}"

[cli.acp]
binary = "{bridge}"
start_args = ["{ledger}"]
transport = "stdio"
"#,
                bridge = bridge.display(),
                ledger = ledger.display(),
            ),
        )
        .unwrap();
        let wt = home.join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        let (tx, _rx) = std::sync::mpsc::channel();
        let runner = AcpStepRunner::new(tx);
        let input = {
            let mut u = crate::domain::WorkUnit::pending("run-oc:u1", "run-oc", 1, "do the thing");
            u.assigned_cli = Some("opencode-seat".to_string());
            u.skill_ref = Some("wicked-garden-domain".to_string());
            StepInput {
                run_id: "run-oc".to_string(),
                unit_ix: 0,
                attempt: 0,
                unit: u,
                workflow_id: "wf-oc".to_string(),
                entity_mode: crate::scope::EntityMode::Isolated,
                workdir: Some(wt.clone()),
                governance: None,
                prior_outputs: vec![],
                elicitation_epoch: 0,
                process_gen: None,
                launch_seq: 0,
                required_skills: Vec::new(),
            }
        };

        let bad = EnvPin::set(OPENCODE_CONFIG_ENV, std::path::Path::new("not json"));
        let out = runner.run_unit(&input);
        assert_eq!(out.status, StepStatus::Failed, "{}", out.output);
        assert!(
            out.output.contains(OPENCODE_CONFIG_ENV)
                && out.output.contains("'opencode-seat'")
                && out.output.contains("not valid JSON"),
            "{}",
            out.output
        );
        assert!(
            ledger_entries(&ledger).is_empty(),
            "no frame reached the bridge, and nothing fell back"
        );
        drop(bad);

        let _good = EnvPin::set(
            OPENCODE_CONFIG_ENV,
            std::path::Path::new(r#"{"permission":{"read":"ask"}}"#),
        );
        let out = runner.run_unit(&input);
        assert_eq!(out.status, StepStatus::Ok, "{}", out.output);
        assert_eq!(
            ledger_entries(&ledger).len(),
            2,
            "session/new + one prompt over ACP"
        );
        runner.drop_session("run-oc");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// core#396 — POSITIVE INVOCATION EVIDENCE THROUGH THE REAL ACP BRIDGE (codex round 3,
    /// finding 7). The wrapped half lives in `tests/skills_live.rs`; this half drives the real
    /// `claude-agent-acp` through the real `AcpStepRunner` and the real registry seat: a fixture
    /// snapshot (in a `crew-state/skills/snapshots/<gen>` shape, its state home classified by the
    /// fence) holds one skill whose `SKILL.md` instructs printing a unique marker; it is handed in
    /// `session/new` (`_meta.claudeCode.options.plugins`), the unit's `skill_ref` names it, the
    /// prompt asks for the marker — and the turn must end `Ok` WITH the marker in the streamed
    /// output. If the bridge ignored the plugins option, or the plugin did not load, or the skill
    /// was not invoked, the marker cannot appear.
    ///
    /// `#[ignore]`d: needs the bridge (`WICKED_SKILLS_LIVE_ACP_BRIDGE=<absolute path>`, else
    /// `claude-agent-acp` on PATH), the engine's OWN logged-in worker home
    /// (`~/.wicked-worker/claude` — never the operator's `~/.claude`), and network. Opt in:
    ///   WICKED_SKILLS_LIVE_TEST=1 cargo test the_real_acp_bridge -- --ignored --nocapture
    #[test]
    #[cfg(unix)]
    #[ignore = "launches the real claude-agent-acp; opt in with WICKED_SKILLS_LIVE_TEST=1 and run with --ignored"]
    fn the_real_acp_bridge_loads_the_snapshot_and_invokes_the_fixture_skill() {
        use crate::skills_snapshot::test_support::{
            gen_dir, scratch as canonical_scratch, skill_dir, snapshot_root,
        };
        use crate::workflow::{StepInput, StepRunner};
        if std::env::var_os("WICKED_SKILLS_LIVE_TEST").is_none() {
            eprintln!("SKIP: set WICKED_SKILLS_LIVE_TEST=1 to launch the real claude-agent-acp");
            return;
        }
        let on_path = |name: &str| {
            std::env::var_os("PATH").and_then(|p| {
                std::env::split_paths(&p)
                    .map(|d| d.join(name))
                    .find(|c| c.is_file())
            })
        };
        let Some(bridge) = std::env::var_os("WICKED_SKILLS_LIVE_ACP_BRIDGE")
            .map(std::path::PathBuf::from)
            .filter(|p| p.is_file())
            .or_else(|| on_path("claude-agent-acp"))
        else {
            eprintln!(
                "SKIP: no claude-agent-acp (WICKED_SKILLS_LIVE_ACP_BRIDGE unset, none on PATH)"
            );
            return;
        };
        // ISOLATED worker home (codex round 8): an operator-prepared, logged-in worker home named
        // by `WICKED_SKILLS_LIVE_WORKER_HOME` — NEVER the operator's real `~/.wicked-worker`,
        // which this test used to re-aim the engine at. Refused when it resolves there; skipped
        // when unset. The pre-main arming stays the fallback value restored afterwards (never
        // `remove_var` — see `hermetic_test_worker_home`).
        let Some(live_worker_home) = std::env::var_os("WICKED_SKILLS_LIVE_WORKER_HOME")
            .map(std::path::PathBuf::from)
            .filter(|p| p.is_dir())
        else {
            eprintln!(
                "SKIP: set WICKED_SKILLS_LIVE_WORKER_HOME to an isolated, logged-in worker home \
                 (never your real ~/.wicked-worker) to launch the live ACP test"
            );
            return;
        };
        if let Some(real) =
            std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".wicked-worker"))
        {
            let same = match (
                std::fs::canonicalize(&live_worker_home),
                std::fs::canonicalize(&real),
            ) {
                (Ok(a), Ok(b)) => a == b || a.starts_with(&b),
                _ => live_worker_home == real,
            };
            assert!(
                !same,
                "WICKED_SKILLS_LIVE_WORKER_HOME must not be (or lie under) the operator's real \
                 ~/.wicked-worker"
            );
        }
        let _env = ENV_LOCK.write().unwrap_or_else(|p| p.into_inner());
        let _serial = REAL_STARTS.lock().unwrap_or_else(|p| p.into_inner());
        let armed = std::env::var_os(wicked_apps_core::spawn::WORKER_HOME_ENV);
        std::env::set_var(wicked_apps_core::spawn::WORKER_HOME_ENV, &live_worker_home);
        // The bridge's directory FIRST on PATH, so the registry seat's bare `claude-agent-acp`
        // resolves to it exactly as the daemon's would.
        let prev_path = std::env::var_os("PATH");
        let mut paths = vec![bridge.parent().unwrap().to_path_buf()];
        paths.extend(std::env::split_paths(
            prev_path.as_deref().unwrap_or_default(),
        ));
        std::env::set_var("PATH", std::env::join_paths(paths).unwrap());
        let restore = |prev_path: Option<std::ffi::OsString>, armed: Option<std::ffi::OsString>| {
            match prev_path {
                Some(p) => std::env::set_var("PATH", p),
                None => std::env::remove_var("PATH"),
            }
            match armed {
                Some(v) => std::env::set_var(wicked_apps_core::spawn::WORKER_HOME_ENV, v),
                None => std::env::remove_var(wicked_apps_core::spawn::WORKER_HOME_ENV),
            }
        };

        const MARKER: &str = "WICKED-PROBE-MARKER-4f9c2e";
        let base = canonical_scratch("acp-live");
        let state = base.join("crew-state");
        let root = snapshot_root(
            &gen_dir(&state, "000001"),
            "000001",
            &[("wicked-probe", "wicked-garden-wicked-probe")],
        );
        std::fs::write(
            skill_dir(&root, "wicked-probe").join("SKILL.md"),
            format!(
                "---\nname: wicked-garden-wicked-probe\ndescription: A probe skill that exists only to prove the harness under test loaded and invoked it.\n---\n\n# wicked-probe\n\nWhen this skill is invoked, reply with exactly this marker on its own line and nothing else:\n\n{MARKER}\n"
            ),
        )
        .unwrap();
        let _snap = EnvPin::set(crate::skills_snapshot::SKILLS_SNAPSHOT_ENV, &root);
        let wt = base.join("wt");
        std::fs::create_dir_all(&wt).unwrap();

        let (tx, _rx) = std::sync::mpsc::channel();
        let runner = AcpStepRunner::new(tx);
        let mut u = crate::domain::WorkUnit::pending(
            "live-acp:u1",
            "live-acp",
            1,
            "Invoke the skill and print its marker. Output only what the skill tells you to output.",
        );
        u.assigned_cli = Some("claude".to_string());
        u.skill_ref = Some("wicked-garden-wicked-probe".to_string());
        let input = StepInput {
            run_id: "live-acp".to_string(),
            unit_ix: 0,
            attempt: 0,
            unit: u,
            workflow_id: "wf-live-acp".to_string(),
            entity_mode: crate::scope::EntityMode::Isolated,
            workdir: Some(wt),
            governance: None,
            prior_outputs: vec![],
            elicitation_epoch: 0,
            process_gen: None,
            launch_seq: 0,
            required_skills: Vec::new(),
        };
        let out = runner.run_unit(&input);
        eprintln!(
            "--- live ACP reply via {} (status {:?}) ---\n{}\n---",
            bridge.display(),
            out.status,
            out.output
        );
        runner.drop_session("live-acp");
        restore(prev_path, armed);
        assert_eq!(out.status, StepStatus::Ok, "{}", out.output);
        assert!(
            out.output.contains(MARKER),
            "the real bridge must load the plugin handed in session/new and invoke the skill; got: {}",
            out.output
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A multi-turn RECORDING bridge for `run_unit`-level tests: answers `initialize`; answers
    /// `session/new` and appends its params to the ledger (`{"new": …}`); answers every
    /// `session/prompt` with `end_turn` and appends the prompt's text (`{"prompt": …}`); any other
    /// request gets a JSON-RPC error. Exits on stdin EOF — no sleeps: the process lives exactly as
    /// long as the engine keeps its stdin open.
    #[cfg(unix)]
    fn write_recording_bridge(dir: &std::path::Path) -> std::path::PathBuf {
        let path = dir.join("recording-bridge");
        std::fs::write(
            &path,
            r#"#!/usr/bin/env python3
import sys, json
ledger = sys.argv[1]

def w(obj):
    print(json.dumps(obj), flush=True)

def record(entry):
    with open(ledger, "a") as f:
        f.write(json.dumps(entry) + "\n")

while True:
    line = sys.stdin.readline()
    if not line:
        break
    line = line.strip()
    if not line:
        continue
    try:
        req = json.loads(line)
    except Exception:
        continue
    method = req.get("method")
    if method == "initialize":
        w({"jsonrpc": "2.0", "id": req["id"], "result": {
            "protocolVersion": "2025-03-26", "capabilities": {},
            "serverInfo": {"name": "recording", "version": "0"}}})
    elif method == "session/new":
        record({"new": req.get("params")})
        w({"jsonrpc": "2.0", "id": req["id"], "result": {
            "sessionId": "recording-session", "protocolVersion": "2025-03-26"}})
    elif method == "session/prompt":
        blocks = (req.get("params") or {}).get("prompt") or []
        text = "".join(b.get("text", "") for b in blocks if isinstance(b, dict))
        record({"prompt": text})
        w({"jsonrpc": "2.0", "id": req["id"], "result": {
            "stopReason": "end_turn", "usage": {"inputTokens": 1, "outputTokens": 1}}})
    elif "id" in req and method:
        w({"jsonrpc": "2.0", "id": req["id"], "error": {"code": -32601, "message": "unknown"}})
"#,
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// A bridge for the quiesce test: like [`write_recording_bridge`], but every `session/prompt`
    /// also BACKGROUNDS a writer in the bridge's own process group — `sleep 30; printf x >>
    /// src/app.ts` in the unit's cwd — and records both pids, so a test can prove the whole group
    /// died with the unit.
    #[cfg(unix)]
    fn write_backgrounding_bridge(dir: &std::path::Path) -> std::path::PathBuf {
        let path = dir.join("backgrounding-bridge");
        std::fs::write(
            &path,
            r#"#!/usr/bin/env python3
import sys, json, os, subprocess
ledger = sys.argv[1]

def w(obj):
    print(json.dumps(obj), flush=True)

def record(entry):
    with open(ledger, "a") as f:
        f.write(json.dumps(entry) + "\n")

while True:
    line = sys.stdin.readline()
    if not line:
        break
    line = line.strip()
    if not line:
        continue
    try:
        req = json.loads(line)
    except Exception:
        continue
    method = req.get("method")
    if method == "initialize":
        w({"jsonrpc": "2.0", "id": req["id"], "result": {
            "protocolVersion": "2025-03-26", "capabilities": {},
            "serverInfo": {"name": "backgrounding", "version": "0"}}})
    elif method == "session/new":
        record({"new": True, "bridge_pid": os.getpid()})
        w({"jsonrpc": "2.0", "id": req["id"], "result": {
            "sessionId": "bg-session", "protocolVersion": "2025-03-26"}})
    elif method == "session/prompt":
        p = subprocess.Popen(["sh", "-c", "sleep 30; printf x >> src/app.ts"])
        record({"prompt": True, "bridge_pid": os.getpid(), "writer_pid": p.pid})
        w({"jsonrpc": "2.0", "id": req["id"], "result": {
            "stopReason": "end_turn", "usage": {"inputTokens": 1, "outputTokens": 1}}})
    elif "id" in req and method:
        w({"jsonrpc": "2.0", "id": req["id"], "error": {"code": -32601, "message": "unknown"}})
"#,
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// F-036 on the ACP carrier (adversarial review on #414): the ACP process is persistent per
    /// run, so (1) an `executes_code: false` unit must never be served by the process that served
    /// the creator's write turn — it gets a FRESH process — and (2) that process, its group and
    /// anything it backgrounded must be dead BEFORE `run_unit` returns, i.e. before the worker
    /// thread takes the worktree guard's final snapshot. Driven end to end through `run_unit`
    /// against a bridge that backgrounds `sleep 30; printf x >> src/app.ts` on every turn.
    #[test]
    #[cfg(unix)]
    fn an_evaluator_unit_never_reuses_the_creators_acp_process_and_its_own_dies_with_it() {
        use crate::skills_snapshot::test_support::scratch as canonical_scratch;
        use crate::workflow::{StepInput, StepRunner};

        let _env = ENV_LOCK.write().unwrap_or_else(|p| p.into_inner());
        let _serial = REAL_STARTS.lock().unwrap_or_else(|p| p.into_inner());
        let home = canonical_scratch("acp-quiesce");
        let _home = EnvPin::set("HOME", &home);
        let ledger = home.join("ledger.ndjson");
        let bridge = write_backgrounding_bridge(&home);
        let council = home.join(".config").join("wicked-council");
        std::fs::create_dir_all(&council).unwrap();
        std::fs::write(
            council.join("clis.toml"),
            format!(
                r#"
[[cli]]
key = "quiesce-seat"
display_name = "Quiesce seat"
binary = "claude"
headless_invocation = "claude -p \"{{PROMPT}}\""

[cli.acp]
binary = "{bridge}"
start_args = ["{ledger}"]
transport = "stdio"
"#,
                bridge = bridge.display(),
                ledger = ledger.display(),
            ),
        )
        .unwrap();
        let wt = home.join("wt");
        std::fs::create_dir_all(wt.join("src")).unwrap();
        let app = wt.join("src").join("app.ts");
        std::fs::write(&app, "export const a = 1;\n").unwrap();

        let (tx, _rx) = std::sync::mpsc::channel();
        let runner = AcpStepRunner::new(tx);
        let unit = |ord: u32, no_code: bool| -> StepInput {
            let mut u = crate::domain::WorkUnit::pending(
                format!("run-Q:u{ord}"),
                "run-Q",
                ord,
                "do the thing",
            );
            u.assigned_cli = Some("quiesce-seat".to_string());
            u.executes_code = !no_code;
            u.worktree_guarded = no_code;
            StepInput {
                run_id: "run-Q".to_string(),
                unit_ix: 0,
                attempt: 0,
                unit: u,
                workflow_id: "wf-quiesce".to_string(),
                entity_mode: crate::scope::EntityMode::Isolated,
                workdir: Some(wt.clone()),
                governance: None,
                prior_outputs: vec![],
                elicitation_epoch: 0,
                process_gen: None,
                launch_seq: 0,
                required_skills: Vec::new(),
            }
        };
        let pid_of = |v: &Value, key: &str| v[key].as_i64().expect(key) as i32;
        let dead_within = |pid: i32, wait: Duration| -> bool {
            let deadline = Instant::now() + wait;
            loop {
                // kill(pid, 0) probes existence; ESRCH once the kernel has reaped it.
                if unsafe { libc::kill(pid, 0) } != 0 {
                    return true;
                }
                if Instant::now() >= deadline {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        };

        // 1. The CREATOR's turn (a code phase): opens the run's process, backgrounds a writer.
        let out = runner.run_unit(&unit(1, false));
        assert_eq!(out.status, StepStatus::Ok, "{}", out.output);
        let e = ledger_entries(&ledger);
        assert_eq!(e.len(), 2, "session/new + one prompt: {e:?}");
        let bridge_a = pid_of(&e[0], "bridge_pid");
        let writer_a = pid_of(&e[1], "writer_pid");
        assert!(
            !dead_within(bridge_a, Duration::from_millis(0)),
            "the creator's process is cached and alive"
        );

        // 2. The EVALUATOR's turn (`executes_code: false`): must NOT reuse that process.
        let out = runner.run_unit(&unit(2, true));
        assert_eq!(out.status, StepStatus::Ok, "{}", out.output);
        let e = ledger_entries(&ledger);
        assert_eq!(
            e.len(),
            4,
            "a FRESH process: a second session/new before the second prompt: {e:?}"
        );
        assert_eq!(e[2]["new"], Value::Bool(true));
        let bridge_b = pid_of(&e[2], "bridge_pid");
        let writer_b = pid_of(&e[3], "writer_pid");
        assert_ne!(bridge_a, bridge_b, "the evaluator got its own process");

        // 3. Everything is dead BEFORE run_unit returned: the creator's process (closed on the
        //    posture switch) with its writer, and the evaluator's own process with its writer.
        for (what, pid) in [
            ("creator bridge", bridge_a),
            ("creator's backgrounded writer", writer_a),
            ("evaluator bridge", bridge_b),
            ("evaluator's backgrounded writer", writer_b),
        ] {
            assert!(
                dead_within(pid, Duration::from_secs(2)),
                "{what} (pid {pid}) survived the quiesce"
            );
        }
        // …and the tree the guard will snapshot is exactly what the seat left.
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(
            std::fs::read_to_string(&app).unwrap(),
            "export const a = 1;\n",
            "a backgrounded writer landed after the unit"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// core#396, the ACP session BINDING — end to end through `run_unit` against the recording
    /// bridge (the deterministic ACP-carrier coverage: a stdio JSON-RPC server that echoes the
    /// `session/new` params it received — no network, no auth, part of the normal suite), with
    /// `WICKED_SKILLS_SNAPSHOT` pointing at the CONCRETE generation path crew exports after
    /// resolving `current` (v3.4 §2 — a handed `current` link is refused at load):
    ///
    /// 1. turn 1 of run A opens the session on generation 1 — the `session/new` frame names gen 1's
    ///    root as `_meta.claudeCode.options.plugins == [{type: local, path}]`, MERGED beside the
    ///    session's own `disallowedTools` and `settings`, and the directive is DISCOVERED from gen
    ///    1's index;
    /// 2. crew publishes generation 2, flips `current`, and re-exports the concrete path of gen 2;
    /// 3. turn 2 of run A — the SAME cached session — still prompts from gen 1's index, still
    ///    ADMITS against gen 1 (turn 3 names a skill only gen 2 holds and is refused naming gen 1's
    ///    root — never reaching the bridge), and reports gen 1 only;
    /// 4. a NEW run B, spawned after the re-export, gets gen 2 — and its first turn runs
    ///    CONCURRENTLY with run A's next turn (barrier-released threads on the shared runner): two
    ///    live sessions on two generations, each prompting from its own index, one bridge each.
    ///
    /// Generation reporting: exactly one `SkillsSnapshotHanded` per session — (A, gen 1) and
    /// (B, gen 2) — and no event ever names (A, gen 2). The read-boundary surface of the binding
    /// is the same `bound` root fed to `assemble_read_roots` (see `exec_turn_inner`); ungoverned
    /// units carry no boundary, so it is not observable through this bridge.
    #[test]
    #[cfg(unix)]
    fn a_cached_acp_session_keeps_the_generation_it_was_opened_with_across_a_current_flip() {
        use crate::skills_snapshot::test_support::{scratch as canonical_scratch, snapshot_root};
        use crate::workflow::{StepInput, StepRunner};

        // ENV_LOCK (write: HOME and the snapshot variable are pinned) before REAL_STARTS — the
        // module's lock order. Pins are declared AFTER the lock guards so they restore first.
        let _env = ENV_LOCK.write().unwrap_or_else(|p| p.into_inner());
        let _serial = REAL_STARTS.lock().unwrap_or_else(|p| p.into_inner());
        let home = canonical_scratch("acp-bind");
        let _home = EnvPin::set("HOME", &home);
        let skills = home.join(".wicked-crew").join("skills");
        let gen1 = snapshot_root(
            &skills.join("snapshots").join("1"),
            "1",
            &[
                ("domain", "wicked-garden-domain"),
                ("mem", "wicked-garden-mem"),
            ],
        );
        // Generation 2 REMOVES `mem` (and adds `search`): the v3.1 §4 case — a cached session
        // pinned to gen 1 must keep using mem after `current` moves on.
        let gen2 = snapshot_root(
            &skills.join("snapshots").join("2"),
            "2",
            &[
                ("domain", "wicked-garden-domain"),
                ("search", "wicked-garden-search"),
            ],
        );
        // crew's `current` link exists beside `snapshots/` — and is NOT what the engine is handed:
        // crew resolves it and exports the concrete generation (v3.4 §2).
        let current = skills.join("current");
        std::os::unix::fs::symlink("snapshots/1", &current).unwrap();
        let _snap = EnvPin::set(crate::skills_snapshot::SKILLS_SNAPSHOT_ENV, &gen1);

        // A claude-binary seat over the recording bridge, through the real user-overlay seam.
        let ledger = home.join("ledger.ndjson");
        let bridge = write_recording_bridge(&home);
        let council = home.join(".config").join("wicked-council");
        std::fs::create_dir_all(&council).unwrap();
        std::fs::write(
            council.join("clis.toml"),
            format!(
                r#"
[[cli]]
key = "skills-seat"
display_name = "Skills seat"
binary = "claude"
headless_invocation = "claude -p \"{{PROMPT}}\""

[cli.acp]
binary = "{bridge}"
start_args = ["{ledger}"]
transport = "stdio"
"#,
                bridge = bridge.display(),
                ledger = ledger.display(),
            ),
        )
        .unwrap();
        let wt = home.join("wt");
        std::fs::create_dir_all(&wt).unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        let runner = AcpStepRunner::new(tx);
        let unit = |run: &str, ord: u32, skill: &str| -> StepInput {
            let mut u =
                crate::domain::WorkUnit::pending(format!("{run}:u{ord}"), run, ord, "do the thing");
            u.assigned_cli = Some("skills-seat".to_string());
            u.skill_ref = Some(skill.to_string());
            StepInput {
                run_id: run.to_string(),
                unit_ix: 0,
                attempt: 0,
                unit: u,
                workflow_id: "wf-skills".to_string(),
                entity_mode: crate::scope::EntityMode::Isolated,
                workdir: Some(wt.clone()),
                governance: None,
                prior_outputs: vec![],
                elicitation_epoch: 0,
                process_gen: None,
                launch_seq: 0,
                required_skills: Vec::new(),
            }
        };

        // 1. Turn 1 of run A opens the session on generation 1.
        let out = runner.run_unit(&unit("run-A", 1, "wicked-garden-domain"));
        assert_eq!(out.status, StepStatus::Ok, "{}", out.output);
        let entries = ledger_entries(&ledger);
        assert_eq!(entries.len(), 2, "session/new + one prompt: {entries:?}");
        assert_eq!(
            entries[0]["new"]["_meta"]["claudeCode"]["options"]["plugins"],
            serde_json::json!([{"type": "local", "path": gen1.to_string_lossy()}]),
            "the bridge received generation 1 as its local plugin: {}",
            entries[0]
        );
        assert!(
            entries[1]["prompt"]
                .as_str()
                .unwrap()
                .contains("Invoke your skill \"wicked-garden:domain\""),
            "{}",
            entries[1]
        );

        // 2. crew publishes generation 2, flips `current` and re-exports the CONCRETE path of gen 2
        //    to the engine (the link itself is never handed).
        std::fs::remove_file(&current).unwrap();
        std::os::unix::fs::symlink("snapshots/2", &current).unwrap();
        let _snap2 = EnvPin::set(crate::skills_snapshot::SKILLS_SNAPSHOT_ENV, &gen2);

        // 3a. Turn 2 of run A, the SAME session, names `mem` — which `current` (gen 2) no longer
        //     holds. v3.1 §4: the cached session is admitted against ITS pinned generation BEFORE
        //     any ambient resolution, so the turn SUCCEEDS (an ambient-first admission would have
        //     refused it against gen 2 without ever consulting the cache) and the directive still
        //     comes from gen 1's index.
        let out = runner.run_unit(&unit("run-A", 2, "wicked-garden-mem"));
        assert_eq!(
            out.status,
            StepStatus::Ok,
            "a cached session keeps a skill its pinned generation has even after current drops it: {}",
            out.output
        );
        let entries = ledger_entries(&ledger);
        assert_eq!(entries.len(), 3, "{entries:?}");
        let prompt = entries[2]["prompt"].as_str().unwrap();
        assert!(
            prompt.contains("\"wicked-garden:mem\""),
            "the reused session prompts from the generation it was opened with: {prompt}"
        );

        // 3b. Turn 3 of run A names a skill only gen 2 holds: refused against gen 1, by name,
        //     before any frame reaches the bridge.
        let out = runner.run_unit(&unit("run-A", 3, "wicked-garden-search"));
        assert_eq!(out.status, StepStatus::Failed, "{}", out.output);
        assert!(
            out.output.contains("wicked-garden-search")
                && out.output.contains(&gen1.display().to_string())
                && !out.output.contains(&gen2.display().to_string()),
            "admission on a reused session is judged against ITS generation: {}",
            out.output
        );
        assert_eq!(
            ledger_entries(&ledger).len(),
            3,
            "the refusal never reached the bridge"
        );

        // 4. Concurrently: run A's turn 4 (bound to gen 1) and run B's first turn (fresh: gen 2).
        let barrier = std::sync::Barrier::new(2);
        let (a, b) = std::thread::scope(|s| {
            let ta = s.spawn(|| {
                barrier.wait();
                runner.run_unit(&unit("run-A", 4, "wicked-garden-mem"))
            });
            let tb = s.spawn(|| {
                barrier.wait();
                runner.run_unit(&unit("run-B", 1, "wicked-garden-search"))
            });
            (ta.join().unwrap(), tb.join().unwrap())
        });
        assert_eq!(a.status, StepStatus::Ok, "{}", a.output);
        assert_eq!(b.status, StepStatus::Ok, "{}", b.output);
        let entries = ledger_entries(&ledger);
        let prompts: Vec<&str> = entries
            .iter()
            .filter_map(|e| e["prompt"].as_str())
            .collect();
        assert_eq!(prompts.len(), 4, "{entries:?}");
        assert!(
            prompts[2..]
                .iter()
                .any(|p| p.contains("\"wicked-garden:mem\"")),
            "run A still prompts from gen 1: {prompts:?}"
        );
        assert!(
            prompts[2..]
                .iter()
                .any(|p| p.contains("\"wicked-garden:search\"")),
            "run B prompts from gen 2: {prompts:?}"
        );
        let news: Vec<&Value> = entries.iter().filter(|e| e.get("new").is_some()).collect();
        assert_eq!(news.len(), 2, "one spawn per session: {entries:?}");
        assert_eq!(
            news[1]["new"]["_meta"]["claudeCode"]["options"]["plugins"][0]["path"],
            gen2.to_string_lossy().as_ref(),
            "the second session was opened on generation 2"
        );

        // v3.1 §3 — SESSION-SPECIFIC configuration, read off the frames the bridge received: each
        // session carries ITS snapshot as the plugin, ITS fence as `disallowedTools`, and ITS OWN
        // settings file (distinct per session, under the worker home's `sessions/`) holding the
        // same rules; the fence is the state-home REGISTRY (no blanket over `~/.wicked-crew`, so
        // the snapshot in the read slot is readable), never names the session's OWN generation
        // and — v3.3 §1 (codex round 4) — DOES deny the SIBLING generation by name, so two live
        // sessions on two generations each read exactly theirs (round 3 asserted "names no
        // snapshot", which left every sibling readable); and the SHARED worker-home
        // `settings.json` carries no state-home rule at all — nothing generation- or
        // launch-dependent lives in a file two launches share.
        let worker_home = worker_config_home().expect("worker home");
        let crew_blanket = format!("Read({}/**)", home.join(".wicked-crew").display());
        let mut settings_paths = Vec::new();
        for (frame, gen) in news.iter().zip([&gen1, &gen2]) {
            let sibling = if gen.as_path() == gen1.as_path() {
                &gen2
            } else {
                &gen1
            };
            let options = &frame["new"]["_meta"]["claudeCode"]["options"];
            assert_eq!(
                options["plugins"][0]["path"],
                gen.to_string_lossy().as_ref()
            );
            let deny: Vec<String> = options["disallowedTools"]
                .as_array()
                .expect("the fence rides the frame")
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect();
            let path = std::path::PathBuf::from(
                options["settings"]
                    .as_str()
                    .expect("a per-session settings path rides the frame"),
            );
            assert!(
                path.starts_with(worker_home.join("sessions")),
                "{}",
                path.display()
            );
            let file: Value = serde_json::from_slice(
                &std::fs::read(&path).expect("the session's settings file exists"),
            )
            .unwrap();
            let file_deny: Vec<String> = file["permissions"]["deny"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect();
            assert_eq!(file_deny, deny, "file and frame carry the same fence");
            assert!(
                !deny.contains(&crew_blanket),
                "no blanket over the state home: {deny:?}"
            );
            assert!(
                deny.iter().any(|r| r.contains("skills/effective")),
                "the registry rules fence the worker state: {deny:?}"
            );
            assert!(
                !deny.iter().any(|r| r.contains(&gen.display().to_string())),
                "no rule names the session's own generation: {deny:?}"
            );
            assert!(
                deny.contains(&format!("Read({})", sibling.display()))
                    && deny.contains(&format!("Read({}/**)", sibling.display())),
                "the sibling generation is denied by name (v3.3 §1): {deny:?}"
            );
            settings_paths.push(path);
        }
        assert_ne!(
            settings_paths[0], settings_paths[1],
            "one settings file per session"
        );
        let shared: Value =
            serde_json::from_slice(&std::fs::read(worker_home.join("settings.json")).unwrap())
                .unwrap();
        assert!(
            !shared["permissions"]["deny"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r.as_str().unwrap().contains(".wicked-crew")),
            "the shared worker-home file is launch-independent: {shared}"
        );

        // Generation reporting: one handoff per session, each naming its own generation; nothing
        // ever reports (A, gen 2).
        let handed: Vec<(String, Option<String>)> = rx
            .try_iter()
            .filter_map(|c| match c {
                Command::EmitEvent(CoreEvent::SkillsSnapshotHanded { session, gen, .. }) => {
                    Some((session, gen))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            handed,
            vec![
                ("run-A".to_string(), Some("1".to_string())),
                ("run-B".to_string(), Some("2".to_string())),
            ]
        );
        runner.drop_session("run-A");
        runner.drop_session("run-B");
        for p in &settings_paths {
            assert!(
                std::fs::symlink_metadata(p).is_err(),
                "{} is reaped with its run",
                p.display()
            );
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    /// codex round 4 — ONE admission policy for fresh and cached sessions (`admit_turn`), end to
    /// end through `run_unit` against the recording bridge; codex round 5 — the two KINDS of turn
    /// are told apart, and a cached session that never received a plugin is not admitted against
    /// one by resolving the ambient configuration. A session's FIRST turn names no skill; its
    /// second names one:
    ///
    /// 1. the inherit-config escape hatch bypasses NOTHING (codex round 6): (a) with NO root on the
    ///    ladder a skill-free session opens with nothing pinned and its skill-bearing turn 2 is
    ///    REFUSED (`NotDelivered`) exactly as without the hatch — rounds 2–5 admitted it onto the
    ///    operator's plugins and generated a directive; (b) with a snapshot handed, a hatch session
    ///    is OPENED ON IT (`plugins` in the handshake — the hatch inherits the operator's
    ///    configuration IN ADDITION to the snapshot), its skill turn carries the directive, and a
    ///    skill the generation lacks is refused by name;
    /// 2. with NO root on the ladder (no snapshot variable, no plugin cache under the pinned HOME
    ///    or the pinned, empty claude config dir) a skill-free session opens with nothing pinned;
    ///    then a snapshot APPEARS (`WICKED_SKILLS_SNAPSHOT` set) and the same session's
    ///    skill-bearing turn 2 is REFUSED naming the skill and advising a fresh session — round 4
    ///    resolved the ambient root here, admitted the turn, sent no new plugin handshake
    ///    (`proc.skills` stayed `None`) and still generated the invocation directive for a plugin
    ///    the bridge never loaded. The refusal never reaches the bridge; a skill-free turn 3 on
    ///    the same session still runs, still with nothing handed and no directive;
    /// 3. a FRESH session under the same, now-available snapshot opens PINNED to it — the plugin
    ///    handshake in `session/new` — even though its turn 1 invokes nothing; turn 2 is admitted
    ///    against that pinned generation with the directive discovered from its index; and a
    ///    skill the pinned generation lacks is still refused against IT, by name.
    #[test]
    #[cfg(unix)]
    fn a_cached_session_opened_without_a_snapshot_refuses_skill_turns_while_a_fresh_one_is_handed_it(
    ) {
        use crate::skills_snapshot::test_support::{scratch as canonical_scratch, snapshot_root};
        use crate::workflow::{StepInput, StepRunner};

        let _env = ENV_LOCK.write().unwrap_or_else(|p| p.into_inner());
        let _serial = REAL_STARTS.lock().unwrap_or_else(|p| p.into_inner());
        let home = canonical_scratch("acp-turns");
        let _home = EnvPin::set("HOME", &home);
        // The ladder, pinned hermetic: no snapshot variable (yet), an EMPTY claude config dir (the
        // live-cache rung looks under it, never under the developer's real one); the fixture's
        // state home derives from its shape (the one input, v3.4 §2).
        let _no_snap = EnvPin::unset(crate::skills_snapshot::SKILLS_SNAPSHOT_ENV);
        let config = home.join("claude-config");
        std::fs::create_dir_all(&config).unwrap();
        let _config = EnvPin::set(CLAUDE_CONFIG_DIR_ENV, &config);
        let skills = home.join(".wicked-crew").join("skills");
        let gen = snapshot_root(
            &skills.join("snapshots").join("000001"),
            "1",
            &[("domain", "wicked-garden-domain")],
        );

        let ledger = home.join("ledger.ndjson");
        let bridge = write_recording_bridge(&home);
        let council = home.join(".config").join("wicked-council");
        std::fs::create_dir_all(&council).unwrap();
        std::fs::write(
            council.join("clis.toml"),
            format!(
                r#"
[[cli]]
key = "skills-seat"
display_name = "Skills seat"
binary = "claude"
headless_invocation = "claude -p \"{{PROMPT}}\""

[cli.acp]
binary = "{bridge}"
start_args = ["{ledger}"]
transport = "stdio"
"#,
                bridge = bridge.display(),
                ledger = ledger.display(),
            ),
        )
        .unwrap();
        let wt = home.join("wt");
        std::fs::create_dir_all(&wt).unwrap();

        let (tx, _rx) = std::sync::mpsc::channel();
        let runner = AcpStepRunner::new(tx);
        let unit = |run: &str, ord: u32, skill: Option<&str>| -> StepInput {
            let mut u =
                crate::domain::WorkUnit::pending(format!("{run}:u{ord}"), run, ord, "do the thing");
            u.assigned_cli = Some("skills-seat".to_string());
            u.skill_ref = skill.map(str::to_string);
            StepInput {
                run_id: run.to_string(),
                unit_ix: 0,
                attempt: 0,
                unit: u,
                workflow_id: "wf-skills".to_string(),
                entity_mode: crate::scope::EntityMode::Isolated,
                workdir: Some(wt.clone()),
                governance: None,
                prior_outputs: vec![],
                elicitation_epoch: 0,
                process_gen: None,
                launch_seq: 0,
                required_skills: Vec::new(),
            }
        };

        // 1. The inherit-config escape hatch, pinned for runs I and H — it bypasses NOTHING
        //    (codex round 6).
        {
            let _hatch = EnvPin::set(
                crate::execute_wrapped::INHERIT_OPERATOR_CONFIG_ENV,
                std::path::Path::new("1"),
            );
            // (a) No root on the ladder: the skill-free session opens with nothing pinned, and
            //     its skill-bearing turn is REFUSED like any session that never received a
            //     plugin — never admitted onto the operator's plugins, no directive generated.
            let out = runner.run_unit(&unit("run-I", 1, None));
            assert_eq!(out.status, StepStatus::Ok, "{}", out.output);
            let entries = ledger_entries(&ledger);
            assert_eq!(entries.len(), 2, "session/new + one prompt: {entries:?}");
            assert!(
                entries[0]["new"]["_meta"]["claudeCode"]["options"]["plugins"].is_null(),
                "no root on the ladder ⇒ nothing handed, hatch or not: {}",
                entries[0]
            );
            let out = runner.run_unit(&unit("run-I", 2, Some("wicked-garden-domain")));
            assert_eq!(
                out.status,
                StepStatus::Failed,
                "under the hatch a session opened without a snapshot still refuses a skill it \
                 never loaded: {}",
                out.output
            );
            assert!(
                out.output.contains("wicked-garden-domain")
                    && out.output.contains("opened without a skills snapshot"),
                "{}",
                out.output
            );
            assert_eq!(
                ledger_entries(&ledger).len(),
                2,
                "the refusal never reached the bridge"
            );
            // (b) A snapshot handed: the hatch session is OPENED ON IT — the plugin handshake —
            //     and admitted against it; the operator's configuration is inherited IN
            //     ADDITION (no per-session fence rides the frame under the hatch).
            {
                let _snap = EnvPin::set(crate::skills_snapshot::SKILLS_SNAPSHOT_ENV, &gen);
                let out = runner.run_unit(&unit("run-H", 1, Some("wicked-garden-domain")));
                assert_eq!(out.status, StepStatus::Ok, "{}", out.output);
                let entries = ledger_entries(&ledger);
                assert_eq!(entries.len(), 4, "{entries:?}");
                assert_eq!(
                    entries[2]["new"]["_meta"]["claudeCode"]["options"]["plugins"],
                    serde_json::json!([{"type": "local", "path": gen.to_string_lossy()}]),
                    "under the hatch the snapshot is still handed at session/new: {}",
                    entries[2]
                );
                // codex round 8: the hatch inherits the operator's SCOPES (no engine
                // `settingSources` override) — never the fence, which rides the frame under it.
                assert!(
                    entries[2]["new"]["_meta"]["claudeCode"]["options"]["disallowedTools"]
                        .as_array()
                        .is_some_and(|d| !d.is_empty()),
                    "the deny fence rides the frame even under the hatch: {}",
                    entries[2]
                );
                assert!(
                    entries[2]["new"]["_meta"]["claudeCode"]["options"]["settingSources"].is_null(),
                    "under the hatch the operator's scopes are inherited (no engine override): {}",
                    entries[2]
                );
                assert!(
                    entries[3]["prompt"]
                        .as_str()
                        .unwrap()
                        .contains("Invoke your skill \"wicked-garden:domain\""),
                    "{}",
                    entries[3]
                );
                let out = runner.run_unit(&unit("run-H", 2, Some("wicked-garden-search")));
                assert_eq!(out.status, StepStatus::Failed, "{}", out.output);
                assert!(
                    out.output.contains("wicked-garden-search")
                        && out.output.contains(&gen.display().to_string()),
                    "under the hatch a missing skill is refused by name against the pinned \
                     generation: {}",
                    out.output
                );
                assert_eq!(ledger_entries(&ledger).len(), 4);
            }
        }

        // 2. NO root on the ladder: run N opens skill-free with nothing pinned — no plugin in the
        //    handshake — and the session is cached.
        let out = runner.run_unit(&unit("run-N", 1, None));
        assert_eq!(out.status, StepStatus::Ok, "{}", out.output);
        let entries = ledger_entries(&ledger);
        assert_eq!(entries.len(), 6, "session/new + one prompt: {entries:?}");
        assert!(
            entries[4]["new"]["_meta"]["claudeCode"]["options"]["plugins"].is_null(),
            "no root on the ladder ⇒ no plugin handed at session/new: {}",
            entries[4]
        );
        // The snapshot APPEARS. The cached session for run N never received it and cannot now:
        // its skill-bearing turn is refused naming the skill, advising a fresh session — never
        // admitted off the ambient root, and no directive is generated for a plugin the bridge
        // never loaded. The refusal never reaches the bridge.
        let _snap = EnvPin::set(crate::skills_snapshot::SKILLS_SNAPSHOT_ENV, &gen);
        let out = runner.run_unit(&unit("run-N", 2, Some("wicked-garden-domain")));
        assert_eq!(
            out.status,
            StepStatus::Failed,
            "a session opened without a snapshot is not admitted for a skill it never loaded: {}",
            out.output
        );
        assert!(
            out.output.contains("wicked-garden-domain")
                && out.output.contains("opened without a skills snapshot")
                && out.output.contains("fresh session")
                && !out.output.contains(&gen.display().to_string()),
            "names the skill, advises a fresh session, never names the ambient root: {}",
            out.output
        );
        assert_eq!(
            ledger_entries(&ledger).len(),
            6,
            "the refusal never reached the bridge — no prompt, no directive"
        );
        // A skill-free turn on that same session still runs — nothing handed, no directive.
        let out = runner.run_unit(&unit("run-N", 3, None));
        assert_eq!(out.status, StepStatus::Ok, "{}", out.output);
        let entries = ledger_entries(&ledger);
        assert_eq!(entries.len(), 7, "{entries:?}");
        assert!(
            !entries[6]["prompt"]
                .as_str()
                .unwrap()
                .contains("Invoke your skill"),
            "a skill-free turn carries no directive: {}",
            entries[6]
        );

        // 3. A FRESH session under the same snapshot: run S opens PINNED to the generation — the
        //    plugin handshake — with a turn that invokes nothing.
        let out = runner.run_unit(&unit("run-S", 1, None));
        assert_eq!(out.status, StepStatus::Ok, "{}", out.output);
        let entries = ledger_entries(&ledger);
        assert_eq!(entries.len(), 9, "{entries:?}");
        assert_eq!(
            entries[7]["new"]["_meta"]["claudeCode"]["options"]["plugins"],
            serde_json::json!([{"type": "local", "path": gen.to_string_lossy()}]),
            "the fresh session is opened on the generation even though turn 1 invokes nothing: {}",
            entries[7]
        );
        let out = runner.run_unit(&unit("run-S", 2, Some("wicked-garden-domain")));
        assert_eq!(out.status, StepStatus::Ok, "{}", out.output);
        let entries = ledger_entries(&ledger);
        assert_eq!(entries.len(), 10, "{entries:?}");
        assert!(
            entries[9]["prompt"]
                .as_str()
                .unwrap()
                .contains("\"wicked-garden:domain\""),
            "the directive is discovered from the pinned generation's index: {}",
            entries[9]
        );
        let out = runner.run_unit(&unit("run-S", 3, Some("wicked-garden-search")));
        assert_eq!(out.status, StepStatus::Failed, "{}", out.output);
        assert!(
            out.output.contains("wicked-garden-search")
                && out.output.contains(&gen.display().to_string()),
            "a skill the pinned generation lacks is refused against IT: {}",
            out.output
        );
        assert_eq!(
            ledger_entries(&ledger).len(),
            10,
            "the refusal never reached the bridge"
        );
        runner.on_run_complete("run-I");
        runner.on_run_complete("run-H");
        runner.on_run_complete("run-N");
        runner.on_run_complete("run-S");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// The escape hatch: `WICKED_WORKER_INHERIT_OPERATOR_CONFIG` set means NO override — the one
    /// legitimate case is an operator deliberately testing their own hooks/skills through a run.
    /// Tested on the decision function (both branches); the call site passing the REAL env
    /// presence is pinned by the source audit below.
    ///
    /// TEST-ONLY RACE FIX (found while adding the core#293 regression tests, which changed the
    /// scheduling enough to surface it ~25% of runs): this test MINTS a worker home and then
    /// deletes it, but took no ENV_LOCK. `worker_config_home()` resolves `WICKED_WORKER_HOME` at
    /// call time, so whenever it interleaved with `an_acp_worker_does_not_inherit_the_operators_
    /// claude_config_dir` — which sets that variable — the mint resolved to THAT test's home and
    /// the cleanup below removed the `settings.json` it was mid-assertion on. Taking the lock and
    /// scoping the home to this test fixes both halves, and stops the cleanup deleting the
    /// developer's real `~/.wicked-worker/claude` (and its login state) as a side effect.
    #[test]
    fn the_inherit_escape_hatch_disables_acp_config_isolation() {
        let _env = ENV_LOCK.write().unwrap_or_else(|p| p.into_inner());
        let base = worker_home_base("inherit-hatch");
        std::env::set_var("WICKED_WORKER_HOME", &base);
        assert!(worker_claude_config_dir(true).is_none());
        let minted = worker_claude_config_dir(false)
            .expect("isolation is the default")
            .expect("minting succeeds");
        assert!(minted.is_dir());
        assert!(
            minted.starts_with(&base),
            "the mint must land in this test's scoped home, not a shared one: {}",
            minted.display()
        );
        restore_hermetic_worker_home();
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The call site must consult the SAME escape-hatch variable as the wrapped path and the
    /// ballot, read from the real environment — a hardcoded decision would pass the behavioural
    /// tests above while silently deleting the operator's opt-out. Since core#410 that read lives
    /// INSIDE the one shared resolver every seat spawn calls
    /// (`wicked_apps_core::spawn::seat_config_for`, which returns `Inherit` from
    /// `inherits_operator_config()` — the same reader `execute_wrapped::inherits_operator_config`
    /// and the skills admission delegate to), so the four cannot disagree; the audit pins the
    /// spawn's call to that resolver, keyed on the seat it resolved. Needle built by concatenation
    /// and matched on whitespace-stripped source so neither this test nor rustfmt can satisfy or
    /// break it.
    #[test]
    fn the_acp_spawn_consults_the_same_inherit_escape_hatch_as_the_wrapped_path() {
        let src: String = include_str!("acp_runner.rs")
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        let needle = format!("wicked_apps_core::spawn::{}(seat_cli)", "seat_config_for");
        assert!(
            src.contains(&needle),
            "start_acp_process no longer decides config isolation through the shared per-seat \
             resolver (which is where the wrapped path's escape-hatch variable is read)"
        );
        // And the resolver's `Inherit` arm IS the hatch: the same predicate the wrapped path reads.
        assert_eq!(
            matches!(
                wicked_apps_core::spawn::seat_config_for(wicked_apps_core::spawn::SeatCli::Codex)
                    .expect("this host's worker home resolves"),
                wicked_apps_core::spawn::SeatConfig::Inherit
            ),
            crate::execute_wrapped::inherits_operator_config()
        );
    }

    /// What the worker's user scope says after every re-sanitize. The deny list must be the
    /// SAME fence the wrapped path ships (not a diverging copy), and no `defaultMode` may be
    /// pinned: on ACP, governance rides `session/request_permission` (FINDING-062), and a mode
    /// that auto-approves edits could resolve them before our policy is ever asked.
    #[test]
    fn the_worker_home_seeds_the_deny_fence_and_pins_no_permission_mode() {
        let _g = ENV_LOCK.write().unwrap_or_else(|p| p.into_inner());
        let base = worker_home_base("fence");
        std::env::set_var("WICKED_WORKER_HOME", &base);
        let dir = ensure_worker_config_home().expect("ensure");
        restore_hermetic_worker_home();
        let settings: Value =
            serde_json::from_slice(&std::fs::read(dir.join("settings.json")).unwrap()).unwrap();
        let deny: Vec<String> = settings["permissions"]["deny"]
            .as_array()
            .expect("permissions.deny present")
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
        // v3.1 §3: the SHARED home carries the launch-independent fence only — every fenced
        // directory except the state home — built by the wrapped path's own function, not a copy
        // that can drift. The state-home rule rides each session's `session/new` options.
        assert_eq!(
            deny,
            crate::execute_wrapped::shared_deny_rules(None).unwrap(),
            "the shared ACP settings must be the launch-independent fence, not a copy that can drift"
        );
        assert!(
            !deny.iter().any(|r| r.contains(".wicked-crew")),
            "the state-home rule is per session, never in the shared file: {deny:?}"
        );
        assert!(deny.iter().any(|r| r.ends_with(".claude/**)")), "{deny:?}");
        assert!(
            settings["permissions"].get("defaultMode").is_none(),
            "a pinned mode that auto-approves would answer session/request_permission before \
             the governance gate sees it: {settings}"
        );
        drop(dir);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Copilot, #426: a re-open with a DIFFERENT scope evicts the chat's pool entries (the seats
    /// re-warm in the new scope), a re-open with the SAME scope leaves them alone — and a chat
    /// that ends up with no warm seat holds no scope, so nothing un-reapable is left behind.
    #[test]
    fn reopening_with_a_new_scope_evicts_the_old_seats_and_a_seatless_chat_holds_no_scope() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let r = AcpStepRunner::new(tx);
        let key = (AcpStepRunner::chat_pool_key("c1"), "claude".to_string());
        let scope_a = ChatScope {
            cwd: std::env::temp_dir().join("wicked-chat-scope-a"),
            code_graph_db: None,
            read_roots: vec![],
        };
        let scope_b = ChatScope {
            cwd: std::env::temp_dir().join("wicked-chat-scope-b"),
            ..scope_a.clone()
        };
        // A recorded scope A with a pool entry (None slot: constructing a live child is not the
        // point here). Re-open with the SAME scope and no seats to warm: the entry is untouched.
        r.sessions.lock().unwrap().insert(key.clone(), None);
        r.chat_scopes.lock().unwrap().insert(
            "c1".to_string(),
            RecordedScope {
                gen: 0,
                scope: scope_a.clone(),
            },
        );
        let _ = r.chat_open("c1", &[], scope_a.clone());
        assert!(
            r.sessions.lock().unwrap().contains_key(&key),
            "a same-scope re-open evicts nothing"
        );
        // Re-open with a DIFFERENT scope: the stale entry is evicted...
        r.chat_scopes.lock().unwrap().insert(
            "c1".to_string(),
            RecordedScope {
                gen: 0,
                scope: scope_a.clone(),
            },
        );
        let _ = r.chat_open("c1", &[], scope_b);
        assert!(
            !r.sessions.lock().unwrap().contains_key(&key),
            "a re-open with a different scope evicts the seats warmed in the old one"
        );
        // ...and since nothing warmed (no clis), no scope is held for a chat the pool forgot.
        assert!(r.chat_scopes.lock().unwrap().get("c1").is_none());
        assert!(r.chat_activity.lock().unwrap().get("c1").is_none());
        // Every seat failing to start (an unknown cli) ends the same way: outcome reported,
        // nothing held.
        let opened = r
            .chat_open("c2", &["no-such-cli-xyz".to_string()], scope_a.clone())
            .expect("a valid scope is accepted");
        assert!(opened[0].1.is_err());
        assert!(r.chat_scopes.lock().unwrap().get("c2").is_none());
        assert!(r.chat_list().is_empty());
        // Copilot, #426: the seatless cleanup drops only ITS OWN open's record — a newer open's
        // record (a higher generation) survives an older open finishing with no seats.
        r.chat_scopes.lock().unwrap().insert(
            "c3".to_string(),
            RecordedScope {
                gen: 7,
                scope: scope_a.clone(),
            },
        );
        r.drop_scope_if_gen("c3", 6);
        assert!(
            r.chat_scopes.lock().unwrap().get("c3").is_some(),
            "an older generation's cleanup leaves a newer record alone"
        );
        r.drop_scope_if_gen("c3", 7);
        assert!(r.chat_scopes.lock().unwrap().get("c3").is_none());
    }

    /// Independent review, C5: the daemon derives its scratch base from Node's `os.tmpdir()`,
    /// which may follow `TMP`/`TEMP` where Rust's `temp_dir()` does not — a scratch root under a
    /// set `TMP` is accepted, one under an arbitrary directory still refused.
    #[test]
    #[cfg(unix)]
    fn a_scratch_root_under_tmp_or_temp_is_accepted_as_a_temp_base() {
        let _env = ENV_LOCK.write().unwrap_or_else(|p| p.into_inner());
        // A root-level path that is under NO temp base (this checkout may itself live under /tmp,
        // so nothing beneath it can serve). Validation never requires the cwd to exist, so nothing
        // is created.
        let alt =
            std::path::PathBuf::from("/").join(format!("w2chat-not-a-temp-{}", std::process::id()));
        let (tx, _rx) = std::sync::mpsc::channel();
        let r = AcpStepRunner::new(tx);
        let scope = ChatScope {
            cwd: alt.join("chats").join("c1"),
            code_graph_db: None,
            read_roots: vec![],
        };
        let _unset_tmp = EnvPin::unset("TMP");
        let _unset_temp = EnvPin::unset("TEMP");
        let err = r
            .validate_chat_scope(&scope)
            .expect_err("outside every temp base");
        assert!(err.contains("system temp"), "{err}");
        let _tmp = EnvPin::set("TMP", &alt);
        r.validate_chat_scope(&scope)
            .expect("a root under $TMP is a temp root");
        assert!(!alt.exists(), "validation creates nothing");
    }

    /// Copilot, #426: a seat whose adapter asks no permissions and runs under no kernel floor
    /// cannot be held to a scoped chat's read-only roots — refused by name for SCOPED chats,
    /// admitted to unscoped ones; an admitted or sandboxed adapter joins either.
    #[test]
    fn a_permission_less_unsandboxed_seat_is_refused_for_a_scoped_chat_only() {
        let scoped = ChatScope {
            cwd: std::env::temp_dir().join("wicked-chat-adm"),
            code_graph_db: None,
            read_roots: vec![std::env::temp_dir()
                .join("repo")
                .to_string_lossy()
                .into_owned()],
        };
        let unscoped = ChatScope {
            read_roots: vec![],
            ..scoped.clone()
        };
        let mut cfg = AcpConfig {
            binary: "pi-acp".into(),
            start_args: vec![],
            transport: AcpTransport::default(),
            auth_method: None,
            acp_input_governance: false,
            os_sandbox: false,
            acp_governance_env: None,
            verified_version: None,
        };
        let err = scoped_seat_admission("pi", &scoped, &cfg).expect_err("refused");
        assert!(
            err.contains("pi") && err.contains("SCOPED") && err.contains("pi-acp"),
            "{err}"
        );
        assert!(
            scoped_seat_admission("pi", &unscoped, &cfg).is_ok(),
            "unscoped admits everyone"
        );
        cfg.os_sandbox = true;
        assert!(
            scoped_seat_admission("pi", &scoped, &cfg).is_ok(),
            "the kernel floor holds it"
        );
        cfg.os_sandbox = false;
        cfg.acp_input_governance = true;
        assert!(
            scoped_seat_admission("claude", &scoped, &cfg).is_ok(),
            "the boundary holds it"
        );
    }

    /// Copilot, #426: a scope is validated BEFORE it is recorded — relative roots, a missing graph
    /// and the engine's own store are refused, and nothing is held for a refused chat.
    #[test]
    fn a_chat_scope_is_validated_before_it_is_recorded() {
        let dir = scratch("chat-scope-validate");
        let state = dir.join("state");
        std::fs::create_dir_all(state.join("project-graphs").join("p1")).unwrap();
        std::fs::write(state.join("core.db"), b"").unwrap();
        let graph = state.join("project-graphs").join("p1").join("estate.db");
        std::fs::write(&graph, b"").unwrap();
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut r = AcpStepRunner::new(tx);
        r.operational_home = Some(state.clone());
        let base = ChatScope {
            cwd: dir.join("chats").join("c1"),
            code_graph_db: Some(graph.to_string_lossy().into_owned()),
            read_roots: vec![repo.to_string_lossy().into_owned()],
        };
        // Valid: accepted (no seats to warm → nothing held, but no error).
        assert!(r.chat_open("ok", &[], base.clone()).is_ok());
        // A relative read root.
        let err = r
            .chat_open(
                "rel",
                &[],
                ChatScope {
                    read_roots: vec!["repos/x".into()],
                    ..base.clone()
                },
            )
            .expect_err("relative root");
        assert!(err.contains("not absolute"), "{err}");
        // A relative scratch root.
        let err = r
            .chat_open(
                "relcwd",
                &[],
                ChatScope {
                    cwd: std::path::PathBuf::from("chats/c1"),
                    ..base.clone()
                },
            )
            .expect_err("relative cwd");
        assert!(err.contains("not absolute"), "{err}");
        // A graph that does not exist.
        let err = r
            .chat_open(
                "nograph",
                &[],
                ChatScope {
                    code_graph_db: Some(dir.join("missing.db").to_string_lossy().into_owned()),
                    ..base.clone()
                },
            )
            .expect_err("missing graph");
        assert!(err.contains("no code graph"), "{err}");
        // The engine's OWN store (a top-level file of its state home) is never a chat's graph.
        let err = r
            .chat_open(
                "opstore",
                &[],
                ChatScope {
                    code_graph_db: Some(state.join("core.db").to_string_lossy().into_owned()),
                    ..base.clone()
                },
            )
            .expect_err("operational store");
        assert!(err.contains("FINDING-067"), "{err}");
        // …nor reached through a link elsewhere (Copilot, #426): judged on the resolved file.
        #[cfg(unix)]
        {
            let link = dir.join("innocent-graph.db");
            std::os::unix::fs::symlink(state.join("core.db"), &link).unwrap();
            let err = r
                .chat_open(
                    "oplink",
                    &[],
                    ChatScope {
                        code_graph_db: Some(link.to_string_lossy().into_owned()),
                        ..base.clone()
                    },
                )
                .expect_err("symlink to the operational store");
            assert!(err.contains("FINDING-067"), "{err}");
            let hard = dir.join("hard-graph.db");
            std::fs::hard_link(state.join("core.db"), &hard).unwrap();
            let err = r
                .chat_open(
                    "ophard",
                    &[],
                    ChatScope {
                        code_graph_db: Some(hard.to_string_lossy().into_owned()),
                        ..base.clone()
                    },
                )
                .expect_err("hard link to the operational store");
            assert!(err.contains("FINDING-067"), "{err}");
        }
        // The scratch root must be a directory of its own under the system temp dir — never the
        // filesystem root (`/`, or `C:\` on Windows — spelled from the temp dir's own root so the
        // case is absolute on every platform), the temp dir itself, the state home or a home
        // directory (Copilot, #426).
        let fs_root = std::env::temp_dir()
            .ancestors()
            .last()
            .expect("a path has a root")
            .to_path_buf();
        for bad in [
            fs_root,
            std::env::temp_dir(),
            state.clone(),
            state.join("chats"),
        ] {
            let err = r
                .chat_open(
                    "badcwd",
                    &[],
                    ChatScope {
                        cwd: bad.clone(),
                        ..base.clone()
                    },
                )
                .expect_err("dangerous cwd");
            assert!(
                err.contains("system temp") || err.contains("state home"),
                "{}: {err}",
                bad.display()
            );
        }
        // A read root inside the engine's state home, or overlapping the scratch root, is refused.
        for (root, needle) in [
            (state.join("project-graphs"), "state home"),
            (base.cwd.clone(), "overlaps"),
            (base.cwd.parent().unwrap().to_path_buf(), "overlaps"),
        ] {
            let err = r
                .chat_open(
                    "badroot",
                    &[],
                    ChatScope {
                        read_roots: vec![root.to_string_lossy().into_owned()],
                        ..base.clone()
                    },
                )
                .expect_err("dangerous read root");
            assert!(err.contains(needle), "{}: {err}", root.display());
        }
        // Nothing was recorded for any refused chat.
        assert!(r.chat_scopes.lock().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Copilot, #426: a turn's eviction removes only the process IT ran on — a replacement warmed
    /// under the same key by a re-open with a new scope survives the old turn's cleanup.
    #[test]
    #[cfg(unix)]
    fn evicting_a_chat_seat_removes_only_the_process_the_turn_ran_on() {
        let _env = ENV_LOCK.read().unwrap_or_else(|p| p.into_inner());
        let _serial = REAL_STARTS.lock().unwrap_or_else(|p| p.into_inner());
        let dir = scratch("chat-evict-identity");
        let script = stub_idle_bridge(&dir);
        let a = Arc::new(Mutex::new(
            start_acp_process(&stub_config(&script, None), &dir, None, None).expect("a"),
        ));
        let b = Arc::new(Mutex::new(
            start_acp_process(&stub_config(&script, None), &dir, None, None).expect("b"),
        ));
        let (tx, _rx) = std::sync::mpsc::channel();
        let r = AcpStepRunner::new(tx);
        let key = (AcpStepRunner::chat_pool_key("c1"), "claude".to_string());
        r.sessions
            .lock()
            .unwrap()
            .insert(key.clone(), Some(Arc::clone(&a)));
        // The old turn (ran on `b`, since replaced) fails: the replacement `a` stays.
        r.chat_evict("c1", "claude", &b);
        assert!(r.sessions.lock().unwrap().contains_key(&key));
        // A turn on `a` itself fails: its own entry goes.
        r.chat_evict("c1", "claude", &a);
        assert!(!r.sessions.lock().unwrap().contains_key(&key));
        drop(a);
        drop(b);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Copilot, #426: a chat's read roots are READ-ONLY for every seat — a permission request to
    /// write under a scoped repository is answered with the agent's reject option, a read under it
    /// and a write in the scratch root with allow, and anything outside both is refused.
    #[test]
    fn a_chat_boundary_denies_writes_under_the_read_roots_and_anything_outside() {
        let dir = scratch("chat-boundary");
        let cwd = dir.join("scratch");
        let repo = dir.join("repo");
        let outside = dir.join("elsewhere");
        for d in [&cwd, &repo, &outside] {
            std::fs::create_dir_all(d).unwrap();
        }
        let scope = ChatScope {
            cwd: cwd.clone(),
            code_graph_db: None,
            read_roots: vec![repo.to_string_lossy().into_owned()],
        };
        let boundary = chat_boundary(&scope, wicked_apps_core::spawn::SeatCli::Codex);
        let request = |tool: &str, path: &std::path::Path| {
            json!({
                "sessionId": "s1",
                "toolName": tool,
                "toolCall": {"toolCallId": "t1", "rawInput": {
                    "file_path": path.to_string_lossy(), "content": "x"}},
                "options": [
                    {"optionId": "allow", "kind": "allow_once"},
                    {"optionId": "reject", "kind": "reject_once"},
                ],
            })
        };
        let answer = |tool: &str, path: &std::path::Path| {
            let (v, allowed) =
                crate::acp_permission::chat_boundary_result(&boundary, &request(tool, path));
            (
                v["outcome"]["optionId"].as_str().unwrap().to_string(),
                allowed,
            )
        };
        assert_eq!(
            answer("Write", &repo.join("src.rs")),
            ("reject".to_string(), false),
            "a write under a read root is refused"
        );
        assert_eq!(
            answer("Edit", &repo.join("src.rs")),
            ("reject".to_string(), false)
        );
        assert_eq!(
            answer("Read", &repo.join("src.rs")),
            ("allow".to_string(), true),
            "a read under a read root is allowed"
        );
        assert_eq!(
            answer("Write", &cwd.join("notes.md")),
            ("allow".to_string(), true),
            "the scratch root is writable"
        );
        assert_eq!(
            answer("Write", &outside.join("x")),
            ("reject".to_string(), false),
            "nothing outside the boundary is writable"
        );
        assert_eq!(
            answer("Read", &outside.join("x")),
            ("reject".to_string(), false),
            "nothing outside the boundary is readable either — the scope IS what the seats see"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Copilot, #426: distinct chat ids never share a default scratch root — the readable prefix
    /// is lossy, the hash of the full id is not.
    #[test]
    fn distinct_chat_ids_get_distinct_default_scratch_roots() {
        let roots: Vec<std::path::PathBuf> = ["a/b", "a_b", "a b", ".", "", "..", "x", "X"]
            .iter()
            .map(|id| ChatScope::scratch_for(id))
            .collect();
        for (i, a) in roots.iter().enumerate() {
            for b in &roots[i + 1..] {
                assert_ne!(a, b, "two ids collapsed onto one root");
            }
            assert!(a.is_absolute());
            let name = a.file_name().unwrap().to_string_lossy();
            assert!(name.starts_with("wicked-core-chat-"), "{name}");
            assert!(
                !name.contains('/') && !name.contains(' ') && !name.contains(".."),
                "the root's own name never spells a path: {name}"
            );
        }
        assert_eq!(
            ChatScope::scratch_for("same"),
            ChatScope::scratch_for("same"),
            "deterministic"
        );
    }

    /// Copilot, #426: the scratch root is never reached through a planted link — a symlink where
    /// the chat's cwd should be refuses the ensure before any seat spawns.
    #[test]
    #[cfg(unix)]
    fn a_planted_symlink_at_the_chat_scratch_root_refuses_the_seat() {
        let dir = scratch("chat-scratch-link");
        let operator_like = dir.join("operator-home");
        std::fs::create_dir_all(&operator_like).unwrap();
        let linked = dir.join("chat-cwd");
        std::os::unix::fs::symlink(&operator_like, &linked).unwrap();
        let err = ensure_chat_scratch_root(&linked).expect_err("a planted link is refused");
        assert!(err.contains("symlink"), "{err}");
        // A relative root would resolve against the daemon's cwd — refused (Copilot, #426).
        let err = ensure_chat_scratch_root(std::path::Path::new("chats/c1")).expect_err("relative");
        assert!(err.contains("relative"), "{err}");
        // A real (or not-yet-existing) directory is created private; an existing too-open one is
        // made private rather than left as found.
        let fresh = dir.join("fresh").join("nested");
        ensure_chat_scratch_root(&fresh).expect("creates");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&fresh).unwrap().permissions().mode() & 0o777,
            0o700
        );
        std::fs::set_permissions(&fresh, std::fs::Permissions::from_mode(0o755)).unwrap();
        ensure_chat_scratch_root(&fresh).expect("idempotent");
        assert_eq!(
            std::fs::metadata(&fresh).unwrap().permissions().mode() & 0o777,
            0o700,
            "privacy is enforced, not merely granted at creation"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn chat_pool_is_isolated_from_run_sessions_and_close_is_idempotent() {
        let (tx, rx) = std::sync::mpsc::channel();
        let r = AcpStepRunner::new(tx);
        // Poisoned run entry + poisoned chat entry (None = failed start; constructing a real
        // AcpProcess needs a live child, so pool-shape tests use the None slot).
        {
            let mut guard = r.sessions.lock().unwrap();
            guard.insert(("run1".into(), "claude".into()), None);
            guard.insert((AcpStepRunner::chat_pool_key("c1"), "claude".into()), None);
        }
        // Seats lists only WARM (Some) sessions — poisoned slots are not seats.
        assert!(r.chat_seats("c1").is_empty());
        // Dropping a RUN's sessions must not touch the chat pool, and vice versa.
        r.drop_session("run1");
        assert_eq!(r.sessions.lock().unwrap().len(), 1);
        r.chat_close("c1", ChatCloseReason::Requested);
        assert_eq!(r.sessions.lock().unwrap().len(), 0);
        r.chat_close("c1", ChatCloseReason::Requested); // idempotent
                                                        // Both closes emitted ChatClosed through
                                                        // the actor emit point, carrying the
                                                        // reason the caller asked for.
        let evs: Vec<_> = rx.try_iter().collect();
        let closed = evs
            .iter()
            .filter(|c| {
                matches!(c, Command::EmitEvent(CoreEvent::ChatClosed { chat, reason })
                    if chat == "c1" && reason == "requested")
            })
            .count();
        assert_eq!(closed, 2);
    }

    /// The TTL these tests reap against. Deliberately small: `Instant` is monotonic-since-boot, so
    /// every backdate below has to be representable on a host that just booted. Keeping the whole
    /// scale within `MAX_BACKDATE` seconds means these tests never depend on machine uptime.
    const TEST_TTL: Duration = Duration::from_secs(4);
    const MAX_BACKDATE: u64 = 10;

    /// An `Instant` `secs` in the past.
    ///
    /// `checked_sub` rather than `-`: subtracting past the start of the monotonic clock panics, and
    /// a bare panic here would read as a reaper bug rather than as a host with less uptime than the
    /// backdate. Every caller stays under `MAX_BACKDATE`, so the expect is unreachable in practice.
    fn backdated(secs: u64) -> Instant {
        debug_assert!(secs <= MAX_BACKDATE, "keep test backdates small");
        Instant::now()
            .checked_sub(Duration::from_secs(secs))
            .expect("monotonic clock older than the backdate (host uptime under 10s?)")
    }

    /// Put `chat_id` in the pool and backdate its last-touch by `idle` seconds.
    ///
    /// Backdating beats sleeping: the reaper's whole contract is about elapsed time, and a test
    /// that slept for it would be both slow and flaky. Constructing a real `AcpProcess` needs a
    /// live child, so — as in the pool-shape test above — the slot is `None`; `chat_list` counts
    /// pool ENTRIES, which is what the reaper acts on.
    fn seed_chat(r: &AcpStepRunner, chat_id: &str, idle: u64) {
        r.sessions.lock().unwrap().insert(
            (AcpStepRunner::chat_pool_key(chat_id), "claude".into()),
            None,
        );
        r.chat_activity
            .lock()
            .unwrap()
            .insert(chat_id.to_string(), backdated(idle));
    }

    fn closed_chats(rx: &std::sync::mpsc::Receiver<Command>) -> Vec<(String, String)> {
        rx.try_iter()
            .filter_map(|c| match c {
                Command::EmitEvent(CoreEvent::ChatClosed { chat, reason }) => Some((chat, reason)),
                _ => None,
            })
            .collect()
    }

    /// The core of FINDING-027: nothing ever reclaimed an abandoned chat, so ~520 MB per seat
    /// stayed pinned for the daemon's lifetime. A chat past the TTL must go; one inside it must
    /// not, or an operator loses a session they are still using.
    #[test]
    fn idle_chats_are_reaped_and_active_ones_are_left_alone() {
        let (tx, rx) = std::sync::mpsc::channel();
        let r = AcpStepRunner::new(tx);
        seed_chat(&r, "stale", MAX_BACKDATE);
        seed_chat(&r, "fresh", 0);

        let reaped = r.chat_reap_idle(TEST_TTL);

        assert_eq!(reaped, vec!["stale".to_string()]);
        assert_eq!(
            r.chat_list().iter().map(|c| &c.chat_id).collect::<Vec<_>>(),
            vec!["fresh"],
            "the chat inside its TTL must survive"
        );
        assert_eq!(
            closed_chats(&rx),
            vec![("stale".to_string(), "idle".to_string())],
            "a reclaim must be distinguishable from an operator's own close"
        );
    }

    /// A touch is what proves a chat is still in use, and `chat_ensure` is the funnel every use
    /// passes through (`chat_turn` calls it too). Without the touch there, a chat being actively
    /// talked to would age out mid-conversation.
    ///
    /// Driven through the FAILING ensure path deliberately: it is the one reachable without a live
    /// child, and it pins the stronger claim — the touch is unconditional, so a chat mid-warm-up
    /// is never mistaken for an abandoned one by a reaper running concurrently.
    #[test]
    fn ensuring_a_seat_touches_the_chat_even_when_the_seat_fails_to_start() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let r = AcpStepRunner::new(tx);
        seed_chat(&r, "c1", MAX_BACKDATE);

        assert!(r.chat_ensure("c1", "no-such-cli-xyz").is_err());

        assert!(
            r.chat_reap_idle(TEST_TTL).is_empty(),
            "a chat someone just tried to warm a seat on is not idle"
        );
    }

    /// The TTL cannot cover chats opened faster than it retires them. The cap is the backstop —
    /// and it must evict the LEAST recently used, never the one being opened.
    #[test]
    fn the_pool_cap_evicts_least_recently_used_and_never_the_newest() {
        let (tx, rx) = std::sync::mpsc::channel();
        let r = AcpStepRunner::new(tx);
        for (id, idle) in [("oldest", 3), ("middle", 2), ("newer", 1), ("newest", 0)] {
            seed_chat(&r, id, idle);
        }

        let evicted = r.chat_enforce_cap(2);

        assert_eq!(evicted, vec!["oldest".to_string(), "middle".to_string()]);
        assert_eq!(
            r.chat_list().iter().map(|c| &c.chat_id).collect::<Vec<_>>(),
            vec!["newer", "newest"]
        );
        let reasons: Vec<String> = closed_chats(&rx).into_iter().map(|(_, r)| r).collect();
        assert_eq!(reasons, vec!["pool_cap".to_string(); 2]);
    }

    /// `WICKED_CHAT_POOL_MAX=0` must not mean "evict everything the instant it opens". A pool that
    /// cannot hold the chat being opened is not a smaller pool, it is a broken one.
    #[test]
    fn a_zero_pool_cap_is_floored_at_one_rather_than_evicting_everything() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let r = AcpStepRunner::new(tx);
        seed_chat(&r, "only", 0);

        assert!(r.chat_enforce_cap(0).is_empty());
        assert_eq!(r.chat_list().len(), 1);
    }

    /// Closing must drop the activity entry too. Chat ids are minted by clients without bound, so
    /// a map that only ever grows trades a 520 MB leak for a slower one.
    #[test]
    fn closing_a_chat_forgets_its_activity() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let r = AcpStepRunner::new(tx);
        seed_chat(&r, "c1", 10);

        r.chat_close("c1", ChatCloseReason::Requested);

        assert!(r.chat_activity.lock().unwrap().is_empty());
        assert!(r.chat_list().is_empty());
    }

    /// A turn outliving the TTL re-touches on its way out, AFTER the reaper has already closed the
    /// chat — leaving an activity entry `chat_close` cannot collect because it ran first. The
    /// sweep must collect it, or the daemon trades a 520 MB leak for a slow unbounded one.
    ///
    /// And it must NOT collect an entry that is merely unpooled-so-far: `chat_ensure` touches
    /// before it inserts, so a chat mid-open looks exactly like an orphan for an instant.
    #[test]
    fn the_sweep_collects_stale_orphan_activity_but_spares_a_chat_mid_open() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let r = AcpStepRunner::new(tx);
        {
            let mut activity = r.chat_activity.lock().unwrap();
            // Re-touched by a long turn after its chat was already closed.
            activity.insert("orphan".to_string(), backdated(MAX_BACKDATE));
            // Touched by `chat_ensure`, whose pool insert has not landed yet.
            activity.insert("opening".to_string(), Instant::now());
        }

        r.chat_reap_idle(TEST_TTL);

        let remaining: Vec<String> = r.chat_activity.lock().unwrap().keys().cloned().collect();
        assert_eq!(
            remaining,
            vec!["opening".to_string()],
            "the stale orphan goes, the chat mid-open stays"
        );
    }

    /// A pool entry with no recorded activity must read as idle-since-forever, not as fresh.
    /// The conservative reading reclaims it; the other one would let any gap in touch-recording
    /// pin memory permanently — which is exactly the defect being fixed.
    #[test]
    fn a_chat_with_no_recorded_activity_is_reaped_rather_than_treated_as_fresh() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let r = AcpStepRunner::new(tx);
        r.sessions.lock().unwrap().insert(
            (AcpStepRunner::chat_pool_key("orphan"), "claude".into()),
            None,
        );

        assert_eq!(r.chat_list()[0].idle_secs, u64::MAX);
        assert_eq!(r.chat_reap_idle(TEST_TTL), vec!["orphan".to_string()]);
    }

    /// The enumerate surface (FINDING-027 gap 4): a leak nobody can list is a leak nobody can
    /// reclaim. Two seats of one chat collapse to ONE entry, and a run's sessions are not chats.
    #[test]
    fn chat_list_collapses_a_chats_seats_and_ignores_run_sessions() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let r = AcpStepRunner::new(tx);
        {
            let mut guard = r.sessions.lock().unwrap();
            guard.insert(("run1".into(), "claude".into()), None);
            guard.insert((AcpStepRunner::chat_pool_key("c1"), "codex".into()), None);
            guard.insert((AcpStepRunner::chat_pool_key("c1"), "claude".into()), None);
        }
        r.chat_touch("c1");

        let listed = r.chat_list();

        assert_eq!(listed.len(), 1, "a run session is not a chat: {listed:?}");
        assert_eq!(listed[0].chat_id, "c1");
        assert!(listed[0].idle_secs < 5);
        // `seats` is the WARM subset, and these slots are `None` — a real `AcpProcess` needs a
        // live child, so unit tests cannot produce one. The seat-name path is covered by
        // `chat_seats`, which reads the same map with the same warm filter.
        assert!(listed[0].seats.is_empty());
    }

    /// core#410 (F-068) — the STREAM gate: a pi banner arriving as the first delta(s) is held and
    /// removed; everything else is released the moment it is known not to be a banner; a
    /// banner-shaped head that never closes is content and is delivered at the end (loss-averse).
    #[test]
    fn the_banner_gate_holds_only_a_banner_head_and_releases_everything_else() {
        let banner =
            "pi v0.83.0\n---\n\n## Skills\n- /x/SKILL.md\n\n## Extensions\n- /z.ts\n\n---\n";
        // Banner in one delta, then the answer: the banner is swallowed, the answer released.
        let mut g = BannerGate::default();
        assert_eq!(g.push(banner), None);
        assert_eq!(g.push("Hello"), Some("Hello".to_string()));
        assert_eq!(
            g.push(" world"),
            Some(" world".to_string()),
            "passthrough after release"
        );
        assert_eq!(g.finish(), None);
        // Banner split across deltas — held until the closing `---`, then only the answer.
        let mut g = BannerGate::default();
        assert_eq!(g.push("pi v0.8"), None);
        assert_eq!(g.push("3.0\n---\n## Skills\n- a\n"), None);
        assert_eq!(g.push("---\nAnswer"), Some("Answer".to_string()));
        // A banner observed twice (core#268) is removed twice.
        let mut g = BannerGate::default();
        assert_eq!(g.push(&format!("{banner}{banner}")), None);
        assert_eq!(g.push("A"), Some("A".to_string()));
        // Ordinary text is released on the FIRST delta — nothing buffered.
        let mut g = BannerGate::default();
        assert_eq!(g.push("Sure, here is"), Some("Sure, here is".to_string()));
        // A head that starts like the banner but diverges is released whole.
        let mut g = BannerGate::default();
        assert_eq!(g.push("pi v"), None);
        assert_eq!(
            g.push("ersion drift is fine"),
            Some("pi version drift is fine".to_string())
        );
        let mut g = BannerGate::default();
        assert_eq!(g.push("pi v1.0\n"), None);
        assert_eq!(
            g.push("not a rule"),
            Some("pi v1.0\nnot a rule".to_string())
        );
        // Leading line breaks alone are held (not yet content), then delivered with the text.
        let mut g = BannerGate::default();
        assert_eq!(g.push("\n"), None);
        assert_eq!(g.push("Hi"), Some("\nHi".to_string()));
        // An unterminated banner-shaped head is content: delivered at the end, never dropped.
        let mut g = BannerGate::default();
        let open = "pi v1.0\n---\n## Skills\n";
        assert_eq!(g.push(open), None);
        assert_eq!(g.finish(), Some(open.to_string()));
        // The hold is bounded: past the cap a banner-shaped head is released as content — judged
        // BEFORE retaining, so `held` never exceeds the cap even for one oversized chunk (Copilot).
        let mut g = BannerGate::default();
        let huge = format!("pi v1.0\n---\n{}", "x".repeat(BannerGate::HOLD_CAP + 1));
        assert_eq!(g.push(&huge), Some(huge.clone()));
        assert!(g.held.is_empty());
        let mut g = BannerGate::default();
        assert_eq!(g.push("pi v1.0\n"), None);
        let big = "y".repeat(BannerGate::HOLD_CAP);
        assert_eq!(g.push(&big), Some(format!("pi v1.0\n{big}")));
        assert!(g.held.is_empty(), "nothing is retained past the cap");
        // `finish` on a complete banner with nothing after it delivers nothing.
        let mut g = BannerGate::default();
        assert_eq!(g.push(banner), None);
        assert_eq!(g.finish(), None);
    }

    /// core#268 — the banner strip is pattern-gated and loss-averse: it removes exactly the
    /// observed rpc-startup shapes and NOTHING else. Falsified by loosening the head gate (the
    /// legit-content arm fails) or by stripping without a closing `---` (the unterminated arm).
    #[test]
    fn strip_pi_banner_removes_observed_shapes_and_nothing_else() {
        let banner = "pi v0.83.0\n---\n\n## Skills\n- /x/SKILL.md\n- /y/SKILL.md\n\n## Extensions\n- /z.ts\n\n---\nNew version available: v0.84.2 (installed v0.83.0). Run: `npm i -g x`\n";
        // Single banner + content.
        let text = format!("{banner}The actual reply.");
        assert_eq!(strip_pi_banner(&text), "The actual reply.");
        // Doubled banner (observed in survey outputs).
        let text = format!("{banner}{banner}Real synthesis here.");
        assert_eq!(strip_pi_banner(&text), "Real synthesis here.");
        // No update-notice variant.
        let text = "pi v1.0.0\n---\n## Skills\n- a\n---\ncontent";
        assert_eq!(strip_pi_banner(text), "content");
        // Legit content that merely CONTAINS --- lines: untouched.
        let doc = "# Title\n---\nbody\n---\nmore";
        assert_eq!(strip_pi_banner(doc), doc);
        // A reply that TALKS about pi but is not a banner: untouched.
        let talk = "pi version notes:\n---\nnope";
        assert_eq!(strip_pi_banner(talk), talk);
        // Unterminated banner (no closing ---): loss-averse, untouched.
        let cut = "pi v0.83.0\n---\n## Skills\n- a\n(no close)";
        assert_eq!(strip_pi_banner(cut), cut);
        // Empty and bannerless.
        assert_eq!(strip_pi_banner(""), "");
        assert_eq!(strip_pi_banner("plain"), "plain");
    }

    /// crew#267 — the SESSION_DIED arms must carry the bridge stderr tail and hit the daemon
    /// log; a seat death that leaves a clean log needs a live repro to diagnose (observed:
    /// 619 log lines, zero errors, one dead seat). Source-scan, same style as the launcher
    /// guard: these literals disappearing means the observability regressed.
    #[test]
    fn session_death_surfaces_stderr_and_logs() {
        let src = include_str!("acp_runner.rs");
        assert!(
            src.contains("using single-shot fallback{stderr_note}"),
            "the fallback reason no longer carries the bridge stderr tail"
        );
        assert!(
            src.contains("chat '{chat_id}' evicting {msg}"),
            "chat evictions no longer reach the daemon log"
        );
    }

    #[test]
    fn chat_ensure_fails_loud_for_unknown_cli_and_does_not_poison() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let r = AcpStepRunner::new(tx);
        let err = match r.chat_ensure("c1", "no-such-cli-xyz") {
            Err(e) => e,
            Ok(_) => panic!("unknown cli must fail"),
        };
        assert!(err.contains("no ACP config"), "{err}");
        // Chat failures are retryable — nothing cached, seat list stays empty.
        assert!(r.chat_seats("c1").is_empty());
        assert!(r.sessions.lock().unwrap().is_empty());
    }

    #[test]
    fn queued_messages_deliver_to_matching_cli_and_stay_for_others() {
        let r = runner();
        assert!(r.queue_operator_message("run1", &InjectTarget::All, "for everyone"));
        assert!(r.queue_operator_message("run1", &InjectTarget::Cli("codex".into()), "codex only"));
        assert!(r.queue_operator_message("run1", &InjectTarget::Cli("agy".into()), "agy only"));

        // claude drains the broadcast but not the CLI-targeted entries; the delivery
        // record carries the ORIGINAL injection target, not the receiving CLI.
        let claude = r.drain_operator_messages("run1", "claude");
        assert_eq!(claude.len(), 1);
        assert_eq!(claude[0].0, "all");
        assert_eq!(claude[0].1.output, "for everyone");
        assert_eq!(claude[0].1.label, "[operator message]");

        // codex drains only its own targeted entry (broadcast already consumed).
        let codex = r.drain_operator_messages("run1", "codex");
        assert_eq!(codex.len(), 1);
        assert_eq!(codex[0].0, "codex");
        assert_eq!(codex[0].1.output, "codex only");

        // agy's entry survived both prior drains.
        let agy = r.drain_operator_messages("run1", "agy");
        assert_eq!(agy.len(), 1);
        assert_eq!(agy[0].1.output, "agy only");

        // Everything consumed; nothing left for anyone.
        assert!(r.drain_operator_messages("run1", "claude").is_empty());
    }

    #[test]
    fn result_usage_parses_ecosystem_adapter_shape() {
        // Official claude adapter result: input + cached reads/writes sum into input.
        let v = serde_json::json!({
            "inputTokens": 2, "outputTokens": 4,
            "cachedReadTokens": 15273, "cachedWriteTokens": 18195, "totalTokens": 33474
        });
        let u = parse_result_usage(&v).expect("usage");
        assert_eq!(u.input_tokens, 2 + 15273 + 18195);
        assert_eq!(u.output_tokens, 4);
        assert_eq!(u.cost_usd, None);

        // Absent / empty / zeroed → None (no fabricated usage).
        assert!(parse_result_usage(&serde_json::Value::Null).is_none());
        assert!(parse_result_usage(&serde_json::json!({})).is_none());
        assert!(
            parse_result_usage(&serde_json::json!({"inputTokens": 0, "outputTokens": 0})).is_none()
        );
    }

    #[test]
    fn usage_update_lifts_cost_only_frames() {
        // Official claude adapter usage_update: {used, size, cost:{amount}} — no token
        // fields. The cost must be lifted and must survive a later result-usage merge.
        let emit_fn = |_: &str| {};
        let emit: &DeltaSink = &emit_fn;
        let mut output = String::new();
        let mut usage: Option<Usage> = None;
        let mut files = Vec::new();
        let v = serde_json::json!({
            "params": {"update": {
                "sessionUpdate": "usage_update",
                "used": 33474, "size": 1000000,
                "cost": {"amount": 0.19, "currency": "USD"}
            }}
        });
        handle_update(&v, emit, &mut output, &mut usage, &mut files, 1024);
        let u = usage.expect("cost-only frame lifts usage");
        assert_eq!(u.cost_usd, Some(0.19));
        assert_eq!(u.input_tokens, 0);

        // Merge semantics from the turn loop: result usage wins tokens, keeps cost.
        let result_usage = parse_result_usage(&serde_json::json!({
            "inputTokens": 10, "outputTokens": 5
        }))
        .unwrap();
        let cost = u.cost_usd;
        let merged = Usage {
            cost_usd: cost.or(result_usage.cost_usd),
            ..result_usage
        };
        assert_eq!(merged.input_tokens, 10);
        assert_eq!(merged.output_tokens, 5);
        assert_eq!(merged.cost_usd, Some(0.19));
    }

    #[test]
    fn drain_is_scoped_per_run_and_drop_session_prunes() {
        let r = runner();
        assert!(r.queue_operator_message("run1", &InjectTarget::All, "run1 msg"));
        assert!(r.queue_operator_message("run2", &InjectTarget::All, "run2 msg"));

        // run2's queue is untouched by run1's drain.
        assert_eq!(r.drain_operator_messages("run1", "claude").len(), 1);
        assert_eq!(r.drain_operator_messages("run1", "claude").len(), 0);

        // drop_session prunes the run's queue outright.
        r.drop_session("run2");
        assert!(r.drain_operator_messages("run2", "claude").is_empty());
    }

    // ── FINDING-060/061: a governed claude unit must never run on ACP ────────────

    /// What the fallback actually invokes, per platform.
    ///
    /// The routing this test exists for is platform-independent, so the test itself is NOT
    /// `#[cfg]`-gated — per the argument at `execute_wrapped.rs`'s `rule_path_sep`, a gated test
    /// runs on one of three CI platforms, which is how a platform bug survives review. Only the
    /// *execution* proof needs a real process, and only that assertion is gated.
    ///
    /// There is no Windows equivalent of `/bin/echo` here, and `cmd /c echo` is not one:
    /// `build_argv` appends the skill prompt as a trailing arg whenever the template omits
    /// `{PROMPT}` (execute_wrapped.rs:1228), and that prompt carries `|||` and newlines — pipes and
    /// command separators to `cmd`. So Windows names a binary that cannot exist: the spawn fails
    /// fast, without a shell, and every assertion below except the execution proof still holds,
    /// because `fallback_with_warning` prepends its warning whether or not the child runs.
    /// FINDING-060's regression, now asserted the other way round.
    ///
    /// The ACP path once armed governance the bridge never applied, so a governed unit ran with
    /// every tool call ungoverned while the engine reported `governed: true`. The interim fix
    /// rerouted governed claude units to the wrapped path, and this test pinned that reroute.
    ///
    /// The reroute cost a governed unit its multi-turn session — one attempt at a task that needs
    /// many — which is why `domain-extraction` could not finish on a real repo (FINDING-100). Now
    /// that the client answers `session/request_permission` with the same policy and the same
    /// audit records as the hook, the reroute is gone and this pins its ABSENCE: a governed claude
    /// unit must stay on the ACP path.
    ///
    /// Pinned by source, because the alternative — driving a real bridge — needs a network and a
    /// live agent, and a test that cannot run is a test that stops being true quietly.
    /// The hole review caught, which my end-to-end permission test could not see.
    ///
    /// `StepOutput.governed` is the runner's ASSERTION to the actor that this unit was gated — the
    /// fold uses it as authority to read and verify the decisions log. The ACP path armed
    /// governance, wrote the marker, and evaluated every tool call, then reported `governed: false`
    /// — so hook denies and evidence-integrity checks would have been skipped for exactly the units
    /// that had them. A unit that is gated and says it is not is the same defect as one that says
    /// it is gated and is not; both make the fold read the wrong evidence.
    ///
    /// My own proof missed it because it exercises `permission_result` directly and never looks at
    /// the StepOutput the runner returns. Fourth instance of that gap in this campaign — hence a
    /// source audit rather than another test of the helper.
    #[cfg(unix)]
    #[allow(dead_code)]
    const CHEAP_OK: &str = "/bin/echo wicked-fallback-ran";
    #[cfg(not(unix))]
    #[allow(dead_code)]
    const CHEAP_OK: &str = "wicked-no-such-binary-fallback-probe";

    /// A unit assigned to `claude` — the routing predicate reads `assigned_cli` — whose actual
    /// invocation is [`CHEAP_OK`], so the wrapped fallback this must reach executes something cheap
    /// instead of a real CLI. The two are deliberately different: the ACP branch classifies by the
    /// assigned key, the wrapped runner by argv[0].
    #[allow(dead_code)]
    fn claude_unit_running_echo() -> crate::domain::WorkUnit {
        crate::domain::WorkUnit {
            id: "u-gov".to_string(),
            session_id: "run-gov".to_string(),
            ord: 1,
            description: "a governed unit".to_string(),
            stage: Default::default(),
            assigned_cli: Some("claude".to_string()),
            assigned_invocation: Some(CHEAP_OK.to_string()),
            council_task_ref: None,
            routing: None,
            denial_reason: None,
            denial: None,
            phase_ref: None,
            conformance_ref: None,
            phase_status: None,
            collection_scope: None,
            skill_ref: None,
            allowed_skills: Vec::new(),
            gate: Default::default(),
            role: Default::default(),
            validator: None,
            tool_cmd: None,
            worker_failed_clis: Vec::new(),
            depends_on: Vec::new(),
            required_deliverables: Vec::new(),
            executes_code: false,
            pre_build_scope: false,
            scope_warnings: Vec::new(),
            worktree_guarded: false,
            worktree_baseline: None,
            worktree_mutation: None,
            repo_checks_floor: false,
            repo_checks: None,
            status: crate::domain::UnitStatus::Pending,
        }
    }

    #[allow(dead_code)]
    fn governed_input(dir: &std::path::Path) -> StepInput {
        StepInput {
            run_id: "run-gov".to_string(),
            unit_ix: 0,
            attempt: 0,
            unit: claude_unit_running_echo(),
            workflow_id: "wf-test".to_string(),
            entity_mode: crate::scope::EntityMode::Shared,
            workdir: Some(dir.to_path_buf()),
            governance: Some(crate::workflow::GovernanceContext {
                db_path: dir.join("estate.db").to_string_lossy().to_string(),
                code_graph_db: None,
                extra_write_roots: Vec::new(),
                extra_read_roots: Vec::new(),
            }),
            prior_outputs: Vec::new(),
            elicitation_epoch: 0,
            process_gen: None,
            launch_seq: 0,
            required_skills: Vec::new(),
        }
    }
    #[test]
    fn a_governed_acp_unit_reports_itself_governed() {
        let src = include_str!("acp_runner.rs");
        // Needles built by concatenation: this assertion's own message names the very strings it
        // searches for, and a source audit that matches itself is the fifth such self-match I have
        // written in this campaign.
        let bad = format!("governed:{}false,", " ");
        let good = format!("governed:{}gate.is_some(),", " ");
        assert!(
            !src.contains(&bad),
            "an ACP StepOutput still hardcodes `governed: false`. If the unit was gated, the fold \
             must be told so — otherwise it skips the hook-deny and evidence-integrity checks for \
             the units that actually have them (FINDING-062)"
        );
        assert!(
            src.contains(&good),
            "the governed flag must follow whether a gate was armed for THIS turn, not a constant"
        );
    }

    #[test]
    fn a_governed_claude_unit_is_no_longer_rerouted_off_the_acp_path() {
        let src = include_str!("acp_runner.rs");
        assert!(
            !src.contains(&format!("runs single{}shot: the ACP bridge", "-")),
            "the governance reroute is back: governed units are single-shot again, and \
             domain-extraction cannot finish on a real repo while it is (FINDING-062/100)"
        );
        // …and the replacement is actually wired, not merely the old branch deleted.
        assert!(
            src.contains("crate::acp_permission::permission_result"),
            "governed turns no longer consult the permission gate — deleting the reroute without \
             answering session/request_permission is the ungoverned ACP path all over again \
             (FINDING-060)"
        );
        assert!(
            src.contains("\"permission\": true"),
            "the client no longer advertises the permission capability, so the bridge never asks \
             and the handler above is unreachable"
        );
        assert!(
            src.contains("write_armed_marker"),
            "the ACP path no longer writes the armed marker, so the fold cannot tell a clean \
             governed run from a bypassed one and will deny it"
        );
    }

    /// ACP input governance is an explicit adapter-proof capability, not a CLI-name heuristic.
    /// The built-in registry admits claude (DES-INPUT-GOV-001 §3) and opencode (DES-INPUT-GOV-006
    /// / oq-opencode-acp-002, via the harness-provisioned `acp_governance_env` forcing function)
    /// today; every other built-in fails safely to the explicitly disclosed ungoverned posture.
    /// Asserted against `builtin()` — never the merged registry, whose answer would depend on the
    /// operator's real clis.toml (review, #371).
    #[test]
    fn acp_input_governance_is_admitted_by_capability_only() {
        for cli in wicked_council::registry::builtin() {
            let admitted = cli
                .acp
                .as_ref()
                .map(|a| a.acp_input_governance)
                .unwrap_or(false);
            if cli.key == "claude" || cli.key == "opencode" {
                assert!(
                    admitted,
                    "{}'s pinned adapter passed the admission proof",
                    cli.key
                );
            } else {
                assert!(
                    !admitted,
                    "{} must not be admitted without a pinned adapter proof",
                    cli.key
                );
            }
        }
    }

    #[test]
    fn unadmitted_acp_governance_is_explicit_on_the_audit_wire() {
        let dir = std::env::temp_dir().join(format!(
            "wicked-acp-ungoverned-event-{}",
            std::process::id()
        ));
        let input = governed_input(&dir);
        // Precondition read from the BUILT-IN roster, not the merged registry — hermetic against
        // the operator's real clis.toml (review, #371).
        let codex_admitted = wicked_council::registry::builtin()
            .into_iter()
            .find(|c| c.key == "codex")
            .and_then(|c| c.acp)
            .map(|a| a.acp_input_governance)
            .unwrap_or(false);
        assert!(
            !codex_admitted,
            "precondition: codex ACP has no pinned admission proof"
        );
        let event = acp_ungoverned_event(&input, "codex").expect("non-empty seat must disclose");
        match event {
            crate::event::CoreEvent::GovernanceUnenforced { cli, reason, .. } => {
                assert_eq!(cli, "codex");
                assert!(reason.contains("acp_input_governance=false"));
                assert!(reason.contains("allow_result"));
            }
            other => panic!("expected GovernanceUnenforced, got {other:?}"),
        }
        assert!(
            acp_ungoverned_event(&input, "").is_none(),
            "an empty seat must not produce an unactionable audit event"
        );
    }

    /// The `gate_ctx` disclosure gate must not fire for a seat with NO ACP config at all: such a
    /// unit never reaches `allow_result` (it falls straight through to the wrapped fallback two
    /// lines below `gate_ctx` in `exec_turn_inner`), so claiming "answered by allow_result,
    /// unchecked" for it would be a false statement on the audit wire — the exact failure an
    /// adversarial review of #364 caught: an unrelated, possibly fully-governed wrapped-path unit
    /// getting mislabeled as ACP-ungoverned just because it has no ACP adapter to be admitted.
    #[test]
    fn acp_ungoverned_disclosure_is_scoped_to_seats_with_an_acp_config() {
        // Deliberately built in-process rather than read through `acp_config_for`/the merged
        // registry: this predicate must hold regardless of what the OPERATOR's real
        // ~/.config/wicked-council/clis.toml happens to contain. (It happens to already contain
        // exactly case (b) below for several built-ins — a live instance of the bug this predicate
        // exists to prevent, not a hypothetical.)
        let unadmitted_but_configured = AcpConfig {
            binary: "some-acp-adapter".into(),
            start_args: vec![],
            transport: AcpTransport::default(),
            auth_method: None,
            acp_input_governance: false,
            os_sandbox: false,
            acp_governance_env: None,
            verified_version: None,
        };
        let admitted = AcpConfig {
            acp_input_governance: true,
            ..unadmitted_but_configured.clone()
        };

        // (a) Configured but unadmitted: this call site must disclose.
        assert!(acp_unadmitted_but_configured(Some(
            &unadmitted_but_configured
        )));
        // (a-negative) Configured AND admitted: must not disclose (that seat takes the armed path).
        assert!(!acp_unadmitted_but_configured(Some(&admitted)));
        // (b) No ACP config at all: the seat never reaches `allow_result` (it falls straight to
        // the wrapped fallback), so disclosing the ACP-unadmitted reason for it would be false.
        assert!(!acp_unadmitted_but_configured(None));
    }

    // ── DES-002 EpochCleanup unit tests (T4) ─────────────────────────────────────

    fn make_maps() -> Arc<Mutex<ElicitationMaps>> {
        Arc::new(Mutex::new(ElicitationMaps::new()))
    }

    fn make_guard(
        maps: Arc<Mutex<ElicitationMaps>>,
        run_id: &str,
        epoch: u64,
        launch_seq: u64,
    ) -> EpochCleanup {
        let (tx, _rx) = std::sync::mpsc::channel();
        EpochCleanup {
            maps,
            run_id: run_id.to_string(),
            epoch,
            launch_seq,
            bus_in_flight_deferred: false,
            tx,
            in_flight_id: None,
            in_flight_action: None,
            in_flight_reason: None,
        }
    }

    /// Test 35: cleanup_run reclaims state for the exact launch token.
    #[test]
    fn cleanup_run_reclaims_state_local_path() {
        let maps_arc = make_maps();
        {
            let mut m = maps_arc.lock().unwrap();
            m.begin_launch("run-local", true);
            m.register("run-local", 1, "e-local", "q", None, "r")
                .unwrap();
        }
        {
            let m = maps_arc.lock().unwrap();
            assert!(m.active_workers.contains(&("run-local".to_string(), 1)));
            assert!(m.pending.contains_key("e-local"));
        }
        {
            let mut m = maps_arc.lock().unwrap();
            m.cleanup_run("run-local", 1, 1);
        }
        let m = maps_arc.lock().unwrap();
        assert!(
            !m.active_workers
                .iter()
                .any(|(run_id, _)| run_id == "run-local"),
            "worker count decremented"
        );
        assert!(!m.pending.contains_key("e-local"), "pending entry removed");
    }

    /// EpochCleanup drop with bus_in_flight_deferred=false clears in-flight immediately.
    #[test]
    fn epoch_cleanup_drop_clears_in_flight_when_not_deferred() {
        let maps_arc = make_maps();
        {
            let mut m = maps_arc.lock().unwrap();
            m.begin_launch("run-drop", true);
            m.mark_bus_in_flight("run-drop", 1);
        }
        {
            let m = maps_arc.lock().unwrap();
            assert!(
                m.is_bus_worker_in_flight("run-drop", 1),
                "in-flight before drop"
            );
        }
        {
            let guard = make_guard(maps_arc.clone(), "run-drop", 1, 1);
            // Drop here — bus_in_flight_deferred is false.
            drop(guard);
        }
        let m = maps_arc.lock().unwrap();
        assert!(
            !m.is_bus_worker_in_flight("run-drop", 1),
            "in-flight cleared after drop"
        );
        assert!(
            !m.active_workers
                .iter()
                .any(|(run_id, _)| run_id == "run-drop"),
            "cleanup_run fired (worker decremented)"
        );
    }

    /// EpochCleanup drop with bus_in_flight_deferred=true does NOT clear in-flight.
    #[test]
    fn epoch_cleanup_drop_skips_clear_when_deferred() {
        let maps_arc = make_maps();
        {
            let mut m = maps_arc.lock().unwrap();
            m.begin_launch("run-defer", true);
            m.mark_bus_in_flight("run-defer", 1);
        }
        {
            let mut guard = make_guard(maps_arc.clone(), "run-defer", 1, 1);
            guard.bus_in_flight_deferred = true;
            drop(guard);
        }
        let m = maps_arc.lock().unwrap();
        // in-flight NOT cleared — bus consumer owns the clear.
        assert!(
            m.is_bus_worker_in_flight("run-defer", 1),
            "in-flight NOT cleared when deferred — bus consumer will clear it"
        );
        // cleanup_run still fired (worker decremented).
        assert!(
            !m.active_workers
                .iter()
                .any(|(run_id, _)| run_id == "run-defer"),
            "cleanup_run still decremented worker count"
        );
    }

    // ── DES-002 ElicitationMaps unit tests (T3, tests 1–11 + 10a) ────────────────

    fn maps() -> ElicitationMaps {
        ElicitationMaps::new()
    }

    /// Test 1: register + remove round-trip — pending and run_index are cleaned up.
    #[test]
    fn register_and_remove_round_trip() {
        let mut m = maps();
        let result = m.register("run-1", 1, "elic-a", "What colour?", None, "response");
        assert!(result.is_some(), "register succeeded");
        assert!(m.pending.contains_key("elic-a"));
        assert!(m.run_index.contains_key("run-1"));

        m.remove("run-1", "elic-a");
        assert!(!m.pending.contains_key("elic-a"), "pending cleared");
        assert!(
            m.run_index.get("run-1").is_none_or(|v| v.is_empty()),
            "run_index entry cleared"
        );
    }

    /// Test 2: register on a cancelled epoch returns None.
    #[test]
    fn register_after_cancel_epoch_is_suppressed() {
        let mut m = maps();
        m.cancel_epoch("run-2", 5);
        let result = m.register("run-2", 5, "elic-b", "msg", None, "response");
        assert!(result.is_none(), "creation suppressed when epoch cancelled");
        assert!(!m.pending.contains_key("elic-b"));
    }

    /// Test 3: cancel_epoch is scoped to (run_id, epoch) — other runs unaffected.
    #[test]
    fn cancel_epoch_cross_run_isolation() {
        let mut m = maps();
        // Register two elicitations: one for run-3/epoch-1, one for run-4/epoch-1.
        let r3 = m.register("run-3", 1, "e3", "q3", None, "r");
        let r4 = m.register("run-4", 1, "e4", "q4", None, "r");
        assert!(r3.is_some());
        assert!(r4.is_some());
        let (rx3, ..) = r3.unwrap();
        let (rx4, ..) = r4.unwrap();

        // Cancel only run-3/epoch-1.
        m.cancel_epoch("run-3", 1);

        // run-3's elicitation receives a cancel.
        let res3 = rx3.recv_timeout(std::time::Duration::from_millis(100));
        assert!(res3.is_ok(), "run-3 received cancel");
        assert_eq!(res3.unwrap().action, "cancel");

        // run-4's elicitation is unaffected (no message).
        let res4 = rx4.recv_timeout(std::time::Duration::from_millis(50));
        assert!(res4.is_err(), "run-4 not cancelled — channel empty");
    }

    /// Test 4: deliver resolves the receiver.
    #[test]
    fn deliver_resolves_receiver() {
        let mut m = maps();
        let (rx, ..) = m
            .register("run-5", 1, "e5", "confirm?", None, "response")
            .unwrap();
        m.deliver(
            "run-5",
            "e5",
            "accept".to_string(),
            Some(serde_json::json!("yes")),
        )
        .unwrap();
        let res = rx
            .recv_timeout(std::time::Duration::from_millis(200))
            .unwrap();
        assert_eq!(res.action, "accept");
        assert_eq!(res.response, Some(serde_json::json!("yes")));
    }

    /// Test 5: deliver with wrong run_id returns Err.
    #[test]
    fn deliver_wrong_run_id_returns_err() {
        let mut m = maps();
        m.register("run-6", 1, "e6", "q", None, "r").unwrap();
        let result = m.deliver("WRONG-RUN", "e6", "cancel".to_string(), None);
        assert!(result.is_err(), "cross-run deliver must fail");
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("belongs to run run-6"));
    }

    /// Test 6: deliver after cancel_epoch fails immediately without blocking.
    #[test]
    fn deliver_after_cancel_epoch_fails_without_blocking() {
        let mut m = maps();
        let (rx, ..) = m.register("run-7", 1, "e7", "q", None, "r").unwrap();
        m.cancel_epoch("run-7", 1);
        let cancelled = rx
            .recv_timeout(std::time::Duration::from_millis(100))
            .unwrap();
        assert_eq!(cancelled.action, "cancel");
        let err = m
            .deliver("run-7", "e7", "cancel".to_string(), None)
            .unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn invalid_delivery_does_not_consume_pending_elicitation() {
        let mut m = maps();
        let (rx, ..) = m
            .register(
                "run-validate",
                1,
                "e-validate",
                "pick",
                Some(vec!["yes".to_string(), "no".to_string()]),
                "answer",
            )
            .unwrap();
        assert!(m
            .deliver(
                "run-validate",
                "e-validate",
                "accept".to_string(),
                Some(serde_json::json!("maybe")),
            )
            .is_err());
        assert!(m.is_pending("e-validate"));

        m.deliver(
            "run-validate",
            "e-validate",
            "accept".to_string(),
            Some(serde_json::json!("yes")),
        )
        .unwrap();
        assert_eq!(rx.recv().unwrap().response, Some(serde_json::json!("yes")));
    }

    /// Test 7: 8 KB byte-length message cap — a 4-byte-per-codepoint string of 2,049
    /// codepoints (8,196 bytes) is truncated to ≤ 8 KB + "[truncated]".
    #[test]
    fn message_truncation_8kb_byte_length_cap() {
        // U+1F600 is 4 bytes in UTF-8; 2049 codepoints = 8196 bytes > 8192.
        let long_msg: String = "😀".repeat(2049);
        assert_eq!(long_msg.len(), 2049 * 4, "each codepoint is 4 bytes");
        let mut m = maps();
        let result = m.register("run-8", 1, "e8", &long_msg, None, "response");
        let (_, stored_msg, ..) = result.unwrap();
        assert!(
            stored_msg.ends_with("[truncated]"),
            "truncation marker appended"
        );
        // The total byte length is ≤ 8192 (truncated part) + len("[truncated]").
        let truncated_part_len = stored_msg.len() - "[truncated]".len();
        assert!(
            truncated_part_len <= 8192,
            "truncated part is ≤ 8 KB: {} bytes",
            truncated_part_len
        );
        // The truncation boundary is a valid UTF-8 char boundary.
        let _ = stored_msg.chars().count(); // panics if not valid UTF-8
    }

    /// Test 8: options entry > 512 bytes (byte-length) is dropped; entry < 512 bytes is kept.
    #[test]
    fn options_entry_over_512_bytes_dropped() {
        // 129 U+1F600 (4 bytes each) = 516 bytes > 512.
        let over_cap: String = "😀".repeat(129);
        assert!(
            over_cap.len() > 512,
            "over-cap entry is {} bytes",
            over_cap.len()
        );
        let under_cap = "short option".to_string();
        let mut m = maps();
        let result = m.register(
            "run-9",
            1,
            "e9",
            "q",
            Some(vec![over_cap, under_cap.clone()]),
            "r",
        );
        let (_, _, opts, _) = result.unwrap();
        let opts = opts.unwrap();
        assert_eq!(
            opts.len(),
            1,
            "over-cap entry dropped; only under-cap remains"
        );
        assert_eq!(opts[0], under_cap);
    }

    /// Test 9: empty-string options entry is dropped; non-empty entry retained.
    #[test]
    fn empty_options_entry_dropped() {
        let mut m = maps();
        let result = m.register(
            "run-10",
            1,
            "e10",
            "q",
            Some(vec!["".to_string(), "valid".to_string()]),
            "r",
        );
        let (_, _, opts, _) = result.unwrap();
        let opts = opts.unwrap();
        assert_eq!(opts, vec!["valid".to_string()], "empty entry dropped");
    }

    /// Test 10: prop_key is preserved from the schema, not hardcoded to "response".
    #[test]
    fn prop_key_preserved_from_schema() {
        let mut m = maps();
        let result = m.register("run-11", 1, "e11", "q", None, "myField");
        let (_, _, _, prop_key) = result.unwrap();
        assert_eq!(prop_key, "myField", "prop_key preserved verbatim");
    }

    /// Test 10a: null constraint fields treated as absent (register proceeds normally).
    #[test]
    fn null_constraint_fields_treated_as_absent() {
        // The caller normalises null schema constraints to None before calling register;
        // verify that register with None options and default prop_key works.
        let mut m = maps();
        let result = m.register("run-12", 1, "e12", "q", None, "response");
        assert!(
            result.is_some(),
            "null/absent constraints do not break registration"
        );
        let (_, _, opts, prop_key) = result.unwrap();
        assert!(opts.is_none(), "options is None");
        assert_eq!(prop_key, "response");
    }

    /// Test 11: cleanup_run decrements active_workers and clears pending/run_index.
    #[test]
    fn cleanup_run_decrements_workers_and_clears_pending() {
        let mut m = maps();
        m.begin_launch("run-13", true);
        assert!(m.active_workers.contains(&("run-13".to_string(), 1)));
        m.register("run-13", 1, "e13a", "q", None, "r").unwrap();
        m.register("run-13", 2, "e13b", "q2", None, "r").unwrap(); // different epoch

        // cleanup for epoch 1 only.
        m.cleanup_run("run-13", 1, 1);
        assert!(
            !m.active_workers
                .iter()
                .any(|(run_id, _)| run_id == "run-13"),
            "worker decremented"
        );
        assert!(
            !m.pending.contains_key("e13a"),
            "epoch-1 registration removed"
        );
        assert!(
            m.pending.contains_key("e13b"),
            "epoch-2 registration survives"
        );
    }

    #[test]
    fn cleanup_is_scoped_to_the_finished_run() {
        let mut m = maps();
        m.begin_launch("run-a", true);
        let epoch_a = m.next_epoch("run-a");
        m.begin_launch("run-b", true);
        let epoch_b = m.next_epoch("run-b");

        m.cleanup_run("run-a", epoch_a, 1);

        assert!(!m.has_active_run("run-a"));
        assert!(m.has_active_run("run-b"));
        assert_eq!(m.current_epoch("run-b"), epoch_b);
        assert!(m.active_workers.contains(&("run-b".to_string(), 1)));
    }

    // ── DES-002 T5 tests: rpc_respond, rpc_expect, validate_elicitation_schema ─────

    /// Test 21: `rpc_respond` echoes the request id VERBATIM — string ids must not be cast to u64.
    ///
    /// The defect this pins: the prior `rpc_expect` used `v.get("id").and_then(Value::as_u64) == Some(id)`
    /// which coerces string ids to `None`, causing any adapter that sends a string-typed `id` (a
    /// common pattern in Claude Code and Codex) to stall. `rpc_respond` must echo as `Value::String`.
    #[test]
    fn rpc_respond_echoes_string_typed_request_id_verbatim() {
        let request_id = serde_json::Value::String("elicit-abc-123".to_string());
        let result = serde_json::json!({"action": "cancel"});

        let mut buf: Vec<u8> = Vec::new();
        rpc_respond(&mut buf, &request_id, result).unwrap();

        let line = std::str::from_utf8(&buf).unwrap().trim_end().to_string();
        let parsed: serde_json::Value = serde_json::from_str(&line).unwrap();
        // The `id` field must survive as a string, not be cast to a number.
        assert_eq!(
            parsed["id"],
            serde_json::Value::String("elicit-abc-123".to_string()),
            "string id must be echoed verbatim, not coerced to integer: {parsed}"
        );
        assert_eq!(parsed["result"]["action"], "cancel");
    }

    /// SEMANTICS CHANGE (Copilot review, core#293). This test previously asserted the opposite —
    /// `rpc_respond_ignores_null_id`: a null id was treated as "notification, stay silent".
    ///
    /// That conflated two different frames. JSON-RPC 2.0 §4.1 defines a notification as a request
    /// with the `id` member ABSENT; `"id": null` is a permitted request id (§5.1 even REQUIRES
    /// error responses to echo null when the id is undeterminable). Under the old rule an agent
    /// that sent a real request with an explicit null id got no answer and blocked for the whole
    /// turn — the same silent-drop class this PR removes. Notifications are now filtered
    /// structurally by `answerable_id`/`is_notification` at the dispatchers, so the responders
    /// answer every id they are handed, `null` included.
    #[test]
    fn rpc_respond_answers_an_explicit_null_id() {
        let mut buf: Vec<u8> = Vec::new();
        rpc_respond(
            &mut buf,
            &serde_json::Value::Null,
            serde_json::json!({"ok": true}),
        )
        .unwrap();
        let parsed: serde_json::Value =
            serde_json::from_str(std::str::from_utf8(&buf).unwrap().trim_end())
                .expect("an explicit null id is a request and must be answered");
        assert_eq!(parsed["id"], serde_json::Value::Null, "id must echo null");
        assert_eq!(parsed["result"]["ok"], true);

        // Same for the refusal path.
        let mut ebuf: Vec<u8> = Vec::new();
        rpc_respond_error(
            &mut ebuf,
            &serde_json::Value::Null,
            METHOD_NOT_FOUND_CODE,
            "nope",
        )
        .unwrap();
        let eparsed: serde_json::Value =
            serde_json::from_str(std::str::from_utf8(&ebuf).unwrap().trim_end()).unwrap();
        assert_eq!(eparsed["id"], serde_json::Value::Null);
        assert_eq!(eparsed["error"]["code"], METHOD_NOT_FOUND_CODE);
    }

    /// The classifier that keeps the two apart: ABSENCE of the `id` member, never its value.
    #[test]
    fn notification_is_absent_id_not_null_id() {
        let notification = serde_json::json!({"jsonrpc":"2.0","method":"some/note","params":{}});
        assert!(is_notification(&notification));
        assert!(answerable_id(&notification).is_none());

        let null_id_request =
            serde_json::json!({"jsonrpc":"2.0","id":null,"method":"some/req","params":{}});
        assert!(
            !is_notification(&null_id_request),
            "an explicit null id is a REQUEST, not a notification"
        );
        assert_eq!(
            answerable_id(&null_id_request),
            Some(&serde_json::Value::Null)
        );

        let numeric = serde_json::json!({"jsonrpc":"2.0","id":7,"method":"some/req"});
        assert!(!is_notification(&numeric));
        assert_eq!(answerable_id(&numeric), Some(&serde_json::json!(7)));

        // A RESPONSE is not agent-originated, so it is never a notification.
        let response = serde_json::json!({"jsonrpc":"2.0","id":7,"result":{}});
        assert!(!is_notification(&response));
    }

    /// End-to-end at the handshake dispatcher: a TRUE notification (id member absent) draws no
    /// output, while a request carrying an EXPLICIT null id is answered — and in both cases the
    /// wait continues until the real response arrives.
    #[test]
    fn rpc_expect_answers_explicit_null_id_but_ignores_true_notifications() {
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        // 1. A true notification — no `id` member at all.
        tx.send(r#"{"jsonrpc":"2.0","method":"some/futureNotification","params":{}}"#.to_string())
            .unwrap();
        // 2. A REQUEST whose id is explicitly null — must be answered, not dropped.
        tx.send(
            r#"{"jsonrpc":"2.0","id":null,"method":"some/futureRequest","params":{}}"#.to_string(),
        )
        .unwrap();
        // 3. The genuine handshake response.
        tx.send(r#"{"jsonrpc":"2.0","id":3,"result":{"sessionId":"s3"}}"#.to_string())
            .unwrap();

        let mut sink: Vec<u8> = Vec::new();
        let v = rpc_expect(&rx, &mut sink, 3, Duration::from_secs(5)).unwrap();
        assert_eq!(v["result"]["sessionId"], "s3");

        let written = std::str::from_utf8(&sink).unwrap();
        let lines: Vec<&str> = written.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(
            lines.len(),
            1,
            "exactly one frame must be written: the notification is ignored and the \
             explicit-null-id request is refused — got {written:?}"
        );
        let refusal: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(
            refusal["id"],
            serde_json::Value::Null,
            "the refusal must echo the request's null id: {refusal}"
        );
        assert_eq!(refusal["error"]["code"], METHOD_NOT_FOUND_CODE);
    }

    /// A failed refusal write during the handshake is PROPAGATED, not discarded (Copilot review).
    /// Swallowing it left the agent blocked and the handshake died in a timeout naming nothing.
    #[test]
    fn rpc_expect_propagates_a_failed_refusal_write() {
        /// A writer whose every write fails, standing in for a closed adapter stdin.
        struct BrokenPipe;
        impl std::io::Write for BrokenPipe {
            fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "closed",
                ))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let (tx, rx) = std::sync::mpsc::channel::<String>();
        tx.send(
            r#"{"jsonrpc":"2.0","id":9,"method":"session/request_permission","params":{}}"#
                .to_string(),
        )
        .unwrap();
        tx.send(r#"{"jsonrpc":"2.0","id":4,"result":{"sessionId":"never"}}"#.to_string())
            .unwrap();

        let err = rpc_expect(&rx, &mut BrokenPipe, 4, Duration::from_secs(5))
            .expect_err("a failed refusal write must fail the handshake immediately");
        let msg = err.to_string();
        assert!(
            msg.contains("session/request_permission") && msg.contains("closed"),
            "the error must name the method and the io failure: {msg}"
        );
    }

    /// The turn loop's counterpart of the same two rules, via the shared permission handler:
    /// an explicit null id is answered, an absent id is not, and a failed write is NAMED in the
    /// turn output instead of being discarded (Copilot review, core#293).
    #[test]
    fn permission_handler_answers_null_id_skips_notifications_and_notes_write_failures() {
        let lock = Mutex::new(());
        let mut output = String::new();

        // 1. Explicit null id ⇒ a request; it must be answered with a null-id response.
        let mut sink: Vec<u8> = Vec::new();
        let frame = serde_json::json!({
            "jsonrpc":"2.0","id":null,"method":"session/request_permission",
            "params":{"options":[{"optionId":"allow","kind":"allow_once"}]}
        });
        answer_permission_request(&mut sink, &lock, None, None, &frame, &mut output, 4096);
        let answered: serde_json::Value =
            serde_json::from_str(std::str::from_utf8(&sink).unwrap().trim_end())
                .expect("an explicit null id must still be answered");
        assert_eq!(answered["id"], serde_json::Value::Null);

        // 2. `id` member ABSENT ⇒ a notification; nothing to answer.
        let mut sink2: Vec<u8> = Vec::new();
        let note_frame = serde_json::json!({
            "jsonrpc":"2.0","method":"session/request_permission","params":{}
        });
        answer_permission_request(
            &mut sink2,
            &lock,
            None,
            None,
            &note_frame,
            &mut output,
            4096,
        );
        assert!(sink2.is_empty(), "a notification must draw no response");

        // 3. A failed write is surfaced in the output, not swallowed.
        struct BrokenPipe;
        impl std::io::Write for BrokenPipe {
            fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "closed",
                ))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        answer_permission_request(
            &mut BrokenPipe,
            &lock,
            None,
            None,
            &frame,
            &mut output,
            4096,
        );
        assert!(
            output.contains("could not answer a permission request") && output.contains("closed"),
            "a lost permission response must be named in the output: {output:?}"
        );
    }

    #[test]
    fn bounded_frame_reader_drops_oversized_frame_and_recovers() {
        let bytes = b"12345\n{\"ok\":true}\n";
        let mut reader = std::io::BufReader::new(std::io::Cursor::new(bytes));
        assert!(matches!(
            read_bounded_frame(&mut reader, 4).unwrap(),
            FrameRead::Oversized
        ));
        match read_bounded_frame(&mut reader, 32).unwrap() {
            FrameRead::Frame(frame) => assert_eq!(frame, r#"{"ok":true}"#),
            _ => panic!("expected the frame after the oversized line"),
        }
    }

    /// Test 22: `rpc_expect` returns the matching response frame and skips non-matching frames.
    #[test]
    fn rpc_expect_returns_matching_response_frame() {
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        // Pre-stage: a notification (should be skipped), then the matching response.
        tx.send(r#"{"jsonrpc":"2.0","method":"session/update","params":{}}"#.to_string())
            .unwrap();
        tx.send(r#"{"jsonrpc":"2.0","id":1,"result":{"sessionId":"s1"}}"#.to_string())
            .unwrap();
        drop(tx); // close channel; should not reach disconnected branch

        let mut sink: Vec<u8> = Vec::new();
        let v = rpc_expect(&rx, &mut sink, 1, Duration::from_secs(5)).unwrap();
        assert_eq!(v["result"]["sessionId"], "s1");
        // No elicitation/create was sent so the sink should be empty.
        assert!(sink.is_empty());
    }

    /// Test 23: `rpc_expect` elicitation guard — a stray `elicitation/create` during the handshake
    /// phase is immediately responded with `action:"cancel"` and the expect loop continues.
    #[test]
    fn rpc_expect_cancels_stray_elicitation_create_during_handshake() {
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        // Pre-stage: stray elicitation/create (with string id), then the expected response.
        tx.send(r#"{"jsonrpc":"2.0","id":"stray-1","method":"elicitation/create","params":{"message":"hi","requestedSchema":{}}}"#.to_string())
            .unwrap();
        tx.send(r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s2"}}"#.to_string())
            .unwrap();

        let mut sink: Vec<u8> = Vec::new();
        let v = rpc_expect(&rx, &mut sink, 2, Duration::from_secs(5)).unwrap();
        assert_eq!(v["result"]["sessionId"], "s2");

        // The sink must contain one cancel response for the stray elicitation id.
        let written = std::str::from_utf8(&sink).unwrap().trim_end();
        let cancel: serde_json::Value = serde_json::from_str(written)
            .expect("rpc_expect must write a cancel response for the stray frame");
        assert_eq!(
            cancel["id"],
            serde_json::Value::String("stray-1".to_string()),
            "cancel response must echo the stray request id verbatim: {cancel}"
        );
        assert_eq!(cancel["result"]["action"], "cancel");
    }

    /// Test 24: `rpc_expect` returns `Err` when the timeout expires with no matching frame.
    #[test]
    fn rpc_expect_returns_err_on_timeout() {
        let (_tx, rx) = std::sync::mpsc::channel::<String>(); // nothing sent
        let mut sink: Vec<u8> = Vec::new();
        let result = rpc_expect(&rx, &mut sink, 1, Duration::from_millis(10));
        assert!(
            result.is_err(),
            "must return Err when timeout expires: {result:?}"
        );
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("timeout"), "error must mention timeout: {msg}");
    }

    /// Test 13: schema with a single non-string property → `validate_elicitation_schema` returns None.
    #[test]
    fn schema_with_non_string_property_is_rejected() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "n": { "type": "integer" }
            }
        });
        assert!(
            validate_elicitation_schema(&schema).is_none(),
            "integer property must be rejected (only string is allowed)"
        );

        let schema_bool = serde_json::json!({
            "type": "object",
            "properties": {
                "flag": { "type": "boolean" }
            }
        });
        assert!(
            validate_elicitation_schema(&schema_bool).is_none(),
            "boolean property must be rejected"
        );
    }

    /// Test 14: schema with more than one property → `validate_elicitation_schema` returns None.
    #[test]
    fn schema_with_multiple_properties_is_rejected() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "first": { "type": "string" },
                "last": { "type": "string" }
            }
        });
        assert!(
            validate_elicitation_schema(&schema).is_none(),
            "multi-property schema must be rejected"
        );
    }

    /// Schema with exactly one string-typed property passes validation.
    #[test]
    fn schema_with_single_string_property_passes() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" }
            }
        });
        let result = validate_elicitation_schema(&schema);
        assert!(
            result.is_some(),
            "single-string schema must pass validation"
        );
        let (prop_name, prop_type) = result.unwrap();
        assert_eq!(prop_name, "name");
        assert_eq!(prop_type, Some("string".to_string()));
    }

    /// Schema with a single property but no `type` field → passes (type constraint is optional).
    #[test]
    fn schema_with_single_property_and_no_type_passes() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "answer": {}
            }
        });
        let result = validate_elicitation_schema(&schema);
        assert!(
            result.is_some(),
            "single property with no type must pass validation"
        );
        let (prop_name, prop_type) = result.unwrap();
        assert_eq!(prop_name, "answer");
        assert!(prop_type.is_none());
    }

    // ── DES-002 T5 exec_turn_acp arm tests (require a real subprocess, unix only) ──

    /// Write a Python 3 mock ACP adapter script to `dir` that handles the standard handshake
    /// then executes the behavior passed as `sys.argv[1]`. Returns the path for use as
    /// `AcpConfig::binary`; pass the behavior name as the first element of `AcpConfig::start_args`.
    ///
    /// Behaviors:
    /// - `"ok"`: completes immediately with `stopReason:"end_turn"`
    /// - `"elicit_ok"`: sends a valid string-schema elicitation, reads one response, completes
    /// - `"elicit_disabled"`: sends a valid string-schema elicitation, tolerates the cancel the
    ///   disabled-epoch path returns (does not assert an accept), completes
    /// - `"elicit_multi_prop"`: sends a multi-property schema → must receive immediate cancel, completes
    /// - `"elicit_non_string"`: sends an integer-type schema → immediate cancel, completes
    /// - `"elicit_nested"`: sends two elicitations in rapid succession (to test test-20)
    /// - `"elicit_disconnect"`: sends elicitation then closes stdout (test-19)
    /// - `"perm_id_collision"`: drives TWO turns, walking its own request counter into the
    ///   client's prompt-id space so one `session/request_permission` carries the same id as the
    ///   in-flight `session/prompt` (core#293)
    /// - `"unknown_request"`: sends an `fs/read_text_file` request this client does not implement
    ///   and requires a JSON-RPC error answer (core#293)
    /// - `"elicit_unadvertised"`: exits nonzero if `initialize` advertised elicitation; then
    ///   sends an elicitation anyway and tolerates the cancel (core#341 — an UNVERIFIED
    ///   adapter must neither be advertised the capability nor have it served)
    ///
    /// The script is written under `file_name` (core#341): the elicitation capability is now
    /// decided from the binary's file STEM, so tests name the mock `claude-agent-acp` (via
    /// [`start_mock_proc`]) to run as a verified adapter, or anything else to run unverified.
    #[cfg(unix)]
    fn write_mock_acp_py(dir: &std::path::Path, file_name: &str) -> std::path::PathBuf {
        // The script uses Python dict literals (which json.dumps handles) to avoid Rust brace
        // escaping in format!. The behavior is controlled entirely via sys.argv[1].
        let path = dir.join(file_name);
        // Write as a raw literal (no format substitutions needed — all braces are Python dict syntax)
        std::fs::write(
            &path,
            r#"#!/usr/bin/env python3
import sys, json, time

behavior = sys.argv[1] if len(sys.argv) > 1 else "ok"

def w(obj):
    print(json.dumps(obj), flush=True)

def r():
    while True:
        line = sys.stdin.readline()
        if not line:
            return None
        line = line.strip()
        if line:
            try:
                return json.loads(line)
            except Exception:
                pass

# initialize — record whether the client ADVERTISED the elicitation capability (core#341)
req = r()
elic_advertised = "elicitation" in ((req.get("params") or {}).get("clientCapabilities") or {})
w({"jsonrpc": "2.0", "id": req["id"], "result": {
    "protocolVersion": "2025-03-26", "capabilities": {},
    "serverInfo": {"name": "mock", "version": "0"}
}})
# session/new
req = r()
w({"jsonrpc": "2.0", "id": req["id"], "result": {
    "sessionId": "mock-session", "protocolVersion": "2025-03-26"
}})
# session/prompt
req = r()
prompt_id = req["id"]

if behavior == "ok":
    w({"jsonrpc": "2.0", "id": prompt_id, "result": {
        "stopReason": "end_turn", "usage": {"inputTokens": 10, "outputTokens": 5}
    }})

elif behavior == "elicit_ok":
    # A turn that is SERVED elicitation must also have been ADVERTISED it (core#341):
    # the client only serves what it advertised, so a missing capability here means the
    # session-start and turn-time gates diverged.
    if not elic_advertised:
        sys.exit(3)
    # Valid string-schema elicitation: wicked-core must register + deliver via channel
    w({"jsonrpc": "2.0", "id": "elicit-1", "method": "elicitation/create", "params": {
        "message": "What is your name?",
        "requestedSchema": {"type": "object", "properties": {"name": {"type": "string"}}}
    }})
    answer = r()
    expected = {"action": "accept", "content": {"name": "Alice"}}
    if answer is None or answer.get("result") != expected:
        sys.exit(2)
    w({"jsonrpc": "2.0", "id": prompt_id, "result": {
        "stopReason": "end_turn", "usage": {"inputTokens": 10, "outputTokens": 5}
    }})

elif behavior == "elicit_disabled":
    # Valid string-schema elicitation that we expect to be CANCELLED because elicitation is
    # disabled for the epoch (epoch=0). Read whatever response arrives WITHOUT asserting an
    # accept (a real adapter would simply proceed), then complete the prompt so the turn is Ok.
    w({"jsonrpc": "2.0", "id": "elicit-1", "method": "elicitation/create", "params": {
        "message": "What is your name?",
        "requestedSchema": {"type": "object", "properties": {"name": {"type": "string"}}}
    }})
    r()  # receives action:cancel from the disabled path — do NOT assert it is an accept
    w({"jsonrpc": "2.0", "id": prompt_id, "result": {
        "stopReason": "end_turn", "usage": {"inputTokens": 10, "outputTokens": 5}
    }})

elif behavior == "elicit_unadvertised":
    # core#341 regression: an UNVERIFIED adapter must not be advertised the capability…
    if elic_advertised:
        sys.exit(4)
    # …and an elicitation it sends anyway must be auto-cancelled, never suspend the turn.
    w({"jsonrpc": "2.0", "id": "elicit-1", "method": "elicitation/create", "params": {
        "message": "What is your name?",
        "requestedSchema": {"type": "object", "properties": {"name": {"type": "string"}}}
    }})
    resp = r()  # must be the immediate action:cancel, not a human-delivered accept
    if resp is None or (resp.get("result") or {}).get("action") != "cancel":
        sys.exit(5)
    w({"jsonrpc": "2.0", "id": prompt_id, "result": {
        "stopReason": "end_turn", "usage": {"inputTokens": 10, "outputTokens": 5}
    }})

elif behavior == "elicit_multi_prop":
    # Multi-property schema: must be immediately cancelled
    w({"jsonrpc": "2.0", "id": "elicit-2", "method": "elicitation/create", "params": {
        "message": "Name?",
        "requestedSchema": {"type": "object", "properties": {
            "first": {"type": "string"}, "last": {"type": "string"}
        }}
    }})
    r()  # must receive action:cancel immediately
    w({"jsonrpc": "2.0", "id": prompt_id, "result": {
        "stopReason": "end_turn", "usage": {"inputTokens": 5, "outputTokens": 2}
    }})

elif behavior == "elicit_non_string":
    # Non-string type property: must be immediately cancelled
    w({"jsonrpc": "2.0", "id": "elicit-3", "method": "elicitation/create", "params": {
        "message": "Pick?",
        "requestedSchema": {"type": "object", "properties": {"n": {"type": "integer"}}}
    }})
    r()  # must receive action:cancel immediately
    w({"jsonrpc": "2.0", "id": prompt_id, "result": {
        "stopReason": "end_turn", "usage": {"inputTokens": 5, "outputTokens": 2}
    }})

elif behavior == "elicit_nested":
    # Send first elicitation, then immediately a second one while the first is pending.
    # The second must be immediately cancelled; the first is resolved by the deliver thread.
    w({"jsonrpc": "2.0", "id": "elicit-n1", "method": "elicitation/create", "params": {
        "message": "First?",
        "requestedSchema": {"type": "object", "properties": {"val": {"type": "string"}}}
    }})
    # The second is sent without waiting for the first to resolve.
    w({"jsonrpc": "2.0", "id": "elicit-n2", "method": "elicitation/create", "params": {
        "message": "Second?",
        "requestedSchema": {"type": "object", "properties": {"val": {"type": "string"}}}
    }})
    # Read the cancel for elicit-n2 (immediate)
    r()
    # Read the response for elicit-n1 (from deliver thread)
    r()
    w({"jsonrpc": "2.0", "id": prompt_id, "result": {
        "stopReason": "end_turn", "usage": {"inputTokens": 5, "outputTokens": 2}
    }})

elif behavior == "perm_id_collision":
    # core#293 regression fixture. Models the bridge SDK's OWN request counter, which starts at
    # 0, is independent of the client's `next_id`, and is never reset per turn. Two turns are
    # driven on ONE session so the counter walks INTO the client's prompt-id space.
    #
    #   turn 1 prompt id = P            (the client's next_id after the handshake)
    #   turn 2 prompt id = P + 1
    #
    # Turn 1 makes exactly P asks (agent ids 0 .. P-1) — all strictly below P, so nothing
    # collides yet and turn 1 is a clean control. The counter is now at P, so turn 2's asks are
    # id P (harmless — that was turn 1's id) and then id P+1, which EQUALS turn 2's prompt id.
    # That second ask is the defect's trigger.
    agent_id = 0

    def ask_permission():
        global agent_id
        rid = agent_id
        agent_id += 1
        w({"jsonrpc": "2.0", "id": rid, "method": "session/request_permission", "params": {
            "sessionId": "mock-session",
            "toolCall": {"toolCallId": "call-%d" % rid, "title": "Write", "kind": "edit",
                         "rawInput": {"file_path": "/tmp/x", "content": "y"}},
            "options": [
                {"optionId": "allow", "name": "Allow", "kind": "allow_once"},
                {"optionId": "reject", "name": "Reject", "kind": "reject_once"},
            ],
        }})
        resp = r()
        # The client MUST answer a permission request with a JSON-RPC result carrying an
        # `outcome`. Anything else (or EOF) means the frame was swallowed or refused.
        if resp is None or not isinstance(resp.get("result"), dict) \
                or "outcome" not in resp["result"]:
            sys.stderr.write("perm_id_collision: bad answer to id=%r: %r\n" % (rid, resp))
            sys.exit(3)
        return rid

    # ── turn 1 ────────────────────────────────────────────────────────────────────────────
    for _ in range(prompt_id):
        ask_permission()
    w({"jsonrpc": "2.0", "id": prompt_id, "result": {
        "stopReason": "end_turn", "usage": {"inputTokens": 1, "outputTokens": 1}
    }})

    # ── turn 2 ────────────────────────────────────────────────────────────────────────────
    req = r()
    if req is None or req.get("method") != "session/prompt":
        sys.stderr.write("perm_id_collision: expected a second session/prompt, got %r\n" % (req,))
        sys.exit(4)
    prompt_id2 = req["id"]

    ask_permission()                 # agent id == prompt_id (turn 1's id) — must be answered
    colliding = ask_permission()     # agent id == prompt_id2 — THE COLLISION
    if colliding != prompt_id2:
        sys.stderr.write("perm_id_collision: fixture drift, ask id %r != prompt id %r\n"
                         % (colliding, prompt_id2))
        sys.exit(5)

    # Only reachable once the colliding permission request was answered as a REQUEST. The marker
    # is what the Rust assertion looks for: with the id-only match it is never emitted, because
    # the turn was already declared complete on the permission frame itself.
    w({"jsonrpc": "2.0", "method": "session/update", "params": {
        "sessionId": "mock-session",
        "update": {"sessionUpdate": "agent_message_chunk",
                   "content": {"type": "text", "text": "PERMISSION_ANSWERED"}}
    }})
    w({"jsonrpc": "2.0", "id": prompt_id2, "result": {
        "stopReason": "end_turn", "usage": {"inputTokens": 2, "outputTokens": 2}
    }})

elif behavior == "elicit_perm":
    # core#293: a permission request that arrives while the turn is SUSPENDED on an elicitation.
    # The 'elicit sub-loop had no arm for it, so it was silently dropped and the agent blocked.
    w({"jsonrpc": "2.0", "id": "elicit-p1", "method": "elicitation/create", "params": {
        "message": "Which one?",
        "requestedSchema": {"type": "object", "properties": {"pick": {"type": "string"}}}
    }})
    w({"jsonrpc": "2.0", "id": 0, "method": "session/request_permission", "params": {
        "sessionId": "mock-session",
        "toolCall": {"toolCallId": "call-e", "title": "Write", "kind": "edit",
                     "rawInput": {"file_path": "/tmp/x", "content": "y"}},
        "options": [
            {"optionId": "allow", "name": "Allow", "kind": "allow_once"},
            {"optionId": "reject", "name": "Reject", "kind": "reject_once"},
        ],
    }})
    # Both answers must arrive; order is not guaranteed (the elicitation is resolved by a
    # separate thread) so classify rather than assume.
    saw_permission = False
    saw_elicitation = False
    for _ in range(2):
        resp = r()
        if resp is None or not isinstance(resp.get("result"), dict):
            break
        if "outcome" in resp["result"]:
            saw_permission = True
        elif "action" in resp["result"]:
            saw_elicitation = True
    if not (saw_permission and saw_elicitation):
        sys.stderr.write("elicit_perm: permission=%r elicitation=%r\n"
                         % (saw_permission, saw_elicitation))
        sys.exit(7)
    w({"jsonrpc": "2.0", "method": "session/update", "params": {
        "sessionId": "mock-session",
        "update": {"sessionUpdate": "agent_message_chunk",
                   "content": {"type": "text", "text": "PERM_DURING_ELICIT"}}
    }})
    w({"jsonrpc": "2.0", "id": prompt_id, "result": {
        "stopReason": "end_turn", "usage": {"inputTokens": 1, "outputTokens": 1}
    }})

elif behavior == "unknown_request":
    # core#293: an inbound REQUEST for a method this client does not implement must come back as
    # a JSON-RPC error, not be dropped. `fs/read_text_file` is the concrete case — `fs: {}`
    # advertises no filesystem capability, so a conforming agent never asks, but a
    # non-conforming one must not be left blocked.
    w({"jsonrpc": "2.0", "id": "fsr-1", "method": "fs/read_text_file", "params": {
        "sessionId": "mock-session", "path": "/etc/hosts"
    }})
    resp = r()
    if resp is None or "error" not in resp or resp.get("id") != "fsr-1":
        sys.stderr.write("unknown_request: expected an error response, got %r\n" % (resp,))
        sys.exit(6)
    w({"jsonrpc": "2.0", "method": "session/update", "params": {
        "sessionId": "mock-session",
        "update": {"sessionUpdate": "agent_message_chunk",
                   "content": {"type": "text", "text": "REFUSED_%d" % resp["error"]["code"]}}
    }})
    w({"jsonrpc": "2.0", "id": prompt_id, "result": {
        "stopReason": "end_turn", "usage": {"inputTokens": 1, "outputTokens": 1}
    }})

elif behavior == "elicit_disconnect":
    # Send elicitation, then close stdout (simulate adapter death mid-suspension).
    w({"jsonrpc": "2.0", "id": "elicit-disc", "method": "elicitation/create", "params": {
        "message": "Are you there?",
        "requestedSchema": {"type": "object", "properties": {"ans": {"type": "string"}}}
    }})
    # Close stdout — wicked-core's line_rx will see Disconnected.
    sys.stdout.close()
    time.sleep(10)  # keep the process alive so stdin-EOF doesn't affect the turn

else:
    w({"jsonrpc": "2.0", "id": prompt_id, "result": {
        "stopReason": "end_turn", "usage": {"inputTokens": 1, "outputTokens": 1}
    }})
"#,
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// Start a mock ACP process using the shared Python bridge script, written under the
    /// VERIFIED adapter name `claude-agent-acp` (core#341): the elicitation capability is
    /// decided from the binary's file stem, so this mock is advertised — and served —
    /// elicitation exactly like the stock claude seat's bridge.
    /// The `behavior` string is passed as `start_args[0]` to the script.
    #[cfg(unix)]
    fn start_mock_proc(dir: &std::path::Path, behavior: &str) -> AcpProcess {
        start_mock_proc_named(dir, behavior, "claude-agent-acp")
    }

    /// [`start_mock_proc`] with an explicit script file name, for tests that need an
    /// UNVERIFIED adapter (any stem not in `ELICITATION_VERIFIED_ADAPTERS`).
    #[cfg(unix)]
    fn start_mock_proc_named(dir: &std::path::Path, behavior: &str, file_name: &str) -> AcpProcess {
        let py_path = write_mock_acp_py(dir, file_name);
        let config = AcpConfig {
            binary: py_path.to_string_lossy().to_string(),
            start_args: vec![behavior.to_string()],
            transport: AcpTransport::default(),
            auth_method: None,
            acp_input_governance: false,
            os_sandbox: false,
            acp_governance_env: None,
            verified_version: None,
        };
        // The spawn resolves WICKED_WORKER_HOME mid-call (`ensure_worker_config_home`), so hold
        // the env read-lock across the start (core#285): without it, a start landing inside a
        // fixture test's mutation window resolved that test's symlink-refusal home and tripped
        // the FINDING-061 guard — the exact full-suite flake this closes. Held for the start
        // only; a running process never re-reads the variable. Lock order: callers must not
        // hold REAL_STARTS when calling this (none do).
        let _env = ENV_LOCK.read().unwrap_or_else(|p| p.into_inner());
        start_acp_process(&config, dir, None, None)
            .unwrap_or_else(|e| panic!("mock ACP start failed for behavior={behavior}: {e}"))
    }

    /// Test 12: when `elicitation_epoch == 0` the arm is disabled — the adapter's `elicitation/create`
    /// gets an immediate cancel and the turn completes normally as `Ok`.
    #[test]
    #[cfg(unix)]
    fn elicitation_disabled_when_epoch_is_zero() {
        let dir = std::env::temp_dir().join(format!("wicked-des002-t12-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let mut proc = start_mock_proc(&dir, "elicit_disabled");
        let maps = Arc::new(Mutex::new(ElicitationMaps::new()));
        let (tx, _rx) = std::sync::mpsc::channel();
        let noop: &DeltaSink = &|_: &str| {};

        // epoch=0 → elicitation disabled for this turn.
        let result = exec_turn_acp(
            &mut proc,
            "hello",
            &[],
            noop,
            Duration::from_secs(5),
            maps,
            "run-t12",
            0, // epoch=0 → disabled
            &tx,
            None,
        )
        .unwrap();
        assert_eq!(
            result.status,
            StepStatus::Ok,
            "turn must complete ok when elicitation is disabled: {:?}",
            result.status
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── core#341: ONE source of truth for elicitation support ─────────────────────────

    /// core#341 — the verified-adapter predicate classifies by the binary's file STEM, so
    /// every seat shape resolves the same way: bare binary names, absolute paths, platform
    /// suffixes. Registry KEYS (`claude`, `codex`, aliased seats) are NOT verified names —
    /// gating on them is the exact divergence #341 closes.
    #[test]
    fn elicitation_verified_adapter_classifies_by_binary_stem() {
        // The verified bridges, in every shape a registry/clis.toml record can spell them.
        for verified in [
            "claude-agent-acp",
            "codex-acp",
            "/opt/bridges/claude-agent-acp",
            "claude-agent-acp.cmd", // the Windows spawn retry's shape
            "./relative/codex-acp",
        ] {
            assert!(
                elicitation_verified_adapter(verified),
                "must be verified: {verified:?}"
            );
        }
        // Registry keys, the wrapped CLIs, and arbitrary bridges are NOT verified.
        for unverified in [
            "claude", // the stock seat's KEY — the old turn-time gate checked this
            "codex",
            "claude-eval", // an aliased seat's key
            "agy-acp",
            "mock-acp-bridge.py",
            "",
        ] {
            assert!(
                !elicitation_verified_adapter(unverified),
                "must not be verified: {unverified:?}"
            );
        }
    }

    /// core#341 — the stock seats' elicitation support is a property of their ACP BINARY.
    /// The built-in `claude` seat (key `claude`, bridge `claude-agent-acp`) and `codex`
    /// seat (key `codex`, bridge `codex-acp`) must classify as verified through the one
    /// predicate both sites use — while the old key-based lookup returns false for every
    /// stock seat, which is exactly how `claude` advertised elicitation and then
    /// auto-cancelled it at turn time.
    #[test]
    fn stock_seats_support_elicitation_by_binary_not_by_key() {
        let builtin = wicked_council::registry::builtin();
        for key in ["claude", "codex"] {
            let seat = builtin
                .iter()
                .find(|c| c.key == key)
                .unwrap_or_else(|| panic!("builtin registry must have a '{key}' seat"));
            let acp = seat
                .acp
                .as_ref()
                .unwrap_or_else(|| panic!("stock '{key}' seat must have an ACP config"));
            assert!(
                elicitation_verified_adapter(&acp.binary),
                "stock '{key}' seat's bridge ({}) must be elicitation-verified",
                acp.binary
            );
            // The divergence regression: the seat KEY is not a verified adapter name, so any
            // gate that consults the key disagrees with the advertisement for every stock seat.
            assert!(
                !ELICITATION_VERIFIED_ADAPTERS.contains(&seat.key.as_str()),
                "ELICITATION_VERIFIED_ADAPTERS holds BINARY names; seat key '{}' must not \
                 appear in it (keys are not a capability truth — core#341)",
                seat.key
            );
        }
        // Advertisement and turn-time behavior are the same stored decision for EVERY seat:
        // both read `AcpProcess::elicitation_advertised`, which is derived from the binary.
        // Spot-check the derivation for every builtin ACP seat so a future roster entry
        // cannot reintroduce a key that shadows a verified binary stem.
        for seat in builtin.iter().filter(|c| c.acp.is_some()) {
            let advertised = elicitation_verified_adapter(&seat.acp.as_ref().unwrap().binary);
            let old_key_gate = ELICITATION_VERIFIED_ADAPTERS.contains(&seat.key.as_str());
            assert!(
                !old_key_gate || advertised,
                "seat '{}': key-based gating would serve elicitation that was never \
                 advertised",
                seat.key
            );
        }
    }

    /// core#341 regression (the fixed side): a VERIFIED adapter both advertises elicitation
    /// at session start AND has it served at turn time. The mock bridge runs under the
    /// verified name `claude-agent-acp` and exits nonzero if `initialize` did NOT advertise
    /// the capability, so a re-divergence fails this test at the wire, not just in the flag.
    #[test]
    #[cfg(unix)]
    fn verified_adapter_advertises_and_serves_elicitation() {
        let dir = std::env::temp_dir().join(format!("wicked-341-served-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let mut proc = start_mock_proc(&dir, "elicit_ok");
        assert!(
            proc.elicitation_advertised,
            "a verified adapter's session must record the advertised capability"
        );

        let maps = Arc::new(Mutex::new(ElicitationMaps::new()));
        let (tx, _rx) = std::sync::mpsc::channel();
        let noop: &DeltaSink = &|_: &str| {};

        // Deliver the human response from a concurrent thread so 'elicit doesn't time out.
        let maps_clone = Arc::clone(&maps);
        let deliver_thread = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            let m = maps_clone.lock().unwrap();
            let elicitation_id = m.pending.keys().next().cloned();
            drop(m);
            if let Some(id) = elicitation_id {
                let mut m = maps_clone.lock().unwrap();
                let _ = m.deliver(
                    "run-341-served",
                    &id,
                    "accept".to_string(),
                    Some(serde_json::json!("Alice")),
                );
            }
        });

        let result = exec_turn_acp(
            &mut proc,
            "go",
            &[],
            noop,
            Duration::from_secs(5),
            Arc::clone(&maps),
            "run-341-served",
            1, // governed epoch: the capability question is decided by the ADAPTER alone
            &tx,
            None,
        )
        .unwrap();
        deliver_thread.join().unwrap();

        // elicit_ok exits 2 if the accept never arrived and 3 if the capability was not
        // advertised — either kills the prompt and this turn would not be Ok.
        assert_eq!(
            result.status,
            StepStatus::Ok,
            "verified adapter must be advertised AND served elicitation"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// core#341 regression (the auto-cancel divergence): an UNVERIFIED adapter must get the
    /// capability at NEITHER site — no advertisement at `initialize`, and an immediate
    /// `action:cancel` if it elicits anyway, even in a governed (non-zero) epoch. The mock
    /// exits nonzero if it was advertised the capability or if the cancel never arrived.
    #[test]
    #[cfg(unix)]
    fn unverified_adapter_neither_advertises_nor_serves_elicitation() {
        let dir = std::env::temp_dir().join(format!("wicked-341-unadv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        // Any stem outside ELICITATION_VERIFIED_ADAPTERS — the pre-#341 mock's own name.
        let mut proc = start_mock_proc_named(&dir, "elicit_unadvertised", "mock-acp-bridge.py");
        assert!(
            !proc.elicitation_advertised,
            "an unverified adapter's session must not record the capability"
        );

        let maps = Arc::new(Mutex::new(ElicitationMaps::new()));
        let (tx, _rx) = std::sync::mpsc::channel();
        let noop: &DeltaSink = &|_: &str| {};

        let result = exec_turn_acp(
            &mut proc,
            "go",
            &[],
            noop,
            Duration::from_secs(5),
            maps,
            "run-341-unadv",
            1, // epoch>0: only the adapter's (un)verified status disables elicitation here
            &tx,
            None,
        )
        .unwrap();
        assert_eq!(
            result.status,
            StepStatus::Ok,
            "unverified adapter's elicitation must be auto-cancelled and the turn completed"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// core#341 adversarial probe: SEATS whose key and binary disagree, in BOTH directions,
    /// loaded through the real user-overlay seam (`registry::load` with a clis.toml — the
    /// same loader `registry_record`/`acp_config_for` consult) and driven to the wire. The
    /// seat KEY must be inert at both sites:
    ///
    /// - a custom seat whose KEY is spelled exactly like the verified bridge
    ///   (`claude-agent-acp`) over an UNVERIFIED binary must not be advertised the
    ///   capability, and an elicitation it raises anyway is auto-cancelled — the old
    ///   key-based turn gate would have SERVED this seat what was never advertised
    /// - a custom seat with an arbitrary key over the VERIFIED bridge binary (spelled as an
    ///   absolute path, the clis.toml shape) must be advertised AND served — the old key
    ///   gate auto-cancelled this shape, which is the stock-`claude`-seat bug itself
    ///
    /// The mock asserts advertisement at the wire (exit 3/4) and the serve/cancel behavior
    /// (exit 2/5), so advertisement == turn-time handling is proven per direction.
    #[test]
    #[cfg(unix)]
    fn seat_key_is_inert_for_elicitation_in_both_divergence_directions() {
        let dir = std::env::temp_dir().join(format!("wicked-341-seatkey-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        // Two mock bridges: one under the VERIFIED stem, one under an arbitrary name.
        let verified_bridge = write_mock_acp_py(&dir, "claude-agent-acp");
        let custom_bridge = write_mock_acp_py(&dir, "my-custom-bridge");

        // A user overlay whose seats cross key and binary in both directions.
        let toml_path = dir.join("clis.toml");
        std::fs::write(
            &toml_path,
            format!(
                r#"
[[cli]]
key = "claude-agent-acp"
display_name = "Verified-looking key, unverified bridge"
binary = "irrelevant"
headless_invocation = "irrelevant \"{{PROMPT}}\""

[cli.acp]
binary = "{custom}"
start_args = ["elicit_unadvertised"]
transport = "stdio"

[[cli]]
key = "totally-custom"
display_name = "Arbitrary key, verified bridge"
binary = "irrelevant"
headless_invocation = "irrelevant \"{{PROMPT}}\""

[cli.acp]
binary = "{verified}"
start_args = ["elicit_ok"]
transport = "stdio"
"#,
                custom = custom_bridge.display(),
                verified = verified_bridge.display(),
            ),
        )
        .unwrap();

        let merged = wicked_council::registry::load(Some(&toml_path)).expect("overlay must parse");

        for (key, expect_advertised) in [("claude-agent-acp", false), ("totally-custom", true)] {
            let seat = merged
                .iter()
                .find(|c| c.key == key)
                .unwrap_or_else(|| panic!("overlay seat '{key}' must be in the merged registry"));
            let acp = seat
                .acp
                .as_ref()
                .unwrap_or_else(|| panic!("overlay seat '{key}' must carry [cli.acp]"));

            // The one predicate both sites share must classify by the BINARY alone.
            assert_eq!(
                elicitation_verified_adapter(&acp.binary),
                expect_advertised,
                "seat '{key}': elicitation is a property of the binary, never the key"
            );

            let run_id = format!("run-341-seatkey-{key}");
            let mut proc = {
                // Hold the env read-lock across the start (core#285) like `start_mock_proc`.
                let _env = ENV_LOCK.read().unwrap_or_else(|p| p.into_inner());
                start_acp_process(acp, &dir, None, None)
                    .unwrap_or_else(|e| panic!("seat '{key}' mock start failed: {e}"))
            };
            assert_eq!(
                proc.elicitation_advertised, expect_advertised,
                "seat '{key}': the stored advertisement decision must follow the binary"
            );

            let maps = Arc::new(Mutex::new(ElicitationMaps::new()));
            let (tx, _rx) = std::sync::mpsc::channel();
            let noop: &DeltaSink = &|_: &str| {};

            // For the served direction, deliver the human response from a concurrent
            // thread (polling: the elicitation registers mid-turn).
            let deliver_thread = expect_advertised.then(|| {
                let maps_clone = Arc::clone(&maps);
                let run_id = run_id.clone();
                std::thread::spawn(move || {
                    for _ in 0..80 {
                        std::thread::sleep(Duration::from_millis(50));
                        let pending = {
                            let m = maps_clone.lock().unwrap();
                            m.pending.keys().next().cloned()
                        };
                        if let Some(id) = pending {
                            let mut m = maps_clone.lock().unwrap();
                            let _ = m.deliver(
                                &run_id,
                                &id,
                                "accept".to_string(),
                                Some(serde_json::json!("Alice")),
                            );
                            return;
                        }
                    }
                })
            });

            let result = exec_turn_acp(
                &mut proc,
                "go",
                &[],
                noop,
                Duration::from_secs(5),
                Arc::clone(&maps),
                &run_id,
                1, // governed epoch: only the adapter's binary decides the capability
                &tx,
                None,
            )
            .unwrap_or_else(|e| panic!("seat '{key}' turn failed: {e}"));
            if let Some(t) = deliver_thread {
                t.join().unwrap();
            }

            // The mock exits nonzero on any advertisement/serve divergence, which would
            // kill the prompt mid-turn and the status would not be Ok.
            assert_eq!(
                result.status,
                StepStatus::Ok,
                "seat '{key}': advertisement and turn-time handling must be one decision"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Test 13 & 14 integration: multi-property and non-string-schema elicitations are immediately
    /// cancelled and the turn still completes as `Ok`.
    #[test]
    #[cfg(unix)]
    fn invalid_schema_elicitations_are_cancelled_and_turn_completes_ok() {
        for behavior in ["elicit_multi_prop", "elicit_non_string"] {
            let dir = std::env::temp_dir().join(format!(
                "wicked-des002-schema-{}-{}",
                behavior,
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).unwrap();

            let mut proc = start_mock_proc(&dir, behavior);
            let maps = Arc::new(Mutex::new(ElicitationMaps::new()));
            let (tx, _rx) = std::sync::mpsc::channel();
            let noop: &DeltaSink = &|_: &str| {};

            // epoch=1, verified adapter → elicitation enabled, but schema invalid → immediate cancel.
            let result = exec_turn_acp(
                &mut proc,
                "go",
                &[],
                noop,
                Duration::from_secs(5),
                maps,
                "run-schema",
                1,
                &tx,
                None,
            )
            .unwrap();
            assert_eq!(
                result.status,
                StepStatus::Ok,
                "turn must complete ok after invalid schema cancel ({behavior}): {:?}",
                result.status
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// Test 15: a valid elicitation schema causes `ElicitationCreated` to be emitted.
    #[test]
    #[cfg(unix)]
    fn valid_elicitation_schema_emits_elicitation_created_event() {
        let dir = std::env::temp_dir().join(format!("wicked-des002-t15-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let mut proc = start_mock_proc(&dir, "elicit_ok");
        let maps = Arc::new(Mutex::new(ElicitationMaps::new()));
        let (tx, event_rx) = std::sync::mpsc::channel();
        let noop: &DeltaSink = &|_: &str| {};

        // Deliver a response from a concurrent thread so 'elicit doesn't time out.
        let maps_clone = Arc::clone(&maps);
        let deliver_thread = std::thread::spawn(move || {
            // Give exec_turn_acp time to register the elicitation.
            std::thread::sleep(Duration::from_millis(200));
            let m = maps_clone.lock().unwrap();
            // Find the elicitation id and deliver a response.
            let elicitation_id = m.pending.keys().next().cloned();
            drop(m);
            if let Some(id) = elicitation_id {
                let mut m = maps_clone.lock().unwrap();
                let _ = m.deliver(
                    "run-t15",
                    &id,
                    "accept".to_string(),
                    Some(serde_json::json!("Alice")),
                );
            }
        });

        let result = exec_turn_acp(
            &mut proc,
            "go",
            &[],
            noop,
            Duration::from_secs(5),
            Arc::clone(&maps),
            "run-t15",
            1,
            &tx,
            None,
        )
        .unwrap();
        deliver_thread.join().unwrap();

        assert_eq!(result.status, StepStatus::Ok);

        // Check that ElicitationCreated was emitted.
        let events: Vec<_> = event_rx.try_iter().collect();
        let created = events.iter().find(|c| {
            matches!(
                c,
                Command::EmitEvent(crate::event::CoreEvent::ElicitationCreated { .. })
            )
        });
        assert!(
            created.is_some(),
            "ElicitationCreated must be emitted for a valid schema"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Test 19: adapter stdout disconnect while the turn is suspended on elicitation → `ElicitationFailed`.
    #[test]
    #[cfg(unix)]
    fn adapter_disconnect_mid_elicitation_returns_elicitation_failed() {
        let dir = std::env::temp_dir().join(format!("wicked-des002-t19-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let mut proc = start_mock_proc(&dir, "elicit_disconnect");
        let maps = Arc::new(Mutex::new(ElicitationMaps::new()));
        let (tx, _rx) = std::sync::mpsc::channel();
        let noop: &DeltaSink = &|_: &str| {};

        let result = exec_turn_acp(
            &mut proc,
            "go",
            &[],
            noop,
            Duration::from_secs(5),
            maps,
            "run-t19",
            1,
            &tx,
            None,
        )
        .unwrap();
        assert_eq!(
            result.status,
            StepStatus::ElicitationFailed,
            "adapter disconnect mid-elicitation must yield ElicitationFailed: {:?}",
            result.status
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Test 20: a second `elicitation/create` while suspended on the first is immediately
    /// cancelled (spec I-5: only one in-flight elicitation per turn).
    #[test]
    #[cfg(unix)]
    fn nested_elicitation_create_is_immediately_cancelled() {
        let dir = std::env::temp_dir().join(format!("wicked-des002-t20-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        // Use the "elicit_nested" behavior: mock sends elicit-nested-2 while elicit-nested-1 is
        // pending, then sends cancel for both, then completes the prompt.
        let mut proc = start_mock_proc(&dir, "elicit_nested");
        let maps = Arc::new(Mutex::new(ElicitationMaps::new()));
        let maps_clone = Arc::clone(&maps);
        let (tx, _event_rx) = std::sync::mpsc::channel();
        let noop: &DeltaSink = &|_: &str| {};

        // Concurrently deliver a cancel for the FIRST elicitation (no human available).
        let deliver_thread = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(400));
            let m = maps_clone.lock().unwrap();
            let elicitation_id = m.pending.keys().next().cloned();
            drop(m);
            if let Some(id) = elicitation_id {
                let mut m = maps_clone.lock().unwrap();
                let _ = m.deliver("run-t20", &id, "cancel".to_string(), None);
            }
        });

        let result = exec_turn_acp(
            &mut proc,
            "go",
            &[],
            noop,
            Duration::from_secs(5),
            Arc::clone(&maps),
            "run-t20",
            1,
            &tx,
            None,
        )
        .unwrap();
        deliver_thread.join().unwrap();

        assert_eq!(
            result.status,
            StepStatus::ElicitationFailed,
            "a human cancellation is terminal and must bypass retry"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── core#293: agent request ids cross client prompt ids ───────────────────────

    /// THE core#293 REGRESSION TEST — two turns on ONE session, driven until the agent's own
    /// request counter walks into the client's prompt-id space.
    ///
    /// The two id spaces are independent: `AcpProcess::next_id` starts at 2 and is never reset
    /// per turn; the bridge SDK counts its own requests from 0. They eventually cross. Before the
    /// fix, the dispatcher matched inbound frames to the in-flight prompt on `id` ALONE, so on a
    /// crossing the agent's `session/request_permission` was consumed as the prompt RESULT: no
    /// `result.stopReason` → `unwrap_or("end_turn")` → turn 2 returned Ok while the agent sat
    /// blocked on a permission nobody would ever answer.
    ///
    /// FAILS BEFORE THE FIX: turn 2 returns with none of the post-permission output, because the
    /// mock never gets an answer to the colliding request and so never emits the marker or the
    /// prompt result. The turn nonetheless reports `Ok` — which is exactly the lie the defect
    /// told, so the assertion is on the OUTPUT, not on the status.
    #[test]
    #[cfg(unix)]
    fn agent_request_id_colliding_with_the_prompt_id_is_answered_not_swallowed() {
        let dir = std::env::temp_dir().join(format!("wicked-core293-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let mut proc = start_mock_proc(&dir, "perm_id_collision");
        let maps = Arc::new(Mutex::new(ElicitationMaps::new()));
        let (tx, _rx) = std::sync::mpsc::channel();
        let noop: &DeltaSink = &|_: &str| {};

        // The prompt id turn 1 is about to use — the fixture asks exactly this many permissions
        // so its counter lands on turn 2's prompt id.
        let turn1_id = proc.next_id;

        let turn1 = exec_turn_acp(
            &mut proc,
            "turn one",
            &[],
            noop,
            Duration::from_secs(10),
            Arc::clone(&maps),
            "run-293",
            0,
            &tx,
            None,
        )
        .unwrap();
        assert_eq!(
            turn1.status,
            StepStatus::Ok,
            "turn 1 is the control — every ask is below the prompt id, so it must pass even \
             with the defect present: status={:?} output={:?}",
            turn1.status,
            turn1.output
        );
        assert_eq!(
            proc.next_id,
            turn1_id + 1,
            "the client's prompt id must advance by exactly one per turn — the fixture's \
             collision arithmetic depends on it"
        );

        let turn2 = exec_turn_acp(
            &mut proc,
            "turn two",
            &[],
            noop,
            Duration::from_secs(10),
            Arc::clone(&maps),
            "run-293",
            0,
            &tx,
            None,
        )
        .unwrap();

        assert!(
            turn2.output.contains("PERMISSION_ANSWERED"),
            "the permission request whose id EQUALS turn 2's prompt id must be answered as a \
             REQUEST; if it is consumed as the prompt result the turn ends early and this marker \
             never arrives. output={:?} status={:?}",
            turn2.output,
            turn2.status
        );
        assert_eq!(
            turn2.status,
            StepStatus::Ok,
            "turn 2 must complete on the agent's real prompt result: output={:?}",
            turn2.output
        );
        assert_eq!(
            turn2.usage.as_ref().map(|u| u.input_tokens),
            Some(2),
            "usage must come from the REAL prompt result (inputTokens=2), not from a permission \
             frame misread as one: {:?}",
            turn2.usage
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// core#293 catch-all: an inbound REQUEST for a method this client does not implement gets a
    /// JSON-RPC error response instead of being dropped. `fs/read_text_file` is the concrete case
    /// — `fs: {}` advertises NO filesystem capability (both `readTextFile` and `writeTextFile`
    /// default to false), so a conforming agent never asks; a non-conforming one must still not
    /// be left blocked until the turn timeout.
    #[test]
    #[cfg(unix)]
    fn unhandled_inbound_request_is_refused_rather_than_dropped() {
        let dir = std::env::temp_dir().join(format!("wicked-core293-unk-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let mut proc = start_mock_proc(&dir, "unknown_request");
        let maps = Arc::new(Mutex::new(ElicitationMaps::new()));
        let (tx, _rx) = std::sync::mpsc::channel();
        let noop: &DeltaSink = &|_: &str| {};

        let result = exec_turn_acp(
            &mut proc,
            "go",
            &[],
            noop,
            Duration::from_secs(10),
            maps,
            "run-293-unk",
            0,
            &tx,
            None,
        )
        .unwrap();

        assert!(
            result
                .output
                .contains(&format!("REFUSED_{METHOD_NOT_FOUND_CODE}")),
            "an unhandled request must be answered with JSON-RPC {METHOD_NOT_FOUND_CODE}; the \
             mock only emits this marker once it has read that error frame. output={:?}",
            result.output
        );
        assert_eq!(result.status, StepStatus::Ok, "output={:?}", result.output);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// core#293, second dispatcher: a `session/request_permission` arriving while the turn is
    /// SUSPENDED on an elicitation must be answered. The `'elicit` sub-loop had no arm for it and
    /// dropped it, blocking the agent exactly as hard as the id collision did.
    ///
    /// FAILS BEFORE THE FIX: the mock never receives the permission answer, exits non-zero, and
    /// the turn ends with no `stopReason`.
    #[test]
    #[cfg(unix)]
    fn permission_request_during_an_elicitation_is_answered_not_dropped() {
        let dir = std::env::temp_dir().join(format!("wicked-core293-ep-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let mut proc = start_mock_proc(&dir, "elicit_perm");
        let maps = Arc::new(Mutex::new(ElicitationMaps::new()));
        let maps_clone = Arc::clone(&maps);
        let (tx, _rx) = std::sync::mpsc::channel();
        let noop: &DeltaSink = &|_: &str| {};

        // Resolve the elicitation from a second thread so `'elicit` is genuinely suspended while
        // the permission request arrives.
        let deliver_thread = std::thread::spawn(move || {
            for _ in 0..50 {
                std::thread::sleep(Duration::from_millis(100));
                let pending = {
                    let m = maps_clone.lock().unwrap_or_else(|p| p.into_inner());
                    m.pending.keys().next().cloned()
                };
                if let Some(id) = pending {
                    let mut m = maps_clone.lock().unwrap_or_else(|p| p.into_inner());
                    let _ = m.deliver(
                        "run-293-ep",
                        &id,
                        "accept".to_string(),
                        Some(serde_json::json!("first")),
                    );
                    return;
                }
            }
        });

        let result = exec_turn_acp(
            &mut proc,
            "go",
            &[],
            noop,
            Duration::from_secs(15),
            Arc::clone(&maps),
            "run-293-ep",
            1, // epoch > 0 + verified adapter ⇒ elicitation enabled, so 'elicit is entered
            &tx,
            None,
        )
        .unwrap();
        deliver_thread.join().unwrap();

        assert!(
            result.output.contains("PERM_DURING_ELICIT"),
            "the suspended turn must still answer permission requests; the mock only emits this \
             marker after it has read BOTH answers. output={:?} status={:?}",
            result.output,
            result.status
        );
        assert_eq!(result.status, StepStatus::Ok, "output={:?}", result.output);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Unit-level statement of the same rule: a frame carrying BOTH a `method` and an id equal to
    /// the one we are waiting on is a REQUEST, not a response.
    #[test]
    fn frame_with_method_is_never_a_response_even_on_a_colliding_id() {
        let permission_request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "session/request_permission",
            "params": {"sessionId": "s"}
        });
        assert!(
            !is_response_to(&permission_request, 4),
            "an agent REQUEST that happens to reuse our id must not be read as our response"
        );

        let real_response = serde_json::json!({
            "jsonrpc": "2.0", "id": 4, "result": {"stopReason": "end_turn"}
        });
        assert!(is_response_to(&real_response, 4));
        assert!(
            !is_response_to(&real_response, 5),
            "a response to a different id is not ours"
        );

        let notification = serde_json::json!({
            "jsonrpc": "2.0", "method": "session/update", "params": {}
        });
        assert!(!is_response_to(&notification, 4));
    }

    /// The other side of the same rule: an adapter that sloppily ECHOES the method back on its
    /// RESPONSE must still be understood as answering us. Tightening the classifier to "any
    /// `method` ⇒ request" without this would refuse such a response as an unknown method and
    /// hang the handshake — trading the core#293 wedge for a different one.
    #[test]
    fn a_response_that_echoes_its_method_is_still_a_response() {
        let echoing = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/new",
            "result": {"sessionId": "s"}
        });
        assert!(agent_method(&echoing).is_none());
        assert!(is_response_to(&echoing, 2));

        let echoing_error = serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "session/new",
            "error": {"code": -32000, "message": "nope"}
        });
        assert!(is_response_to(&echoing_error, 2));

        // A bare `{"id":n}` carries no method, so it stays a (useless) response and still
        // terminates the wait, exactly as before — never a silent hang.
        let bare = serde_json::json!({"jsonrpc": "2.0", "id": 2});
        assert!(is_response_to(&bare, 2));
    }

    /// The handshake dispatcher obeys the same rule: a colliding agent REQUEST is refused (so the
    /// agent is not left blocked) and `rpc_expect` keeps waiting for the REAL response.
    #[test]
    fn rpc_expect_refuses_a_colliding_request_and_waits_for_the_real_response() {
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        // A request that reuses the very id the handshake is waiting on.
        tx.send(
            r#"{"jsonrpc":"2.0","id":2,"method":"session/request_permission","params":{}}"#
                .to_string(),
        )
        .unwrap();
        // Then the genuine response to id 2.
        tx.send(r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s-real"}}"#.to_string())
            .unwrap();

        let mut sink: Vec<u8> = Vec::new();
        let v = rpc_expect(&rx, &mut sink, 2, Duration::from_secs(5)).unwrap();
        assert_eq!(
            v["result"]["sessionId"], "s-real",
            "the colliding request must not be returned as the handshake response: {v}"
        );

        let written = std::str::from_utf8(&sink).unwrap().trim_end();
        let refusal: serde_json::Value = serde_json::from_str(written)
            .expect("the colliding request must be answered, not dropped");
        assert_eq!(refusal["id"], 2);
        assert_eq!(refusal["error"]["code"], METHOD_NOT_FOUND_CODE);
    }

    /// An unknown NOTIFICATION (method, no id) blocks nobody and must NOT draw an error response —
    /// JSON-RPC forbids responding to a notification.
    #[test]
    fn rpc_expect_ignores_unknown_notifications_without_responding() {
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        tx.send(r#"{"jsonrpc":"2.0","method":"some/futureNotification","params":{}}"#.to_string())
            .unwrap();
        tx.send(r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#.to_string())
            .unwrap();

        let mut sink: Vec<u8> = Vec::new();
        let v = rpc_expect(&rx, &mut sink, 1, Duration::from_secs(5)).unwrap();
        assert_eq!(v["result"]["ok"], true);
        assert!(
            sink.is_empty(),
            "a notification must never be responded to: {:?}",
            std::str::from_utf8(&sink)
        );
    }

    /// Test 37: `session/prompt` result usage replaces (not merges with) prior `usage_update` tokens
    /// when both are present. The result carries authoritative token counts; only cost is kept from
    /// the notification path because adapters like the official claude bridge report cost in
    /// `usage_update` and tokens in the prompt result.
    #[test]
    fn session_prompt_result_usage_replaces_prior_usage_update_tokens() {
        // Simulate a usage_update arriving before the result (cost-only, no tokens).
        let emit_fn = |_: &str| {};
        let emit: &DeltaSink = &emit_fn;
        let mut output = String::new();
        let mut prior_usage: Option<Usage> = Some(Usage {
            input_tokens: 999, // would be wrong if kept
            output_tokens: 888,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            cost_usd: Some(0.42),
        });
        let mut files: Vec<String> = Vec::new();

        // A session/prompt result arrives with authoritative token counts.
        let result_frame = serde_json::json!({
            "result": {
                "stopReason": "end_turn",
                "usage": {
                    "inputTokens": 100,
                    "outputTokens": 50
                }
            }
        });
        if let Some(result_usage) = parse_result_usage(&result_frame["result"]["usage"]) {
            let cost = prior_usage.as_ref().and_then(|u| u.cost_usd);
            prior_usage = Some(Usage {
                cost_usd: cost.or(result_usage.cost_usd),
                ..result_usage
            });
        }
        let _ = (emit, &mut output, &mut files); // silence unused warnings

        let final_usage = prior_usage.unwrap();
        // Tokens come from the result frame, not the prior notification.
        assert_eq!(
            final_usage.input_tokens, 100,
            "result tokens replace notification tokens"
        );
        assert_eq!(final_usage.output_tokens, 50);
        // Cost is preserved from the prior usage_update notification.
        assert_eq!(
            final_usage.cost_usd,
            Some(0.42),
            "cost from usage_update must survive the result merge"
        );
    }

    // ── DES-002 T6 tests: tombstone / epoch separation ───────────────────────────

    /// Test 25: Tombstone race — `cancel_epoch` before `register` → returns None;
    /// no entry in `pending` or `run_index`; `is_epoch_cancelled` returns true.
    #[test]
    fn tombstone_race_cancel_before_register() {
        let mut m = maps();
        // Cancel epoch 1 BEFORE any registration.
        m.cancel_epoch("run-25", 1);

        // is_epoch_cancelled must reflect the tombstone.
        assert!(
            m.is_epoch_cancelled("run-25", 1),
            "is_epoch_cancelled must return true after cancel_epoch"
        );

        // register for the same (run_id, epoch) must return None (creation suppressed).
        let result = m.register("run-25", 1, "eid-25", "msg", None, "r");
        assert!(
            result.is_none(),
            "register must return None when epoch was pre-cancelled"
        );
        assert!(
            !m.pending.contains_key("eid-25"),
            "pending must not contain the suppressed elicitation"
        );
        assert!(
            m.run_index
                .get("run-25")
                .is_none_or(|v| !v.iter().any(|(id, _)| id == "eid-25")),
            "run_index must not contain the suppressed elicitation"
        );
    }

    /// Test 27: Epoch separation — `cancel_epoch` never bumps epoch; `next_epoch` is the
    /// sole bumper; epochs are independently gated.
    #[test]
    fn epoch_separation_cancel_epoch_never_bumps() {
        let mut m = maps();

        // cancel_epoch tombstones epoch 1 but does NOT bump the epoch counter.
        m.cancel_epoch("run-27", 1);
        assert_eq!(
            m.current_epoch("run-27"),
            0,
            "cancel_epoch must not allocate an epoch"
        );

        // register on the cancelled epoch returns None.
        let r1 = m.register("run-27", 1, "eid-27a", "msg", None, "r");
        assert!(r1.is_none(), "register on cancelled epoch 1 returns None");

        // next_epoch allocates epoch 2 (the first next_epoch for this run → epoch 1... wait,
        // run_epoch for "run-27" starts at 0, so next_epoch returns 1. But epoch 1 is cancelled.
        // That means a new worker registering under epoch 1 would be suppressed. The test spec
        // says next_epoch→2, which implies epoch 1 was already allocated somehow.
        //
        // Actually re-reading the spec: "cancel_epoch(run, 1) → tombstone; register(run, eid, 1) → None;
        // next_epoch(run) → 2". This means run_epoch starts at 1 (perhaps begin_launch or
        // initial allocation), then cancel_epoch tombstones 1, and next_epoch allocates 2.
        //
        // To match the spec, we need to first allocate epoch 1 (so run_epoch["run-27"] == 1),
        // then cancel it, then next_epoch → 2.
        // Let's reset and redo:
        let mut m = maps();

        // Allocate epoch 1 first (simulating an initial dispatch_unit call).
        let ep1 = m.next_epoch("run-27");
        assert_eq!(ep1, 1, "first next_epoch returns 1");

        // Tombstone epoch 1.
        m.cancel_epoch("run-27", ep1);
        assert!(m.is_epoch_cancelled("run-27", 1), "epoch 1 tombstoned");

        // register under epoch 1 returns None (suppressed).
        let r1 = m.register("run-27", 1, "eid-27a", "msg", None, "r");
        assert!(r1.is_none(), "register on tombstoned epoch 1 returns None");

        // next_epoch allocates epoch 2 (NOT affected by cancel_epoch).
        let ep2 = m.next_epoch("run-27");
        assert_eq!(ep2, 2, "next_epoch returns 2 (cancel_epoch never bumps)");

        // register under epoch 2 succeeds.
        let r2 = m.register("run-27", 2, "eid-27b", "msg2", None, "r");
        assert!(r2.is_some(), "register on live epoch 2 returns Some(rx)");

        // epoch 1 remains cancelled; epoch 2 is not.
        assert!(
            m.is_epoch_cancelled("run-27", 1),
            "epoch 1 still tombstoned"
        );
        assert!(!m.is_epoch_cancelled("run-27", 2), "epoch 2 not tombstoned");
    }

    /// Test 30: Present-but-empty schema (zero properties) → validate_elicitation_schema returns None
    /// → immediate cancel (F16); absent requestedSchema → Null value → same result.
    ///
    /// Both empty-properties and absent schema are non-representable in form mode.
    #[test]
    fn empty_or_absent_schema_is_cancelled() {
        // Schema A: present but zero properties.
        let schema_a = serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        });
        assert!(
            validate_elicitation_schema(&schema_a).is_none(),
            "zero-properties schema must return None → cancel"
        );

        // Schema B: absent requestedSchema → JSON Null.
        let schema_b = serde_json::Value::Null;
        assert!(
            validate_elicitation_schema(&schema_b).is_none(),
            "absent (Null) requestedSchema must return None → cancel"
        );

        // Schema C: requestedSchema present but no 'properties' key at all.
        let schema_c = serde_json::json!({"type": "object"});
        assert!(
            validate_elicitation_schema(&schema_c).is_none(),
            "schema without 'properties' key must return None → cancel"
        );
    }

    /// Test 31: Stale worker epoch stays cancelled after `next_epoch` bumps.
    ///
    /// A worker holding a reference to epoch 1 that has been tombstoned must still
    /// receive None from `register` even after `next_epoch` allocates epoch 2.
    #[test]
    fn stale_worker_epoch_stays_cancelled_after_bump() {
        let mut m = maps();

        // Allocate and tombstone epoch 1.
        let ep1 = m.next_epoch("run-31");
        assert_eq!(ep1, 1);
        m.cancel_epoch("run-31", ep1);
        assert!(m.is_epoch_cancelled("run-31", 1), "epoch 1 tombstoned");

        // Bump to epoch 2 — stale worker still holds ep1.
        let ep2 = m.next_epoch("run-31");
        assert_eq!(ep2, 2);

        // A fresh epoch-2 worker succeeds.
        let r2 = m.register("run-31", 2, "eid-31b", "q", None, "r");
        assert!(r2.is_some(), "epoch 2 must be accepted");

        // A stale epoch-1 worker is still rejected (tombstone persists).
        let r1_stale = m.register("run-31", 1, "eid-31c", "q2", None, "r");
        assert!(
            r1_stale.is_none(),
            "stale epoch-1 registration must still return None"
        );
        assert!(
            m.is_epoch_cancelled("run-31", 1),
            "epoch 1 tombstone persists after next_epoch"
        );
        // Epoch 2 registration did not disturb epoch 1's tombstone or epoch 2.
        assert!(
            !m.is_epoch_cancelled("run-31", 2),
            "epoch 2 must not be tombstoned"
        );
    }

    /// Test 29d: EpochCleanup RAII guard emits ElicitationResolved with reason="teardown"
    /// when the epoch is tombstoned (channel Disconnected path).
    #[test]
    fn epoch_cleanup_emits_elicitation_resolved_teardown_on_epoch_cancel() {
        let maps_arc = make_maps();
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<Command>();

        // Allocate epoch 1 and register an elicitation.
        let epoch = {
            let mut m = maps_arc.lock().unwrap();
            m.begin_launch("run-29d", false);
            m.next_epoch("run-29d")
        };
        assert_eq!(epoch, 1);

        // Create the EpochCleanup guard with the in_flight elicitation details.
        let guard = EpochCleanup {
            maps: Arc::clone(&maps_arc),
            run_id: "run-29d".to_string(),
            epoch,
            launch_seq: 1,
            bus_in_flight_deferred: false,
            tx: cmd_tx,
            in_flight_id: Some("eid-29d".to_string()),
            in_flight_action: Some("cancel".to_string()),
            in_flight_reason: Some("teardown".to_string()),
        };

        // Drop the guard — it must emit ElicitationResolved via cmd_tx.
        drop(guard);

        // Receive the command from the channel.
        let cmd = cmd_rx
            .recv_timeout(std::time::Duration::from_millis(200))
            .expect("EpochCleanup must emit Command::EmitEvent on drop");

        match cmd {
            Command::EmitEvent(crate::event::CoreEvent::ElicitationResolved {
                session,
                elicitation_id,
                action,
                reason,
            }) => {
                assert_eq!(session, "run-29d");
                assert_eq!(elicitation_id, "eid-29d");
                assert_eq!(action, "cancel");
                assert_eq!(
                    reason, "teardown",
                    "teardown path must emit reason=teardown"
                );
            }
            _other => panic!("expected ElicitationResolved event"),
        }
    }

    /// Test 36: Write failure on deliberate-kill teardown →
    /// `is_epoch_cancelled` returns true when the epoch was tombstoned before write failure,
    /// which the Phase 3 gate uses to produce reason="teardown" (not "adapter_write_failure").
    #[test]
    fn phase3_gate_distinguishes_teardown_from_adapter_write_failure() {
        let mut m = maps();

        // Allocate epoch 1 for a run.
        let ep = m.next_epoch("run-36");
        assert_eq!(ep, 1);

        // NOT cancelled → is_epoch_cancelled returns false → adapter_write_failure.
        assert!(
            !m.is_epoch_cancelled("run-36", ep),
            "before cancel_epoch: is_epoch_cancelled must return false → adapter_write_failure"
        );

        // Simulate teardown: cancel the epoch before the write attempt.
        m.cancel_epoch("run-36", ep);

        // NOW cancelled → is_epoch_cancelled returns true → teardown.
        assert!(
            m.is_epoch_cancelled("run-36", ep),
            "after cancel_epoch: is_epoch_cancelled must return true → teardown"
        );

        // Verify the gate logic (mirrors Phase 3 in exec_turn_acp):
        let reason = if m.is_epoch_cancelled("run-36", ep) {
            "teardown"
        } else {
            "adapter_write_failure"
        };
        assert_eq!(
            reason, "teardown",
            "deliberate-kill path must produce reason=teardown, not adapter_write_failure"
        );
    }

    /// Test 38 (FINDING-254): exec_turn_acp must hold `proc.write_lock` around every
    /// `proc.stdin` write so `shared_run_terminal`'s `try_lock()` can detect an
    /// in-flight write and delay teardown until the write completes.
    ///
    /// Proof by blocking: pre-acquire `write_lock` on the test thread before spawning
    /// exec_turn_acp. If exec_turn_acp correctly acquires the lock before writing, it
    /// blocks until we release. The mock never receives the `session/prompt` while the
    /// lock is held, so it cannot send a response and the turn cannot complete. After
    /// we drop the guard, exec_turn_acp proceeds and the turn succeeds with `Ok`.
    ///
    /// Mutation check: removing the `write_lock.lock()` call from the initial `rpc_send`
    /// would let exec_turn_acp write without holding the lock; the mock would respond
    /// immediately and `done_rx` would fire BEFORE we release the guard — causing the
    /// `try_recv().is_err()` assertion to fail.
    #[test]
    #[cfg(unix)]
    fn write_lock_is_held_during_rpc_send_invariant() {
        let dir = std::env::temp_dir().join(format!("wicked-254-wl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut proc = start_mock_proc(&dir, "ok");

        // Pre-acquire write_lock on the test thread; exec_turn_acp must block on it.
        let wl = Arc::clone(&proc.write_lock);
        let guard = wl.lock().expect("write_lock must start unlocked");

        let maps = Arc::new(Mutex::new(ElicitationMaps::new()));
        let (cmd_tx, _cmd_rx) = std::sync::mpsc::channel::<Command>();
        // Channel exec_turn_acp sends to when it completes.
        let (done_tx, done_rx) = std::sync::mpsc::sync_channel::<StepStatus>(1);

        let handle = std::thread::spawn(move || {
            let noop: &DeltaSink = &|_: &str| {};
            let result = exec_turn_acp(
                &mut proc,
                "hello",
                &[],
                noop,
                Duration::from_secs(5),
                maps,
                "run-wl-invariant",
                0,
                &cmd_tx,
                None,
            );
            let status = result.map(|r| r.status).unwrap_or(StepStatus::Failed);
            let _ = done_tx.send(status);
            status
        });

        // Give the thread time to reach the write_lock acquisition attempt inside
        // exec_turn_acp. 100ms is generous — the thread starts and enters the fn
        // in microseconds. If write_lock is not acquired (pre-fix bug), the mock
        // receives the prompt immediately and done_rx fires within ~5ms.
        std::thread::sleep(Duration::from_millis(100));

        assert!(
            done_rx.try_recv().is_err(),
            "exec_turn_acp must still be blocked on write_lock — \
             the done channel must not have fired yet"
        );

        // Releasing write_lock lets exec_turn_acp write the prompt. The mock
        // immediately responds, completing the turn.
        drop(guard);

        let status = done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("exec_turn_acp must complete after write_lock is released");
        assert_eq!(
            status,
            StepStatus::Ok,
            "turn must complete Ok after write_lock is released"
        );
        handle.join().expect("background thread must not panic");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Test 39: EpochCleanup guard drops clean — `active_workers` and `run_epoch` are
    /// reclaimed when the guard (constructed the same way `exec_turn` does it) is dropped
    /// after a successful `exec_turn_acp` call. This validates the RAII invariant required
    /// by core#234's DoD: the guard must remove epoch state on drop, with no leak.
    #[test]
    #[cfg(unix)]
    fn epoch_cleanup_guard_drop_removes_run_state_no_leak() {
        let dir = std::env::temp_dir().join(format!("wicked-t39-cleanup-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let maps_arc = Arc::new(Mutex::new(ElicitationMaps::new()));

        // Simulate begin_launch + next_epoch (what the actor does before dispatching a unit).
        let epoch = {
            let mut m = maps_arc.lock().unwrap();
            m.begin_launch("run-cleanup-integration", true);
            m.next_epoch("run-cleanup-integration")
        };
        assert_eq!(epoch, 1, "first epoch must be 1");

        // Verify the pre-call state: one active worker, run_epoch entry present.
        {
            let m = maps_arc.lock().unwrap();
            assert!(
                m.has_active_run("run-cleanup-integration"),
                "run_epoch must be set before exec_turn_acp"
            );
            assert!(
                m.active_workers
                    .contains(&("run-cleanup-integration".to_string(), 1)),
                "active_workers must contain the launch token before exec_turn_acp"
            );
        }

        let mut proc = start_mock_proc(&dir, "ok");
        let (cmd_tx, _cmd_rx) = std::sync::mpsc::channel::<Command>();
        let noop: &DeltaSink = &|_: &str| {};

        // Construct the EpochCleanup guard the same way exec_turn does.
        let guard = EpochCleanup {
            maps: Arc::clone(&maps_arc),
            run_id: "run-cleanup-integration".to_string(),
            epoch,
            launch_seq: 1,
            bus_in_flight_deferred: false,
            tx: cmd_tx.clone(),
            in_flight_id: None,
            in_flight_action: None,
            in_flight_reason: None,
        };

        let result = exec_turn_acp(
            &mut proc,
            "hello",
            &[],
            noop,
            Duration::from_secs(5),
            Arc::clone(&maps_arc),
            "run-cleanup-integration",
            epoch,
            &cmd_tx,
            None,
        );
        assert_eq!(
            result.map(|r| r.status).unwrap_or(StepStatus::Failed),
            StepStatus::Ok,
            "turn must complete Ok on normal subprocess completion"
        );

        // Drop the guard (simulating exec_turn returning) — this fires cleanup_run.
        drop(guard);

        // Post-call state: epoch reclaimed, no leak.
        let m = maps_arc.lock().unwrap();
        assert!(
            !m.has_active_run("run-cleanup-integration"),
            "run_epoch entry must be removed after EpochCleanup::drop (no leak)"
        );
        assert!(
            !m.active_workers
                .iter()
                .any(|(r, _)| r == "run-cleanup-integration"),
            "active_workers entry must be removed after EpochCleanup::drop (no leak)"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
