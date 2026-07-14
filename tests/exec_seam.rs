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
        core1.launch_run(spec("restart-run")).expect("launch");
        assert!(
            wait_status(&core1, "restart-run", SessionStatus::Executing),
            "the run reached Executing (a unit was dispatched)"
        );
        let bus = BusDb::open(&bus_db).unwrap();
        assert_eq!(
            bus.poll(TASK_DISPATCHED, 0, 100).unwrap().len(),
            1,
            "exactly one task.dispatched (unit 0)"
        );
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

/// Seam finding #2/#3 (WEDGE-ON-RE-DISPATCH): a conditional-gate Approve under exec-mediation must
/// RE-RUN the cursor unit (bumped attempt → fresh idempotency key), not wedge on a deduped dispatch.
#[test]
fn a_conditional_gate_approve_re_runs_the_unit_under_exec_mediation() {
    let dir = tmp_dir("condgate");
    let estate_db = dir.join("estate.db").to_str().unwrap().to_string();
    let bus_db = dir.join("bus.db").to_str().unwrap().to_string();
    // Deny the bug workflow's terminal `verify` phase (unit-4) so its verdict is not-pass → the
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
    core.launch_run(LaunchSpec {
        problem: "fix the bug".into(),
        clis: vec![cli("fake-a"), cli("fake-b")],
        entity_mode: EntityMode::Shared,
        session_id: "cg".into(),
        human_confirm: HumanConfirm::None,
        repo_ref: None,
        workflow: Some("bug".into()),
    })
    .expect("launch bug workflow");

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
    core.confirm_gate("cg", HumanDecision::Approve { amend: None })
        .expect("approve the conditional gate");

    // A NEW task.dispatched for the verify unit at attempt 1 appears — the fresh key proves a genuine
    // re-dispatch reached a worker (the wedge would have left NO new dispatch and no progress).
    let deadline = Instant::now() + Duration::from_secs(5);
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
    core.launch_run(spec("fallback-run")).expect("launch_run");
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

    core.launch_run(spec("live-run")).expect("launch_run");

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
