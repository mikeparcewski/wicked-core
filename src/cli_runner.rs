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
//!  * The actor's `apply_step_result` guard applies a `task.completed` only when its `(unit_ix, attempt)`
//!    is the CURRENT one (`unit_ix == cursor` AND `attempt == session.attempt`) and the unit isn't
//!    `Done` — a redelivered or SUPERSEDED-attempt result is a no-op (`Stale`), exactly-once apply.
//!
//! ## Durability across a crash/restart (the LOST-ON-CRASH fix)
//! Both consumers persist a DURABLE cursor in the bus db's `core_exec_cursors` table
//! ([`BusDb::save_cursor`]), advanced ONLY AFTER an event is handled+acked. On start each consumer
//! RESUMES from its persisted cursor and falls back to the bus tail only on a true first run (no
//! persisted cursor). So a `task.dispatched` that arrived before a crash but was never handled is
//! re-polled and run on the next start rather than skipped forever. Complementarily, on actor bootstrap
//! in ARMED mode any session left `Executing` is RE-DRIVEN (its cursor unit re-dispatched under a bumped
//! attempt) so a dispatch lost across the restart recovers. Earlier revisions of this module claimed a
//! cross-restart re-run the code did not actually provide (seam finding #9); it now does.
//!
//! ## Live output under exec-mediation (parity gap #11 — now bridged in-process)
//!  * The `cli-runner` runs IN-PROCESS with the actor (spawned by `actor::run` on its own thread,
//!    holding no store handle). So it reaches the actor's SINGLE emit point via the exact `self_tx`
//!    write-back the in-process worker uses: each incremental output chunk becomes a
//!    `Command::CliOutputDelta` the actor fans out as `CoreEvent::CliOutputDelta`. The studio's live
//!    pane therefore ticks under exec-mediation with BYTE-IDENTICAL incremental streaming to the
//!    in-process path (same `run_unit_streaming` sink).
//!  * HONEST LIMIT: the delta stream does NOT ride the bus. Execution MEDIATION is over the bus
//!    (`task.dispatched` → run → `task.completed`); the live-output deltas are an in-process UX
//!    side-channel over the command channel. This is correct precisely because the `cli-runner` is
//!    co-process with the actor here. If the `cli-runner` were ever moved to a SEPARATE process, live
//!    output would need the optional §2.2 `task.output.delta` bus event instead (the command channel
//!    would no longer reach the actor). The FINAL output + verdict already ride the bus and are
//!    identical on both paths.
//!  * TTL (#10): a `task.dispatched`/`task.completed` event is subject to the bus's 72h `expires_at`
//!    TTL. A consumer offline past the TTL would find the event swept before it polls — an unconsumed
//!    task event can be dropped. The restart re-drive (above) is the recovery for a lost dispatch; a
//!    lost completed is recovered the same way (the re-driven unit re-runs and re-publishes).

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
pub const TASK_DISPATCHED: &str = "wicked.crew.task.dispatched";
/// The event the `cli-runner` publishes and the reducer consumes.
pub const TASK_COMPLETED: &str = "wicked.crew.task.completed";

/// The `wicked.task.dispatched` payload. Carries what the `cli-runner` needs to reconstruct the
/// [`StepInput`] the in-process worker would have run — so it reuses the same [`StepRunner`] with
/// no store handle and no duplicated execution logic. `agent_review_target` is the creator's COLD output
/// the actor resolved on-thread (seam finding #8) so the evaluator judges the right artifact off-actor.
///
/// SECURITY (seam finding #7): the unit's APPROVED deterministic validator's shell SCRIPT is NOT
/// serialized here. The `cli-runner` needs only the validator's CRITERION (for the LLM agent judge),
/// never the script — the deterministic script is re-verified at the GATE on the ACTOR, from the unit
/// the actor reads out of its OWN store. [`strip_validator_script`] blanks the script before the unit
/// rides the bus; `validator_pin` carries the content-address of the approved validator for provenance
/// (a re-load-by-pin handle should the cli-runner ever need the full script, which today it does not).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct DispatchedTask {
    run_id: String,
    unit_ix: usize,
    attempt: u32,
    workflow_id: String,
    entity_mode: EntityMode,
    /// The run's worktree (the wrapped-CLI runner's cwd). `None` ⇒ the runner's default cwd.
    workdir: Option<String>,
    /// The unit — with its validator's SCRIPT blanked (finding #7); carries description, validator
    /// criterion, role, skill scope… everything the off-actor runner + agent judge legitimately need.
    unit: crate::domain::WorkUnit,
    /// The creator's cold output an Evaluator-role unit judges (else `None` ⇒ judge the unit's own output).
    agent_review_target: Option<String>,
    /// The assigned CLI key — the routing/filter dimension (§2.2: `task.dispatched` filtered by cli).
    cli: Option<String>,
    /// The content-address PIN of the unit's approved validator (finding #7) — provenance / re-load
    /// handle. `None` ⇒ the unit carried no validator.
    #[serde(default)]
    validator_pin: Option<String>,
}

/// Blank the deterministic validator's SHELL SCRIPT before a unit rides the bus (seam finding #7): the
/// approved script must never be serialized in plaintext onto the event log. Returns the sanitized unit
/// plus the validator's content-address pin (computed over the ORIGINAL, script-and-all, so it is the
/// real approved-validator address). The cli-runner uses only the criterion; the gate re-verifies the
/// script from the actor's own store, so blanking it here changes nothing about the outcome.
fn strip_validator_script(
    unit: &crate::domain::WorkUnit,
) -> (crate::domain::WorkUnit, Option<String>) {
    let pin = unit.validator.as_ref().map(crate::validator_vault::pin);
    let mut sanitized = unit.clone();
    if let Some(v) = sanitized.validator.as_mut() {
        v.script = String::new();
    }
    (sanitized, pin)
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
    // (DES-STUDIO-COCKPIT-001 §3 B3/B4) The runner's adapter usage/files ride the bus so the armed
    // (exec-mediated) daemon path emits `CliUsage`/`DataUsed` just like the in-process path. `#[serde(default)]`
    // keeps older payloads (no usage/files) parseable — absent ⇒ silent (passthrough seats).
    #[serde(default)]
    usage: Option<crate::workflow::Usage>,
    #[serde(default)]
    files: Vec<String>,
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
    run_unit_and_judge_with_roster(
        runner,
        input,
        agent_review_target,
        emit_delta,
        &crate::registry_roster(),
    )
}

/// The roster-injectable core of [`run_unit_and_judge`] — split out ONLY so the seat-selection (C1) is
/// unit-testable with a fabricated roster and no live registry. Production always passes the live
/// [`crate::registry_roster`].
fn run_unit_and_judge_with_roster(
    runner: &Arc<dyn StepRunner>,
    input: &StepInput,
    agent_review_target: Option<&str>,
    emit_delta: &DeltaSink,
    roster: &[crate::AgenticCli],
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
                // GAP B + C1: run the agent judge under a council seat whose identity is DISTINCT from
                // BOTH the deterministic validator's author (`DETERMINISTIC_VALIDATOR_SEAT`) AND the
                // WORK's own author (the unit's `assigned_cli`, falling back to the deterministic author
                // when unassigned). Excluding the work author is what stops a self-grade — the judge can
                // never be dispatched under the very seat that WROTE the work. When no identity-distinct
                // seat exists, `agent_validate` falls back to the single default runner (documented).
                let work_author = input
                    .unit
                    .assigned_cli
                    .as_deref()
                    .unwrap_or(crate::validator::DETERMINISTIC_VALIDATOR_SEAT);
                let excluded = [crate::validator::DETERMINISTIC_VALIDATOR_SEAT, work_author];
                match crate::validator::agent_validate(
                    &v.criterion,
                    work_for_agent,
                    &excluded,
                    roster,
                    &**runner,
                ) {
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
            // #8: the publisher INSERT runs on the single-writer actor thread — a 5s busy-wait behind a
            // concurrent writer would stall every other actor command. A short timeout makes SQLITE_BUSY
            // surface fast so `try_publish_dispatched` falls back to the in-process worker instead.
            let _ = db.set_busy_timeout(Duration::from_millis(250));
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
        // #7: blank the approved validator's shell SCRIPT before the unit is serialized onto the bus.
        let (unit, validator_pin) = strip_validator_script(&input.unit);
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
            unit,
            agent_review_target: agent_review_target.map(|s| s.to_string()),
            cli: input.unit.assigned_cli.clone(),
            validator_pin,
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

// ── Durable-cursor consumer identities + atomic init (findings #1, #4, #5) ───────────────────────────

/// The durable-cursor key for the `cli-runner` subscriber (row in `core_exec_cursors`).
const CONSUMER_CLI_RUNNER: &str = "wicked-core.cli-runner";
/// The durable-cursor key for the `task.completed` poller.
const CONSUMER_TASK_COMPLETED: &str = "wicked-core.task-completed";

/// Resolve a consumer's START floor: its DURABLE cursor if one exists (RESUME across a crash/restart —
/// the LOST-ON-CRASH fix), else the bus tail on a true first run (start at latest, never replay
/// history). `None` ⇒ the cursor row could not be read AND the tail could not be snapshotted → the
/// caller must NOT arm exec-mediation (refuse to replay from 0), leaving the in-process path.
fn resume_floor(db: &BusDb, consumer: &str) -> Option<i64> {
    match db.load_cursor(consumer) {
        Ok(Some(floor)) => Some(floor), // resume from the persisted cursor
        Ok(None) => db.tail_event_id().ok(), // true first run → start at the tail (no replay)
        Err(_) => None,                 // cursor unreadable → disable (don't replay from 0)
    }
}

/// Persist a consumer's durable cursor, logging (never failing the loop) on a write error. The floor and
/// the persisted cursor must advance TOGETHER so a restart resumes exactly where the consumer left off.
fn persist_cursor(db: &BusDb, consumer: &str, id: i64) {
    if let Err(e) = db.save_cursor(consumer, id) {
        eprintln!("wicked-core: {consumer} could not persist cursor at {id}: {e}");
    }
}

/// Both exec-mediation consumers, each with an OPEN bus connection and a RESOLVED start floor — built on
/// the actor thread BEFORE the publisher is armed (the ATOMIC-ARM invariant, finding #4). Owning the open
/// connections here (rather than opening lazily inside each spawned thread) is what makes "both consumers
/// can initialize" a fact the caller checks before arming: if either can't open its bus db or resolve its
/// cursor, [`init_exec_consumers`] returns `None` and the caller leaves exec-mediation OFF, so a
/// `task.dispatched` is never published with no runner to consume it.
pub(crate) struct ExecConsumers {
    cli_runner_db: BusDb,
    cli_runner_floor: i64,
    completed_db: BusDb,
    completed_floor: i64,
}

/// Initialize BOTH consumers against `bus_db_path` (finding #4 — atomicity). Returns `None` if EITHER
/// consumer cannot open the bus db or resolve its durable cursor; the caller then does NOT arm the
/// publisher (the in-process path stands). Runs on the actor thread; the opened connections are MOVED
/// into the consumer threads by [`spawn_exec_consumers`] (`rusqlite::Connection` is `Send`), so a
/// successful init here == a working bus handle in the thread — no second-open race that could leave the
/// publisher armed with a dead consumer.
pub(crate) fn init_exec_consumers(bus_db_path: &str) -> Option<ExecConsumers> {
    let cli_runner_db = BusDb::open(bus_db_path)
        .map_err(|e| eprintln!("wicked-core: cli-runner cannot open bus db {bus_db_path}: {e}"))
        .ok()?;
    let cli_runner_floor = resume_floor(&cli_runner_db, CONSUMER_CLI_RUNNER)?;
    let completed_db = BusDb::open(bus_db_path)
        .map_err(|e| {
            eprintln!("wicked-core: task.completed poller cannot open bus db {bus_db_path}: {e}")
        })
        .ok()?;
    let completed_floor = resume_floor(&completed_db, CONSUMER_TASK_COMPLETED)?;
    Some(ExecConsumers {
        cli_runner_db,
        cli_runner_floor,
        completed_db,
        completed_floor,
    })
}

/// Spawn both off-actor consumer threads from a pre-initialized [`ExecConsumers`]. Called ONLY after the
/// publisher is armed, so arm+consumers land together (finding #4).
pub(crate) fn spawn_exec_consumers(
    consumers: ExecConsumers,
    runner: Arc<dyn StepRunner>,
    tx: Sender<Command>,
    poll_interval: Duration,
    stop: Arc<AtomicBool>,
) -> Vec<JoinHandle<()>> {
    let ExecConsumers {
        cli_runner_db,
        cli_runner_floor,
        completed_db,
        completed_floor,
    } = consumers;
    vec![
        run_cli_runner(
            cli_runner_db,
            cli_runner_floor,
            runner,
            tx.clone(),
            poll_interval,
            stop.clone(),
        ),
        run_task_completed_poller(completed_db, completed_floor, tx, poll_interval, stop),
    ]
}

/// Bounded-join then DETACH an exec consumer thread at shutdown (finding #5). The `cli-runner` may be
/// mid-CLI (an unbounded subprocess) when `stop` is set — the flag is only observed at poll boundaries,
/// so a straight `join()` would block shutdown (and the actor's store release) for the CLI's full
/// duration, unlike the detached in-process worker. We wait up to `timeout` for a clean exit, then detach
/// (drop the handle) and rely on the stop flag + process exit. The consumer holds NO store handle, so
/// detaching is store-safe.
pub(crate) fn join_bounded(handle: JoinHandle<()>, timeout: Duration) {
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = handle.join();
        let _ = done_tx.send(());
    });
    let _ = done_rx.recv_timeout(timeout);
}

// ── The cli-runner SUBSCRIBER (off-actor: consumes task.dispatched → runs work → publishes task.completed) ─

/// Sleep `interval` in short slices, honoring `stop` promptly (shared cancellable-wait helper).
fn cancellable_sleep(stop: &Arc<AtomicBool>, interval: Duration) {
    let slice = Duration::from_millis(50);
    let mut slept = Duration::ZERO;
    while slept < interval && !stop.load(Ordering::SeqCst) {
        std::thread::sleep(slice);
        slept += slice;
    }
}

/// The `cli-runner` subscriber loop (own bus connection MOVED in, no store handle): poll
/// `wicked.task.dispatched` from `floor_init`, run each unit's work via the SAME `runner`, publish
/// `wicked.task.completed`, and PERSIST the durable cursor after each handled event so a restart RESUMES
/// here instead of re-snapshotting to the tail (the LOST-ON-CRASH fix, #1). Idempotent: an in-process
/// dedup set skips a `(run, unit, attempt)` already completed, and the completed event's deterministic
/// key dedups across restarts. At-least-once: the floor advances (and the cursor persists) only after a
/// successful publish, so a transient publish fault re-attempts rather than dropping the task.
///
/// LIVE OUTPUT (parity gap #11 closed): `actor_tx` is a clone of the actor's `self_tx`. The unit's
/// incremental output is streamed to the actor's single emit point via `Command::CliOutputDelta` — the
/// SAME write-back the in-process worker uses — so the studio's live pane ticks under exec-mediation
/// with byte-identical streaming. This reaches the actor ONLY over the command channel (no store handle)
/// and works because the `cli-runner` is co-process with the actor (see the module doc's HONEST LIMIT).
fn run_cli_runner(
    db: BusDb,
    floor_init: i64,
    runner: Arc<dyn StepRunner>,
    actor_tx: Sender<Command>,
    poll_interval: Duration,
    stop: Arc<AtomicBool>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut floor = floor_init;
        // The `(run, unit, attempt)` keys already completed in THIS process — the at-least-once dedup
        // that stops a redelivered dispatch from re-running the CLI.
        let mut done: std::collections::HashSet<String> = std::collections::HashSet::new();

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
                        persist_cursor(&db, CONSUMER_CLI_RUNNER, floor);
                        continue;
                    }
                };
                let dedup = task_key("done", &task.run_id, task.unit_ix, task.attempt);
                if done.contains(&dedup) {
                    floor = ev.event_id; // already handled — advance past the redelivery
                    persist_cursor(&db, CONSUMER_CLI_RUNNER, floor);
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
                // Live-output sink (parity gap #11): stream each chunk to the actor's single emit
                // point as a `Command::CliOutputDelta`, exactly as the in-process worker does. The
                // `Mutex` makes the `!Sync` `Sender` shareable across the runner's concurrent
                // stdout/stderr drains. Reaches the actor ONLY via the command channel (no store
                // handle) — the same self_tx write-back posture as the `task.completed` poller.
                let delta_run_id = task.run_id.clone();
                let delta_ord = task.unit.ord;
                let delta_tx = std::sync::Mutex::new(actor_tx.clone());
                let emit_delta = move |chunk: &str| {
                    if let Ok(g) = delta_tx.lock() {
                        let _ = g.send(Command::CliOutputDelta {
                            run_id: delta_run_id.clone(),
                            ord: delta_ord,
                            chunk: chunk.to_string(),
                        });
                    }
                };
                let (output, agent_verdict) = run_unit_and_judge(
                    &runner,
                    &input,
                    task.agent_review_target.as_deref(),
                    &emit_delta,
                );
                let completed = CompletedTask {
                    run_id: output.run_id.clone(),
                    unit_ix: output.unit_ix,
                    attempt: output.attempt,
                    output: output.output.clone(),
                    status: status_to_str(output.status).to_string(),
                    agent_verdict: agent_verdict
                        .map(|(pass, reasoning)| AgentVerdictWire { pass, reasoning }),
                    usage: output.usage.clone(),
                    files: output.files.clone(),
                };
                let payload = match serde_json::to_value(&completed) {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!(
                            "wicked-core: cli-runner could not serialize task.completed for {}#{}: {e}",
                            task.run_id, task.unit_ix
                        );
                        floor = ev.event_id; // can't ever serialize — don't wedge the batch
                        persist_cursor(&db, CONSUMER_CLI_RUNNER, floor);
                        continue;
                    }
                };
                let key = task_key(TASK_COMPLETED, &task.run_id, task.unit_ix, task.attempt);
                let ev_out =
                    BusEmit::new(TASK_COMPLETED, CORE_DOMAIN, "core.task", payload).with_key(key);
                match db.emit(&ev_out) {
                    Ok(_) => {
                        done.insert(dedup);
                        floor = ev.event_id; // handled — advance the floor + persist the durable cursor
                        persist_cursor(&db, CONSUMER_CLI_RUNNER, floor);
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

/// The reducer's inbound poller loop (own bus connection MOVED in): read `wicked.task.completed` from
/// `floor_init` and post a `Command::ApplyStepResult` to the actor over `tx` — the same command the
/// in-process worker posts. The actor's `apply_step_result` idempotency guard makes a redelivered (or
/// superseded-attempt) result a no-op, so the floor advances — and the DURABLE cursor persists (#1) —
/// once the command is enqueued (a durable mpsc send). Exits when `stop` is set or the actor is gone.
fn run_task_completed_poller(
    db: BusDb,
    floor_init: i64,
    tx: Sender<Command>,
    poll_interval: Duration,
    stop: Arc<AtomicBool>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
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
                        persist_cursor(&db, CONSUMER_TASK_COMPLETED, floor);
                        continue;
                    }
                };
                let output = StepOutput {
                    run_id: task.run_id,
                    unit_ix: task.unit_ix,
                    attempt: task.attempt,
                    output: task.output,
                    status: status_from_str(&task.status),
                    usage: task.usage,
                    files: task.files,
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
                floor = ev.event_id; // enqueued durably — advance + persist (redelivery is a no-op)
                persist_cursor(&db, CONSUMER_TASK_COMPLETED, floor);
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

    /// Seam finding #7: the APPROVED deterministic validator's shell SCRIPT must NOT be serialized onto
    /// the bus. `try_publish_dispatched` publishes a `task.dispatched` whose unit carries the validator
    /// CRITERION (the cli-runner's agent judge needs it) but a BLANK script — the deterministic script is
    /// re-verified at the gate from the actor's own store. The content-address `validator_pin` rides along
    /// for provenance, computed over the ORIGINAL script so it still addresses the real approved validator.
    #[test]
    fn validator_script_is_never_serialized_onto_the_bus() {
        let dir =
            std::env::temp_dir().join(format!("wicked-core-clirunner-v7-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bus_path = dir.join("bus.db").to_str().unwrap().to_string();

        let mut unit = crate::domain::WorkUnit::pending("r:u1", "r", 1, "do it");
        let validator = crate::validator::DeterministicValidator {
            criterion: "the file exists".into(),
            script: "test -f /super/secret/path && rm -rf /".into(),
            approved: true,
        };
        let expected_pin = crate::validator_vault::pin(&validator);
        unit.validator = Some(validator);
        let input = StepInput {
            run_id: "r".into(),
            unit_ix: 0,
            attempt: 0,
            unit,
            workflow_id: "wf-r".into(),
            entity_mode: EntityMode::Shared,
            workdir: None,
        };

        // Arm the publisher on THIS thread, publish, then disarm (thread-local is per-thread).
        assert!(arm_exec_publisher(&bus_path), "arm publisher");
        assert!(
            try_publish_dispatched(&input, None),
            "publish task.dispatched"
        );
        disarm_exec_publisher();

        let bus = BusDb::open(&bus_path).unwrap();
        let evs = bus.poll(TASK_DISPATCHED, 0, 10).unwrap();
        assert_eq!(evs.len(), 1, "one task.dispatched published");
        // The RAW serialized payload must not contain the script anywhere.
        let raw = serde_json::to_string(&evs[0].payload).unwrap();
        assert!(
            !raw.contains("rm -rf") && !raw.contains("/super/secret/path"),
            "the validator SCRIPT must never appear in the serialized task.dispatched payload: {raw}"
        );
        let task: DispatchedTask = serde_json::from_value(evs[0].payload.clone()).unwrap();
        let v = task
            .unit
            .validator
            .expect("the criterion + approval still ride (only the script is stripped)");
        assert_eq!(v.criterion, "the file exists", "criterion is preserved");
        assert!(v.approved, "approval flag is preserved");
        assert_eq!(v.script, "", "the script is blanked");
        assert_eq!(
            task.validator_pin.as_deref(),
            Some(expected_pin.as_str()),
            "the content-address pin (over the ORIGINAL script) rides along for provenance"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn seat(key: &str, invocation: &str) -> crate::AgenticCli {
        use wicked_council::{Category, Confidence, InputMode};
        crate::AgenticCli {
            key: key.into(),
            display_name: key.into(),
            binary: "unused".into(),
            headless_invocation: invocation.into(),
            category: Category::default(),
            input_mode: InputMode::default(),
            version_probe: vec![],
            trust_flags: vec![],
            alt_binaries: vec![],
            confidence: Confidence::default(),
            enabled_for_council: true,
        }
    }

    /// C1 (self-grading in the real path): the agent judge must NOT be dispatched under the seat that
    /// WROTE the work. `run_unit_and_judge` computes the work author from the unit's `assigned_cli` and
    /// excludes BOTH it and the deterministic author when selecting the judge seat. Proven via a recording
    /// stub that captures the assigned_cli of every dispatched unit — the LAST is the judge.
    #[test]
    fn agent_judge_excludes_the_work_author_seat_c1() {
        use crate::workflow::{StepOutput, StepRunner};
        use std::sync::Mutex;

        #[derive(Default)]
        struct RecordingRunner {
            seen: Mutex<Vec<Option<String>>>,
        }
        impl StepRunner for RecordingRunner {
            fn run_unit(&self, input: &StepInput) -> StepOutput {
                self.seen
                    .lock()
                    .unwrap()
                    .push(input.unit.assigned_cli.clone());
                StepOutput {
                    run_id: input.run_id.clone(),
                    unit_ix: input.unit_ix,
                    attempt: input.attempt,
                    output: "PASS recorded".into(),
                    status: StepStatus::Ok,
                    usage: None,
                    files: Vec::new(),
                }
            }
        }

        // A work unit AUTHORED BY `agy` (assigned_cli = "agy"), carrying an APPROVED validator so the
        // agent judge runs. workdir must be Some for the layer-2 judge to fire.
        let dir = std::env::temp_dir().join(format!("wicked-core-c1-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut unit = crate::domain::WorkUnit::pending("r:u1", "r", 1, "do the work");
        unit.assigned_cli = Some("agy".into());
        unit.validator = Some(crate::validator::DeterministicValidator {
            criterion: "the work is correct".into(),
            script: "test -f x".into(),
            approved: true,
        });
        let input = StepInput {
            run_id: "r".into(),
            unit_ix: 0,
            attempt: 0,
            unit,
            workflow_id: "wf-r".into(),
            entity_mode: EntityMode::Isolated,
            workdir: Some(dir.clone()),
        };
        let noop: &DeltaSink = &|_: &str| {};

        // 3-seat roster [claude, agy, pi]: excluding BOTH the det author (claude) and the work author
        // (agy) leaves `pi` as the ONLY distinct judge — proving exclude-both DISPATCHES a distinct seat.
        let rec = Arc::new(RecordingRunner::default());
        let runner: Arc<dyn StepRunner> = rec.clone();
        let roster3 = vec![
            seat("claude", "claude -p {PROMPT}"),
            seat("agy", "agy run {PROMPT}"),
            seat("pi", "pi ask {PROMPT}"),
        ];
        let (_out, verdict) = run_unit_and_judge_with_roster(&runner, &input, None, noop, &roster3);
        assert!(
            verdict.is_some(),
            "an approved validator + workdir ⇒ a layer-2 verdict runs"
        );
        let seen = rec.seen.lock().unwrap();
        let judge_seat = seen.last().cloned().flatten();
        assert_eq!(
            judge_seat.as_deref(),
            Some("pi"),
            "the judge must be the distinct seat, NOT the work author (agy) NOR the det author (claude)"
        );
        assert_ne!(
            judge_seat.as_deref(),
            Some("agy"),
            "never self-grade under the work author"
        );
        drop(seen);

        // 2-seat roster [claude, agy]: both identities excluded ⇒ documented fallback (no explicit seat).
        let rec2 = Arc::new(RecordingRunner::default());
        let runner2: Arc<dyn StepRunner> = rec2.clone();
        let roster2 = vec![
            seat("claude", "claude -p {PROMPT}"),
            seat("agy", "agy run {PROMPT}"),
        ];
        let _ = run_unit_and_judge_with_roster(&runner2, &input, None, noop, &roster2);
        let seen2 = rec2.seen.lock().unwrap();
        assert_eq!(
            seen2.last().cloned().flatten(),
            None,
            "no distinct seat ⇒ fallback carries no explicit seat (and is NOT agy)"
        );
        drop(seen2);
        let _ = std::fs::remove_dir_all(&dir);
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
