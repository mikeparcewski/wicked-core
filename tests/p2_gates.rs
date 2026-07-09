//! P2 proving test — interactive human-confirm gates, redirect (amend), reject, and cancel.
//!
//! The engine pauses BEFORE a unit per the run's `HumanConfirm` policy, persists `AwaitingHuman`,
//! and waits for `confirm_gate`. Approve resumes (optionally amending the next unit's instruction —
//! the gate is steering, not just bless-or-bounce); Reject cancels; `cancel_run` terminates.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use wicked_council::types::{Category, Confidence, Dispatcher, InputMode, Vote};
use wicked_council::{AgenticCli, CouncilTask};

use wicked_core::{
    Core, HumanConfirm, HumanDecision, LaunchSpec, SessionStatus, StepInput, StepOutput,
    StepRunner, StepStatus, UnitStatus,
};

/// The (unit_ix, instruction) log a `RecordingRunner` fills in — shared with the test thread.
type RanLog = Arc<Mutex<Vec<(usize, String)>>>;

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

/// Records the (unit_ix, instruction) of each unit it runs, so tests can assert order + amendments.
struct RecordingRunner {
    ran: RanLog,
}
impl StepRunner for RecordingRunner {
    fn run_unit(&self, input: &StepInput) -> StepOutput {
        self.ran
            .lock()
            .unwrap()
            .push((input.unit_ix, input.unit.description.clone()));
        StepOutput {
            run_id: input.run_id.clone(),
            unit_ix: input.unit_ix,
            attempt: input.attempt,
            output: format!("did: {}", input.unit.description),
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

fn spec(session_id: &str, hc: HumanConfirm) -> LaunchSpec {
    LaunchSpec {
        problem: "Do step one. Do step two".into(),
        clis: vec![cli("a"), cli("b")],
        entity_mode: wicked_core::EntityMode::Shared,
        session_id: session_id.into(),
        human_confirm: hc,
        repo_ref: None,
        workflow: None,
    }
}

fn new_core(name: &str) -> (Core, RanLog) {
    let dir = std::env::temp_dir().join(format!("wicked-core-p2-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("estate.db").to_str().unwrap().to_string();
    let ran = Arc::new(Mutex::new(Vec::new()));
    let core = Core::spawn_with_engine(
        db,
        Arc::new(StubDispatcher),
        Arc::new(RecordingRunner { ran: ran.clone() }),
    );
    (core, ran)
}

/// Poll until the run reaches `want` (or time out).
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

fn ran_ix(ran: &RanLog) -> Vec<usize> {
    ran.lock().unwrap().iter().map(|(i, _)| *i).collect()
}

#[test]
fn gate_before_specific_unit_pauses_then_approve_resumes() {
    let (core, ran) = new_core("approve");
    // Pause before unit 2 (ord == 2). Unit 1 runs, then the run pauses.
    core.launch_run(spec("r", HumanConfirm::Before(2)))
        .expect("launch");

    assert!(
        wait_status(&core, "r", SessionStatus::AwaitingHuman),
        "the run must pause at the gate before unit 2"
    );
    assert_eq!(ran_ix(&ran), vec![0], "only unit 1 ran before the gate");

    // Approve (no amendment) → resume → unit 2 runs → complete.
    let status = core
        .confirm_gate("r", HumanDecision::Approve { amend: None })
        .expect("confirm");
    assert_eq!(status, SessionStatus::Executing);
    assert!(
        wait_status(&core, "r", SessionStatus::Completed),
        "the run completes after approval"
    );
    assert_eq!(ran_ix(&ran), vec![0, 1], "both units ran, in order");

    let views = core.sessions_detail().unwrap();
    let v = views.iter().find(|v| v.session.id == "r").unwrap();
    assert!(v.units.iter().all(|u| u.status == UnitStatus::Done));
}

#[test]
fn gate_reject_cancels_the_run() {
    let (core, ran) = new_core("reject");
    // Pause before unit 1 (the very first unit).
    core.launch_run(spec("r", HumanConfirm::Before(1)))
        .expect("launch");
    assert!(
        wait_status(&core, "r", SessionStatus::AwaitingHuman),
        "the run pauses before unit 1"
    );
    assert_eq!(
        ran_ix(&ran),
        Vec::<usize>::new(),
        "nothing ran before the gate"
    );

    let status = core
        .confirm_gate("r", HumanDecision::Reject)
        .expect("reject");
    assert_eq!(status, SessionStatus::Cancelled);
    // A rejected run never runs any unit.
    std::thread::sleep(Duration::from_millis(60));
    assert_eq!(
        ran_ix(&ran),
        Vec::<usize>::new(),
        "a rejected run runs nothing"
    );
}

#[test]
fn gate_all_pauses_each_unit_and_amend_redirects() {
    let (core, ran) = new_core("amend");
    // Pause before EVERY unit.
    core.launch_run(spec("r", HumanConfirm::All))
        .expect("launch");

    // Pause before unit 1 → approve WITH an amendment that redirects the work.
    assert!(wait_status(&core, "r", SessionStatus::AwaitingHuman));
    assert_eq!(ran_ix(&ran), Vec::<usize>::new());
    core.confirm_gate(
        "r",
        HumanDecision::Approve {
            amend: Some("prioritise security".into()),
        },
    )
    .expect("approve unit 1 with amend");

    // Pause before unit 2 → approve plainly → complete.
    assert!(wait_status(&core, "r", SessionStatus::AwaitingHuman));
    assert_eq!(
        ran_ix(&ran),
        vec![0],
        "unit 1 ran (amended) before the unit-2 gate"
    );
    core.confirm_gate("r", HumanDecision::Approve { amend: None })
        .expect("approve unit 2");
    assert!(wait_status(&core, "r", SessionStatus::Completed));
    assert_eq!(ran_ix(&ran), vec![0, 1]);

    // The amendment was injected into unit 1's instruction (the runner saw it).
    let unit1_instruction = &ran.lock().unwrap()[0].1;
    assert!(
        unit1_instruction.contains("prioritise security"),
        "the operator amendment must steer the unit's instruction, got: {unit1_instruction}"
    );
}

#[test]
fn cancel_run_terminates_a_paused_run() {
    let (core, _ran) = new_core("cancel");
    core.launch_run(spec("r", HumanConfirm::Before(1)))
        .expect("launch");
    assert!(wait_status(&core, "r", SessionStatus::AwaitingHuman));

    let status = core.cancel_run("r").expect("cancel");
    assert_eq!(status, SessionStatus::Cancelled);
    // Cancelling again is a safe no-op that still reports Cancelled.
    assert_eq!(core.cancel_run("r").unwrap(), SessionStatus::Cancelled);
    // Confirming a cancelled (non-paused) run errors rather than silently resuming.
    assert!(
        core.confirm_gate("r", HumanDecision::Approve { amend: None })
            .is_err(),
        "confirming a cancelled run must error"
    );
}
