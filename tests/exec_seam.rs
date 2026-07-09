//! Law 1 EXECUTION-MEDIATION SEAM — real round-trip proof (DES-EXEC-001 §2.3, §5).
//!
//! Proves the honest gap is closed: with exec-mediation ON, a unit's work flows
//! actor → (bus `wicked.task.dispatched`) → cli-runner → (bus `wicked.task.completed`) → actor → gate,
//! producing the SAME outcome as the in-process path — and the actor did NOT run the work in-process
//! (the cli-runner did). Uses the stub runner (no real CLI). Env-free entry (`spawn_with_engine_exec`)
//! so it cannot race other tests on process env.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use wicked_council::types::{Category, Confidence, Dispatcher, InputMode, Vote};
use wicked_council::{AgenticCli, CouncilTask};

use wicked_core::{
    BusDb, Core, CoreEvent, EntityMode, HumanConfirm, LaunchSpec, StepInput, StepOutput,
    StepRunner, StepStatus, TASK_COMPLETED, TASK_DISPATCHED,
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

fn spec(session_id: &str) -> LaunchSpec {
    LaunchSpec {
        // Two sentences → the free-text planner decomposes into 2 units.
        problem: "Do step one. Do step two".into(),
        clis: vec![cli("fake-a"), cli("fake-b")],
        entity_mode: EntityMode::Shared,
        session_id: session_id.into(),
        human_confirm: HumanConfirm::None,
        repo_ref: None,
        workflow: None,
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

    let run_id = core.launch_run(spec("exec-run")).expect("launch_run");
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
    core2.launch_run(spec("inproc-run")).expect("launch_run");
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
