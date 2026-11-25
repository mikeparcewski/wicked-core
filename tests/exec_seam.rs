//! Law 1 EXECUTION-MEDIATION SEAM — real round-trip proof (DES-EXEC-001 §2.3, §5).
//!
//! Proves the honest gap is closed: with exec-mediation ON, a unit's work flows
//! actor → (bus `wicked.task.dispatched`) → cli-runner → (bus `wicked.task.completed`) → actor → gate,
//! producing the SAME outcome as the in-process path — and the actor did NOT run the work in-process
//! (the cli-runner did). Uses the stub runner (no real CLI). Env-free entry (`spawn_with_engine_exec`)
//! so it cannot race other tests on process env.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use wicked_apps_core::open_store;
use wicked_council::types::{Category, Confidence, Dispatcher, InputMode, Vote};
use wicked_council::{AgenticCli, CouncilTask};
use wicked_governance::{register_policy, Effect, Policy, Severity, Trigger};

use wicked_core::{
    BusDb, Core, CoreEvent, EntityMode, HumanConfirm, HumanDecision, LaunchSpec, SessionStatus,
    StepInput, StepOutput, StepRunner, StepStatus, TASK_COMPLETED, TASK_DISPATCHED,
};

/// Deterministic council stub — votes without a subprocess.
struct StubDispatcher;
impl Dispatcher for StubDispatcher {
    fn dispatch(&self, cli: &AgenticCli, _task: &CouncilTask) -> Option<Vote> {
        Some(Vote {
            cli: cli.key.clone(),
            recommendation: "fake-a".into(),
            top_risk: "none".into(),
            change_my_mind: "no".into(),
            disqualifier: None,
            confidence: Confidence::default(),
            provenance: "stub".into(),
        })
    }
}

/// A runner that completes every unit immediately, counting how many times it ran (to prove the work
/// executed exactly once per unit — never both in-process AND over the bus).
struct CountingRunner {
    runs: Arc<AtomicUsize>,
}
impl StepRunner for CountingRunner {
    fn run_unit(&self, input: &StepInput) -> StepOutput {
        self.runs.fetch_add(1, Ordering::SeqCst);
        StepOutput {
            run_id: input.run_id.clone(),
            unit_ix: input.unit_ix,
            attempt: input.attempt,
            output: format!("stub-output for {}", input.unit.description),
            status: StepStatus::Ok,
            usage: None,
            files: Vec::new(),
            tools: Vec::new(),
            governed: false,
        }
    }
}

fn cli(key: &str) -> AgenticCli {
    AgenticCli {
        key: key.into(),
        display_name: key.into(),
        binary: "unused".into(),
        headless_invocation: "unused {PROMPT}".into(),
        category: Category::default(),
        input_mode: InputMode::default(),
        version_probe: vec![],
        trust_flags: vec![],
        alt_binaries: vec![],
        confidence: Confidence::default(),
        enabled_for_council: true,
        seat_eligible_for_work: true,
        acp: None,
        capabilities: None,
        login_invocation: None,
        logout_invocation: None,
        governance_class: None,
        credential: None,
        free_tier: None,
        health: None,
    }
}

fn tmp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "wicked-core-execseam-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The two-unit plan the seam proofs need (one `task.dispatched` per unit). D-11 (core#393): a
/// free-text problem plans ONE unit now, so the multi-unit shape is workflow DATA — a 2-phase def
/// registered by [`launch`] on whichever `Core` runs the test.
const SEAM_DEF: &str = r#"{"id":"seam-2","phases":[
  {"id":"one","kind":"build","gate":"auto"},
  {"id":"two","kind":"build","gate":"auto","depends_on":["one"]}]}"#;

/// Register the 2-phase def on `core` (idempotent — re-registration on a reopened store is judged
/// as authored) and launch `spec(session_id)` against it.
fn launch(core: &Core, session_id: &str) -> anyhow::Result<String> {
    core.register_workflow(SEAM_DEF)
        .expect("register the 2-phase def");
    core.launch_run(spec(session_id))
}

fn spec(session_id: &str) -> LaunchSpec {
    LaunchSpec {
        base_ref: None,
        project_id: None,
        // Two phases (the def) → two units; the prose is the run's intent, no longer its plan.
        problem: "Do step one. Do step two".into(),
        clis: vec![cli("fake-a"), cli("fake-b")],
        entity_mode: EntityMode::Shared,
        session_id: session_id.into(),
        human_confirm: HumanConfirm::None,
        auto_deliver: false,
        repo_ref: None,
        workflow: Some("seam-2".into()),
        extra_write_roots: Vec::new(),
        extra_read_roots: Vec::new(),
        optional_read_roots: Vec::new(),
        project_graph: None,
        plan: None,
        deliver_step: None,
        exclude_seats: Vec::new(),
        evidence_root: None,
        primary: None,
        reduced_assurance: false,
        stand_in: Default::default(),
        power: false,
        voice: None,
        deliverables: Vec::new(),
        qe_acceptance: Default::default(),
    }
}

fn wait_for_completion(events: &std::sync::mpsc::Receiver<CoreEvent>, run_id: &str) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        match events.recv_timeout(Duration::from_secs(1)) {
            Ok(CoreEvent::SessionCompleted { session }) if session == run_id => return,
            Ok(CoreEvent::Error { message, .. }) => panic!("run {run_id} errored: {message}"),
            _ => continue,
        }
    }
    panic!("run {run_id} did not reach SessionCompleted within the deadline");
}

/// Read each unit's captured work output (the gate-approved transcript) for a run, ordered by unit.
fn unit_outputs(core: &Core, run_id: &str) -> Vec<Option<String>> {
    let detail = core
        .sessions_detail()
        .unwrap()
        .into_iter()
        .find(|v| v.session.id == run_id)
        .unwrap_or_else(|| panic!("run {run_id} not found on the store"));
    detail
        .units
        .iter()
        .map(|u| core.work_output(&u.id))
        .collect()
}

#[test]
fn exec_seam_round_trip_matches_in_process() {
    // ── The bus-mediated run (exec-mediation ON) ───────────────────────────────────────────────────
    let dir = tmp_dir("exec");
    let estate_db = dir.join("estate.db").to_str().unwrap().to_string();
    let bus_db = dir.join("bus.db").to_str().unwrap().to_string();

    let runs = Arc::new(AtomicUsize::new(0));
    let core = Core::spawn_with_engine_exec(
        estate_db,
        Arc::new(StubDispatcher),
        Arc::new(CountingRunner { runs: runs.clone() }),
        bus_db.clone(),
    );
    let events = core.subscribe();

    let run_id = launch(&core, "exec-run").expect("launch_run");
    assert_eq!(run_id, "exec-run");
    wait_for_completion(&events, "exec-run");

    // The run is a real, persisted, COMPLETED session — the gate ran to the end (same outcome shape as
    // the in-process path: work flowed all the way through the reducer's gate).
    let exec_outputs = unit_outputs(&core, "exec-run");
    let n_units = exec_outputs.len();
    assert!(
        n_units >= 2,
        "the two-sentence problem planned {n_units} units"
    );

    // PROOF THE WORK WENT OVER THE BUS: the reducer published one `task.dispatched` per unit and the
    // cli-runner published one `task.completed` per unit. These events exist ONLY on the bus-mediated
    // path — the in-process `dispatch_unit` branch spawns a worker and emits NEITHER. So their presence
    // (one per unit) is proof the actor did NOT run the work in-process; the cli-runner did.
    let bus = BusDb::open(&bus_db).unwrap();
    let dispatched = bus.poll(TASK_DISPATCHED, 0, 100).unwrap();
    let completed = bus.poll(TASK_COMPLETED, 0, 100).unwrap();
    assert_eq!(
        dispatched.len(),
        n_units,
        "the actor published exactly one task.dispatched per unit (the reducer's publish)"
    );
    assert_eq!(
        completed.len(),
        n_units,
        "the cli-runner published exactly one task.completed per unit"
    );
    // Every dispatched event is published by the reducer (domain = wicked-core) and carries the run id.
    for ev in &dispatched {
        assert_eq!(
            ev.domain, "wicked-core",
            "task.dispatched is the reducer's publish"
        );
        assert_eq!(ev.payload["run_id"], "exec-run");
    }
    for ev in &completed {
        assert_eq!(ev.payload["run_id"], "exec-run");
    }

    // The work executed EXACTLY ONCE per unit — never both in-process AND over the bus (a double-run
    // would leave the counter at 2×n_units). Exactly n_units means the single execution was the
    // cli-runner's (the in-process branch was bypassed by the early publish-and-return).
    assert_eq!(
        runs.load(Ordering::SeqCst),
        n_units,
        "each unit's work ran exactly once (via the cli-runner, not also in-process)"
    );

    // ── The in-process baseline (exec-mediation OFF — the default path) ─────────────────────────────
    let dir2 = tmp_dir("inproc");
    let estate_db2 = dir2.join("estate.db").to_str().unwrap().to_string();
    let core2 = Core::spawn_with_engine(
        estate_db2,
        Arc::new(StubDispatcher),
        Arc::new(CountingRunner {
            runs: Arc::new(AtomicUsize::new(0)),
        }),
    );
    let events2 = core2.subscribe();
    launch(&core2, "inproc-run").expect("launch_run");
    wait_for_completion(&events2, "inproc-run");
    let inproc_outputs = unit_outputs(&core2, "inproc-run");

    // SAME OUTCOME: the bus-mediated run produced the identical per-unit gate-approved outputs the
    // in-process run did.
    assert_eq!(
        exec_outputs, inproc_outputs,
        "the bus-mediated seam produced the same per-unit outcome as the in-process path"
    );

    drop(bus);
}

/// #133 (DURABLE TRANSCRIPTS): a unit's gate-approved `work_output` must survive a daemon restart —
/// a run that COMPLETED in a prior process must still return its transcript from a fresh Core on the
/// same store. The reported symptom was `work_output` returning `null` for every run that finished
/// before the current daemon started (transcripts treated as process-lifetime state). This is the
/// falsifiable proof of the fix: complete a run in `core1`, drop it, reopen the SAME estate db in
/// `core2`, and read the transcript back — `core2` never ran the work, so a non-null answer can only
/// come from the durable store (`put_node` on approval), not in-memory actor state.
#[test]
fn work_output_survives_a_daemon_restart() {
    let dir = tmp_dir("durable-output");
    let estate_db = dir.join("estate.db").to_str().unwrap().to_string();

    // ── Process 1: run to completion, capture the per-unit transcripts + their stable unit ids.
    let (unit_ids, before): (Vec<String>, Vec<Option<String>>) = {
        let core1 = Core::spawn_with_engine(
            estate_db.clone(),
            Arc::new(StubDispatcher),
            Arc::new(CountingRunner {
                runs: Arc::new(AtomicUsize::new(0)),
            }),
        );
        let events = core1.subscribe();
        launch(&core1, "durable-run").expect("launch");
        wait_for_completion(&events, "durable-run");

        let detail = core1
            .sessions_detail()
            .unwrap()
            .into_iter()
            .find(|v| v.session.id == "durable-run")
            .expect("run present");
        let ids: Vec<String> = detail.units.iter().map(|u| u.id.clone()).collect();
        let outs: Vec<Option<String>> = ids.iter().map(|id| core1.work_output(id)).collect();
        assert!(
            ids.len() >= 2,
            "two-sentence problem planned {} units",
            ids.len()
        );
        for (id, out) in ids.iter().zip(&outs) {
            let o = out.as_ref().unwrap_or_else(|| {
                panic!("unit {id} is MISSING its work_output in the live process")
            });
            assert!(
                o.contains("stub-output for"),
                "the transcript is the real output: {o:?}"
            );
        }
        (ids, outs)
    }; // core1 dropped here — the "daemon" that ran the work is gone.

    // ── Process 2 (restart): a fresh Core on the SAME estate db, with NO launch. The completed run
    // is terminal, so bootstrap does not re-drive it — any transcript we read is served from the
    // durable store, exactly the path the reported bug returned null on.
    let core2 = Core::spawn_with_engine(
        estate_db,
        Arc::new(StubDispatcher),
        Arc::new(CountingRunner {
            runs: Arc::new(AtomicUsize::new(0)),
        }),
    );
    let after: Vec<Option<String>> = unit_ids.iter().map(|id| core2.work_output(id)).collect();
    assert_eq!(
        after, before,
        "work_output returned the SAME transcript after a restart — transcripts are durable in \
         core.db, not process-lifetime state (#133). A null here is the reported regression."
    );
}

// ── Shared helpers for the seam-finding regression tests below ────────────────────────────────────────

/// Poll the store until `run_id` reaches `want` (or a deadline). Avoids racing the live event stream.
fn wait_status(core: &Core, run_id: &str, want: SessionStatus) -> bool {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if let Ok(views) = core.sessions_detail() {
            if let Some(v) = views.iter().find(|v| v.session.id == run_id) {
                if v.session.status == want {
                    return true;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

/// A deny policy scoped EXACTLY to one phase (governance matches the phase name exactly).
/// A `bug`-SHAPED def for the conditional-gate tests: the same four phase ids in the same order, the
/// same `HumanConfirmIf(VerdictNotPass)` + `verified_evidence` gate on `verify` (registration arms it
/// with the shipped evidence floor, exactly as `bug/verify` carries), but a `fix` phase that is a
/// NON-code Creator. Since wicked-core F-039 the shipped `bug/fix` is a PINNED code phase — its
/// `auto` gate re-derives the worktree diff and a distinct seat judges it — so on an UNBOUND run
/// with a stub seat it fails closed at unit 3 before `verify` ever runs. That is the pinned-phase
/// doctrine ("a repo-less run cannot satisfy a pinned phase"), not the conditional-gate mechanics
/// these tests pin; this def keeps the `unit-4` deny policy and the `units.len() == 4` reads exact.
const CONDGATE_WORKFLOW: &str = r#"{
    "id": "condgate",
    "phases": [
        { "id": "triage", "kind": "recon", "gate": "auto" },
        { "id": "reproduce", "kind": "test", "gate": "auto", "depends_on": ["triage"] },
        { "id": "fix", "kind": "build", "gate": "auto", "role": "creator", "depends_on": ["reproduce"] },
        {
            "id": "verify",
            "kind": "test",
            "gate": {"human_confirm_if": "verdict_not_pass"},
            "verified_evidence": true,
            "validator_pin": "e2e7af1db9e48454",
            "role": "evaluator",
            "depends_on": ["fix"]
        }
    ]
}"#;

fn deny_policy(phase: &str, pattern: &str) -> Policy {
    Policy {
        id: format!("deny-{phase}"),
        kind: "guard".into(),
        applies_to: vec![phase.into()],
        effect: Effect::Deny,
        trigger: Trigger {
            contains: Some(pattern.into()),
        },
        obligations: vec![],
        criteria: String::new(),
        severity: Severity::High,
        rule: "deny".into(),
        retired: false,
    }
}

/// Benign runner — completes every unit `Ok` (never trips a content deny). The interactive-lane control.
struct OkRunner;
impl StepRunner for OkRunner {
    fn run_unit(&self, i: &StepInput) -> StepOutput {
        StepOutput {
            run_id: i.run_id.clone(),
            unit_ix: i.unit_ix,
            attempt: i.attempt,
            output: "ok".into(),
            status: StepStatus::Ok,
            usage: None,
            files: Vec::new(),
            tools: Vec::new(),
            governed: false,
        }
    }
}

/// A runner that BLOCKS on its work in "process 1" (simulating a worker that never completes before a
/// crash) and completes normally in "process 2" (the restart). `released` frees the leaked blocked
/// thread at teardown so the test process exits cleanly.
struct RestartRunner {
    block: bool,
    released: Arc<AtomicBool>,
    runs: Arc<AtomicUsize>,
}
impl StepRunner for RestartRunner {
    fn run_unit(&self, input: &StepInput) -> StepOutput {
        if self.block {
            // Park (never completing) until teardown releases us or a hard cap elapses — the "crash"
            // happens (Core dropped) while we're parked, so the session stays persisted `Executing`.
            let deadline = Instant::now() + Duration::from_secs(30);
            while !self.released.load(Ordering::SeqCst) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(25));
            }
            return StepOutput {
                run_id: input.run_id.clone(),
                unit_ix: input.unit_ix,
                attempt: input.attempt,
                output: "blocked".into(),
                status: StepStatus::Ok,
                usage: None,
                files: Vec::new(),
                tools: Vec::new(),
                governed: false,
            };
        }
        self.runs.fetch_add(1, Ordering::SeqCst);
        StepOutput {
            run_id: input.run_id.clone(),
            unit_ix: input.unit_ix,
            attempt: input.attempt,
            output: format!("recovered for {}", input.unit.description),
            status: StepStatus::Ok,
            usage: None,
            files: Vec::new(),
            tools: Vec::new(),
            governed: false,
        }
    }
}

/// Seam finding #1 (LOST-ON-CRASH): a `task.dispatched`-but-not-`completed` across a crash/restart must
/// NOT be skipped forever. On restart, the actor re-drives the persisted `Executing` session (bumped
/// attempt) so the unit re-runs and the run completes.
#[test]
fn a_dispatched_but_uncompleted_task_re_runs_after_a_restart() {
    let dir = tmp_dir("restart");
    let estate_db = dir.join("estate.db").to_str().unwrap().to_string();
    let bus_db = dir.join("bus.db").to_str().unwrap().to_string();
    let released = Arc::new(AtomicBool::new(false));

    // ── Process 1: launch, dispatch unit 0, block mid-work → the session persists as `Executing`.
    {
        let runs1 = Arc::new(AtomicUsize::new(0));
        let core1 = Core::spawn_with_engine_exec(
            estate_db.clone(),
            Arc::new(StubDispatcher),
            Arc::new(RestartRunner {
                block: true,
                released: released.clone(),
                runs: runs1,
            }),
            bus_db.clone(),
        );
        launch(&core1, "restart-run").expect("launch");
        assert!(
            wait_status(&core1, "restart-run", SessionStatus::Executing),
            "the run reached Executing (a unit was dispatched)"
        );
        let bus = BusDb::open(&bus_db).unwrap();
        // THROWAWAY REPRO (core#1935): how long after Executing does task.dispatched appear?
        let t0 = Instant::now();
        let mut first_read = None;
        let mut polls = 0u32;
        while Instant::now() < t0 + Duration::from_secs(15) {
            let n = bus.poll(TASK_DISPATCHED, 0, 100).unwrap().len();
            first_read.get_or_insert(n);
            polls += 1;
            if n > 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let n = bus.poll(TASK_DISPATCHED, 0, 100).unwrap().len();
        // Conditional so the code after it stays reachable (clippy -D warnings).
        if polls < u32::MAX {
            panic!(
                "REPRO-1935 {}: first read {:?} events; {} event(s) after {} ms ({} polls)",
                sys_label(),
                first_read,
                n,
                t0.elapsed().as_millis(),
                polls
            );
        }
        assert_eq!(
            bus.poll(TASK_COMPLETED, 0, 100).unwrap().len(),
            0,
            "no task.completed — the worker is blocked (the lost-on-crash condition)"
        );
        drop(bus);
        // Simulate a crash: drop the Core (the blocked cli-runner is bounded-joined + detached, #5).
        drop(core1);
    }

    // ── Process 2 (restart): a fresh Core on the SAME dbs. No new launch — the bootstrap re-drive alone
    // must carry the persisted `Executing` run to completion.
    let runs2 = Arc::new(AtomicUsize::new(0));
    let core2 = Core::spawn_with_engine_exec(
        estate_db,
        Arc::new(StubDispatcher),
        Arc::new(RestartRunner {
            block: false,
            released: released.clone(),
            runs: runs2.clone(),
        }),
        bus_db.clone(),
    );
    assert!(
        wait_status(&core2, "restart-run", SessionStatus::Completed),
        "the re-drive carried the lost run to Completed on restart"
    );
    assert!(
        runs2.load(Ordering::SeqCst) >= 1,
        "the cursor unit re-ran on restart"
    );

    // Proof the re-drive minted a FRESH idempotency key: a task.dispatched for unit 0 at a BUMPED
    // attempt (>= 1) exists (a same-keyed re-emit would have deduped and wedged).
    let bus = BusDb::open(&bus_db).unwrap();
    let dispatched = bus.poll(TASK_DISPATCHED, 0, 100).unwrap();
    assert!(
        dispatched.iter().any(|e| {
            e.payload["unit_ix"].as_u64() == Some(0) && e.payload["attempt"].as_u64() == Some(1)
        }),
        "the re-drive emitted a fresh-keyed task.dispatched at attempt 1"
    );
    drop(bus);

    released.store(true, Ordering::SeqCst); // free the leaked process-1 worker thread
}

// ── core#1581: a daemon restart never releases a held power-run pause ────────────────────────────

/// The pause a checkpoint persists (`power_gate::on_checkpoint`) for the given triggers, keyed as
/// it keys them: `(destructive, spread, sensitive new dependency, struggle)`.
fn checkpoint_pause(
    destructive: bool,
    spread: bool,
    dependency: bool,
    struggle: bool,
) -> (wicked_core::power::PowerPause, Vec<String>) {
    let struggled = if struggle {
        vec!["3 failed test runs".to_string()]
    } else {
        Vec::new()
    };
    let rows = wicked_core::power::sudden_complexity(destructive, spread, dependency, &struggled);
    let keys = rows
        .iter()
        .map(|(k, _)| k.to_string())
        .filter(|k| k != "spread")
        .collect();
    let recommended = if spread { "split" } else { "continue_high" };
    let pause = wicked_core::power::PowerPause {
        at: 1_700_000_000_000,
        checkpoint: 3,
        reasons: rows.into_iter().map(|(_, r)| r).collect(),
        recommended: recommended.into(),
    };
    (pause, keys)
}

/// The unit a held run is held on: the second, so the first one's completed task has moved the
/// dispatch consumer's durable cursor and a restart replays the second one's task through the
/// cli-runner's predecessor path (a synthetic failure at the interrupted attempt).
const HELD_IX: u64 = 1;

/// Every `task.dispatched` attempt for the held unit, in bus order.
fn dispatched_held(bus_db: &str) -> Vec<u64> {
    let bus = BusDb::open(bus_db).unwrap();
    bus.poll(TASK_DISPATCHED, 0, 200)
        .unwrap()
        .into_iter()
        .filter(|e| e.payload["unit_ix"].as_u64() == Some(HELD_IX))
        .filter_map(|e| e.payload["attempt"].as_u64())
        .collect()
}

/// A runner that completes every unit at once except the held unit, which it counts on entry and
/// then parks until `open` (or a 30 s cap): a worker mid-unit when the daemon dies, or (after a
/// restart) a released worker held still so the test can count dispatches exactly.
struct HoldRunner {
    open: Arc<AtomicBool>,
    held_runs: Arc<AtomicUsize>,
}
impl StepRunner for HoldRunner {
    fn run_unit(&self, input: &StepInput) -> StepOutput {
        if input.unit_ix as u64 == HELD_IX {
            self.held_runs.fetch_add(1, Ordering::SeqCst);
            let deadline = Instant::now() + Duration::from_secs(30);
            while !self.open.load(Ordering::SeqCst) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(25));
            }
        }
        StepOutput {
            run_id: input.run_id.clone(),
            unit_ix: input.unit_ix,
            attempt: input.attempt,
            output: format!("done: {}", input.unit.description),
            status: StepStatus::Ok,
            usage: None,
            files: Vec::new(),
            tools: Vec::new(),
            governed: false,
        }
    }
}

fn view_of(core: &Core, run_id: &str) -> wicked_core::SessionView {
    core.sessions_detail()
        .unwrap()
        .into_iter()
        .find(|v| v.session.id == run_id)
        .unwrap_or_else(|| panic!("run {run_id} not on the store"))
}

fn power_of(core: &Core, run_id: &str) -> wicked_core::power::PowerState {
    view_of(core, run_id).session.power.expect("a power run")
}

/// Process 2 of a restart, and what the test holds of it.
struct Restarted {
    core: Core,
    /// Entries into the held unit after the restart.
    held_runs: Arc<AtomicUsize>,
    /// The held unit's attempts dispatched before the restart (process 1's one).
    before: Vec<u64>,
    bus_db: String,
    /// Releases both processes' parked workers (on drop too, so a failed assert never leaves one).
    open: Arc<AtomicBool>,
}
impl Drop for Restarted {
    fn drop(&mut self) {
        self.open.store(true, Ordering::SeqCst);
    }
}

/// A power run whose orchestrator is mid-unit when the daemon dies. Process 1 runs unit 0 and
/// parks on the held unit; the store then holds the power record a checkpoint pause leaves
/// (`pause = None`: a run that was not paused); process 2 is a fresh Core over the same dbs.
fn restarted_power_run(
    name: &str,
    run_id: &str,
    pause: Option<(wicked_core::power::PowerPause, Vec<String>)>,
    on: bool,
) -> Restarted {
    use wicked_apps_core::ToNode;
    let dir = tmp_dir(name);
    let estate_db = dir.join("estate.db").to_str().unwrap().to_string();
    let bus_db = dir.join("bus.db").to_str().unwrap().to_string();
    let open = Arc::new(AtomicBool::new(false));
    let before = {
        let core1 = Core::spawn_with_engine_exec(
            estate_db.clone(),
            Arc::new(StubDispatcher),
            Arc::new(HoldRunner {
                open: open.clone(),
                held_runs: Arc::new(AtomicUsize::new(0)),
            }),
            bus_db.clone(),
        );
        launch(&core1, run_id).expect("launch");
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline && dispatched_held(&bus_db).is_empty() {
            std::thread::sleep(Duration::from_millis(20));
        }
        let before = dispatched_held(&bus_db);
        assert_eq!(before.len(), 1, "the held unit dispatched once: {before:?}");
        let v = view_of(&core1, run_id);
        assert_eq!(v.session.status, SessionStatus::Executing);
        assert_eq!(v.session.unit_ix as u64, HELD_IX);
        drop(core1);
        before
    };
    {
        // What the store holds when the daemon dies: the run's power record, with the pause the
        // checkpoint persisted (and the triggers that answering it acknowledges).
        let mut store = open_store(Some(&estate_db)).unwrap();
        let mut s = wicked_core::get_session(&store, run_id).unwrap().unwrap();
        let mut p = wicked_core::power::PowerState::launched(on, 1_700_000_000_000);
        p.checkpoints = 3;
        if let Some((pause, keys)) = pause {
            p.pause = Some(pause);
            p.acknowledged = keys;
        }
        s.power = Some(p);
        wicked_core::put_node(&mut store, s.to_node()).unwrap();
    }
    let held_runs = Arc::new(AtomicUsize::new(0));
    let core = Core::spawn_with_engine_exec(
        estate_db,
        Arc::new(StubDispatcher),
        Arc::new(HoldRunner {
            open: open.clone(),
            held_runs: held_runs.clone(),
        }),
        bus_db.clone(),
    );
    // The actor answers a read only after its boot (redrive and orphan report) is done.
    let _ = view_of(&core, run_id);
    Restarted {
        core,
        held_runs,
        before,
        bus_db,
        open,
    }
}

impl Restarted {
    /// Held: nothing dispatched or run, the run still executing with the same pause (the same
    /// card) on the record, its interrupted attempt superseded (the replayed task's synthetic
    /// failure applied as stale, never failing the run); a resume and a reassign do not get past
    /// it either.
    fn assert_held(&self, run_id: &str, want: &wicked_core::power::PowerPause) {
        std::thread::sleep(Duration::from_millis(1500));
        assert_eq!(
            dispatched_held(&self.bus_db),
            self.before,
            "the restart dispatched the unit of a run held at its pause"
        );
        assert_eq!(self.held_runs.load(Ordering::SeqCst), 0, "no worker ran");
        let v = view_of(&self.core, run_id);
        assert_eq!(v.session.status, SessionStatus::Executing);
        assert!(
            u64::from(v.session.attempt) > self.before[0],
            "the attempt the restart interrupted is superseded"
        );
        let power = v.session.power.expect("a power run");
        assert_eq!(
            power.pause.as_ref(),
            Some(want),
            "the same pause, re-opened"
        );
        assert_eq!(
            self.core.resume_run(run_id).expect("resume"),
            SessionStatus::Executing
        );
        let err = self
            .core
            .reassign_unit(run_id, HELD_IX as u32 + 1, Some("fake-b".into()))
            .expect_err("a reassign of a held run is refused");
        assert!(
            err.to_string().contains("held at power checkpoint"),
            "{err}"
        );
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(
            dispatched_held(&self.bus_db),
            self.before,
            "a resume or a reassign dispatched past the held pause"
        );
        assert_eq!(power_of(&self.core, run_id).pause.as_ref(), Some(want));
    }

    /// Released: the held unit is dispatched exactly once more, under a fresh attempt, and its
    /// worker entered once (it is parked there, so nothing else can re-dispatch it meanwhile).
    fn assert_released_once(&self) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline && self.held_runs.load(Ordering::SeqCst) == 0 {
            std::thread::sleep(Duration::from_millis(20));
        }
        std::thread::sleep(Duration::from_millis(500));
        let attempts = dispatched_held(&self.bus_db);
        assert_eq!(
            attempts.len(),
            self.before.len() + 1,
            "exactly one re-dispatch: {attempts:?}"
        );
        assert!(attempts[1] > attempts[0], "a fresh attempt: {attempts:?}");
        assert_eq!(
            self.held_runs.load(Ordering::SeqCst),
            1,
            "the unit ran once"
        );
    }

    /// Answering the re-opened pause records the answer and releases the run exactly once.
    fn answer(
        &self,
        run_id: &str,
        decision: wicked_core::power::PauseDecision,
    ) -> wicked_core::power::PowerState {
        let st = self
            .core
            .answer_power_pause(run_id, decision, None, "operator:test")
            .expect("answer the re-opened pause");
        assert!(st.pause.is_none(), "the answer closed the pause");
        assert_eq!(st.operator_gates, 1, "the answer is recorded");
        self.assert_released_once();
        let power = power_of(&self.core, run_id);
        assert!(power.pause.is_none());
        assert_eq!(power.operator_gates, 1);
        power
    }
}

/// core#1581: a power run paused at a checkpoint (struggle and a new dependency in a sensitive
/// area) stays paused across a restart; answering "continue at high rigor" resumes it.
#[test]
fn a_restart_keeps_a_power_pause_held_until_it_is_answered() {
    let run = "pause-restart";
    let (pause, keys) = checkpoint_pause(false, false, true, true);
    let r = restarted_power_run("pause_restart", run, Some((pause.clone(), keys)), true);
    r.assert_held(run, &pause);
    let power = r.answer(run, wicked_core::power::PauseDecision::ContinueHigh);
    assert!(
        power.history.iter().any(|c| c.by == "operator:test"),
        "the operator's raise is on the rigor history"
    );
}

/// core#1581: the same for a destructive-step pause, answered "continue".
#[test]
fn a_restart_keeps_a_destructive_step_pause_held_until_it_is_answered() {
    let run = "pause-destructive";
    let (pause, keys) = checkpoint_pause(true, false, false, false);
    assert!(pause.reasons.iter().any(|r| r.contains("destructive")));
    let r = restarted_power_run("pause_destructive", run, Some((pause.clone(), keys)), true);
    r.assert_held(run, &pause);
    r.answer(run, wicked_core::power::PauseDecision::Continue);
}

/// core#1581: a spread pause too, answered "switch to phased" (the switch is on the record).
#[test]
fn a_restart_keeps_a_spread_pause_held_until_it_is_answered() {
    let run = "pause-spread";
    let (pause, keys) = checkpoint_pause(false, true, false, false);
    let r = restarted_power_run("pause_spread", run, Some((pause.clone(), keys)), true);
    r.assert_held(run, &pause);
    let power = r.answer(run, wicked_core::power::PauseDecision::SwitchPhased);
    assert!(!power.on, "switched to phased");
    assert!(power
        .toggles
        .iter()
        .any(|t| !t.on && t.by == "operator:test"));
}

/// core#1581: the power switch turned off on a held run answers its pause: after a restart, with no
/// worker waiting, the switch releases the run (never leaving it executing with nothing behind it).
#[test]
fn a_restart_then_the_switch_to_phased_releases_a_held_pause() {
    let run = "pause-switch";
    let (pause, keys) = checkpoint_pause(false, false, true, false);
    let r = restarted_power_run("pause_switch", run, Some((pause.clone(), keys)), true);
    r.assert_held(run, &pause);
    let st = r
        .core
        .set_power_mode(run, false, "operator:test", Some("phased from here".into()))
        .expect("switch to phased");
    assert!(
        !st.on && st.pause.is_none(),
        "the switch answered the pause"
    );
    r.assert_released_once();
}

/// core#1581, upgrade: an older engine persisted a pause on a run switched to phased mid-turn
/// (`power.on = false` WITH a pause). That pause holds nothing: on restart it is cleared, the run
/// redrives exactly once as before, and no pause card shows.
#[test]
fn an_upgrade_redrives_a_switched_off_run_with_a_stale_pause() {
    let run = "pause-stale-off";
    let (pause, keys) = checkpoint_pause(true, false, false, false);
    let r = restarted_power_run("pause_stale_off", run, Some((pause, keys)), false);
    r.assert_released_once();
    let power = power_of(&r.core, run);
    assert!(!power.on);
    assert!(power.pause.is_none(), "the stale pause card is cleared");
}

/// core#1581: a power run with no pause still redrives on restart as before, exactly once.
#[test]
fn a_restart_still_redrives_a_power_run_that_was_not_paused() {
    let r = restarted_power_run("pause_none", "pause-none", None, true);
    r.assert_released_once();
}

/// Seam finding #2/#3 (WEDGE-ON-RE-DISPATCH): a conditional-gate Approve under exec-mediation must
/// RE-RUN the cursor unit (bumped attempt → fresh idempotency key), not wedge on a deduped dispatch.
#[test]
fn a_conditional_gate_approve_re_runs_the_unit_under_exec_mediation() {
    let dir = tmp_dir("condgate");
    let estate_db = dir.join("estate.db").to_str().unwrap().to_string();
    let bus_db = dir.join("bus.db").to_str().unwrap().to_string();
    // Deny the bug-shaped def's terminal `verify` phase (unit-4) so its verdict is not-pass → the
    // HumanConfirmIf(VerdictNotPass) gate escalates to a human.
    {
        let mut store = open_store(Some(&estate_db)).unwrap();
        register_policy(&mut store, &deny_policy("unit-4", "verify")).unwrap();
    }
    let core = Core::spawn_with_engine_exec(
        estate_db,
        Arc::new(StubDispatcher),
        Arc::new(OkRunner),
        bus_db.clone(),
    );
    core.register_workflow(CONDGATE_WORKFLOW)
        .expect("register the bug-shaped conditional-gate def");
    core.launch_run(LaunchSpec {
        base_ref: None,
        project_id: None,
        problem: "fix the bug".into(),
        clis: vec![cli("fake-a"), cli("fake-b")],
        entity_mode: EntityMode::Shared,
        session_id: "cg".into(),
        human_confirm: HumanConfirm::None,
        auto_deliver: false,
        repo_ref: None,
        workflow: Some("condgate".into()),
        extra_write_roots: Vec::new(),
        extra_read_roots: Vec::new(),
        optional_read_roots: Vec::new(),
        project_graph: None,
        plan: None,
        deliver_step: None,
        exclude_seats: Vec::new(),
        evidence_root: None,
        primary: None,
        reduced_assurance: false,
        stand_in: Default::default(),
        power: false,
        voice: None,
        deliverables: Vec::new(),
        qe_acceptance: Default::default(),
    })
    .expect("launch the bug-shaped workflow");

    assert!(
        wait_status(&core, "cg", SessionStatus::AwaitingHuman),
        "the conditional gate paused for a human on the not-pass verify verdict"
    );

    // Before the approve: the verify unit (unit_ix 3) was dispatched exactly once, at attempt 0.
    let bus = BusDb::open(&bus_db).unwrap();
    let before: Vec<_> = bus
        .poll(TASK_DISPATCHED, 0, 200)
        .unwrap()
        .into_iter()
        .filter(|e| e.payload["unit_ix"].as_u64() == Some(3))
        .collect();
    assert_eq!(before.len(), 1, "verify unit dispatched once so far");
    assert_eq!(
        before[0].payload["attempt"].as_u64(),
        Some(0),
        "at attempt 0"
    );

    // Approve the conditional gate → the unit must RE-RUN (does not wedge).
    core.confirm_gate(
        "cg",
        HumanDecision::Approve {
            amend: None,
            amend_scope: Default::default(),
        },
    )
    .expect("approve the conditional gate");

    // A NEW task.dispatched for the verify unit at attempt 1 appears — the fresh key proves a genuine
    // re-dispatch reached a worker (the wedge would have left NO new dispatch and no progress).
    // 15s to match this file's other bus waits: 5s was close enough to the unloaded time that a
    // concurrent `cargo test --all` could time it out before the re-dispatch landed.
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut saw_attempt1 = false;
    while Instant::now() < deadline {
        let hit = bus
            .poll(TASK_DISPATCHED, 0, 200)
            .unwrap()
            .into_iter()
            .any(|e| {
                e.payload["unit_ix"].as_u64() == Some(3) && e.payload["attempt"].as_u64() == Some(1)
            });
        if hit {
            saw_attempt1 = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        saw_attempt1,
        "confirm_gate Approve re-dispatched the verify unit under a bumped attempt (no wedge)"
    );
    drop(bus);
}

/// Seam finding #4 (PARTIAL-ARM WEDGE): if a consumer cannot initialize, the publisher must NOT arm —
/// the run falls back to the in-process path and completes, rather than publishing with no runner.
#[test]
fn partial_arm_falls_back_to_in_process_when_a_consumer_cannot_init() {
    let dir = tmp_dir("partialarm");
    let estate_db = dir.join("estate.db").to_str().unwrap().to_string();
    // A bus path whose PARENT directory does not exist → BusDb::open fails → the consumers cannot
    // initialize → exec-mediation must NOT arm; the run proceeds on the default in-process path.
    let bad_bus = dir
        .join("missing-subdir")
        .join("bus.db")
        .to_str()
        .unwrap()
        .to_string();

    let runs = Arc::new(AtomicUsize::new(0));
    let core = Core::spawn_with_engine_exec(
        estate_db,
        Arc::new(StubDispatcher),
        Arc::new(CountingRunner { runs: runs.clone() }),
        bad_bus.clone(),
    );
    let events = core.subscribe();
    launch(&core, "fallback-run").expect("launch_run");
    wait_for_completion(&events, "fallback-run");

    // The work ran IN-PROCESS (the counter advanced) despite the unusable bus, and no bus db was ever
    // created (the publisher never armed) — proof the partial-arm wedge is avoided.
    assert!(
        runs.load(Ordering::SeqCst) >= 2,
        "units ran in-process (the fallback path)"
    );
    assert!(
        !std::path::Path::new(&bad_bus).exists(),
        "the bus was never opened → the publisher never armed"
    );
}

// ── LIVE OUTPUT under exec-mediation (parity gap #11 — bridged in-process) ────────────────────────────

/// A runner that STREAMS incremental output through the delta sink (like the real wrapped-CLI runner),
/// so we can prove the cli-runner forwards those chunks to the actor's emit point under exec-mediation.
struct StreamingRunner;
impl StepRunner for StreamingRunner {
    fn run_unit(&self, i: &StepInput) -> StepOutput {
        StepOutput {
            run_id: i.run_id.clone(),
            unit_ix: i.unit_ix,
            attempt: i.attempt,
            output: "streamed".into(),
            status: StepStatus::Ok,
            usage: None,
            files: Vec::new(),
            tools: Vec::new(),
            governed: false,
        }
    }
    fn run_unit_streaming(&self, i: &StepInput, emit: &(dyn Fn(&str) + Send + Sync)) -> StepOutput {
        // Two incremental chunks — exactly what the studio's live pane accumulates per unit.
        emit("chunk-one ");
        emit("chunk-two");
        self.run_unit(i)
    }
}

/// Parity gap #11: under exec-mediation the OFF-actor cli-runner must still stream `CliOutputDelta`
/// to subscribers (the studio's live pane). Before the fix it ran with a no-op sink and no delta ever
/// reached the actor. This drives a STREAMING runner through `spawn_with_engine_exec` and asserts the
/// subscriber observes the runner's incremental chunks as `CoreEvent::CliOutputDelta` — proving the
/// in-process bridge (cli-runner → self_tx → actor emit point) is live under the event path.
#[test]
fn live_output_streams_from_the_cli_runner_under_exec_mediation() {
    let dir = tmp_dir("liveoutput");
    let estate_db = dir.join("estate.db").to_str().unwrap().to_string();
    let bus_db = dir.join("bus.db").to_str().unwrap().to_string();

    let core = Core::spawn_with_engine_exec(
        estate_db,
        Arc::new(StubDispatcher),
        Arc::new(StreamingRunner),
        bus_db.clone(),
    );
    let events = core.subscribe();

    launch(&core, "live-run").expect("launch_run");

    // Collect deltas for this run until it completes (or a deadline). The cli-runner runs OFF the actor
    // and forwards each chunk over the command channel; the actor fans them out as CliOutputDelta.
    let mut chunks: Vec<String> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut completed = false;
    while Instant::now() < deadline && !completed {
        match events.recv_timeout(Duration::from_secs(1)) {
            Ok(CoreEvent::CliOutputDelta { session, chunk, .. }) if session == "live-run" => {
                chunks.push(chunk);
            }
            Ok(CoreEvent::SessionCompleted { session }) if session == "live-run" => {
                completed = true
            }
            Ok(CoreEvent::Error { message, .. }) => panic!("run errored: {message}"),
            _ => continue,
        }
    }
    assert!(completed, "the run completed under exec-mediation");

    // The bus round-trip really happened (task.dispatched exists → the actor did NOT run in-process)…
    let bus = BusDb::open(&bus_db).unwrap();
    assert!(
        !bus.poll(TASK_DISPATCHED, 0, 100).unwrap().is_empty(),
        "the unit was dispatched over the bus (event-mediated, not in-process)"
    );
    // …AND the live-output deltas the OFF-actor cli-runner produced reached the subscriber.
    let joined = chunks.join("");
    assert!(
        joined.contains("chunk-one") && joined.contains("chunk-two"),
        "the cli-runner's streamed chunks surfaced as CliOutputDelta under exec-mediation; got {chunks:?}"
    );
    drop(bus);
}

// ── Test-harness hygiene (core#311) — not a test ─────────────────────────────────────────────
/// Arm the hermetic emit spool BEFORE main (pre-main is single-threaded, so no test thread can
/// race it): engine paths under test fire coarse fire-and-forget `wicked.*` emissions, and with
/// no shared store configured those spool — which must land in a per-process temp file, never in
/// the operator's real `~/.something-wicked/wicked-apps/emit-outbox.ndjson` replay queue. Every
/// binary in this suite carries this block; `harness_hygiene.rs` fails the suite if one is missing.
///
/// SAFETY (`ctor(unsafe)`): runs before `main` on one thread and only sets one process env var
/// via the std API — no allocator setup, no threads, no panics across the FFI boundary.
#[ctor::ctor(unsafe)]
fn arm_hermetic_emit_spool() {
    wicked_apps_core::emit::hermetic_test_spool();
}

fn sys_label() -> &'static str {
    "estate-0.36.0"
}
