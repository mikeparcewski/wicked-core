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
//! | agy      | agy-acp (ambiguous bin name — see below)             | stdio     |
//! | copilot  | copilot --acp (native)                               | stdio     |
//! | opencode | opencode acp (native)                                | stdio     |
//!
//! `agy-acp` is resolved by name off PATH and TWO programs ship that bin: wicked-crew's own
//! bridge (`packages/agent-acp-bridges`) and a community `agy-acp` on npm (0.5.2,
//! Apache-2.0, ACP v1 — published, not wicked-maintained). Neither is proven against the agy
//! seat and PATH order decides which one a spawn gets; see `wicked-council`'s registry note.
//!
//! When an ACP binary is unavailable or fails during the handshake, `AcpStepRunner`
//! discloses it on `CoreEvent::AcpFallback` and in the daemon log, never in the seat's
//! output (core#970). The run then continues with single-shot fallback.
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
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;
// The in-file tests reach these through `use super::*`.
#[cfg(test)]
use serde_json::Value;
#[cfg(all(test, unix))]
use std::time::Instant;

use crate::command::Command;
use crate::execute_wrapped::WrappedCliStepRunner;
use crate::workflow::{DeltaSink, StepInput, StepOutput, StepRunner};

// ── KillHandle and WriteReg (DES-002 T6) ─────────────────────────────────────

/// A kill handle for an in-flight ACP child process.
///
/// Carries an `Arc<Mutex<Option<Child>>>` so that multiple callers (teardown step 1,
/// step 6 second sweep, `EpochCleanup::drop`) can all safely signal the child without
/// PID-reuse races. After the first `signal()` takes the child, subsequent calls are no-ops.
pub struct KillHandle {
    inner: Mutex<Option<std::process::Child>>,
    /// (core#936) The bridge's registration as engine infrastructure (`outstanding_work`): held
    /// exactly while this handle owns the unreaped child, released BEFORE the pid is (codex r4 —
    /// a reaped pid the OS reuses must never be excused).
    infra: Mutex<Option<crate::outstanding_work::InfraGuard>>,
    /// (codex r6) The exit status, once observed: a reaped child answers every later probe with it.
    exited: Mutex<Option<std::process::ExitStatus>>,
}

impl KillHandle {
    /// Construct a no-op handle for tests (no child to kill).
    pub fn noop() -> Self {
        Self {
            inner: Mutex::new(None),
            infra: Mutex::new(None),
            exited: Mutex::new(None),
        }
    }

    /// Construct a handle that will kill `child` on `signal()`. Not infrastructure.
    pub fn new(child: std::process::Child) -> Self {
        Self {
            inner: Mutex::new(Some(child)),
            infra: Mutex::new(None),
            exited: Mutex::new(None),
        }
    }

    /// (core#936) A handle for an ACP bridge: the child — and the `levels` of descendants that
    /// are the bridge's own machinery (a resident launcher's bridge, an adapter's CLI runtime) —
    /// is registered as engine infrastructure for as long as this handle owns it unreaped.
    pub fn new_bridge(child: std::process::Child, levels: u8) -> Self {
        let infra = crate::outstanding_work::InfraGuard::register(child.id(), levels);
        Self {
            inner: Mutex::new(Some(child)),
            infra: Mutex::new(Some(infra)),
            exited: Mutex::new(None),
        }
    }

    /// Whether this handle still holds its infrastructure registration.
    #[cfg(all(test, unix))]
    fn is_infra(&self) -> bool {
        self.infra
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_some()
    }

    fn release_infra(&self) {
        self.infra.lock().unwrap_or_else(|p| p.into_inner()).take();
    }

    /// Report the child's exit status if it has ALREADY exited — non-blocking, never killing
    /// (`try_wait` leaves an unexited child untouched, and reaps one that has exited). `None`
    /// when the child is still running,
    /// was already taken by `signal()`, or this is a no-op handle. crew#267: the SESSION_DIED
    /// arms use this so a bridge death reports HOW the process ended, not only that it stopped
    /// answering.
    pub fn try_exit_status(&self) -> Option<std::process::ExitStatus> {
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let mut exited = self.exited.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(status) = *exited {
            return Some(status);
        }
        let child = guard.as_mut()?;
        // (codex r5) Observe the exit WITHOUT reaping, release the registration while the zombie
        // still reserves the pid, and only then reap: a reused pid is never registered.
        if !matches!(crate::validator::has_exited_unreaped(child), Ok(true)) {
            return None;
        }
        self.release_infra();
        *exited = child.try_wait().ok().flatten();
        *exited
    }

    /// Kill and reap the child process. Idempotent: the first call kills; subsequent calls
    /// are no-ops (the child has been taken). Releases the mutex before `wait()` so a
    /// concurrent `signal()` on another thread never deadlocks.
    pub fn signal(&self) {
        let taken = {
            let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            guard.take()
        };
        self.release_infra();
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

#[cfg(test)]
mod runtime_adapter_tests {}

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

/// The largest elicitation option (bytes) the decider surface carries; longer entries are
/// dropped at registration and never count as answerable at validation (core#599 review).
const ELICITATION_OPTION_CAP: usize = 512;

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
    /// Dispatch-mode-agnostic tombstone set. Populated by `tombstone_run` (called inside
    /// `cancel_run`, which all cancel paths go through) and `tombstone_bus_run` (shared_run_terminal
    /// bus guard). `is_run_cancelled` checks this so `try_next_epoch_bus` can reject stale bus tasks
    /// for both locally-cancelled and bus-cancelled runs.
    all_cancelled_runs: HashSet<String>,
    /// Elicitation ids for which `ElicitationCreated` has been announced to subscribers.
    /// Used by the EmitEvent suppression guard: once announced, a concurrent `cancel_epoch`
    /// must NOT insert a stale suppression marker (the event is already out).
    creation_announced: HashSet<String>,
    /// Elicitation ids whose paired `ElicitationResolved` event must be suppressed.
    /// Set when the `ElicitationCreated` was suppressed; cleared by `take_suppressed_resolution`.
    suppressed_resolutions: HashSet<String>,
    /// Live tool child process-group ids keyed by run_id (core#500 / AC2): the actor thread reads
    /// this to synchronously kill the groups BEFORE emitting `RunCancelled` / `UnitReassigned`, so
    /// the wire never asserts cancellation while a child is still alive. Usually 0 or 1 pgids per
    /// run (tool units are serialised), but a `ReassignUnit` that lands between an attempt's
    /// `proc.spawn()` and its registration leaves two attempts briefly live at once, and BOTH have
    /// to stay reachable (core#577). The background thread registers on spawn and deregisters its
    /// own pgid when the poll loop returns.
    tool_child_pgroups: HashMap<String, Vec<u32>>,
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
            tool_child_pgroups: HashMap::new(),
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
            const OPT_CAP: usize = ELICITATION_OPTION_CAP;
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
    /// Called by `cancel_run` before the group-kill block; all cancel callers
    /// (`CancelRun`, `campaign::cancel`, `campaign::fail_fast`, human Reject,
    /// governance Deny) inherit it through `cancel_run`.
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

    // ── Tool child process-group registry (core#500 / AC2) ─────────────────────────────────────

    /// Register the live tool child's pgid for `run_id`. Called by the tool background thread
    /// immediately after `proc.spawn()` — before the poll loop — so the actor thread can find
    /// and kill the group during cancel / supersede.
    ///
    /// EVERY live pgid is kept, not only the newest (core#577). One live tool child per run is the
    /// ordinary state — a run advances one unit at a time (the actor is the single writer and
    /// dispatches the next unit only when the previous one's result posts back), `LaunchRun`
    /// refuses a second launch of a live run id (`RunBusy`, via the `in_flight` guard), and
    /// `ReassignUnit` drains the registry with [`Self::take_tool_child_pgroups`] and kills the
    /// group BEFORE dispatching the replacement — but it is not an invariant, and this registry
    /// used to overwrite on the premise that it was.
    ///
    /// The window that breaks it: `ReassignUnit` advances the launch sequence and drains the
    /// registry while an attempt sits between `proc.spawn()` (`actor.rs`, `run_tool_cmd`) and this
    /// call. The drain finds nothing, the replacement is dispatched, and two tool children are
    /// live at once. NOTHING orders their two registrations — the superseded attempt's worker
    /// thread only has to be off-CPU across the replacement's dispatch and spawn — so the second
    /// registration is as likely to be the SUPERSEDED attempt's as the replacement's.
    ///
    /// That direction is what made an overwrite a defect rather than an untidiness: it evicted the
    /// REPLACEMENT — the live one — and then the superseded attempt's own
    /// [`Self::deregister_tool_child`] matched its own pgid and emptied the run's entry, so a later
    /// cancel took `[]`, reported `toolChildrenKilled: 0`, and left a live tool child running that
    /// nothing could reach. That is precisely the escape core#500 exists to close, so the storage
    /// now holds what IS live rather than what is expected to be.
    ///
    /// The return value is the pgids already registered for this run — empty in the ordinary case,
    /// non-empty only in that race. It is a state worth logging, not a failure: both children are
    /// tracked and a cancel kills both.
    #[must_use = "co-resident tool children mean a reassign race — log it, do not drop it"]
    pub fn register_tool_child(&mut self, run_id: &str, pgid: u32) -> Vec<u32> {
        let live = self
            .tool_child_pgroups
            .entry(run_id.to_string())
            .or_default();
        let already: Vec<u32> = live.iter().copied().filter(|p| *p != pgid).collect();
        if !live.contains(&pgid) {
            live.push(pgid);
        }
        already
    }

    /// Deregister the tool child `pgid` for `run_id`, leaving any other live pgid of that run in
    /// place. Called by the background thread when its poll loop exits. Removing only its OWN pgid
    /// is what keeps a superseded attempt's late `on_done` from evicting a concurrently live
    /// replacement (core#500: the registry was keyed by run_id only, so a deregister removed the
    /// current entry regardless of which attempt wrote it; core#577: with per-attempt entries the
    /// compare is against a set rather than against a single slot).
    pub fn deregister_tool_child(&mut self, run_id: &str, pgid: u32) {
        if let Some(live) = self.tool_child_pgroups.get_mut(run_id) {
            live.retain(|p| *p != pgid);
            if live.is_empty() {
                self.tool_child_pgroups.remove(run_id);
            }
        }
    }

    /// Remove and return all live tool child pgids for `run_id`. Called by the actor thread
    /// BEFORE emitting `RunCancelled` or dispatching a replacement unit for `ReassignUnit`.
    /// Returns an empty vec when no tool child is registered (gate reject, non-tool run, etc.).
    ///
    /// Usually 0 or 1 pgids; 2 while a superseded attempt and its replacement are both live (see
    /// [`Self::register_tool_child`] for the window that produces that). The callers' `for pgid in
    /// pgroups { … }` kills every one of them, which is the point of returning them all (core#577).
    pub fn take_tool_child_pgroups(&mut self, run_id: &str) -> Vec<u32> {
        self.tool_child_pgroups.remove(run_id).unwrap_or_default()
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
                    answer: None,
                    held_ms: None,
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

/// The SHARED worker fence for a council ballot (wicked-crew#524 follow-up, review on core#436).
///
/// `wicked_council::dispatch::run_in_isolation` runs a claude seat under the worker home
/// (`seat_config_for_carrier`) with the trust flag appended, but creates no fence: the shared
/// `settings.json` — `permissions.deny` = [`crate::execute_wrapped::shared_deny_rules`] — used
/// to be written only by the ACP worker spawn ([`ensure_worker_config_home`]), so a council
/// convened before any worker had spawned ran its claude ballot with no deny fence at all. The
/// distributor calls this before convening a council that seats a claude carrier: the SAME writer
/// (one generator, one file), idempotent — an atomic rewrite of the same bytes — so a later worker
/// spawn changes nothing. `operational_home` is passed as `None` because the shared file's
/// content does not depend on it: the operational home is both fenced and a state-home candidate,
/// so it is excluded from the shared blanket either way (its rule rides each session's options).
///
/// Under the operator's inherit hatch nothing is written and `Ok(None)` says so — the ballot then
/// runs on the operator's configuration, which is what the hatch means. `Err` names why the fence
/// could not be written; the caller refuses the council (fail closed), as the ACP spawn refuses.
pub(crate) fn ensure_shared_worker_fence() -> anyhow::Result<Option<std::path::PathBuf>> {
    if crate::execute_wrapped::inherits_operator_config() {
        return Ok(None);
    }
    ensure_worker_config_home(None).map(Some)
}

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
    sanitize_claude_home(&dir)?;
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

/// Write `bytes` to `path` atomically: an exclusively-created private temp file in the same
/// directory (`<name>.<pid>.<seq>.tmp`, [`settings_temp_name`]), then `rename` over `path` — a
/// reader sees the old file or the new one, never a torn one, and a planted link at `path` is
/// replaced as a link (rename does not follow its target).
pub(crate) fn write_atomic(
    dir: &std::path::Path,
    path: &std::path::Path,
    bytes: &[u8],
) -> anyhow::Result<()> {
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
/// Remove every [`WORKER_HOME_SANITIZED`] entry from a worker Claude config dir, judged on
/// `symlink_metadata` (a planted link is removed AS a link). Shared by the primary worker home and
/// every secondary-instance home (core#595 review): `settings.local.json` merges OVER the fence's
/// `settings.json` and `hooks/` runs code, so a home that skips this can void its own deny rules.
pub(crate) fn sanitize_claude_home(dir: &std::path::Path) -> anyhow::Result<()> {
    for entry in WORKER_HOME_SANITIZED {
        remove_entry_no_follow(&dir.join(entry))?;
    }
    Ok(())
}

fn remove_entry_no_follow(p: &std::path::Path) -> anyhow::Result<()> {
    let meta = match std::fs::symlink_metadata(p) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => anyhow::bail!("cannot stat {} while sanitizing ({e})", p.display()),
    };
    let removed = if meta.file_type().is_dir() {
        std::fs::remove_dir_all(p)
    } else {
        std::fs::remove_file(p)
    };
    // A concurrent sanitize of the same home (secondary instances have no process-wide ENSURE
    // mutex) may have removed it between the stat and here: gone is the goal, not an error.
    match removed {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

/// The scopes a worker session reads its settings from — NONE (core#914). Not the operator's
/// `user` scope (FINDING-047), and not the repository's `project`/`local` scopes either: a
/// governed seat never loads a repository's own agent configuration — its hooks, permission
/// rules, MCP servers, skills or agents. Its `CLAUDE.md` would go too (the CLI loads it from the
/// `project` source), so a unit session lists its tree as an additional directory with
/// [`CLAUDE_MD_ADDITIONAL_DIRS_ENV`] set, which loads the instructions alone. `[]` is the SDK's
/// isolation mode; the bridge passes it to the CLI as
/// `--setting-sources=`. What the ENGINE hands the session is a separate source and still
/// applies: the per-session settings file (`settings`), the skills snapshot (`plugins`), the deny
/// fence (`disallowedTools`) and the strict MCP pin. Measured on claude 2.1.287 through
/// claude-agent-acp 0.73.0 with a scripted model: no repository hook ran, no repository MCP
/// server spawned, the garden snapshot's skills (the governed-worker base skill included) were
/// offered, and the engine's own `PreToolUse` hook fired.
pub(crate) const ENGINE_SETTING_SOURCES: &[&str] = &[];

/// The scopes a session reads under the [`crate::execute_wrapped::INHERIT_OPERATOR_CONFIG_ENV`]
/// hatch: the operator's own `user` scope — what the hatch means — and never the repository's
/// (core#914). Before, the hatch sent no `settingSources` and the bridge's default
/// (`user,project,local`) loaded the repository's hooks too.
pub(crate) const INHERIT_SETTING_SOURCES: &[&str] = &["user"];

/// The `--setting-sources` VALUE for a claude CLI argv, from the same lists the ACP frame carries
/// (one source of truth for both carriers): `""` on a worker — the CLI loads no filesystem scope
/// — and `user` under the hatch.
pub(crate) fn setting_sources_arg(inherit: bool) -> String {
    if inherit {
        INHERIT_SETTING_SOURCES
    } else {
        ENGINE_SETTING_SOURCES
    }
    .join(",")
}

/// claude's own switch for loading the instruction files of its ADDITIONAL directories
/// (`--add-dir` on the CLI, `additionalDirectories` in the SDK). Set to `1` on every governed
/// claude unit launch, wrapped and ACP, together with the unit's working tree as an additional
/// directory: the ONE way to give a seat its repository's `CLAUDE.md` back while the scope
/// selection stays [`ENGINE_SETTING_SOURCES`] (core#914 took the `project` source away, and
/// `CLAUDE.md` went with it).
///
/// Measured on claude 2.1.287 (wrapped `claude -p`, and through claude-agent-acp 0.73.0) with a
/// scripted model against a repository carrying hooks in `.claude/settings{,.local}.json`, a
/// settings `env`, an allow-all, `.mcp.json` with `enableAllProjectMcpServers`, and a repo skill,
/// agent and command. With `--setting-sources ""`, the tree as an additional directory and this
/// variable:
///
/// - LOADED: `CLAUDE.md`, `.claude/CLAUDE.md`, unconditional `.claude/rules/*.md`, and the
///   files they `@import`;
/// - NOT loaded (the CLI gates these on the `project`/`local` sources): `CLAUDE.local.md`, a
///   nested `<dir>/CLAUDE.md` (also not after a file under it is read), and a path-scoped rule
///   (`paths:` frontmatter);
/// - still NOT loaded: every repository hook, the settings `env` and permission rules, the
///   repository's MCP servers (not spawned, even without `--strict-mcp-config`), and its skills,
///   agents and commands. The engine's own `--settings` hook and plugin still load.
///
/// The additional directory alone (no variable) loads nothing, and so does the variable alone.
pub(crate) const CLAUDE_MD_ADDITIONAL_DIRS_ENV: &str =
    "CLAUDE_CODE_ADDITIONAL_DIRECTORIES_CLAUDE_MD";

// ── JSON-RPC helpers ──────────────────────────────────────────────────────────

// ── Turn execution ────────────────────────────────────────────────────────────

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

// ── DES-TEAMING-001 S3: monitor→worker advice over `_session/steering` ─────────────────────

// ── Fallback helpers ──────────────────────────────────────────────────────────

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
/// Every fallback is disclosed on [`CoreEvent::AcpFallback`] and in the daemon log
/// (core#970); the seat's output is left untouched, so the disclosure never reads as
/// part of the agent's answer.
/// Stable `fallback_kind` slugs carried on [`CoreEvent::AcpFallback`] for UI dispatch.
pub(crate) mod fallback_kind {
    // RETIRED: `handshake_failed`. Its only emitter was the governed-ACP branch removed with
    // FINDING-060. The shared session path reports every startup failure — spawn or handshake — as
    // `binary_unavailable`, so the slug is no longer produced; a consumer still switching on it is
    // waiting for an event that cannot arrive.
}

pub struct AcpStepRunner {
    /// Back-channel to the actor's single emit point (relay via `Command::EmitEvent`).
    tx: std::sync::mpsc::Sender<Command>,
    /// The carrier every unit runs on: each CLI's headless mode (C5: ACP is removed from the seats).
    fallback: WrappedCliStepRunner,
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
    /// (DES-TEAMING-002 T6) The carrier's handle on team publishing — the engine's bus and team
    /// outbox — installed by `spawn_with_acp_sessions`. A teamed attempt's turn publishes its
    /// `checkpoint.reached` and steer rows through it; `None` = no turn is teamed on this runner.
    team_runner: std::sync::OnceLock<crate::team::runner::TeamRunner>,
}

/// What a READ-ONLY member session runs against (core#410 / crew#502, F-067; the warm chat pool
/// that first used it is gone, ASK-K4): the scratch directory it runs IN, the code graph it is
/// grounded ON, and the repository roots it may READ. Never the daemon's own working directory: a
/// member runs in a private scratch directory of its own ([`ChatScope::scratch_for`]).
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
        Self {
            // Give the fallback runner the same tx so it can relay GovernanceContextArmed
            // events (EVT-016 "wrapped_cli" path) when ACP falls back to the wrapped-CLI runner.
            fallback: WrappedCliStepRunner::with_tx(tx.clone()),
            tx,
            operational_home: None,
            elicitation_maps,
            write_reg,
            team_runner: std::sync::OnceLock::new(),
        }
    }

    /// Accessor for the shared `ElicitationMaps` arc (used by `spawn_with_acp_sessions`
    /// to `Arc::ptr_eq`-verify that the actor and runner share the same instance).
    pub fn elicitation_maps(&self) -> &Arc<Mutex<ElicitationMaps>> {
        &self.elicitation_maps
    }

    /// Release what a finished run held here: its write-lock registrations.
    pub fn drop_session(&self, run_id: &str) {
        self.write_reg
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|(rid, _, _), _| rid != run_id);
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

        // DES-TEAMING-002: the worker's `ADVICE` / `HELP:` / `STEP` lines are read by the worker
        // thread's seam (`team::runner::complete`), on every carrier, after this returns.
        // Container floor: no seat runs over ACP (the ACP bridges run on the host, outside any
        // seat container). Every unit — governed or not, chats included — runs its CLI's
        // headless mode on the wrapped carrier, which puts governed units in a seat container
        // and resumes the run's session per seat (`crate::headless`).
        // C5: ACP is removed from the seats. Every unit runs its CLI's headless mode on the wrapped
        // carrier: in a seat container under the container floor, else on the labelled interim
        // host floor (WICKED_SEAT_FLOOR=host).
        // The host-floor rule (needs_container_floor) is judged by the wrapped carrier on the
        // final argv.
        let output = self.fallback.run_unit_streaming(input, emit);
        let mut output = output;
        if ok_turn_is_a_seat_refusal(&output) {
            output.status = crate::workflow::StepStatus::Failed;
        }

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
}

// ── DES-TEAMING-002 T6: the team's carrier half ──────────────────────────────────────────────────
//
// `team_attach` (turn start) gives a claimed attempt its checkpoint and steer point; the
// supervisor (`team::supervisor`, on the bus) runs its members through `monitor_ensure` /
// `monitor_turn`: warm, READ-ONLY ACP sessions beside the chat machinery, judged by the chat
// boundary, emitting no chat events.

impl AcpStepRunner {
    /// Install the carrier's team publishing handle (`spawn_with_acp_sessions`). Once.
    pub(crate) fn install_team_runner(&self, runner: crate::team::runner::TeamRunner) {
        let _ = self.team_runner.set(runner);
    }
}

/// Team monitors were warm read-only ACP sessions on the host. ACP is removed from the seats (C5),
/// so a monitor is refused with the reason (the supervisor reports it), as the container floor
/// already refused them.
const NO_MONITORS: &str = "team monitors ran over ACP, which is removed from the seats (C5); \
     they are not available until the headless carrier hosts them";

/// (C5, codex c5-r2; core#755) Whether an `ok` turn is really a provider's refusal: no tool call,
/// a SHORT output (a refusal is one error message; a long answer that merely quotes the phrase is
/// work), and the council's own quota / sign-in / approval classifier says so with no exit
/// evidence. Such a turn fails, so the seat is benched and the unit fails over.
fn ok_turn_is_a_seat_refusal(output: &StepOutput) -> bool {
    output.status == crate::workflow::StepStatus::Ok
        && output.tools.is_empty()
        && output.output.trim().len() <= 2000
        && wicked_council::types::SeatFailureReason::classify_refusal(&output.output, false)
            .is_some()
}

/// The supervisor's view of the runner — WEAK, so the supervisor thread never keeps the runner
/// (and through it the actor channel) alive.
pub(crate) struct RunnerHost(pub(crate) std::sync::Weak<AcpStepRunner>);

impl RunnerHost {
    fn runner(&self) -> Result<Arc<AcpStepRunner>, String> {
        self.0
            .upgrade()
            .ok_or_else(|| "the engine is shutting down".to_string())
    }
}

impl crate::team::MonitorHost for RunnerHost {
    fn admitted(&self, _seat: &str) -> Result<(), String> {
        Err(NO_MONITORS.into())
    }
    fn open(
        &self,
        pool_key: &str,
        seat: &str,
        _scope: &crate::team::MonitorScope,
    ) -> Result<(), String> {
        self.runner()?;
        Err(format!("monitor '{pool_key}' on '{seat}': {NO_MONITORS}"))
    }
    fn turn(&self, pool_key: &str, _prompt: &str, _budget: Duration) -> Result<String, String> {
        Err(format!("monitor '{pool_key}': {NO_MONITORS}"))
    }
    fn close(&self, _pool_key: &str) {}
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
        // The ACP inject queue went with ACP (C5): the headless carrier says whether it can deliver.
        self.fallback
            .queue_operator_message(run_id, target, message)
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
    fn on_run_resumed(&self, run_id: &str, min_launch_seq: u64) {
        self.fallback.reopen_run_sessions(run_id, min_launch_seq);
    }

    fn on_shutdown(&self) {
        self.fallback.on_shutdown();
    }

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
        self.write_reg
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|(rid, _, _), _| rid != run_id);
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
        // The container carrier's headless sessions on that seat go too (codex hl-r1).
        self.fallback.forget_seat_sessions(run_id, cli_key);
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
        let _ = (run_id, cli_key);
        std::thread::spawn(move || {
            for kill in kill_handles {
                kill.signal();
            }
        });
    }
}

// ── Registry helper ───────────────────────────────────────────────────────────

/// The merged-registry record for `cli_key` (built-ins + user overlay). Deliberately
/// NOT `registry_roster()`: that filters to `enabled_for_council` seats (a seat disabled
/// for voting can still execute units over ACP) and swallows load errors. A malformed
/// overlay falls back to built-ins instead of stripping every ACP config.
/// (IG1-core-3) The governance class of seat instance `seat` as the engine judges it — the merged
/// registry record's ([`registry_record`], an instance reading its cli's record), the class
/// `monitor_admission` and the arm sites act on. An unregistered key is judged as a bare record
/// (no ACP adapter, not codex): `os_sandbox`, the same default `monitor_admission` names. Stated on
/// `path.started.governance` and `member.joined.governance_class`; never read back from input.
pub(crate) fn seat_governance_class(seat: &str) -> wicked_council::GovernanceClass {
    registry_record(seat)
        .as_ref()
        .map(wicked_council::governance_class)
        .unwrap_or(wicked_council::GovernanceClass::OsSandboxFloor)
}

fn registry_record(cli_key: &str) -> Option<wicked_council::AgenticCli> {
    let user = wicked_council::registry::default_user_path();
    let clis = wicked_council::registry::load(user.as_deref())
        .unwrap_or_else(|_| wicked_council::registry::builtin());
    // core#591: a seat INSTANCE (`claude#2`) reads its cli's record unless the operator gave the
    // instance one of its own — the same two-step lookup the wrapped carrier applies
    // (`execute_wrapped::resolve_invocation`), so one key never resolves two ways.
    for key in [cli_key, wicked_apps_core::spawn::seat_cli_key(cli_key)] {
        if let Some(c) = clis.iter().find(|c| c.key == key) {
            return Some(c.clone());
        }
    }
    None
}

/// The seat identity the ACP carrier judges for ONE launch of `cli_key` (core#396), as the
/// [`WorkerCli`](crate::skills_snapshot::WorkerCli) the skills admission and the Claude-only
/// handshake key off: the MERGED registry record read by key — the operator's `clis.toml`
/// overriding a built-in wholesale — whose `binary` decides whether this is a claude seat
/// (`binary_is_claude`, the same test the wrapped runner applies to its template) and whose
/// `[cli.acp]` bridge is the CARRIER this path spawns (v3.2, F-079): a lever that rides the
/// environment is the SEAT's whatever carries it — pi behind `pi-acp` is judged as pi and handed
/// `WICKED_PI_SKILL_DIRS` for the bridge to forward, opencode behind `opencode acp` as opencode;
/// an argv-only lever (copilot's `--add-dir`) exists only where the bridge IS the CLI
/// (`copilot --acp`), since a separate program forwards no CLI flags
/// (`WorkerCli::for_binaries`). An unregistered key is its own binary, exactly as
/// `resolve_invocation` treats it.
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

#[cfg(test)]
mod tests {

    use super::{
        is_transient_cli_failure, is_worker_originated_failure, ok_turn_is_a_seat_refusal,
    };

    /// (C5) An exit-0, tool-free, short provider refusal is a failed turn; real work is not.
    #[test]
    fn an_ok_turn_that_is_only_a_provider_refusal_fails() {
        let out = |text: &str, tools: Vec<String>| crate::workflow::StepOutput {
            run_id: "r".into(),
            unit_ix: 0,
            attempt: 0,
            output: text.into(),
            status: crate::workflow::StepStatus::Ok,
            usage: None,
            files: vec![],
            tools,
            governed: false,
        };
        assert!(ok_turn_is_a_seat_refusal(&out(
            "Error: You have exceeded your monthly quota",
            vec![]
        )));
        assert!(ok_turn_is_a_seat_refusal(&out(
            "Not logged in. Please run /login",
            vec![]
        )));
        assert!(!ok_turn_is_a_seat_refusal(&out(
            "Not logged in. Please run /login",
            vec!["Bash".into()]
        )));
        let long = format!(
            "Implemented the auth guard: a user who is not signed in is redirected. {}",
            "x".repeat(3000)
        );
        assert!(!ok_turn_is_a_seat_refusal(&out(&long, vec![])));
        assert!(!ok_turn_is_a_seat_refusal(&out(
            "Done: tests pass.",
            vec![]
        )));
    }

    // Used only by the unix-gated tests below (Windows clippy: unused import).
    #[cfg(unix)]
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

    // ── FINDING-022: handshake budgets, start gate, stderr capture ──────────────

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

    // ── crew#340: liveness probe before cached-session reuse ────────────────────

    // ── crew#290 defense-in-depth: bridge spawns in its own process group ────────

    // ── FINDING-061: an ACP worker must not run under the operator's CLI config ─

    // ── core#591 KNOWN GAP (prereq of DES-TEAMING-001 S2): instance seats on the ACP spawn ──

    /// core#936 (codex r4): a bridge is registered infrastructure exactly while its handle owns
    /// the unreaped child. Reaped by an exit probe or killed by `signal()`, its pid (free for the
    /// OS to reuse) is no longer excused from a unit's outstanding-work wait.
    #[cfg(unix)]
    #[test]
    fn core_936_a_bridge_pid_is_infrastructure_only_until_it_is_reaped() {
        // spawn-audit: test-only — short-lived children standing in for a bridge.
        let quick = std::process::Command::new("true")
            .spawn()
            .expect("true runs");
        let handle = KillHandle::new_bridge(quick, 1);
        assert!(handle.is_infra());
        let deadline = Instant::now() + Duration::from_secs(10);
        while handle.try_exit_status().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        // By the handle's own registration, not the pid: a parallel test may already have been
        // handed the reused pid for its own bridge.
        assert!(!handle.is_infra(), "a reaped bridge pid is released");
        assert!(
            handle.try_exit_status().is_some(),
            "a reaped bridge keeps answering its exit status (codex r6)"
        );
        // spawn-audit: test-only — a long-lived stand-in killed through the handle.
        let slow = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("sleep runs");
        let handle = KillHandle::new_bridge(slow, 1);
        assert!(handle.is_infra());
        handle.signal();
        assert!(!handle.is_infra());
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

    /// wicked-crew#524 follow-up (review on core#436): the council ballot path gets the shared
    /// fence from the SAME writer the ACP spawn uses — the same bytes, idempotent — and writes
    /// nothing under the operator's inherit hatch (saying so).
    #[test]
    fn the_council_ballot_fence_is_the_shared_writer_idempotent_and_hatch_aware() {
        let _g = ENV_LOCK.write().unwrap_or_else(|p| p.into_inner());
        let hatch = crate::execute_wrapped::INHERIT_OPERATOR_CONFIG_ENV;
        let prev_hatch = std::env::var_os(hatch);
        std::env::remove_var(hatch);
        let base = worker_home_base("ballot-fence");
        std::env::set_var("WICKED_WORKER_HOME", &base);
        let first = super::ensure_shared_worker_fence()
            .expect("the fence writes")
            .expect("written, not inherited");
        assert_eq!(first, base.join("claude"));
        let bytes = std::fs::read(first.join("settings.json")).expect("settings.json written");
        let settings: Value = serde_json::from_slice(&bytes).unwrap();
        let deny: Vec<String> = settings["permissions"]["deny"]
            .as_array()
            .expect("permissions.deny present")
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
        assert_eq!(
            deny,
            crate::execute_wrapped::shared_deny_rules(None).unwrap(),
            "the ballot fence IS the shared fence — one generator, one file"
        );
        let again = super::ensure_shared_worker_fence()
            .expect("the fence writes")
            .expect("written");
        assert_eq!(again, first);
        assert_eq!(
            std::fs::read(first.join("settings.json")).unwrap(),
            bytes,
            "idempotent: a second call rewrites the same bytes"
        );
        // The inherit hatch: nothing is written, and the caller is told so.
        let fresh = worker_home_base("ballot-fence-hatch");
        std::env::set_var("WICKED_WORKER_HOME", &fresh);
        std::env::set_var(hatch, "1");
        assert!(
            super::ensure_shared_worker_fence()
                .expect("the hatch is not an error")
                .is_none(),
            "under the hatch the ballot runs on the operator's configuration — no fence"
        );
        assert!(!fresh.join("claude").join("settings.json").exists());
        match prev_hatch {
            Some(v) => std::env::set_var(hatch, v),
            None => std::env::remove_var(hatch),
        }
        restore_hermetic_worker_home();
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&fresh);
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
            src.contains("using single-shot fallback{detail}") && src.contains("&stderr_note,"),
            "the fallback reason no longer carries the bridge stderr tail"
        );
        assert!(
            src.contains("chat '{chat_id}' evicting {msg}"),
            "chat evictions no longer reach the daemon log"
        );
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
    /// because `fallback_with_warning` discloses the fallback whether or not the child runs.
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
            capture_report: None,
            requires_capture_report: false,
            base_skill_ref: None,
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
            owner: Default::default(),
            budget_secs: None,
            pool: None,
            pool_seating: None,
            consent_choice: None,
            validator: None,
            tool_cmd: None,
            worker_failed_clis: Vec::new(),
            authored_by: Vec::new(),
            authored_binaries: Vec::new(),
            depends_on: Vec::new(),
            required_deliverables: Vec::new(),
            executes_code: false,
            pre_build_scope: false,
            team_run: false,
            team: None,
            member_step: None,
            scope_warnings: Vec::new(),
            worktree_guarded: false,
            worktree_baseline: None,
            worktree_mutation: None,
            notes_root: None,
            qe_ledger_root: None,
            run_base_commit: None,
            repo_checks_floor: false,
            default_floor: false,
            repo_checks: None,
            last_attempt: None,
            rework_of: None,
            rework_amendment: None,
            review_rounds: Vec::new(),
            operator_rulings: Vec::new(),
            carried_items: Vec::new(),
            floor_auto_retries: 0,
            instructions: None,
            amendments: Vec::new(),
            structured_description: false,
            status: crate::domain::UnitStatus::Pending,
            catalog: None,
            exclude_seats: Vec::new(),
            assurance: None,
            qe_acceptance: None,
            qe_reviewer_seats: Vec::new(),
            qe_review_session: None,
            qe_review: None,
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
                human_confirm: Default::default(),
                db_path: dir.join("estate.db").to_string_lossy().to_string(),
                code_graph_db: None,
                extra_write_roots: Vec::new(),
                extra_read_roots: Vec::new(),
                project_id: None,
                repo_id: None,
            }),
            prior_outputs: Vec::new(),
            elicitation_epoch: 0,
            process_gen: None,
            launch_seq: 0,
            required_skills: Vec::new(),
        }
    }

    /// ACP input governance is an explicit adapter-proof capability, not a CLI-name heuristic.
    /// The built-in registry admits claude (DES-INPUT-GOV-001 §3) and opencode (DES-INPUT-GOV-006
    /// / oq-opencode-acp-002, via the harness-provisioned `acp_governance_env` forcing function)
    /// and pi (core#563, its pi-governance gate through the same forcing function, backed by the
    /// bypass detector) today; every other built-in fails safely to the explicitly disclosed
    /// ungoverned posture.
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
            if cli.key == "claude" || cli.key == "opencode" || cli.key == "pi" {
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

    // ── DES-002 T5 exec_turn_acp arm tests (require a real subprocess, unix only) ──

    // ── core#341: ONE source of truth for elicitation support ─────────────────────────

    // ── core#293: agent request ids cross client prompt ids ───────────────────────

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
                ..
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

    // ── F-079 (core#441): the ACP skills lever from the SEAT binary; the launcher environment ─

    /// The ACP carrier judges pi's lever off the SEAT binary: the REAL built-in pi record carries
    /// `[cli.acp] binary = "pi-acp"` (a separate bridge program), and its identity is now
    /// `PiSkillFlags`, not `Absent`; codex behind `codex-acp` is `CodexSkillsDir` (its lever is
    /// its engine-minted home, carrier-independent) — while agy behind `agy-acp` stays lever-less,
    /// copilot (`copilot --acp`) and opencode (`opencode acp`) keep theirs, and an unregistered
    /// key is its own binary.
    #[test]
    fn the_acp_identity_of_a_pi_seat_behind_pi_acp_is_pi_with_its_skill_flags_lever() {
        use crate::skills_snapshot::{SkillsLever, WorkerCli};
        let builtin = wicked_council::registry::builtin();
        let record = |key: &str| builtin.iter().find(|c| c.key == key).expect(key);
        let pi = record("pi");
        assert_eq!(
            pi.acp.as_ref().map(|a| a.binary.as_str()),
            Some("pi-acp"),
            "the fixture is the real built-in: a separate bridge program carries pi"
        );
        assert_eq!(
            super::seat_identity_of(Some(pi), "pi"),
            WorkerCli::Other {
                key: "pi".into(),
                lever: SkillsLever::PiSkillFlags
            }
        );
        let codex = record("codex");
        assert_eq!(
            codex.acp.as_ref().map(|a| a.binary.as_str()),
            Some("codex-acp")
        );
        assert_eq!(
            super::seat_identity_of(Some(codex), "codex").lever(),
            SkillsLever::CodexSkillsDir
        );
        assert_eq!(
            super::seat_identity_of(Some(record("agy")), "agy").lever(),
            SkillsLever::Absent
        );
        assert_eq!(
            super::seat_identity_of(Some(record("copilot")), "copilot").lever(),
            SkillsLever::CopilotAddDir
        );
        assert_eq!(
            super::seat_identity_of(Some(record("opencode")), "opencode").lever(),
            SkillsLever::OpencodeConfig
        );
        assert!(matches!(
            super::seat_identity_of(Some(record("claude")), "claude"),
            WorkerCli::Claude
        ));
        assert_eq!(
            super::seat_identity_of(None, "pi").lever(),
            SkillsLever::PiSkillFlags
        );
        assert_eq!(
            super::seat_identity_of(None, "codex").lever(),
            SkillsLever::CodexSkillsDir
        );
        assert_eq!(
            super::seat_identity_of(None, "agy").lever(),
            SkillsLever::Absent
        );
    }

    /// A recording ACP bridge (python3, like [`write_recording_bridge`]) that ALSO records, on
    /// `session/new`, the environment it was spawned with (the F-079 variables and `PATH`) and its
    /// own argv after the ledger path — so a `run_unit` test can read back what the engine handed
    /// the carrier without a real pi.
    #[cfg(unix)]
    fn write_env_recording_bridge(dir: &std::path::Path) -> std::path::PathBuf {
        let path = dir.join("env-recording-bridge");
        std::fs::write(
            &path,
            r#"#!/usr/bin/env python3
import sys, json, os
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
        keys = ("WICKED_PI_SKILL_DIRS", "WICKED_GARDEN_ROOT", "CODEX_HOME", "PATH")
        record({"new": req.get("params"),
                "env": {k: os.environ.get(k, "UNSET") for k in keys},
                "argv": sys.argv[2:]})
        w({"jsonrpc": "2.0", "id": req["id"], "result": {
            "sessionId": "recording-session", "protocolVersion": "2025-03-26"}})
    elif method == "session/prompt":
        blocks = (req.get("params") or {}).get("prompt") or []
        text = "".join(b.get("text", "") for b in blocks if isinstance(b, dict))
        record({"prompt": text})
        w({"jsonrpc": "2.0", "id": req["id"], "result": {
            "stopReason": "end_turn", "usage": {"inputTokens": 1, "outputTokens": 1}}})
    elif "id" in req:
        w({"jsonrpc": "2.0", "id": req["id"], "result": {}})
"#,
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// core#653 fixture: a governed claude unit whose worker home carries an org
    /// `allowManagedHooksOnly: true` (`<worker home>/claude/remote-settings.json`), on a claude
    /// seat whose `[cli.acp]` is `acp_binary`. The unit's wrapped invocation is a fake `claude`
    /// that records it ran. Returns `(home, bridge ledger, ran marker, input)`.
    #[cfg(unix)]
    fn managed_hooks_only_claude(
        tag: &str,
        acp_binary: Option<&std::path::Path>,
    ) -> (
        std::path::PathBuf,
        std::path::PathBuf,
        std::path::PathBuf,
        StepInput,
    ) {
        use std::os::unix::fs::PermissionsExt;
        let home = crate::skills_snapshot::test_support::scratch(tag);
        let worker = home.join("worker");
        std::fs::create_dir_all(worker.join("claude")).unwrap();
        std::fs::write(
            worker.join("claude").join("remote-settings.json"),
            r#"{"allowManagedHooksOnly": true}"#,
        )
        .unwrap();
        let bin = home.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let ran = home.join("wrapped-ran.txt");
        let claude = bin.join("claude");
        std::fs::write(
            &claude,
            format!("#!/bin/sh\necho ran > \"{}\"\nexit 0\n", ran.display()),
        )
        .unwrap();
        std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
        let ledger = home.join("ledger.ndjson");
        let bridge = match acp_binary {
            Some(b) => b.to_path_buf(),
            None => {
                let b = bin.join("claude-agent-acp");
                std::fs::rename(write_env_recording_bridge(&home), &b).unwrap();
                b
            }
        };
        let council = home.join(".config").join("wicked-council");
        std::fs::create_dir_all(&council).unwrap();
        std::fs::write(
            council.join("clis.toml"),
            format!(
                r#"
[[cli]]
key = "claude"
display_name = "claude"
binary = "claude"
headless_invocation = "claude -p {{PROMPT}}"

[cli.acp]
binary = "{bridge}"
start_args = ["{ledger}"]
transport = "stdio"
acp_input_governance = true
"#,
                bridge = bridge.display(),
                ledger = ledger.display(),
            ),
        )
        .unwrap();
        let wt = home.join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        let run_id = format!("run-653-{tag}-{}", std::process::id());
        let mut u = crate::domain::WorkUnit::pending(format!("{run_id}:u1"), &run_id, 1, "build");
        u.assigned_cli = Some("claude".to_string());
        u.assigned_invocation = Some(format!("{} -p {{PROMPT}}", claude.display()));
        u.executes_code = true;
        let input = StepInput {
            run_id,
            unit_ix: 0,
            attempt: 0,
            unit: u,
            workflow_id: "wf-653".to_string(),
            entity_mode: crate::scope::EntityMode::Isolated,
            workdir: Some(wt),
            governance: Some(crate::workflow::GovernanceContext {
                human_confirm: Default::default(),
                db_path: home.join("estate.db").to_string_lossy().to_string(),
                code_graph_db: None,
                extra_write_roots: Vec::new(),
                extra_read_roots: Vec::new(),
                project_id: None,
                repo_id: None,
            }),
            prior_outputs: vec![],
            elicitation_epoch: 0,
            process_gen: None,
            launch_seq: 0,
            required_skills: Vec::new(),
        };
        (home, ledger, ran, input)
    }

    /// core#653: the same unit when ACP is UNAVAILABLE (the seat's bridge binary is missing) —
    /// the fallback reaches the wrapped carrier, which refuses it with the escalation instead of
    /// running claude with a gate-hook the org settings drop.
    #[test]
    #[cfg(unix)]
    fn managed_hooks_only_with_acp_unavailable_refuses_the_governed_claude_unit() {
        use crate::workflow::StepRunner;
        if wicked_apps_core::spawn::inherits_operator_config() {
            return;
        }
        let _env = ENV_LOCK.write().unwrap_or_else(|p| p.into_inner());
        let _serial = REAL_STARTS.lock().unwrap_or_else(|p| p.into_inner());
        let missing = std::env::temp_dir().join("wicked-653-no-such-acp-bridge");
        let (home, _ledger, ran, input) = managed_hooks_only_claude("mho-noacp", Some(&missing));
        let _home = EnvPin::set("HOME", &home);
        let _worker = EnvPin::set(
            wicked_apps_core::spawn::WORKER_HOME_ENV,
            &home.join("worker"),
        );
        let _nosnap = EnvPin::unset(crate::skills_snapshot::SKILLS_SNAPSHOT_ENV);
        let (tx, _rx) = std::sync::mpsc::channel();
        let runner = AcpStepRunner::new(tx);
        let out = runner.run_unit(&input);
        assert_eq!(out.status, StepStatus::Failed, "{}", out.output);
        assert!(
            out.output
                .contains(crate::execute_wrapped::MANAGED_HOOKS_ONLY_REFUSAL)
                && out.output.contains("silence the engine's gate hook"),
            "refused with the escalation: {}",
            out.output
        );
        assert!(!out.governed);
        assert!(!ran.exists(), "claude must never run ungoverned");
        runner.drop_session(&input.run_id);
        let _ = std::fs::remove_dir_all(crate::gate_hook::gov_run_dir(&input.run_id));
        let _ = std::fs::remove_dir_all(&home);
    }

    // ── core#577: the tool-child registry tracks every LIVE pgid, not just the newest ───────────

    /// core#577, THE DEFECT — a superseded attempt that registers late must not be able to strand
    /// its own live replacement.
    ///
    /// The window: `ReassignUnit` advances the launch sequence, drains the registry, and dispatches
    /// a replacement while the superseded attempt sits between `proc.spawn()` and its `on_spawn`
    /// registration, so the drain finds nothing and two tool children are live. Nothing orders the
    /// two registrations, so the SUPERSEDED attempt can register second — and while
    /// `register_tool_child` overwrote, that evicted the replacement, after which the superseded
    /// attempt's own `on_done` deregistered its own pgid and left the entry EMPTY. A cancel then
    /// took `[]`, reported `toolChildrenKilled: 0`, and the live tool child ran on unreachable —
    /// the exact escape core#500 closes (a cancelled run's deliver script committing and pushing
    /// under the daemon's token).
    ///
    /// This walks that sequence and asserts on REACHABILITY, deliberately ignoring what
    /// `register_tool_child` returns: a report is not a fix, so this test must fail against a
    /// registry that reports the race perfectly and still loses the pgid.
    #[test]
    fn a_late_superseded_attempt_cannot_strand_the_live_replacement() {
        let mut maps = super::ElicitationMaps::new();

        // 1. The superseded attempt (pgid 100) has spawned but not yet registered, so
        //    `ReassignUnit`'s drain finds nothing…
        assert!(maps.take_tool_child_pgroups("run-r").is_empty());
        // 2. …and the REPLACEMENT (pgid 200) is dispatched, spawns, and registers first.
        let _ = maps.register_tool_child("run-r", 200);
        // 3. The superseded attempt finally reaches its registration. Pre-core#577 this overwrote
        //    the entry with 100 and 200 was gone from the registry.
        let _ = maps.register_tool_child("run-r", 100);
        // 4. It observes `stop()`, kills its own group, and `on_done(100)` deregisters ITSELF.
        //    Pre-core#577 the entry was `100`, so this compare-and-remove emptied the run.
        maps.deregister_tool_child("run-r", 100);
        // 5. A cancel now. The replacement is still live, so it must still be killable.
        assert_eq!(
            maps.take_tool_child_pgroups("run-r"),
            vec![200],
            "the live replacement was stranded: a cancel would take [] and leave it running"
        );

        // The other order (the replacement registers second) has to survive too, and its late
        // `on_done` must remove only its own pgid.
        let _ = maps.register_tool_child("run-s", 100);
        let _ = maps.register_tool_child("run-s", 200);
        assert_eq!(
            maps.take_tool_child_pgroups("run-s"),
            vec![100, 200],
            "while both are live a cancel must reach both"
        );
        let _ = maps.register_tool_child("run-t", 100);
        let _ = maps.register_tool_child("run-t", 200);
        maps.deregister_tool_child("run-t", 100);
        assert_eq!(
            maps.take_tool_child_pgroups("run-t"),
            vec![200],
            "a superseded attempt's own deregistration must not evict the replacement"
        );
    }

    /// core#577, THE REPORT — co-residency is rare enough to be worth a log line, so
    /// `register_tool_child` hands back the pgids already live for that run.
    #[test]
    fn registering_beside_a_live_tool_child_reports_the_co_resident_pgid() {
        let mut maps = super::ElicitationMaps::new();

        // The ordinary path: one attempt, nothing co-resident.
        assert!(maps.register_tool_child("run-a", 111).is_empty());
        // Idempotent re-registration of the SAME pgid is one child, not two.
        assert!(maps.register_tool_child("run-a", 111).is_empty());
        assert_eq!(
            maps.take_tool_child_pgroups("run-a"),
            vec![111],
            "an idempotent re-registration must not double the entry"
        );
        // Another run is a different key — never co-resident.
        assert!(maps.register_tool_child("run-a", 111).is_empty());
        assert!(maps.register_tool_child("run-b", 222).is_empty());
        assert_eq!(maps.take_tool_child_pgroups("run-a"), vec![111]);
        assert_eq!(maps.take_tool_child_pgroups("run-b"), vec![222]);
        assert!(maps.take_tool_child_pgroups("run-b").is_empty());

        // The race: a second, DIFFERENT live pgid for one run comes back so the caller can log it.
        assert!(maps.register_tool_child("run-c", 333).is_empty());
        assert_eq!(
            maps.register_tool_child("run-c", 444),
            vec![333],
            "a co-resident live pgid must be handed back so the caller can log the race"
        );

        // A deregistration for a pgid this run never held changes nothing (core#500's
        // compare-and-remove, now against the set).
        maps.deregister_tool_child("run-c", 999);
        assert_eq!(maps.take_tool_child_pgroups("run-c"), vec![333, 444]);
    }

    // ── crew#998: a governed unit on an admitted seat never falls back to an unchecked carrier ──
}

/// DES-TEAMING-001 S2 (#601) through the REAL ACP turn loop: a scripted bridge drives the
/// frames, so what is pinned is the carrier's behaviour, not a hand-built event. Kept in its own
/// module, away from the steering/turn tests S3 adds.
#[cfg(all(test, unix))]
mod team_s2_tests {}

/// core#762 through the REAL ACP turn loop: a scripted bridge sends the worker's final message and
/// then never closes the turn.
#[cfg(all(test, unix))]
mod settle_tests {}

/// core#563: the bypass detector's rule — a governed pi seat's tool call that COMPLETES must have
/// been granted through the pi-governance gate first.
#[cfg(test)]
mod pi_bypass_tests {}
