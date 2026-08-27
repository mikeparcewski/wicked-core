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
use crate::execute_wrapped::{unit_prompt, WrappedCliStepRunner};
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
            let _ = child.kill();
            let _ = child.wait();
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
    let mut note = stderr_context(&proc.stderr_tail);
    match proc.kill_handle.try_exit_status() {
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
/// FINDING-060 happened.
const CLAUDE_CONFIG_DIR_ENV: &str = "CLAUDE_CONFIG_DIR";

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
/// branches are testable without mutating the test process's environment.
fn worker_claude_config_dir(inherit_operator: bool) -> Option<anyhow::Result<std::path::PathBuf>> {
    if inherit_operator {
        return None;
    }
    Some(ensure_worker_config_home())
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
/// `WICKED_WORKER_HOME` overrides the BASE dir (tests point it at scratch space).
fn worker_config_home() -> anyhow::Result<std::path::PathBuf> {
    if let Some(base) = std::env::var_os("WICKED_WORKER_HOME") {
        return Ok(std::path::PathBuf::from(base).join("claude"));
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .ok_or_else(|| anyhow::anyhow!("neither HOME nor USERPROFILE is set"))?;
    Ok(std::path::PathBuf::from(home)
        .join(".wicked-worker")
        .join("claude"))
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
fn ensure_worker_config_home() -> anyhow::Result<std::path::PathBuf> {
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
    // settings.json is re-written every spawn; clear any planted entry (symlink included)
    // first so the write can never travel through a link.
    let settings_path = dir.join("settings.json");
    remove_entry_no_follow(&settings_path)?;
    let settings = json!({
        "permissions": { "deny": crate::execute_wrapped::deny_rules() }
    });
    std::fs::write(&settings_path, serde_json::to_vec(&settings)?)?;
    Ok(dir)
}

/// Refuse a worker home whose leaf or parent is a symlink — a redirect here re-aims every
/// write the worker's CLI makes at a path the operator never chose. FAIL CLOSED on any stat
/// error other than not-found (a PermissionDenied probe must not read as "not a symlink").
fn refuse_symlinked_home(dir: &std::path::Path) -> anyhow::Result<()> {
    for probe in [dir.parent(), Some(dir)].into_iter().flatten() {
        match std::fs::symlink_metadata(probe) {
            Ok(m) if m.file_type().is_symlink() => {
                anyhow::bail!(
                    "refusing worker config home {}: {} is a symlink",
                    dir.display(),
                    probe.display()
                );
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                anyhow::bail!(
                    "refusing worker config home {}: cannot stat {} ({e})",
                    dir.display(),
                    probe.display()
                );
            }
        }
    }
    Ok(())
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
/// `code_graph_db` is the run's repo-local estate graph (`<repo>/.codegraph/estate.db`), or `None`
/// for an ungoverned / repo-less session. When present, the worker's `session/new` advertises the
/// estate MCP server scoped to that store (FINDING-122) — the ACP-array twin of the wrapped path's
/// `settings.json` injection. `None` ⇒ no estate server (never the daemon store; see FINDING-067).
fn start_acp_process(
    config: &AcpConfig,
    cwd: &std::path::Path,
    code_graph_db: Option<&str>,
    // `Some` ⇒ point the worker's platform temp env (`TMPDIR`/`TMP`/`TEMP`) at this dir
    // (core#264) so scratch lands inside the unit boundary instead of tripping (advisory)
    // denies in the system temp. UNIT sessions pass `<cwd>/tmp`; CHAT sessions pass `None` —
    // dropping a `tmp/` dir into a user's own working directory would be intrusive.
    scratch_tmp: Option<&std::path::Path>,
) -> anyhow::Result<AcpProcess> {
    // FINDING-061: decided BEFORE the spawn closure so both spawn attempts (the bare binary and
    // the Windows `.cmd` retry) carry the same isolation. Fail CLOSED on a mint failure: a spawn
    // that proceeded without the override would run under the operator's own configuration,
    // which is the exact leak being fixed — and the caller's fallback is the wrapped path, which
    // carries its own isolation.
    let worker_config_dir = match worker_claude_config_dir(
        std::env::var_os(crate::execute_wrapped::INHERIT_OPERATOR_CONFIG_ENV).is_some(),
    ) {
        None => None,
        Some(Ok(dir)) => Some(dir),
        Some(Err(e)) => {
            return Err(anyhow::anyhow!(
                "ACP worker config isolation failed ({e}); refusing to start an ACP worker \
                 under the operator's own CLI configuration (FINDING-061)"
            ))
        }
    };
    let build_cmd = |binary: &str| {
        let mut cmd = std::process::Command::new(binary);
        // The engine's internal environment is stripped through the one chokepoint (FINDING-067): an
        // agent CLI that inherits `WICKED_ESTATE_DB` has every estate tool it can spawn pointed at the
        // engine's operational store by default. Governed units do not come through here (they take the
        // wrapped path, FINDING-060), but an ungoverned worker in a repo runs the same
        // `wicked-estate index .`. Harden FIRST — anything set below is set deliberately.
        cmd.hardened();
        // Set AFTER `hardened()`, per the ordering contract in `wicked_apps_core::spawn`: clear
        // to a known slate, then set exactly what this path intends. This also overrides any
        // CLAUDE_CONFIG_DIR the daemon itself inherited — the operator's live config dir is
        // frequently exactly that variable.
        if let Some(dir) = &worker_config_dir {
            cmd.env(CLAUDE_CONFIG_DIR_ENV, dir);
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
        cmd.current_dir(cwd);
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
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
    // (ELICITATION_VERIFIED_ADAPTERS). Other adapters receive no elicitation capability so
    // they cannot suspend turns waiting for a human response that will never arrive.
    let form_enabled = ELICITATION_VERIFIED_ADAPTERS.contains(&config.binary.as_str());
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
            json!([{
                "name": "wicked-estate",
                "command": command,
                "args": args,
                "env": []
            }])
        })
        .unwrap_or_else(|| json!([]));
    let session_new_id = next_id;
    next_id += 1;
    if let Err(e) = rpc_send(
        &mut stdin,
        session_new_id,
        "session/new",
        json!({
            "cwd": cwd.to_string_lossy().as_ref(),
            "mcpServers": mcp_servers
        }),
    ) {
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
        write_lock: Arc::new(Mutex::new(())),
        stdin,
        line_rx: rx,
        _reader: reader_thread,
        stderr_tail,
        _stderr_reader: stderr_reader,
        session_id,
        next_id,
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
/// `elicitation/create` suspension boundary (OQ-R-6). Only adapters on this list
/// may receive `elicitation/create` via the allow-list guard in `exec_turn_acp`.
///
/// Adding a new adapter REQUIRES a verifiable artifact (link to passing integration
/// test run or source-code audit in the PR description) — self-assertion alone is
/// insufficient (spec §Ask first).
const ELICITATION_VERIFIED_ADAPTERS: &[&str] = &["claude-agent-acp", "codex-acp"];

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
    adapter_key: &str,
    tx: &std::sync::mpsc::Sender<Command>,
    gate: Option<&crate::acp_permission::AcpGate<'_>>,
) -> anyhow::Result<TurnResult> {
    let id = proc.next_id;
    proc.next_id += 1;

    // Clone the write_lock Arc so we can hold it around each proc.stdin write without
    // borrowing proc for the whole function. shared_run_terminal's try_lock() must see
    // this held to detect an in-flight write (FINDING-254 / core#254).
    let write_lock = Arc::clone(&proc.write_lock);

    // Elicitation is gated on a non-zero epoch AND on the adapter being in the verified
    // allow-list (OQ-R-6). Chat turns always pass epoch=0 and are never suspended.
    let elicitation_enabled = epoch > 0 && ELICITATION_VERIFIED_ADAPTERS.contains(&adapter_key);

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
            StepStatus::Cancelled
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
/// PreToolUse hook. `gate` absent ⇒ permitted, as this path has always behaved — but said out loud
/// rather than left to a capability we quietly withheld.
fn answer_permission_request<W: Write>(
    stdin: &mut W,
    write_lock: &Mutex<()>,
    gate: Option<&crate::acp_permission::AcpGate<'_>>,
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
    let result = match gate {
        Some(g) => crate::acp_permission::permission_result(g, &params).0,
        None => crate::acp_permission::allow_result(&params),
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
    fallback: WrappedCliStepRunner,
    timeout: Duration,
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

/// One live chat, for the enumerate surface. A leak nobody can list is a leak nobody can reclaim
/// (FINDING-027 gap 4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatInfo {
    pub chat_id: String,
    /// The seats currently warm, sorted.
    pub seats: Vec<String>,
    /// Seconds since the last open/ensure/turn on this chat.
    pub idle_secs: u64,
}

impl AcpStepRunner {
    pub(crate) fn new(tx: std::sync::mpsc::Sender<Command>) -> Self {
        let maps = Arc::new(Mutex::new(ElicitationMaps::new()));
        let write_reg: WriteReg = Arc::new(Mutex::new(HashMap::new()));
        Self::new_with_maps(tx, maps, write_reg)
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
            timeout: Duration::from_secs(secs),
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
    fn chat_ensure(
        &self,
        chat_id: &str,
        cli_key: &str,
        cwd: &std::path::Path,
    ) -> Result<Arc<Mutex<AcpProcess>>, String> {
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
        let config =
            acp_config_for(cli_key).ok_or_else(|| format!("no ACP config for '{cli_key}'"))?;
        if config.transport == AcpTransport::Http {
            return Err(format!(
                "ACP HTTP transport not supported for chat ('{cli_key}')"
            ));
        }
        // Chat is repo-less exploration → no estate MCP server (FINDING-122).
        let proc = start_acp_process(&config, cwd, None, None).map_err(|e| e.to_string())?;
        let arc = Arc::new(Mutex::new(proc));
        let mut guard = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
        // A racing ensure may have inserted first — reuse theirs, drop ours.
        if let Some(Some(existing)) = guard.get(&key) {
            return Ok(existing.clone());
        }
        guard.insert(key, Some(arc.clone()));
        Ok(arc)
    }

    /// Eagerly warm one session per seat; per-seat outcome, `ChatSessionReady`/
    /// `ChatSessionFailed` emitted for each.
    pub fn chat_open(
        &self,
        chat_id: &str,
        clis: &[String],
        cwd: &std::path::Path,
    ) -> Vec<(String, Result<(), String>)> {
        let opened: Vec<(String, Result<(), String>)> = clis
            .iter()
            .map(|cli| {
                let outcome = self.chat_ensure(chat_id, cli, cwd).map(|_| ());
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
        // Enforce the cap only AFTER the new chat is warm and touched, so it is the freshest entry
        // and therefore the last possible victim. Doing it first would let a full pool evict a
        // chat, warm the new one, and leave the pool at the cap anyway — same memory, one more
        // reap. Cap breaches are rare, so paying the reap on the open path costs nothing typical.
        //
        // Evictions are not logged here: each one emits `ChatClosed { reason: "pool_cap" }`, which
        // is the surface an operator actually watches. A second, log-only channel would be the one
        // that goes stale.
        self.chat_enforce_cap(Self::chat_pool_cap());
        opened
    }

    /// One seat's turn on a chat message. Streams deltas via `ChatDelta`, returns the
    /// completed reply text. On failure the seat's session is EVICTED (next ensure
    /// re-warms) and the error is returned — never floored, never faked.
    pub fn chat_turn(
        &self,
        chat_id: &str,
        cli_key: &str,
        text: &str,
        cwd: &std::path::Path,
    ) -> Result<String, String> {
        let arc = self.chat_ensure(chat_id, cli_key, cwd)?;
        let tx = self.tx.clone();
        let (chat_ev, cli_ev) = (chat_id.to_string(), cli_key.to_string());
        let emit: Box<crate::workflow::DeltaSink> = Box::new(move |delta: &str| {
            let _ = tx.send(Command::EmitEvent(CoreEvent::ChatDelta {
                chat: chat_ev.clone(),
                cli_key: cli_ev.clone(),
                text: delta.to_string(),
            }));
        });
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
                cli_key,
                &self.tx,
                None,
            )
        };
        // Touch again on the way out. `chat_ensure` touched on the way in, but a long turn would
        // then be counted as idle for its whole duration — a 40-minute agent turn would be reaped
        // out from under the operator the moment it finished.
        self.chat_touch(chat_id);
        match result {
            Ok(turn) if turn.status == StepStatus::Ok => Ok(turn.output),
            Ok(turn) => {
                self.chat_evict(chat_id, cli_key);
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
                self.chat_evict(chat_id, cli_key);
                let msg = format!("seat '{cli_key}' session error: {e}");
                eprintln!("[wicked-core] chat '{chat_id}' evicting {msg}");
                Err(msg)
            }
        }
    }

    fn chat_evict(&self, chat_id: &str, cli_key: &str) {
        let mut guard = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
        guard.remove(&(Self::chat_pool_key(chat_id), cli_key.to_string()));
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
        self.emit_event(CoreEvent::ChatClosed {
            chat: chat_id.to_string(),
            reason: reason.as_str().to_string(),
        });
    }

    /// Close all ACP sessions for `run_id` and kill their child processes. Idempotent.
    /// Call this after the last unit of a run completes (mirrors
    /// [`PersistentStepRunner::drop_session`]).
    pub fn drop_session(&self, run_id: &str) {
        let mut guard = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
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
        let gate_ctx = match (&input.governance, cli_runs_claude(&cli_key)) {
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
                // the shared evidence-derived assembly (skills dir + repo root). Built HERE, on
                // the runner with the governance context in hand — the in-process evaluation
                // cannot read it from any env.
                let boundary = crate::gate_hook::BoundaryCtx {
                    roots: crate::path_policy::AllowedRoots {
                        write: std::iter::once(unit_cwd.clone())
                            .chain(g.extra_write_roots.iter().map(std::path::PathBuf::from))
                            .collect(),
                        read: crate::execute_wrapped::assemble_read_roots(
                            g.code_graph_db.as_deref(),
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
                };
                Some((scope, phase, decisions_path, g.db_path.clone(), boundary))
            }
            _ => None,
        };

        // SHARED ACP SESSION PATH — ungoverned units of every CLI, plus governed units of
        // non-claude CLIs (input arming is claude-only; their output-side gates still run).
        let acp_config = match acp_config_for(&cli_key) {
            Some(c) => c,
            None => return self.fallback.run_unit_streaming(input, emit),
        };

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
        let session_key = (run_id.clone(), cli_key.clone());
        // `None` in the map means a previous startup for this key failed; fall back
        // immediately without re-attempting spawn (avoids repeated warnings per run).
        let proc_arc: Arc<Mutex<AcpProcess>> = {
            let guard = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(slot) = guard.get(&session_key) {
                match slot {
                    Some(arc) => arc.clone(),
                    None => {
                        drop(guard); // release sessions lock before the blocking fallback call
                        return self.fallback.run_unit_streaming(input, emit);
                    }
                }
            } else {
                drop(guard);
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
                match start_acp_process(&acp_config, &cwd, code_graph_db, Some(&cwd.join("tmp"))) {
                    Ok(proc) => {
                        let acp_session_id = proc.session_id.clone();
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
                        }
                        result
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
        let prompt = unit_prompt(input);

        let gate = gate_ctx
            .as_ref()
            .map(|(scope, phase, decisions_path, db, boundary)| {
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
                    }),
                }
            });
        let turn = exec_turn_acp(
            &mut proc,
            &prompt,
            prior_outputs,
            emit,
            self.timeout,
            Arc::clone(&self.elicitation_maps),
            &run_id,
            input.elicitation_epoch,
            &cli_key,
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
            Ok(result) if result.status == StepStatus::Ok => StepOutput {
                run_id: input.run_id.clone(),
                unit_ix: input.unit_ix,
                attempt: input.attempt,
                output: result.output,
                status: StepStatus::Ok,
                usage: result.usage,
                files: result.files,
                tools: result.tools,
                governed: gate.is_some(),
            },
            Ok(result) if result.status == StepStatus::Cancelled => {
                // Timeout — drop the session: the reader thread may wedge on a full pipe
                // if we leave the ACP process running while no longer consuming its output.
                drop(proc);
                self.drop_session(&run_id);
                StepOutput {
                    run_id: input.run_id.clone(),
                    unit_ix: input.unit_ix,
                    attempt: input.attempt,
                    output: result.output,
                    status: StepStatus::Cancelled,
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
                             every worker stays logged in. Using single-shot fallback meanwhile, \
                             which runs under the operator's own auth"
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

fn acp_config_for(cli_key: &str) -> Option<AcpConfig> {
    // The MERGED registry, not builtin(): a user record replaces its built-in wholesale,
    // so its [cli.acp] table (or its absence) must decide the transport here exactly as
    // it does everywhere else.
    registry_record(cli_key).and_then(|c| c.acp)
}

/// Whether this seat runs Claude Code — classified by the REGISTERED BINARY, not the
/// key: `binary_is_claude` is a path-stem classifier and registry keys can diverge from
/// binary names (e.g. a "claude-eval" seat whose binary is `claude`). Ad-hoc CLIs not
/// in the registry fall back to classifying the key itself (the historical behaviour).
fn cli_runs_claude(cli_key: &str) -> bool {
    match registry_record(cli_key) {
        Some(c) => crate::execute_wrapped::binary_is_claude(&c.binary),
        None => crate::execute_wrapped::binary_is_claude(cli_key),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        is_transient_cli_failure, is_worker_originated_failure, should_retry_worker,
        MAX_TRANSIENT_RETRIES,
    };
    use crate::workflow::StepStatus;

    /// Guards process-global env (WICKED_WORKER_HOME) — cargo runs tests in one process, in
    /// parallel. Same pattern as execute_wrapped's ENV_LOCK, plus a READ side (core#285):
    /// tests that MUTATE the variable hold `write()`; tests that drive a REAL
    /// `start_acp_process` hold `read()` across the start, because the spawn resolves the
    /// ambient variable mid-call (`ensure_worker_config_home`). Without the read side, a start
    /// landing inside a mutator's window resolves the MUTATOR's fixture home — the
    /// symlink-refusal fixture, in the flake that motivated this — and trips the FINDING-061
    /// guard. Lock order everywhere: ENV_LOCK before REAL_STARTS.
    static ENV_LOCK: std::sync::RwLock<()> = std::sync::RwLock::new(());

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
        // that way lives in `actor::deliverable_floor_tests`. Here: prove the classifiers hold no
        // deliverable-shaped special case, so nobody re-adds one instead of keeping the floor at
        // the fold. Needle by concatenation so this test's own text cannot satisfy the search.
        let src = include_str!("acp_runner.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap_or(src);
        let needle = format!("did not produce its {}", "declared deliverable");
        assert!(
            !prod.contains(needle.as_str()),
            "a substring carve-out for the deliverable floor is back in acp_runner's production \
             code — the floor belongs at the fold, where it is a status, not a sentence to grep"
        );
        // And the classifiers judge the INFRASTRUCTURAL shape only: prose that merely mentions a
        // deliverable is neither transient nor worker-originated, with or without a carve-out.
        let prose =
            "unit u3 reported done but did not produce its declared deliverable(s): rg.json";
        assert!(!is_transient_cli_failure(prose));
        assert!(!is_worker_originated_failure(prose));
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
        // Never retry a success, a cancel (our own timeout), or a non-transient failure.
        assert!(!should_retry_worker(true, StepStatus::Ok, transient, 0));
        assert!(!should_retry_worker(
            true,
            StepStatus::Cancelled,
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

        std::env::remove_var("WICKED_WORKER_HOME");
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

        std::env::remove_var("WICKED_WORKER_HOME");
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
        std::env::remove_var("WICKED_WORKER_HOME");
        let _ = std::fs::remove_dir_all(&base);
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
    /// shared dir would interleave ledgers.
    #[cfg(unix)]
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
        std::env::remove_var("WICKED_WORKER_HOME");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&seen_dir);
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
        let graph_db = "/tmp/wicked-122-repo/.codegraph/estate.db";
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
        assert!(
            !seen2.contains("wicked-estate"),
            "a repo-less session must advertise no estate server: {seen2}"
        );
        drop(proc2);
        let _ = std::fs::remove_dir_all(&dir);
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
        std::env::remove_var("WICKED_WORKER_HOME");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The call site must consult the SAME escape-hatch variable as the wrapped path, read from
    /// the real environment — a hardcoded `false` would pass the behavioural test above while
    /// silently deleting the operator's opt-out. Needle built by concatenation and matched on
    /// whitespace-stripped source so neither this test nor rustfmt can satisfy or break it.
    #[test]
    fn the_acp_spawn_consults_the_same_inherit_escape_hatch_as_the_wrapped_path() {
        let src: String = include_str!("acp_runner.rs")
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        let needle = format!(
            "worker_claude_config_dir(std::env::var_os(crate::execute_wrapped::{}).is_some()",
            "INHERIT_OPERATOR_CONFIG_ENV"
        );
        assert!(
            src.contains(&needle),
            "start_acp_process no longer decides config isolation from the wrapped path's \
             escape-hatch variable"
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
        std::env::remove_var("WICKED_WORKER_HOME");
        let settings: Value =
            serde_json::from_slice(&std::fs::read(dir.join("settings.json")).unwrap()).unwrap();
        let deny: Vec<String> = settings["permissions"]["deny"]
            .as_array()
            .expect("permissions.deny present")
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
        assert_eq!(
            deny,
            crate::execute_wrapped::deny_rules(),
            "the ACP fence must be the wrapped path's fence, not a copy that can drift"
        );
        assert!(
            settings["permissions"].get("defaultMode").is_none(),
            "a pinned mode that auto-approves would answer session/request_permission before \
             the governance gate sees it: {settings}"
        );
        drop(dir);
        let _ = std::fs::remove_dir_all(&base);
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

        assert!(r
            .chat_ensure("c1", "no-such-cli-xyz", &std::env::temp_dir())
            .is_err());

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
        let cwd = std::env::temp_dir();
        let err = match r.chat_ensure("c1", "no-such-cli-xyz", &cwd) {
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
            pre_build_scope: false,
            scope_warnings: Vec::new(),
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
            }),
            prior_outputs: Vec::new(),
            elicitation_epoch: 0,
            process_gen: None,
            launch_seq: 0,
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

    /// The predicate the branch turns on, at its boundary. `cli_runs_claude` classifies by the
    /// REGISTERED BINARY, so a seat whose key is not `claude` still routes to wrapped when its
    /// binary is — and a non-claude seat keeps the shared ACP path, since input arming was always
    /// claude-only and rerouting it would cost multi-turn for nothing.
    #[test]
    fn the_reroute_predicate_follows_the_binary_not_the_key() {
        assert!(cli_runs_claude("claude"));
        // Ad-hoc keys not in the registry classify on the key itself, path-stem-wise.
        assert!(cli_runs_claude("/opt/somewhere/claude"));
        assert!(!cli_runs_claude("codex"));
        assert!(!cli_runs_claude("claude-ish"));
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
        answer_permission_request(&mut sink, &lock, None, &frame, &mut output, 4096);
        let answered: serde_json::Value =
            serde_json::from_str(std::str::from_utf8(&sink).unwrap().trim_end())
                .expect("an explicit null id must still be answered");
        assert_eq!(answered["id"], serde_json::Value::Null);

        // 2. `id` member ABSENT ⇒ a notification; nothing to answer.
        let mut sink2: Vec<u8> = Vec::new();
        let note_frame = serde_json::json!({
            "jsonrpc":"2.0","method":"session/request_permission","params":{}
        });
        answer_permission_request(&mut sink2, &lock, None, &note_frame, &mut output, 4096);
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
        answer_permission_request(&mut BrokenPipe, &lock, None, &frame, &mut output, 4096);
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
    #[cfg(unix)]
    fn write_mock_acp_py(dir: &std::path::Path) -> std::path::PathBuf {
        // The script uses Python dict literals (which json.dumps handles) to avoid Rust brace
        // escaping in format!. The behavior is controlled entirely via sys.argv[1].
        let path = dir.join("mock-acp-bridge.py");
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

# initialize
req = r()
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

    /// Start a mock ACP process using the shared Python bridge script.
    /// The `behavior` string is passed as `start_args[0]` to the script.
    #[cfg(unix)]
    fn start_mock_proc(dir: &std::path::Path, behavior: &str) -> AcpProcess {
        let py_path = write_mock_acp_py(dir);
        let config = AcpConfig {
            binary: py_path.to_string_lossy().to_string(),
            start_args: vec![behavior.to_string()],
            transport: AcpTransport::default(),
            auth_method: None,
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
            "claude-agent-acp",
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
                "claude-agent-acp",
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
            "claude-agent-acp",
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
            "claude-agent-acp",
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
            "claude-agent-acp",
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
            "claude-agent-acp",
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
            "claude-agent-acp",
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
            "claude-agent-acp",
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
            "claude-agent-acp",
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
                "claude-agent-acp",
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
            "claude-agent-acp",
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
