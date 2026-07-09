//! P2 run-level CONTRACT tests — the behaviors the P2 review flagged as undecided/untested.
//!
//! THE CONTRACT: a run is `Completed` only if every unit was governance-approved and ran without
//! worker failure. A governance `Deny` or a `StepStatus::Failed` worker halts the run as `Failed`
//! (never silently completing past a rejection). A `StepStatus::Cancelled` worker, or `cancel_run`
//! while a worker is in flight, terminates the run as `Cancelled` and never wedges it (`RunBusy`
//! forever).

use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use wicked_apps_core::open_store;
use wicked_council::types::{Category, Confidence, Dispatcher, InputMode, Vote};
use wicked_council::{AgenticCli, CouncilTask};
use wicked_governance::{register_policy, Effect, Policy, Severity, Trigger};

use wicked_core::{
    Core, HumanConfirm, LaunchSpec, SessionStatus, StepInput, StepOutput, StepRunner, StepStatus,
    UnitStatus,
};

struct StubDispatcher;
impl Dispatcher for StubDispatcher {
    fn dispatch(&self, cli: &AgenticCli, _t: &CouncilTask) -> Option<Vote> {
        Some(Vote {
            cli: cli.key.clone(),
            recommendation: "x".into(),
            top_risk: "none".into(),
            change_my_mind: "no".into(),
            disqualifier: None,
            confidence: Confidence::default(),
            provenance: "stub".into(),
        })
    }
}

type Ran = Arc<Mutex<Vec<usize>>>;

/// Emits a fixed output (used to trip a governance deny policy via the output `work` context).
struct OutRunner {
    out: String,
    ran: Ran,
}
impl StepRunner for OutRunner {
    fn run_unit(&self, i: &StepInput) -> StepOutput {
        self.ran.lock().unwrap().push(i.unit_ix);
        StepOutput {
            run_id: i.run_id.clone(),
            unit_ix: i.unit_ix,
            attempt: i.attempt,
            output: self.out.clone(),
            status: StepStatus::Ok,
        }
    }
}

/// Reports a fixed terminal `StepStatus` (Failed or Cancelled) for every unit.
struct StatusRunner {
    status: StepStatus,
    ran: Ran,
}
impl StepRunner for StatusRunner {
    fn run_unit(&self, i: &StepInput) -> StepOutput {
        self.ran.lock().unwrap().push(i.unit_ix);
        StepOutput {
            run_id: i.run_id.clone(),
            unit_ix: i.unit_ix,
            attempt: i.attempt,
            output: "x".into(),
            status: self.status,
        }
    }
}

/// Emits the deny `trigger` ONLY on `deny_ix` (the unit_ix), benign output elsewhere — used to trip
/// a deny on a unit BEYOND the first, to prove deny coverage past unit-64.
struct IxRunner {
    deny_ix: usize,
    trigger: String,
    ran: Ran,
}
impl StepRunner for IxRunner {
    fn run_unit(&self, i: &StepInput) -> StepOutput {
        self.ran.lock().unwrap().push(i.unit_ix);
        let output = if i.unit_ix == self.deny_ix {
            format!("this step would {}", self.trigger)
        } else {
            "benign step output".into()
        };
        StepOutput {
            run_id: i.run_id.clone(),
            unit_ix: i.unit_ix,
            attempt: i.attempt,
            output,
            status: StepStatus::Ok,
        }
    }
}

/// Blocks the worker until released — to hold a unit in flight while the test cancels it.
struct BlockRunner {
    started: Mutex<Sender<()>>,
    release: Mutex<Receiver<()>>,
}
impl StepRunner for BlockRunner {
    fn run_unit(&self, i: &StepInput) -> StepOutput {
        let _ = self.started.lock().unwrap().send(());
        let _ = self.release.lock().unwrap().recv();
        StepOutput {
            run_id: i.run_id.clone(),
            unit_ix: i.unit_ix,
            attempt: i.attempt,
            output: "x".into(),
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

fn spec(session_id: &str, problem: &str) -> LaunchSpec {
    LaunchSpec {
        problem: problem.into(),
        clis: vec![cli("a")],
        entity_mode: wicked_core::EntityMode::Shared,
        session_id: session_id.into(),
        human_confirm: HumanConfirm::None,
        repo_ref: None,
        workflow: None,
    }
}

fn db_path(name: &str) -> String {
    let dir = std::env::temp_dir().join(format!("wicked-core-p2c-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("estate.db").to_str().unwrap().to_string()
}

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

fn wait_status(core: &Core, run_id: &str, want: SessionStatus) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Ok(views) = core.sessions_detail() {
            if let Some(v) = views.iter().find(|v| v.session.id == run_id) {
                if v.session.status == want {
                    return true;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    false
}

#[test]
fn governance_deny_through_the_engine_halts_run_as_failed() {
    let db = db_path("deny");
    // Seed a deny policy on the first unit's phase BEFORE the actor opens the store.
    {
        let mut store = open_store(Some(&db)).unwrap();
        // execute uses phase name "unit-<ord>"; the gate's `work` context = the worker output.
        register_policy(&mut store, &deny_policy("unit-1", "DENYME")).unwrap();
    }
    let ran: Ran = Arc::new(Mutex::new(Vec::new()));
    let core = Core::spawn_with_engine(
        db,
        Arc::new(StubDispatcher),
        Arc::new(OutRunner {
            out: "result contains DENYME token".into(),
            ran: ran.clone(),
        }),
    );

    // Two units; unit 1's output trips the deny → the run must halt as Failed BEFORE unit 2.
    core.launch_run(spec("r", "task one. task two")).unwrap();
    assert!(
        wait_status(&core, "r", SessionStatus::Failed),
        "a governance-denied unit halts the run as Failed (never Completed)"
    );

    let views = core.sessions_detail().unwrap();
    let v = views.iter().find(|v| v.session.id == "r").unwrap();
    assert_eq!(v.units[0].status, UnitStatus::Rejected, "unit 1 was denied");
    assert_ne!(
        v.units[1].status,
        UnitStatus::Done,
        "unit 2 never ran — the run stopped at the rejection"
    );
    assert_eq!(
        *ran.lock().unwrap(),
        vec![0],
        "only unit 1 executed before the deny"
    );

    // PROVENANCE (#4): the denied unit carries a human-readable reason naming the deny.
    let reason = v.units[0]
        .denial_reason
        .as_deref()
        .expect("denied unit records WHY it was rejected");
    assert!(
        reason.contains("DENIED") && reason.contains("deny-unit-1"),
        "denial_reason names the decision + firing policy, got: {reason}"
    );
    // PROVENANCE (#3): every distributed unit carries its routing decision (here: stub council).
    assert!(
        v.units[0].routing.is_some(),
        "the distributed unit records WHY its CLI was assigned (routing)"
    );
}

#[test]
fn a_deny_policy_registered_through_the_engine_api_actually_halts_a_run() {
    // Regression: the governance tab registers via `Core::register_deny_policy`. Governance matches
    // `applies_to` EXACTLY against the per-unit phase (`unit-N`), so the policy must target those —
    // otherwise the tab would silently never deny. This proves it denies for real.
    let ran: Ran = Arc::new(Mutex::new(Vec::new()));
    let core = Core::spawn_with_engine(
        db_path("ui-deny"),
        Arc::new(StubDispatcher),
        Arc::new(OutRunner {
            out: "this step would DEPLOY to prod".into(),
            ran: ran.clone(),
        }),
    );
    core.register_deny_policy("exec", "DEPLOY").unwrap();
    core.launch_run(spec("r", "task one. task two")).unwrap();
    assert!(
        wait_status(&core, "r", SessionStatus::Failed),
        "a deny policy registered via the engine API halts the run (it targets the real unit phases)"
    );
    let views = core.sessions_detail().unwrap();
    let v = views.iter().find(|v| v.session.id == "r").unwrap();
    assert_eq!(v.units[0].status, UnitStatus::Rejected);
    assert!(
        v.units[0]
            .denial_reason
            .as_deref()
            .is_some_and(|r| r.contains("DENIED")),
        "the halted unit explains WHY, got: {:?}",
        v.units[0].denial_reason
    );
}

#[test]
fn deny_policy_fires_on_a_unit_beyond_the_64th() {
    // Regression for the deny-phase span: a UI deny policy must cover units PAST unit-64 (the old
    // hard cap silently let them run ungoverned). 65 units; only the 65th (ix 64) trips the deny.
    let ran: Ran = Arc::new(Mutex::new(Vec::new()));
    let core = Core::spawn_with_engine(
        db_path("deny-65"),
        Arc::new(StubDispatcher),
        Arc::new(IxRunner {
            deny_ix: 64,
            trigger: "BLOCKED".into(),
            ran: ran.clone(),
        }),
    );
    core.register_deny_policy("exec", "BLOCKED").unwrap();
    let problem = (1..=65)
        .map(|i| format!("step {i}"))
        .collect::<Vec<_>>()
        .join(". ");
    core.launch_run(spec("r", &problem)).unwrap();
    assert!(
        wait_status(&core, "r", SessionStatus::Failed),
        "the deny policy fires on the 65th unit — governance covers beyond unit-64"
    );
    let views = core.sessions_detail().unwrap();
    let v = views.iter().find(|v| v.session.id == "r").unwrap();
    assert_eq!(v.units.len(), 65, "the run planned 65 units");
    assert_eq!(
        v.units[63].status,
        UnitStatus::Done,
        "unit 64 was approved (its output was benign)"
    );
    assert_eq!(
        v.units[64].status,
        UnitStatus::Rejected,
        "unit 65 was DENIED — proves deny coverage extends past unit-64"
    );
    assert!(v.units[64]
        .denial_reason
        .as_deref()
        .is_some_and(|r| r.contains("DENIED")));
}

#[test]
fn a_run_exceeding_the_governed_unit_limit_is_rejected_fail_closed() {
    // The deny-phase span is finite; a run with more units than it would run its tail UNGOVERNED, so
    // launch must reject it rather than silently fail open.
    let ran: Ran = Arc::new(Mutex::new(Vec::new()));
    let core = Core::spawn_with_engine(
        db_path("too-many"),
        Arc::new(StubDispatcher),
        Arc::new(OutRunner {
            out: "x".into(),
            ran,
        }),
    );
    let problem = (1..=300)
        .map(|i| format!("step {i}"))
        .collect::<Vec<_>>()
        .join(". ");
    let err = core
        .launch_run(spec("r", &problem))
        .expect_err("an over-limit run is rejected at launch");
    assert!(
        err.to_string().contains("governed"),
        "rejection explains the governed-unit limit, got: {err}"
    );
}

#[test]
fn worker_failure_halts_run_as_failed() {
    let ran: Ran = Arc::new(Mutex::new(Vec::new()));
    let core = Core::spawn_with_engine(
        db_path("fail"),
        Arc::new(StubDispatcher),
        Arc::new(StatusRunner {
            status: StepStatus::Failed,
            ran: ran.clone(),
        }),
    );
    core.launch_run(spec("r", "task one. task two")).unwrap();
    assert!(
        wait_status(&core, "r", SessionStatus::Failed),
        "a worker failure halts the run as Failed"
    );
    let views = core.sessions_detail().unwrap();
    let v = views.iter().find(|v| v.session.id == "r").unwrap();
    assert_eq!(v.units[0].status, UnitStatus::Rejected);
    assert_eq!(*ran.lock().unwrap(), vec![0]);
    // PROVENANCE (#4): a worker failure also records WHY for the UI.
    assert!(
        v.units[0]
            .denial_reason
            .as_deref()
            .is_some_and(|r| r.contains("Worker FAILED")),
        "a worker-failed unit records a denial_reason, got: {:?}",
        v.units[0].denial_reason
    );
}

#[test]
fn worker_cancelled_output_terminates_without_wedging() {
    let ran: Ran = Arc::new(Mutex::new(Vec::new()));
    let core = Core::spawn_with_engine(
        db_path("wcancel"),
        Arc::new(StubDispatcher),
        Arc::new(StatusRunner {
            status: StepStatus::Cancelled,
            ran: ran.clone(),
        }),
    );
    core.launch_run(spec("r", "task one")).unwrap();
    assert!(
        wait_status(&core, "r", SessionStatus::Cancelled),
        "a Cancelled worker output terminates the run as Cancelled"
    );
    // CRITICAL: the run must NOT be wedged in_flight — a subsequent command must not be RunBusy.
    let status = core
        .resume_run("r")
        .expect("resume must not be RunBusy on a terminated run");
    assert_eq!(
        status,
        SessionStatus::Cancelled,
        "the run is cleanly terminal, not stuck"
    );
}

#[test]
fn cancel_while_a_worker_is_in_flight_terminates_and_is_not_wedged() {
    let (started_tx, started_rx) = channel::<()>();
    let (release_tx, release_rx) = channel::<()>();
    let core = Core::spawn_with_engine(
        db_path("inflight-cancel"),
        Arc::new(StubDispatcher),
        Arc::new(BlockRunner {
            started: Mutex::new(started_tx),
            release: Mutex::new(release_rx),
        }),
    );
    core.launch_run(spec("r", "task one. task two")).unwrap();
    started_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("unit 0's worker is in flight (blocked)");

    // Cancel while the worker is still running.
    assert_eq!(core.cancel_run("r").unwrap(), SessionStatus::Cancelled);
    // The run is terminal and NOT wedged: another command succeeds (not RunBusy).
    assert_eq!(core.resume_run("r").unwrap(), SessionStatus::Cancelled);

    // Release the blocked worker; its late result hits the terminal guard and is discarded (no
    // double-apply, no resurrection of the cancelled run).
    let _ = release_tx.send(());
    std::thread::sleep(Duration::from_millis(60));
    let views = core.sessions_detail().unwrap();
    let v = views.iter().find(|v| v.session.id == "r").unwrap();
    assert_eq!(
        v.session.status,
        SessionStatus::Cancelled,
        "a late worker result must not resurrect a cancelled run"
    );
}
