//! The Law 1 EXECUTION-MEDIATION SEAM (DES-EXEC-001 §2.3, §5) — the one edge that must decouple: the
//! reducer (actor) no longer calls execution directly, it PUBLISHES `wicked.task.dispatched`; a
//! `cli-runner` SUBSCRIBER consumes it, runs the unit's work OFF the actor via the *same* [`StepRunner`]
//! seam, and PUBLISHES `wicked.task.completed` back; the actor consumes that and folds it into the SAME
//! `apply_step_result` it already runs. This makes "the actor no longer calls execution directly" REAL
//! for the execution seam (Law 1 already held for the launch trigger — `run.requested` → `LaunchRun`).
//!
//! ## Opt-in — the default in-process path is byte-for-byte untouched
//! The whole seam is gated on [`is_exec_enabled`] (set from `WICKED_BUS_EXEC` + `WICKED_BUS_DB`, or the
//! explicit `Core::spawn_with_engine_exec` test entry). When OFF (the default), [`dispatch_unit`] spawns
//! the in-process worker exactly as before and NONE of this module runs. When ON, `dispatch_unit`
//! publishes `task.dispatched` instead of spawning, and two dedicated OFF-ACTOR threads carry the work.
//!
//! ## Actor-safety (the load-bearing invariant — same posture as the launch bridge)
//!  * The `cli-runner` subscriber and the `task.completed` poller each run on their OWN `std::thread`
//!    with their OWN `rusqlite` connection to the bus db (a different file from the estate store the
//!    actor owns — no writer-lock contention). Neither holds a store handle: the `cli-runner` reads only
//!    the dispatched event + publishes the result; the actor stays the ONLY writer.
//!  * The actor reaches nothing here by a blocking poll. It only *publishes* `task.dispatched`, a single
//!    bounded local INSERT into a WAL-mode db via an actor-thread-local [`BusDb`] — the reducer's publish
//!    role (§2.3), analogous to the actor's own store writes, never an unbounded poll or a CLI call.
//!  * The `task.completed` poller reaches the actor ONLY by sending `Command::ApplyStepResult` over a
//!    `Sender<Command>` clone — the exact `self_tx` write-back the in-process worker already uses.
//!
//! ## Idempotency (exactly-once *effect* over at-least-once delivery)
//!  * `task.dispatched` and `task.completed` carry a DETERMINISTIC idempotency key per
//!    `(run_id, unit_ix, attempt)`, so a re-emit dedups to one physical row (the bus's UNIQUE key).
//!  * The `cli-runner` dedups on that key in-process (never runs the same task twice within a run) and,
//!    across process restarts, a re-run publishes the SAME-keyed completed row (harmless dedup).
//!  * The actor's existing `apply_step_result` guard (result applied only if `unit_ix == cursor` and the
//!    unit isn't `Done`) makes a redelivered `task.completed` a no-op (`Stale`) — exactly-once apply.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::bus::{deterministic_key, BusDb, BusEmit, CORE_DOMAIN};
use crate::command::Command;
use crate::scope::EntityMode;
use crate::workflow::{DeltaSink, StepInput, StepOutput, StepRunner, StepStatus};

/// The event the reducer publishes and the `cli-runner` consumes (filtered).
pub const TASK_DISPATCHED: &str = "wicked.task.dispatched";
/// The event the `cli-runner` publishes and the reducer consumes.
pub const TASK_COMPLETED: &str = "wicked.task.completed";

/// The `wicked.task.dispatched` payload. Carries everything the `cli-runner` needs to reconstruct the
/// EXACT [`StepInput`] the in-process worker would have run — so it reuses the same [`StepRunner`] with
/// no store handle and no duplicated execution logic. `agent_review_target` is the creator's COLD output
/// the actor resolved on-thread (seam finding #8) so the evaluator judges the right artifact off-actor.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct DispatchedTask {
    run_id: String,
    unit_ix: usize,
    attempt: u32,
    workflow_id: String,
    entity_mode: EntityMode,
    /// The run's worktree (the wrapped-CLI runner's cwd). `None` ⇒ the runner's default cwd.
    workdir: Option<String>,
    /// The full unit (`WorkUnit` is `Serialize`) — carries description, validator, role, skill scope…
    unit: crate::domain::WorkUnit,
    /// The creator's cold output an Evaluator-role unit judges (else `None` ⇒ judge the unit's own output).
    agent_review_target: Option<String>,
    /// The assigned CLI key — the routing/filter dimension (§2.2: `task.dispatched` filtered by cli).
    cli: Option<String>,
}

/// The `wicked.task.completed` payload — mirrors the fields `Command::ApplyStepResult` carries
/// (`StepOutput` + the LAYER-2 agent verdict). `status` is a string because [`StepStatus`] is not
/// `Serialize` (and workflow.rs is out of scope); [`status_to_str`]/[`status_from_str`] map it.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CompletedTask {
    run_id: String,
    unit_ix: usize,
    attempt: u32,
    output: String,
    status: String,
    agent_verdict: Option<AgentVerdictWire>,
}

/// The wire form of the `(pass, reasoning)` agent verdict `ApplyStepResult` carries.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct AgentVerdictWire {
    pass: bool,
    reasoning: String,
}

fn status_to_str(s: StepStatus) -> &'static str {
    match s {
        StepStatus::Ok => "ok",
        StepStatus::Failed => "failed",
        StepStatus::Cancelled => "cancelled",
    }
}

fn status_from_str(s: &str) -> StepStatus {
    match s {
        "failed" => StepStatus::Failed,
        "cancelled" => StepStatus::Cancelled,
        _ => StepStatus::Ok,
    }
}

/// The deterministic idempotency key for a task's dispatched/completed pair, per `(run, unit, attempt)`.
fn task_key(event_type: &str, run_id: &str, unit_ix: usize, attempt: u32) -> String {
    deterministic_key(&[
        event_type,
        run_id,
        &unit_ix.to_string(),
        &attempt.to_string(),
    ])
}

// ── The shared execute+judge core (reused by BOTH the in-process worker AND the cli-runner) ──────────

/// Run one unit's slow work via `runner` and compute the rev0.4 LAYER-2 agent verdict — the EXACT
/// behavior the in-process worker had, extracted so both dispatch paths run byte-identical logic (this
/// is what guarantees "same outcome as the in-process path"). Holds no store handle: `agent_review_target`
/// is passed in (resolved by the actor on-thread). The LLM `agent_validate` runs here — OFF the actor —
/// exactly as it did on the worker thread. A non-`Ok` step or a workdir-less run gets no agent verdict
/// (the actor handles a failed/cancelled worker before any gate; layer-1 fails closed without a worktree).
pub(crate) fn run_unit_and_judge(
    runner: &Arc<dyn StepRunner>,
    input: &StepInput,
    agent_review_target: Option<&str>,
    emit_delta: &DeltaSink,
) -> (StepOutput, Option<(bool, String)>) {
    let output = runner.run_unit_streaming(input, emit_delta);
    let work_for_agent = agent_review_target.unwrap_or(&output.output);
    let agent_verdict = if output.status == StepStatus::Ok && input.workdir.is_some() {
        input
            .unit
            .validator
            .as_ref()
            .filter(|v| v.approved)
            .map(|v| {
                match crate::validator::agent_validate(&v.criterion, work_for_agent, &**runner) {
                    Ok(av) => (av.pass, av.reasoning),
                    Err(e) => (false, format!("agent validator errored (fail-closed): {e}")),
                }
            })
    } else {
        None
    };
    (output, agent_verdict)
}

// ── The actor-thread publish seam (thread-local — dispatch_unit consults it) ─────────────────────────

thread_local! {
    /// The actor thread's bus publisher when exec-mediation is ON. `dispatch_unit` (which only ever runs
    /// on the actor thread) reads this: `Some` ⇒ publish `task.dispatched`; `None` (the default) ⇒ spawn
    /// the in-process worker as before. A thread-local is the clean way to make the mode available deep in
    /// the actor's private call tree WITHOUT threading a parameter through `launch_run_inner` /
    /// `advance_or_pause` / `confirm_gate` (whose signatures campaign.rs depends on — out of scope).
    static EXEC_PUBLISHER: RefCell<Option<BusDb>> = const { RefCell::new(None) };
}

/// Arm exec-mediation on the CURRENT (actor) thread with an open bus publisher. Returns `false` if the
/// bus db can't be opened — the caller then leaves exec mode OFF and the default in-process path stands
/// (the same disable-on-uninitialized posture as the launch bridge's floor snapshot).
pub(crate) fn arm_exec_publisher(bus_db_path: &str) -> bool {
    match BusDb::open(bus_db_path) {
        Ok(db) => {
            EXEC_PUBLISHER.with(|cell| *cell.borrow_mut() = Some(db));
            true
        }
        Err(e) => {
            eprintln!(
                "wicked-core: exec-mediation disabled — cannot open bus db {bus_db_path} to publish \
                 task.dispatched: {e}; falling back to in-process dispatch"
            );
            false
        }
    }
}

/// Disarm exec-mediation on the current thread (actor loop exit).
pub(crate) fn disarm_exec_publisher() {
    EXEC_PUBLISHER.with(|cell| *cell.borrow_mut() = None);
}

/// Whether exec-mediation is armed on THIS thread (the actor). `dispatch_unit` branches on this.
pub(crate) fn is_exec_enabled() -> bool {
    EXEC_PUBLISHER.with(|cell| cell.borrow().is_some())
}

/// Publish `task.dispatched` for one unit (the reducer's publish, on the actor thread). A bounded local
/// INSERT (see the module actor-safety note). Idempotent by the `(run, unit, attempt)` key so a re-issued
/// dispatch dedups. Returns `true` if published (exec mode armed), `false` if the in-process path should
/// run instead. A publish error is surfaced as `false` so the run still makes progress in-process rather
/// than wedging with no worker.
pub(crate) fn try_publish_dispatched(input: &StepInput, agent_review_target: Option<&str>) -> bool {
    EXEC_PUBLISHER.with(|cell| {
        let guard = cell.borrow();
        let Some(db) = guard.as_ref() else {
            return false;
        };
        let task = DispatchedTask {
            run_id: input.run_id.clone(),
            unit_ix: input.unit_ix,
            attempt: input.attempt,
            workflow_id: input.workflow_id.clone(),
            entity_mode: input.entity_mode,
            workdir: input
                .workdir
                .as_ref()
                .map(|p| p.to_string_lossy().to_string()),
            unit: input.unit.clone(),
            agent_review_target: agent_review_target.map(|s| s.to_string()),
            cli: input.unit.assigned_cli.clone(),
        };
        let payload = match serde_json::to_value(&task) {
            Ok(v) => v,
            Err(e) => {
                eprintln!(
                    "wicked-core: exec-mediation could not serialize task.dispatched for {}#{}: {e}; \
                     falling back to in-process dispatch",
                    input.run_id, input.unit_ix
                );
                return false;
            }
        };
        let key = task_key(TASK_DISPATCHED, &input.run_id, input.unit_ix, input.attempt);
        let ev = BusEmit::new(TASK_DISPATCHED, CORE_DOMAIN, "core.task", payload).with_key(key);
        match db.emit(&ev) {
            Ok(_) => true,
            Err(e) => {
                eprintln!(
                    "wicked-core: exec-mediation failed to publish task.dispatched for {}#{}: {e}; \
                     falling back to in-process dispatch",
                    input.run_id, input.unit_ix
                );
                false
            }
        }
    })
}

// ── The cli-runner SUBSCRIBER (off-actor: consumes task.dispatched → runs work → publishes task.completed) ─

/// Snapshot the bus tail SYNCHRONOUSLY on the caller's thread, BEFORE spawning a poller thread, so a
/// request emitted right after spawn is never missed and history is never replayed (the exact
/// happens-before guarantee the launch bridge documents). `None` ⇒ the bus db can't be read → the caller
/// disables that poller (spawns a thread that just exits) rather than replaying from 0.
fn snapshot_floor(bus_db_path: &str) -> Option<i64> {
    BusDb::open(bus_db_path)
        .ok()
        .and_then(|db| db.tail_event_id().ok())
}

/// Sleep `interval` in short slices, honoring `stop` promptly (shared cancellable-wait helper).
fn cancellable_sleep(stop: &Arc<AtomicBool>, interval: Duration) {
    let slice = Duration::from_millis(50);
    let mut slept = Duration::ZERO;
    while slept < interval && !stop.load(Ordering::SeqCst) {
        std::thread::sleep(slice);
        slept += slice;
    }
}

/// Spawn the `cli-runner` subscriber: poll `wicked.task.dispatched`, run each unit's work via the SAME
/// `runner`, and publish `wicked.task.completed`. Off-actor, own bus connection, no store handle.
/// Idempotent: an in-process dedup set skips a `(run, unit, attempt)` already completed, and the completed
/// event's deterministic key dedups across restarts. At-least-once: the floor advances only after a
/// successful publish, so a transient publish fault re-attempts rather than dropping the task.
pub(crate) fn spawn_cli_runner(
    bus_db_path: String,
    runner: Arc<dyn StepRunner>,
    poll_interval: Duration,
    stop: Arc<AtomicBool>,
) -> JoinHandle<()> {
    let Some(floor_init) = snapshot_floor(&bus_db_path) else {
        eprintln!(
            "wicked-core: cli-runner disabled — cannot snapshot cursor floor from bus db \
             {bus_db_path}; refusing to replay history"
        );
        return std::thread::spawn(|| {});
    };
    std::thread::spawn(move || {
        let db = match BusDb::open(&bus_db_path) {
            Ok(d) => d,
            Err(e) => {
                eprintln!(
                    "wicked-core: cli-runner disabled — cannot open bus db {bus_db_path}: {e}"
                );
                return;
            }
        };
        let mut floor = floor_init;
        // The `(run, unit, attempt)` keys already completed in THIS process — the at-least-once dedup
        // that stops a redelivered dispatch from re-running the CLI.
        let mut done: std::collections::HashSet<String> = std::collections::HashSet::new();
        // The bus-mediated path does not stream deltas to the studio (a `task.output.delta` fan-out is a
        // separate, optional §2.2 event); the runner still runs identically with a no-op sink.
        let noop_delta = |_: &str| {};

        while !stop.load(Ordering::SeqCst) {
            let events = match db.poll(TASK_DISPATCHED, floor, 100) {
                Ok(evs) => evs,
                Err(e) => {
                    eprintln!("wicked-core: cli-runner poll error: {e}");
                    Vec::new()
                }
            };
            for ev in events {
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                let task: DispatchedTask = match serde_json::from_value(ev.payload.clone()) {
                    Ok(t) => t,
                    Err(e) => {
                        // Poison payload — advance past it (retrying can never parse it).
                        eprintln!(
                            "wicked-core: cli-runner dropping unparseable task.dispatched {} ({e})",
                            ev.event_id
                        );
                        floor = ev.event_id;
                        continue;
                    }
                };
                let dedup = task_key("done", &task.run_id, task.unit_ix, task.attempt);
                if done.contains(&dedup) {
                    floor = ev.event_id; // already handled — advance past the redelivery
                    continue;
                }
                let input = StepInput {
                    run_id: task.run_id.clone(),
                    unit_ix: task.unit_ix,
                    attempt: task.attempt,
                    unit: task.unit.clone(),
                    workflow_id: task.workflow_id.clone(),
                    entity_mode: task.entity_mode,
                    workdir: task.workdir.clone().map(std::path::PathBuf::from),
                };
                let (output, agent_verdict) = run_unit_and_judge(
                    &runner,
                    &input,
                    task.agent_review_target.as_deref(),
                    &noop_delta,
                );
                let completed = CompletedTask {
                    run_id: output.run_id.clone(),
                    unit_ix: output.unit_ix,
                    attempt: output.attempt,
                    output: output.output.clone(),
                    status: status_to_str(output.status).to_string(),
                    agent_verdict: agent_verdict
                        .map(|(pass, reasoning)| AgentVerdictWire { pass, reasoning }),
                };
                let payload = match serde_json::to_value(&completed) {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!(
                            "wicked-core: cli-runner could not serialize task.completed for {}#{}: {e}",
                            task.run_id, task.unit_ix
                        );
                        floor = ev.event_id; // can't ever serialize — don't wedge the batch
                        continue;
                    }
                };
                let key = task_key(TASK_COMPLETED, &task.run_id, task.unit_ix, task.attempt);
                let ev_out =
                    BusEmit::new(TASK_COMPLETED, CORE_DOMAIN, "core.task", payload).with_key(key);
                match db.emit(&ev_out) {
                    Ok(_) => {
                        done.insert(dedup);
                        floor = ev.event_id; // handled — advance the floor
                    }
                    // Transient publish fault → do NOT advance; break the batch and re-poll (at-least-once).
                    Err(e) => {
                        eprintln!(
                            "wicked-core: cli-runner could not publish task.completed for {} (transient, \
                             will retry): {e}",
                            ev.event_id
                        );
                        break;
                    }
                }
            }
            cancellable_sleep(&stop, poll_interval);
        }
    })
}

// ── The actor-inbound poller (off-actor: task.completed → Command::ApplyStepResult) ──────────────────

/// Spawn the reducer's inbound poller: read `wicked.task.completed` and post a `Command::ApplyStepResult`
/// to the actor over `tx` — the same command the in-process worker posts. Off-actor, own bus connection.
/// The actor's `apply_step_result` idempotency guard makes a redelivered result a no-op, so the floor is
/// advanced once the command is enqueued (a durable mpsc send). Exits when `stop` is set or the actor is
/// gone (send fails).
pub(crate) fn spawn_task_completed_poller(
    bus_db_path: String,
    tx: Sender<Command>,
    poll_interval: Duration,
    stop: Arc<AtomicBool>,
) -> JoinHandle<()> {
    let Some(floor_init) = snapshot_floor(&bus_db_path) else {
        eprintln!(
            "wicked-core: task.completed poller disabled — cannot snapshot cursor floor from bus db \
             {bus_db_path}; refusing to replay history"
        );
        return std::thread::spawn(|| {});
    };
    std::thread::spawn(move || {
        let db = match BusDb::open(&bus_db_path) {
            Ok(d) => d,
            Err(e) => {
                eprintln!(
                    "wicked-core: task.completed poller disabled — cannot open bus db {bus_db_path}: {e}"
                );
                return;
            }
        };
        let mut floor = floor_init;
        while !stop.load(Ordering::SeqCst) {
            let events = match db.poll(TASK_COMPLETED, floor, 100) {
                Ok(evs) => evs,
                Err(e) => {
                    eprintln!("wicked-core: task.completed poll error: {e}");
                    Vec::new()
                }
            };
            for ev in events {
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                let task: CompletedTask = match serde_json::from_value(ev.payload.clone()) {
                    Ok(t) => t,
                    Err(e) => {
                        eprintln!(
                            "wicked-core: task.completed poller dropping unparseable event {} ({e})",
                            ev.event_id
                        );
                        floor = ev.event_id;
                        continue;
                    }
                };
                let output = StepOutput {
                    run_id: task.run_id,
                    unit_ix: task.unit_ix,
                    attempt: task.attempt,
                    output: task.output,
                    status: status_from_str(&task.status),
                };
                let agent_verdict = task.agent_verdict.map(|v| (v.pass, v.reasoning));
                // Reach the actor ONLY via the command channel (the self_tx write-back pattern). A closed
                // channel ⇒ the actor is gone → exit so `join()` returns.
                if tx
                    .send(Command::ApplyStepResult {
                        output,
                        agent_verdict,
                    })
                    .is_err()
                {
                    return;
                }
                floor = ev.event_id; // enqueued durably — advance (redelivery is a no-op via the guard)
            }
            cancellable_sleep(&stop, poll_interval);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_string_roundtrips() {
        for s in [StepStatus::Ok, StepStatus::Failed, StepStatus::Cancelled] {
            assert_eq!(status_from_str(status_to_str(s)), s);
        }
        // Unknown token fails safe to Ok (the actor's failed/cancelled arms are the deny paths; an
        // unknown status must not spuriously fail a run — Ok goes through the normal gate).
        assert_eq!(status_from_str("garbage"), StepStatus::Ok);
    }

    #[test]
    fn task_key_is_deterministic_per_run_unit_attempt() {
        let a = task_key(TASK_DISPATCHED, "run-1", 2, 0);
        let b = task_key(TASK_DISPATCHED, "run-1", 2, 0);
        assert_eq!(a, b, "same (run, unit, attempt) ⇒ same key (idempotent)");
        assert_ne!(
            a,
            task_key(TASK_DISPATCHED, "run-1", 2, 1),
            "attempt varies the key"
        );
        assert_ne!(
            a,
            task_key(TASK_COMPLETED, "run-1", 2, 0),
            "event type varies the key"
        );
    }
}
