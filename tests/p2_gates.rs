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
            usage: None,
            files: Vec::new(),
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

/// T-D4 (DES-STUDIO-COCKPIT-001 §3 B2) — every dispatch emits `UnitDispatched` at the funnel, and the
/// attempt is the unit's REAL dispatch attempt. REWORK-HONESTY (cockpit adversarial review): a PRE-unit
/// human gate approval is the gated unit's FIRST dispatch, so it must carry `attempt=0` — NOT a bump.
/// Bumping there would book the unit's first run as rework (`attempt>0`), reporting false rework (~100%
/// under `human_confirm: all`). The bump belongs only to a genuine re-dispatch of an ALREADY-RUN unit
/// (see `t_d4b`). This drives the funnel (unit 1) + a pre-unit-gate approve (unit 2, first dispatch).
#[test]
fn t_d4_pre_unit_gate_approval_is_a_first_dispatch_not_rework() {
    let (core, _ran) = new_core("dispatched");
    let events = core.subscribe();
    // Pause before unit 2 (ord 2): unit 1 (ord 1) dispatches at attempt 0, then the run pauses.
    core.launch_run(spec("r", wicked_core::HumanConfirm::Before(2)))
        .expect("launch");
    assert!(wait_status(&core, "r", SessionStatus::AwaitingHuman));

    // Approve → the cursor unit (ord 2) has NEVER run, so this is its FIRST dispatch: attempt stays 0.
    core.confirm_gate("r", HumanDecision::Approve { amend: None })
        .expect("confirm");
    assert!(wait_status(&core, "r", SessionStatus::Completed));

    // Drain and keep only UnitDispatched, in order.
    let mut dispatched: Vec<(u32, u32)> = Vec::new();
    while let Ok(ev) = events.recv_timeout(Duration::from_millis(200)) {
        if let wicked_core::CoreEvent::UnitDispatched { ord, attempt, .. } = ev {
            dispatched.push((ord, attempt));
        }
    }
    assert_eq!(
        dispatched,
        vec![(1, 0), (2, 0)],
        "unit 1 at attempt 0 (advance funnel); unit 2 at attempt 0 — a pre-unit gate approval is the \
         unit's FIRST dispatch, never rework. A false attempt>0 here reports phantom rework in the cockpit."
    );
}

/// T-D5 (DES-STUDIO-COCKPIT-001 §3 B1) — the gate emits `GateEvaluated` with the depth, and its fields
/// agree with `combine_verdict`. This run has no pinned validator (structural phase) ⇒ deterministic
/// layer passes and no agent judge runs ⇒ `combine_verdict(true, None) == Approve`, so `combined` (and
/// the back-compat `GateDecided.allow`) must both be `true`. Every `GateEvaluated` is immediately
/// followed by a `GateDecided` carrying the same bool.
#[test]
fn t_d5_gate_evaluated_carries_depth_and_matches_combine_verdict() {
    use wicked_core::{combine_verdict, CoreEvent, GateVerdict};

    let (core, _ran) = new_core("gate-eval");
    let events = core.subscribe();
    core.launch_run(spec("r", wicked_core::HumanConfirm::None))
        .expect("launch");
    assert!(wait_status(&core, "r", SessionStatus::Completed));

    // Walk the stream: each GateEvaluated must be followed by a GateDecided with allow == combined.
    let mut evaluated = 0;
    let mut pending: Option<bool> = None; // the combined of the last GateEvaluated awaiting its GateDecided
    while let Ok(ev) = events.recv_timeout(Duration::from_millis(200)) {
        match ev {
            CoreEvent::GateEvaluated {
                deterministic_pass,
                agent_verdict,
                agent_reasoning,
                combined,
                criterion,
                has_deterministic_floor,
                evaluator_pass,
                denial_reason,
                ..
            } => {
                // No pinned validator on these units ⇒ deterministic floor passes, no agent judge ran.
                assert!(deterministic_pass, "structural phase passes the det floor");
                assert_eq!(
                    agent_verdict, None,
                    "no agent judge ran (no approved validator)"
                );
                assert_eq!(agent_reasoning, None);
                // (M5) An ungated phase (no pinned validator) has NO deterministic floor, so the criterion
                // is honestly `None` — the unit description is never relabeled a "criterion".
                assert!(
                    !has_deterministic_floor,
                    "structural phase has no deterministic floor"
                );
                assert_eq!(
                    criterion, None,
                    "ungated phase carries no criterion (never the description)"
                );
                // (S2) These units all approve, so the evaluator≠creator pass approved and there is no
                // denial reason — the record is consistent.
                assert_eq!(evaluator_pass, Some(true), "the evaluator pass approved");
                assert_eq!(
                    denial_reason, None,
                    "an approved gate carries no denial reason"
                );
                // The emitted `combined` must equal what `combine_verdict` computes from the same depth.
                let expected = matches!(
                    combine_verdict(deterministic_pass, None),
                    GateVerdict::Approve
                );
                assert_eq!(
                    combined, expected,
                    "combined must agree with combine_verdict"
                );
                pending = Some(combined);
                evaluated += 1;
            }
            CoreEvent::GateDecided { allow, .. } => {
                if let Some(combined) = pending.take() {
                    assert_eq!(
                        allow, combined,
                        "GateDecided.allow must carry the same bool as GateEvaluated.combined"
                    );
                }
            }
            _ => {}
        }
    }
    assert!(evaluated >= 1, "at least one GateEvaluated was emitted");
    assert!(
        pending.is_none(),
        "every GateEvaluated was matched by a GateDecided"
    );
}

/// A runner emitting a fixed output — lets a deny policy fire on the gate context's `output`.
struct FixedOutRunner(String);
impl StepRunner for FixedOutRunner {
    fn run_unit(&self, i: &StepInput) -> StepOutput {
        StepOutput {
            run_id: i.run_id.clone(),
            unit_ix: i.unit_ix,
            attempt: i.attempt,
            output: self.0.clone(),
            status: StepStatus::Ok,
            usage: None,
            files: Vec::new(),
        }
    }
}

/// T-D5 (S2 honesty) — when the deterministic floor PASSES, no agent judge runs, and the
/// evaluator≠creator second pass REJECTS, `GateEvaluated` must SURFACE the denying layer rather than
/// read the self-contradictory "deterministic_pass=true, agent_verdict=None, combined=false" with no
/// reason. The denied unit's event must carry `evaluator_pass=Some(false)` and a `denial_reason` naming
/// the evaluator, with `combined=false` (== `GateDecided.allow`).
#[test]
fn t_d5_gate_evaluated_surfaces_the_evaluator_denial_reason() {
    use wicked_apps_core::open_store;
    use wicked_core::CoreEvent;
    use wicked_governance::{register_policy, Effect, Policy, Severity, Trigger};

    let dir = std::env::temp_dir().join("wicked-core-p2-gate-eval-deny");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("estate.db").to_str().unwrap().to_string();
    {
        // Deny ONLY the evaluator pass on unit 1 (phase `eval-unit-1`); the primary `unit-1` gate ALLOWS
        // and the unit has no pinned validator, so the deterministic floor vacuously passes.
        let mut store = open_store(Some(&db)).unwrap();
        register_policy(
            &mut store,
            &Policy {
                id: "deny-eval-unit-1".into(),
                kind: "guard".into(),
                applies_to: vec!["eval-unit-1".into()],
                effect: Effect::Deny,
                trigger: Trigger {
                    contains: Some("EVALDENY".into()),
                },
                obligations: vec![],
                criteria: String::new(),
                severity: Severity::High,
                rule: "deny".into(),
            },
        )
        .unwrap();
    }

    let core = Core::spawn_with_engine(
        db,
        Arc::new(StubDispatcher),
        Arc::new(FixedOutRunner("EVALDENY appears in the output".into())),
    );
    let events = core.subscribe();
    core.launch_run(spec("r", wicked_core::HumanConfirm::None))
        .expect("launch");
    assert!(wait_status(&core, "r", SessionStatus::Failed));

    // Find the GateEvaluated for the denied unit (ord 1) and prove the denying layer is visible.
    let mut saw_evaluator_reject = false;
    while let Ok(ev) = events.recv_timeout(Duration::from_millis(200)) {
        if let CoreEvent::GateEvaluated {
            ord,
            deterministic_pass,
            agent_verdict,
            evaluator_pass,
            denial_reason,
            combined,
            ..
        } = ev
        {
            if ord == 1 {
                assert!(
                    deterministic_pass,
                    "the deterministic floor passed (no pinned validator)"
                );
                assert_eq!(agent_verdict, None, "no agent judge ran");
                assert_eq!(
                    evaluator_pass,
                    Some(false),
                    "the evaluator≠creator second pass REJECTED — surfaced, not hidden"
                );
                assert!(!combined, "the combined gate denied");
                assert!(
                    denial_reason
                        .as_deref()
                        .is_some_and(|r| r.contains("evaluator")),
                    "the denial reason names the evaluator layer (record is not self-contradictory): {denial_reason:?}"
                );
                saw_evaluator_reject = true;
            }
        }
    }
    assert!(
        saw_evaluator_reject,
        "a GateEvaluated for the evaluator-denied unit was emitted with the reason surfaced"
    );
}
