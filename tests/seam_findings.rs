//! Integration proofs for the cross-cutting execution-engine seam findings:
//!   #1 — the SYNC driver (`Core::launch` → `run_session`) halts as `Failed` on a deny, never
//!        completing past a rejection (the run-level deny contract the interactive lane enforces).
//!   #3 — a `HumanConfirmIf(VerdictNotPass)` phase ESCALATES a not-pass verdict to a human
//!        (`AwaitingHuman`) instead of the gate being dead.
//!   #9 — the evaluator≠creator second-pass verdict GATES: an evaluator Deny halts the run (and
//!        leaks no approved work_output — finding #2 in the interactive lane).

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use wicked_apps_core::open_store;
use wicked_council::types::{Category, Confidence, Dispatcher, InputMode, Vote};
use wicked_council::{AgenticCli, CouncilTask};
use wicked_governance::{register_policy, Effect, Policy, Severity, Trigger};

use wicked_core::{
    Core, CoreEvent, EntityMode, HumanConfirm, HumanDecision, LaunchSpec, SessionStatus, StepInput,
    StepOutput, StepRunner, StepStatus, UnitStatus,
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

/// Emits a fixed output for every unit (to trip a deny policy via the `work`/`output` context).
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
            usage: None,
            files: Vec::new(),
            governed: false,
        }
    }
}

/// Emits benign output (never trips a content deny) — the interactive-lane control runner.
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
    }
}

fn db_path(name: &str) -> String {
    let dir = std::env::temp_dir().join(format!("wicked-core-seam-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("estate.db").to_str().unwrap().to_string()
}

/// A deny policy scoped EXACTLY to one phase (`applies_to == [phase]`) — governance matches the phase
/// name exactly, so this fires only on that phase's gate context.
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

/// Finding #1: the SYNC driver (`Core::launch` → `run_session`) must NOT mark a session `Completed`
/// when a unit was DENIED — it halts as `Failed` at the denied unit, mirroring the interactive lane.
#[test]
fn sync_launch_halts_as_failed_on_a_governance_deny() {
    let db = db_path("sync-deny");
    // Seed a deny on the first unit's phase BEFORE the actor opens the store. The SYNC path's stub
    // output feeds the unit's DESCRIPTION into the gate context, so a deny keyed on a description token
    // fires. (`unit-1` is the first per-unit execution phase.)
    {
        let mut store = open_store(Some(&db)).unwrap();
        register_policy(&mut store, &deny_policy("unit-1", "DENYME")).unwrap();
    }
    let core = Core::spawn_with_engine(db, Arc::new(StubDispatcher), Arc::new(OkRunner));

    // Two units; unit 1's description trips the deny → the SYNC run must halt as Failed BEFORE unit 2.
    let _ = core.launch(LaunchSpec {
        problem: "please DENYME this task. then a second task".into(),
        clis: vec![cli("a"), cli("b")],
        entity_mode: EntityMode::Shared,
        session_id: "r".into(),
        human_confirm: HumanConfirm::None,
        repo_ref: None,
        workflow: None,
    });

    assert!(
        wait_status(&core, "r", SessionStatus::Failed),
        "a DENIED unit halts the SYNC driver as Failed, never Completed (finding #1)"
    );
    let views = core.sessions_detail().unwrap();
    let v = views.iter().find(|v| v.session.id == "r").unwrap();
    assert_eq!(v.units[0].status, UnitStatus::Rejected, "unit 1 was denied");
    assert_ne!(
        v.units[1].status,
        UnitStatus::Done,
        "unit 2 never ran/completed — the SYNC run stopped at the rejection"
    );
}

/// Finding #3: a `HumanConfirmIf(VerdictNotPass)` phase whose OWN verdict is not-pass ESCALATES to a
/// human (`AwaitingHuman`) instead of the gate being dead. The built-in `bug` workflow's terminal
/// `verify` phase (unit-4) carries exactly this gate; a deny on it drives a not-pass verdict.
#[test]
fn a_conditional_gate_pauses_on_a_not_pass_verdict() {
    let db = db_path("cond-gate");
    {
        let mut store = open_store(Some(&db)).unwrap();
        // Deny ONLY the verify phase (unit-4); its description "verify — ..." carries the token.
        register_policy(&mut store, &deny_policy("unit-4", "verify")).unwrap();
    }
    let core = Core::spawn_with_engine(db, Arc::new(StubDispatcher), Arc::new(OkRunner));
    core.launch_run(LaunchSpec {
        problem: "fix the bug".into(),
        clis: vec![cli("a"), cli("b")],
        entity_mode: EntityMode::Shared,
        session_id: "r".into(),
        human_confirm: HumanConfirm::None,
        repo_ref: None,
        workflow: Some("bug".into()),
    })
    .expect("launch bug workflow");

    assert!(
        wait_status(&core, "r", SessionStatus::AwaitingHuman),
        "a not-pass verdict on the HumanConfirmIf `verify` phase PAUSES for a human (finding #3)"
    );
    let views = core.sessions_detail().unwrap();
    let v = views.iter().find(|v| v.session.id == "r").unwrap();
    assert_eq!(v.units.len(), 4, "bug workflow plans 4 phases");
    assert_eq!(
        v.units[3].status,
        UnitStatus::Rejected,
        "the verify unit's verdict genuinely did not pass"
    );
    assert_ne!(
        v.session.status,
        SessionStatus::Failed,
        "a conditional gate ESCALATES rather than hard-failing"
    );

    // The human declines → the run cancels (Reject path).
    assert_eq!(
        core.confirm_gate("r", HumanDecision::Reject).unwrap(),
        SessionStatus::Cancelled,
        "rejecting the conditional gate cancels the run"
    );
}

/// T-D4b (cockpit adversarial review) — a GENUINE re-dispatch DOES bump the attempt, and only that case.
/// The `HumanConfirmIf(VerdictNotPass)` retry re-runs an ALREADY-RUN (`Rejected`) unit, so
/// `confirm_gate(Approve)` must dispatch it at `attempt=1`: this both mints a fresh `(run,unit,attempt)`
/// idempotency key (wedge-avoidance, seam finding #2/#3) AND is the TRUE rework signal the cockpit Burn
/// panel counts. Contrast `p2_gates::t_d4_pre_unit_gate_approval_is_a_first_dispatch_not_rework`, where a
/// pre-unit gate approval is a FIRST dispatch (attempt 0, never rework). Together they pin the corrected
/// contract: `attempt>0` iff the unit was actually re-run.
#[test]
fn t_d4b_conditional_gate_retry_bumps_attempt() {
    let db = db_path("cond-retry");
    {
        let mut store = open_store(Some(&db)).unwrap();
        // Deny ONLY the verify phase (unit-4) so its own verdict is not-pass → escalates every run.
        register_policy(&mut store, &deny_policy("unit-4", "verify")).unwrap();
    }
    let core = Core::spawn_with_engine(db, Arc::new(StubDispatcher), Arc::new(OkRunner));
    let events = core.subscribe();
    core.launch_run(LaunchSpec {
        problem: "fix the bug".into(),
        clis: vec![cli("a"), cli("b")],
        entity_mode: EntityMode::Shared,
        session_id: "r".into(),
        human_confirm: HumanConfirm::None,
        repo_ref: None,
        workflow: Some("bug".into()),
    })
    .expect("launch bug workflow");
    assert!(
        wait_status(&core, "r", SessionStatus::AwaitingHuman),
        "the verify phase's not-pass verdict escalates to a human (first run, attempt 0)"
    );

    // Approve → the ALREADY-RUN (Rejected) verify unit re-dispatches at a BUMPED attempt (the retry then
    // runs in-flight; we assert on the DISPATCH, not on where the re-run lands).
    core.confirm_gate("r", HumanDecision::Approve { amend: None })
        .expect("approve the retry");

    // Collect UnitDispatched for the verify unit (ord 4): its first run (attempt 0) THEN the retry
    // (attempt 1). Drain until the retry dispatch is observed (or a generous ceiling), tolerating latency.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut verify_attempts: Vec<u32> = Vec::new();
    while Instant::now() < deadline && verify_attempts != vec![0, 1] {
        if let Ok(CoreEvent::UnitDispatched { ord, attempt, .. }) =
            events.recv_timeout(Duration::from_millis(200))
        {
            if ord == 4 {
                verify_attempts.push(attempt);
            }
        }
    }
    assert_eq!(
        verify_attempts,
        vec![0, 1],
        "the verify unit ran first at attempt 0, then the confirm_gate retry re-dispatched it at \
         attempt 1 — a genuine re-dispatch of an already-run unit DOES bump (wedge-key freshness + real rework)"
    );

    let _ = core.cancel_run("r"); // clean up the in-flight run
}

/// Finding #9 (+ #2 in the interactive lane): the evaluator≠creator second-pass verdict GATES — an
/// evaluator Deny halts the run as Failed, and the denied unit leaks NO approved work_output.
#[test]
fn an_evaluator_second_pass_deny_halts_the_run_and_leaks_no_output() {
    let db = db_path("eval-deny");
    {
        let mut store = open_store(Some(&db)).unwrap();
        // Deny the EVALUATOR pass on unit 1 (phase `eval-unit-1`) — the primary `unit-1` gate ALLOWS.
        register_policy(&mut store, &deny_policy("eval-unit-1", "EVALDENY")).unwrap();
    }
    let ran: Ran = Arc::new(Mutex::new(Vec::new()));
    let core = Core::spawn_with_engine(
        db,
        Arc::new(StubDispatcher),
        Arc::new(OutRunner {
            out: "EVALDENY appears in the output".into(),
            ran: ran.clone(),
        }),
    );
    core.launch_run(LaunchSpec {
        problem: "task one. task two".into(),
        clis: vec![cli("a"), cli("b")],
        entity_mode: EntityMode::Shared,
        session_id: "r".into(),
        human_confirm: HumanConfirm::None,
        repo_ref: None,
        workflow: None,
    })
    .expect("launch");

    assert!(
        wait_status(&core, "r", SessionStatus::Failed),
        "an evaluator second-pass Deny halts the run as Failed (finding #9)"
    );
    let views = core.sessions_detail().unwrap();
    let v = views.iter().find(|v| v.session.id == "r").unwrap();
    assert_eq!(v.units[0].status, UnitStatus::Rejected);
    assert!(
        v.units[0]
            .denial_reason
            .as_deref()
            .is_some_and(|r| r.contains("evaluator")),
        "the denial names the evaluator pass, got: {:?}",
        v.units[0].denial_reason
    );
    // Finding #2 in the interactive lane: a validator/evaluator-denied unit leaks NO approved output.
    assert!(
        core.work_output("r:u1").is_none(),
        "an evaluator-denied unit must leave no readable approved work_output"
    );
}
