//! core#1496 — a reviewer that ends with a VERDICT gave a verdict; it is never a worker error.
//!
//! Live case (run 4ff99f51, seq 79971-80469): a power run's security review on codex ended
//! `VERDICT: FAIL` with a reproducible finding, the runner marked the turn failed, and its answer
//! said "typecheck, lint, and test all exited 0" — so the worker-error classifier (a substring
//! match on `exited`) took it for a seat failure. The engine failed the step over to pi, whose PASS
//! was the only verdict the gate showed: reviewer shopping, done by the engine.
//!
//! Through the REAL engine (`Core::launch_run` → distribute → the worker seam → the fold):
//!  * ADVERSARIAL: the first reviewer plants a FAIL in a turn shaped like a worker error (status
//!    failed, `exited` in its text). The review is dispatched once, never failed over, and the run
//!    parks at the review's NOT-PASS gate with the FAIL as the verdict;
//!  * control: a turn with NO verdict (a real seat failure) still fails over, and the gate that
//!    reviews the failover seat's PASS names the seat that gave no verdict first.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use wicked_core::{
    Core, CoreEvent, EntityMode, HumanConfirm, LaunchSpec, StepInput, StepOutput, StepRunner,
    StepStatus,
};
use wicked_council::types::{Category, Confidence, Dispatcher, InputMode, Vote};
use wicked_council::{AgenticCli, CouncilTask};

#[ctor::ctor(unsafe)]
fn arm_hermetic_emit_spool() {
    wicked_apps_core::emit::hermetic_test_spool();
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

struct NoBallots;
impl Dispatcher for NoBallots {
    fn dispatch(&self, _c: &AgenticCli, _: &CouncilTask) -> Option<Vote> {
        None
    }
}

/// build → review → wrap-up; the review's own gate (it fires after the review) shows its result.
const DEF: &str = r#"{"id":"c1496-build-review","phases":[
  {"id":"build","kind":"build","role":"creator","gate":"auto"},
  {"id":"review","kind":"review","role":"evaluator","gate":{"human_confirm":{"unconditional":true}},"depends_on":["build"]},
  {"id":"wrapup","kind":"build","gate":"auto","depends_on":["review"]}]}"#;

/// The build answers; the FIRST review turn is `first_review` (status failed), every later review
/// turn passes. Every review dispatch's seat is recorded.
struct Scripted {
    first_review: &'static str,
    reviews: AtomicU32,
    review_seats: Mutex<Vec<String>>,
}

impl StepRunner for Scripted {
    fn run_unit(&self, i: &StepInput) -> StepOutput {
        let phase = i.unit.id.rsplit(':').next().unwrap_or("").to_string();
        let (output, status) = if phase == "review" {
            self.review_seats
                .lock()
                .unwrap()
                .push(i.unit.assigned_cli.clone().unwrap_or_default());
            if self.reviews.fetch_add(1, Ordering::SeqCst) == 0 {
                (self.first_review.to_string(), StepStatus::Failed)
            } else {
                (
                    "ITEM 1: PASS — covered\nVERDICT: PASS".to_string(),
                    StepStatus::Ok,
                )
            }
        } else {
            ("built it\nHANDOFF: done".to_string(), StepStatus::Ok)
        };
        StepOutput {
            run_id: i.run_id.clone(),
            unit_ix: i.unit_ix,
            attempt: i.attempt,
            output,
            status,
            usage: None,
            files: vec![],
            tools: Vec::new(),
            governed: false,
        }
    }
}

fn spec(sid: &str) -> LaunchSpec {
    LaunchSpec {
        scope: None,
        base_ref: None,
        project_id: None,
        problem: "Build the thing.".into(),
        clis: vec![cli("claude"), cli("codex"), cli("pi")],
        entity_mode: EntityMode::Shared,
        session_id: sid.into(),
        human_confirm: HumanConfirm::None,
        auto_deliver: false,
        repo_ref: None,
        workflow: Some("c1496-build-review".into()),
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

fn run(sid: &str, first_review: &'static str) -> (Vec<CoreEvent>, Arc<Scripted>) {
    let runner = Arc::new(Scripted {
        first_review,
        reviews: AtomicU32::new(0),
        review_seats: Mutex::new(Vec::new()),
    });
    let core = Core::spawn_with_engine(":memory:".to_string(), Arc::new(NoBallots), runner.clone());
    core.register_workflow(DEF).expect("register the def");
    let ev = core.subscribe();
    core.launch_run(spec(sid)).expect("launch");
    let mut out = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(15);
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match ev.recv_timeout(left.min(Duration::from_millis(200))) {
            Ok(e) => {
                let done = matches!(&e,
                    CoreEvent::AwaitingHuman { session, .. }
                    | CoreEvent::SessionCompleted { session, .. }
                    | CoreEvent::SessionFailed { session, .. } if session == sid);
                out.push(e);
                if done {
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
        }
    }
    (out, runner)
}

/// The reviewer's FAIL, in a turn the runner marked failed and whose text carries the words the
/// worker-error classifier matches (`exited`, `connection`, `timed out`).
const PLANTED_FAIL: &str = "ITEM 1: FAIL — the lease is not released on reject (src/lease.rs:42); \
    reproduced with the new test.\nVerification: typecheck, lint, and test all exited 0; the \
    connection pool test timed out once and passed on retry.\nVERDICT: FAIL";

#[test]
fn a_planted_fail_in_a_failed_turn_is_the_verdict_and_never_fails_over() {
    let sid = "c1496-planted-fail";
    let (evs, runner) = run(sid, PLANTED_FAIL);
    assert!(
        !evs.iter().any(|e| matches!(e,
            CoreEvent::StepFailed { session, detail, .. }
                if session == sid && detail.contains("failing over to"))),
        "a FAIL is never failed over: {evs:?}"
    );
    assert_eq!(
        runner.review_seats.lock().unwrap().len(),
        1,
        "the review ran once, on one seat"
    );
    let escalated = evs.iter().find_map(|e| match e {
        CoreEvent::GateEscalated {
            session,
            condition,
            verdict_summary,
            ..
        } if session == sid => Some((condition.clone(), verdict_summary.clone())),
        _ => None,
    });
    let (condition, summary) = escalated.unwrap_or_else(|| panic!("the review's gate: {evs:?}"));
    assert_eq!(
        condition, "verdict_not_pass",
        "judged as the reviewer's verdict"
    );
    assert!(summary.contains("FAIL"), "{summary}");
    assert!(
        evs.iter()
            .any(|e| matches!(e, CoreEvent::AwaitingHuman { session, .. } if session == sid)),
        "the run waits at the FAIL's gate, never moves on: {evs:?}"
    );
}

#[test]
fn a_turn_with_no_verdict_still_fails_over_and_the_gate_names_the_failed_seat() {
    let sid = "c1496-no-verdict";
    let (evs, runner) = run(sid, "(cli `codex` exited 1) connection reset by peer");
    assert!(
        evs.iter().any(|e| matches!(e,
            CoreEvent::StepFailed { session, detail, .. }
                if session == sid && detail.contains("failing over to"))),
        "no verdict: the ladder still moves the work: {evs:?}"
    );
    let seats = runner.review_seats.lock().unwrap().clone();
    assert_eq!(seats.len(), 2, "{seats:?}");
    let prompt = evs
        .iter()
        .find_map(|e| match e {
            CoreEvent::AwaitingHuman {
                session, prompt, ..
            } if session == sid => Some(prompt.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("the wrap-up's gate: {evs:?}"));
    assert!(
        prompt.contains(&format!(
            "[failover: this step's earlier attempts on `{}`",
            seats[0]
        )) && prompt.contains(&format!("this result is from `{}`]", seats[1])),
        "the gate shows every attempt: {prompt}"
    );
}

/// (codex r1) A failed turn's PASS is never promoted to a result: it may have been cut off,
/// refused, or have echoed a verdict it never gave. It still fails over, as a seat failure.
#[test]
fn a_failed_turns_pass_is_not_a_result_and_still_fails_over() {
    let sid = "c1496-failed-pass";
    let (evs, runner) = run(sid, "(cli `codex` exited 1) cut off\nVERDICT: PASS");
    assert!(
        evs.iter().any(|e| matches!(e,
            CoreEvent::StepFailed { session, detail, .. }
                if session == sid && detail.contains("failing over to"))),
        "{evs:?}"
    );
    assert_eq!(runner.review_seats.lock().unwrap().len(), 2);
}
