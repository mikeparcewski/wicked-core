//! P1 proving test — the re-entrant, off-thread interactive engine.
//!
//! Proves the three properties the design pulled into P1:
//!  (a) OFF-THREAD (execution): while a unit's worker step is in flight (blocked), the actor still
//!      serves reads — the single-writer thread is no longer frozen for the duration of unit
//!      EXECUTION. (Note: plan+distribute still runs synchronously on the actor; moving the council
//!      dispatch off-thread is P5 work. This test scopes the claim to the execute phase, honestly.)
//!  (b) IN-FLIGHT GUARD: a second mutating command for the same run returns `RunBusy` (no
//!      double-dispatch of non-idempotent work).
//!  (c) RESUME-FROM-CURSOR: a FRESH `Core` (new actor, empty in-flight memory) resumes a run from its
//!      persisted cursor and drives it to completion — running ONLY the remaining unit, never
//!      re-running an already-applied one (instrumented + asserted).

use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use wicked_council::types::{Category, Confidence, Dispatcher, InputMode, Vote};
use wicked_council::{AgenticCli, CouncilTask};

use wicked_core::{
    Core, HumanConfirm, LaunchSpec, SessionStatus, StepInput, StepOutput, StepRunner, StepStatus,
    UnitStatus,
};

/// Deterministic council stub — votes without spawning a subprocess (the real dispatcher hangs under
/// the test harness). Mirrors the in-crate pipeline test's stub.
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

/// A step runner that announces each unit start on `started`, then BLOCKS until it receives one
/// release token on `release`. Lets the test hold a worker mid-flight while it probes the actor.
struct GatedRunner {
    started: Mutex<Sender<usize>>,
    release: Mutex<Receiver<()>>,
}
impl StepRunner for GatedRunner {
    fn run_unit(&self, input: &StepInput) -> StepOutput {
        let _ = self.started.lock().unwrap().send(input.unit_ix);
        let _ = self.release.lock().unwrap().recv(); // block until the test grants one token
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

/// A non-blocking runner (the resuming process) — runs each unit immediately and RECORDS which
/// unit_ix values it executed, so the resume test can prove it ran only the remaining unit.
struct FastRunner {
    ran: Arc<Mutex<Vec<usize>>>,
}
impl StepRunner for FastRunner {
    fn run_unit(&self, input: &StepInput) -> StepOutput {
        self.ran.lock().unwrap().push(input.unit_ix);
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
        acp: None,
        capabilities: None,
    }
}

fn spec(session_id: &str) -> LaunchSpec {
    LaunchSpec {
        problem: "Do step one. Do step two".into(),
        clis: vec![cli("fake-a"), cli("fake-b")],
        entity_mode: wicked_core::EntityMode::Shared,
        session_id: session_id.into(),
        human_confirm: HumanConfirm::None,
        repo_ref: None,
        workflow: None,
    }
}

fn tmp_db(name: &str) -> String {
    let dir = std::env::temp_dir().join(format!("wicked-core-p1-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("estate.db").to_str().unwrap().to_string()
}

#[test]
fn engine_is_off_thread_guards_inflight_and_resumes_from_cursor() {
    let db = tmp_db("reentrant");
    let run_id = "p1run";

    // ── Core A: gated runner so we can hold a worker mid-flight. ──
    let (started_tx, started_rx) = channel::<usize>();
    let (release_tx, release_rx) = channel::<()>();
    let gated = Arc::new(GatedRunner {
        started: Mutex::new(started_tx),
        release: Mutex::new(release_rx),
    });
    let core_a = Core::spawn_with_engine(db.clone(), Arc::new(StubDispatcher), gated);

    let returned = core_a.launch_run(spec(run_id)).expect("launch_run");
    assert_eq!(
        returned, run_id,
        "launch_run returns the run id immediately"
    );

    // Unit 0's worker has started and is now blocked on the release token.
    let first = started_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("unit 0's worker should start");
    assert_eq!(first, 0, "the first unit dispatched is unit_ix 0");

    // ── (a) The actor serves a read WHILE the worker step is in flight (off-thread proof). ──
    let sessions = core_a
        .sessions()
        .expect("actor must serve reads while a step is in flight");
    assert!(
        sessions.iter().any(|s| s == run_id),
        "the launched run is readable mid-flight (actor not frozen)"
    );

    // ── (b) A second mutating command for the in-flight run is rejected as RunBusy. ──
    let busy_resume = core_a.resume_run(run_id);
    assert!(
        busy_resume.is_err()
            && busy_resume
                .as_ref()
                .unwrap_err()
                .to_string()
                .contains("busy"),
        "resume of an in-flight run must return RunBusy, got {busy_resume:?}"
    );
    let busy_launch = core_a.launch_run(spec(run_id));
    assert!(
        busy_launch.is_err()
            && busy_launch
                .as_ref()
                .unwrap_err()
                .to_string()
                .contains("busy"),
        "relaunch of an in-flight run must return RunBusy, got {busy_launch:?}"
    );

    // Release unit 0 → it applies, the cursor advances to 1, unit 1 dispatches and blocks.
    release_tx.send(()).unwrap();
    let second = started_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("unit 1's worker should start after unit 0 applies");
    assert_eq!(
        second, 1,
        "the engine advanced the cursor and dispatched unit 1"
    );

    // Persisted mid-run cursor: unit 0 Done, unit 1 not yet Done, cursor at 1, still Executing.
    let views = core_a.sessions_detail().expect("sessions_detail");
    let view = views
        .iter()
        .find(|v| v.session.id == run_id)
        .expect("run present");
    assert_eq!(view.session.unit_ix, 1, "cursor advanced past unit 0");
    assert_eq!(view.session.status, SessionStatus::Executing);
    assert_eq!(view.units[0].status, UnitStatus::Done, "unit 0 applied");
    assert_ne!(
        view.units[1].status,
        UnitStatus::Done,
        "unit 1 not yet applied"
    );

    // Drop Core A — the last handle dropping fires `Shutdown`, so actor A breaks its loop and
    // RELEASES the store (this is the P1.5 lifecycle fix; before it, the actor leaked forever).
    // Its unit-1 worker is still blocked on `release_rx`; when `release_tx` drops at end of test it
    // unblocks and posts into actor A's now-closed channel — harmless. Even if it somehow reached a
    // live actor, the idempotency guard (unit_ix vs cursor) would mark it Stale, never double-apply.
    drop(core_a);
    // The actor thread is detached (no join handle); give its queued Shutdown a moment to drain so
    // Core B is the sole writer. The idempotency guard makes this race-safe regardless.
    std::thread::sleep(Duration::from_millis(150));

    // ── (c) A FRESH Core resumes from the persisted cursor and finishes the run. ──
    let ran = Arc::new(Mutex::new(Vec::<usize>::new()));
    let core_b = Core::spawn_with_engine(
        db.clone(),
        Arc::new(StubDispatcher),
        Arc::new(FastRunner { ran: ran.clone() }),
    );
    let events = core_b.subscribe();
    let status = core_b.resume_run(run_id).expect("resume_run on fresh Core");
    assert_eq!(
        status,
        SessionStatus::Executing,
        "resume re-dispatches the remaining unit"
    );

    // Wait for completion via the live event stream.
    let mut completed = false;
    while let Ok(ev) = events.recv_timeout(Duration::from_secs(5)) {
        if matches!(ev, wicked_core::CoreEvent::SessionCompleted { .. }) {
            completed = true;
            break;
        }
        if matches!(ev, wicked_core::CoreEvent::Error { .. }) {
            panic!("resume produced an error event: {ev:?}");
        }
    }
    assert!(completed, "the resumed run reached SessionCompleted");

    let views = core_b
        .sessions_detail()
        .expect("sessions_detail after resume");
    let view = views
        .iter()
        .find(|v| v.session.id == run_id)
        .expect("run present");
    assert_eq!(view.session.status, SessionStatus::Completed);
    assert!(
        view.units.iter().all(|u| u.status == UnitStatus::Done),
        "every unit is Done after resume-to-completion"
    );

    // The decisive resume-FROM-CURSOR proof: Core B executed ONLY unit 1 (the remaining unit), never
    // re-ran unit 0. Without the persisted cursor this would be [0, 1] (a full re-run).
    assert_eq!(
        *ran.lock().unwrap(),
        vec![1],
        "resume must run only the remaining unit (from the cursor), not re-run from 0"
    );

    // Cleanup: release Core A's leaked, blocked unit-1 worker so its thread exits (it posts into the
    // already-closed actor-A channel — harmless).
    let _ = release_tx.send(());
}

/// The P1.5 lifecycle fix, proven directly: when the last `Core` handle drops, the actor thread
/// actually EXITS (it does not leak, holding the store open forever). Observable because the exiting
/// actor drops its subscriber `Sender`s, so a subscribed event `Receiver` disconnects.
#[test]
fn actor_shuts_down_when_last_core_drops() {
    let db = tmp_db("shutdown");
    let core = Core::spawn_with_engine(
        db,
        Arc::new(StubDispatcher),
        Arc::new(FastRunner {
            ran: Arc::new(Mutex::new(Vec::new())),
        }),
    );
    let events = core.subscribe();
    core.ping(); // round-trips through the actor → proves Subscribe was processed + actor alive

    drop(core); // last handle gone → ShutdownGuard sends Shutdown → actor breaks → drops subscribers

    let mut disconnected = false;
    for _ in 0..200 {
        match events.recv_timeout(Duration::from_millis(50)) {
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                disconnected = true;
                break;
            }
            // buffered Heartbeat from ping(), or still draining — keep waiting for disconnect.
            _ => continue,
        }
    }
    assert!(
        disconnected,
        "the actor must exit (and drop its event senders) when the last Core handle drops"
    );
}

/// Edge case the main test doesn't cover: resuming an already-Completed run is a safe no-op (returns
/// Completed, re-executes nothing) — the idempotency the interactive engine relies on.
#[test]
fn resume_of_completed_run_is_a_noop() {
    let db = tmp_db("edges");
    let ran = Arc::new(Mutex::new(Vec::<usize>::new()));
    let core = Core::spawn_with_engine(
        db,
        Arc::new(StubDispatcher),
        Arc::new(FastRunner { ran: ran.clone() }),
    );

    // A normal 2-unit run to completion.
    let events = core.subscribe();
    core.launch_run(spec("done-run")).expect("launch");
    let mut completed = false;
    while let Ok(ev) = events.recv_timeout(Duration::from_secs(5)) {
        if matches!(ev, wicked_core::CoreEvent::SessionCompleted { session } if session == "done-run")
        {
            completed = true;
            break;
        }
    }
    assert!(completed, "the run completed");
    assert_eq!(*ran.lock().unwrap(), vec![0, 1], "ran both units in order");

    // Resuming a Completed run is a no-op: returns Completed, dispatches nothing new.
    let before = ran.lock().unwrap().clone();
    let status = core.resume_run("done-run").expect("resume completed run");
    assert_eq!(status, SessionStatus::Completed);
    std::thread::sleep(Duration::from_millis(80));
    assert_eq!(
        *ran.lock().unwrap(),
        before,
        "resuming a Completed run must not re-execute any unit"
    );
}
