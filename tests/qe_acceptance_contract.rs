//! (QE-IN-APP-WORKFLOWS, operator ruling 2026-10-10) — through the REAL engine
//! (`Core::launch_run`): the workflows that make application changes (`bug` here, the `feature`
//! and `migration` built-in presets) require `qe_acceptance`; the launch records a provisional
//! decision, or the operator's explicit skip (with a reason) or force; a skip or force on a run
//! that does not require QE acceptance, or a skip without a reason, refuses the launch; and the
//! run's QE unit makes the binding decision at its dispatch (`qeAcceptanceDecided`).

use std::sync::Arc;
use std::time::{Duration, Instant};

use wicked_core::assurance::QeOverride;
use wicked_core::{
    Core, CoreEvent, EntityMode, HumanConfirm, HumanDecision, LaunchSpec, StepInput, StepOutput,
    StepRunner, StepStatus,
};
use wicked_council::types::{Category, Confidence, Dispatcher, InputMode, Vote};
use wicked_council::{AgenticCli, CouncilTask};

/// Pre-main: arm the hermetic emit spool (core#311). SAFETY (`ctor(unsafe)`): runs before `main`
/// on one thread and only sets process env vars via the std API.
#[ctor::ctor(unsafe)]
fn arm_hermetic_emit_spool() {
    wicked_apps_core::emit::hermetic_test_spool();
}

fn db_path(name: &str) -> String {
    let dir = std::env::temp_dir().join(format!(
        "wicked-core-qe-contract-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("estate.db").to_str().unwrap().to_string()
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

/// No ballot is dispatched to route a unit (core#590 S5); counts any that is, so a test can prove
/// the dead-seat path convened nothing.
struct NoBallots;
static BALLOTS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
impl Dispatcher for NoBallots {
    fn dispatch(&self, _c: &AgenticCli, _: &CouncilTask) -> Option<Vote> {
        BALLOTS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        None
    }
}

/// Completes every agent unit immediately; a reviewer says `VERDICT: PASS`.
struct OkRunner;
impl StepRunner for OkRunner {
    fn run_unit(&self, i: &StepInput) -> StepOutput {
        StepOutput {
            run_id: i.run_id.clone(),
            unit_ix: i.unit_ix,
            attempt: i.attempt,
            output: "done.\nVERDICT: PASS".into(),
            status: StepStatus::Ok,
            usage: None,
            files: vec![],
            tools: Vec::new(),
            governed: false,
        }
    }
}

fn collect_until(
    events: &std::sync::mpsc::Receiver<CoreEvent>,
    within: Duration,
    until: impl Fn(&CoreEvent) -> bool,
) -> Vec<CoreEvent> {
    let mut out = Vec::new();
    let deadline = Instant::now() + within;
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or(Duration::ZERO);
        if remaining.is_zero() {
            break;
        }
        match events.recv_timeout(remaining.min(Duration::from_millis(200))) {
            Ok(ev) => {
                let done = until(&ev);
                out.push(ev);
                if done {
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
        }
    }
    out
}

const NO_QE: &str = r#"{"id":"qe-none","phases":[
  {"id":"build","kind":"build","gate":"auto"}]}"#;

fn spec(sid: &str, workflow: &str, qe: QeOverride) -> LaunchSpec {
    LaunchSpec {
        base_ref: None,
        project_id: None,
        problem: "Fix the thing.".into(),
        clis: vec![cli("codex"), cli("claude")],
        entity_mode: EntityMode::Shared,
        session_id: sid.into(),
        human_confirm: HumanConfirm::None,
        auto_deliver: false,
        repo_ref: None,
        workflow: Some(workflow.into()),
        extra_write_roots: Vec::new(),
        extra_read_roots: Vec::new(),
        project_graph: None,
        plan: None,
        deliver_step: None,
        exclude_seats: Vec::new(),
        evidence_root: None,
        primary: None,
        reduced_assurance: false,
        qe_acceptance: qe,
        deliverables: Vec::new(),
    }
}

fn engine(name: &str) -> Core {
    let core = Core::spawn_with_engine(db_path(name), Arc::new(NoBallots), Arc::new(OkRunner));
    core.register_workflow(NO_QE).expect("register qe-none");
    core
}

fn started(ev: &std::sync::mpsc::Receiver<CoreEvent>, sid: &str) -> serde_json::Value {
    let evs = collect_until(
        ev,
        Duration::from_secs(20),
        |e| matches!(e, CoreEvent::SessionStarted { session, .. } if session == sid),
    );
    evs.iter()
        .map(|e| e.to_json())
        .find(|j| j["type"] == "sessionStarted" && j["session"] == sid)
        .unwrap_or_else(|| panic!("sessionStarted for {sid}: {evs:?}"))
}

/// `bug`, `feature` and `migration` (built-in presets since M1) and `editor-plugin` (X3, whose test step
/// runs the editor conformance harness) require QE acceptance;
/// the launch's decision is provisional and `required` (a plan has no diff to waive on).
#[test]
fn app_change_workflows_require_qe_acceptance_provisionally() {
    for (i, wf) in ["bug", "feature", "migration", "editor-plugin"]
        .iter()
        .enumerate()
    {
        let sid = format!("qe-req-{i}");
        let core = engine(&format!("req-{i}"));
        let ev = core.subscribe();
        core.launch_run(spec(&sid, wf, QeOverride::Auto))
            .expect("launch");
        let evs = collect_until(
            &ev,
            Duration::from_secs(20),
            |e| matches!(e, CoreEvent::SessionStarted { session, .. } if *session == sid),
        );
        let s = evs
            .iter()
            .map(|e| e.to_json())
            .find(|j| j["type"] == "sessionStarted")
            .unwrap_or_else(|| panic!("{wf}: {evs:?}"));
        let a = &s["assurance"];
        assert_eq!(
            a["required"],
            serde_json::json!(["distinct_evaluator", "judge", "qe_acceptance"]),
            "{wf}: {a}"
        );
        assert_eq!(a["qe"]["status"], "required", "{wf}: {a}");
        assert_eq!(a["qe"]["basis"], "plan", "{wf}: {a}");
        assert_eq!(a["qe"]["threshold"], 20, "{wf}: {a}");
        assert!(
            a["qe"]["reason"].as_str().unwrap().contains("provisional"),
            "{wf}: {a}"
        );
        let _ = core.cancel_run(&sid);
    }
}

/// The operator's explicit skip is persisted with its reason; force requires it whatever the
/// score would say. Neither is ever inferred.
#[test]
fn an_explicit_skip_carries_its_reason_and_force_is_recorded() {
    let core = engine("skip");
    let ev = core.subscribe();
    core.launch_run(spec(
        "qe-skip",
        "bug",
        QeOverride::Skip("docs-only hotfix".into()),
    ))
    .expect("launch");
    let s = started(&ev, "qe-skip");
    let qe = &s["assurance"]["qe"];
    assert_eq!(
        (qe["status"].as_str(), qe["basis"].as_str()),
        (Some("skipped"), Some("operator"))
    );
    assert_eq!(
        qe["reason"],
        "QE acceptance skipped by operator: docs-only hotfix"
    );

    let core = engine("force");
    let ev = core.subscribe();
    core.launch_run(spec("qe-force", "bug", QeOverride::Force))
        .expect("launch");
    let s = started(&ev, "qe-force");
    let qe = &s["assurance"]["qe"];
    assert_eq!(
        (qe["status"].as_str(), qe["basis"].as_str()),
        (Some("required"), Some("operator"))
    );
    assert!(
        qe["reason"]
            .as_str()
            .unwrap()
            .contains("forced by operator"),
        "{qe}"
    );
}

/// codex r1: a PRESET launch (`feature`, a team plan whose first rev is the PA's scope step) with an
/// explicit skip launches and runs on to its plan: the word is judged where the contract is built
/// (the launch stub), never again against the scope step's def.
#[test]
fn a_preset_launch_takes_an_explicit_skip_through_to_its_plan() {
    let core = engine("preset-skip");
    let ev = core.subscribe();
    core.launch_run(spec(
        "qe-preset-skip",
        "feature",
        QeOverride::Skip("spike".into()),
    ))
    .expect("launch");
    let evs = collect_until(
        &ev,
        Duration::from_secs(30),
        |e| matches!(e, CoreEvent::UnitPlanned { session, .. } | CoreEvent::SessionFailed { session, .. } if session == "qe-preset-skip"),
    );
    let s = evs
        .iter()
        .map(|e| e.to_json())
        .find(|j| j["type"] == "sessionStarted")
        .unwrap_or_else(|| panic!("{evs:?}"));
    assert_eq!(s["assurance"]["qe"]["status"], "skipped", "{s}");
    assert!(
        !evs.iter()
            .any(|e| matches!(e, CoreEvent::SessionFailed { .. })),
        "the plan never re-judges the word: {evs:?}"
    );
    assert!(
        evs.iter()
            .any(|e| matches!(e, CoreEvent::UnitPlanned { .. })),
        "the run planned: {evs:?}"
    );
    let _ = core.cancel_run("qe-preset-skip");
}

/// A skip with no reason, and a skip or force on a run that does not require QE acceptance, are
/// refused synchronously — never a silently dropped word.
#[test]
fn a_reasonless_skip_or_a_word_with_nothing_to_apply_to_is_refused() {
    let core = engine("refuse");
    let err = core
        .launch_run(spec("qe-blank", "bug", QeOverride::Skip("   ".into())))
        .expect_err("a reasonless skip is refused");
    assert!(format!("{err:#}").contains("needs a reason"), "{err:#}");
    for over in [QeOverride::Skip("why".into()), QeOverride::Force] {
        let err = core
            .launch_run(spec("qe-none", "qe-none", over.clone()))
            .expect_err("nothing to apply to");
        assert!(
            format!("{err:#}").contains("does not require QE acceptance"),
            "{over:?}: {err:#}"
        );
    }
}

/// A build that changes code (held by a human gate, so no pin is needed) and the code-verifying
/// step after it: the run's QE unit.
const BUILD_VERIFY: &str = r#"{"id":"qe-bind","required_instruments":["distinct_evaluator","judge","qe_acceptance"],"phases":[
  {"id":"build","kind":"build","executes_code":true,"role":"creator","gate":{"human_confirm":{"unconditional":true}}},
  {"id":"verify","kind":"test","verified_evidence":true,"validator_pin":"e2e7af1db9e48454","role":"evaluator","gate":"auto","depends_on":["build"]}]}"#;

/// The binding decision: when the run's QE unit (`verify`, the code-verifying step) dispatches, the
/// engine scores the run's diff; a repo-less run has none, so it is `required` with that reason,
/// published as `qeAcceptanceDecided`, stamped on the unit and carried on the session's contract.
#[test]
fn the_qe_unit_makes_the_binding_decision_at_dispatch() {
    let sid = "qe-bind";
    let core = engine("bind");
    core.register_workflow(BUILD_VERIFY)
        .expect("register qe-bind");
    let ev = core.subscribe();
    // One seat, by explicit opt-in: reduced assurance waives only the seat-bound instruments,
    // never QE acceptance (the decision below is the point).
    let mut one_seat = spec(sid, "qe-bind", QeOverride::Auto);
    one_seat.clis = vec![cli("codex")];
    one_seat.reduced_assurance = true;
    core.launch_run(one_seat).expect("launch");
    let held = collect_until(
        &ev,
        Duration::from_secs(60),
        |e| matches!(e, CoreEvent::AwaitingHuman { session, .. } if session == sid),
    );
    assert!(
        !held
            .iter()
            .any(|e| e.to_json()["type"] == "qeAcceptanceDecided"),
        "nothing is decided before the QE unit dispatches: {held:?}"
    );
    core.confirm_gate(
        sid,
        HumanDecision::Approve {
            amend: None,
            amend_scope: Default::default(),
        },
    )
    .expect("approve the build");
    let evs = collect_until(&ev, Duration::from_secs(60), |e| {
        e.to_json()["type"] == "qeAcceptanceDecided"
    });
    let decided = evs
        .iter()
        .map(|e| e.to_json())
        .find(|j| j["type"] == "qeAcceptanceDecided")
        .unwrap_or_else(|| panic!("the verify unit decided: {evs:?}"));
    assert_eq!(decided["ord"], 2, "verify is unit 2: {decided}");
    assert_eq!(decided["qe"]["status"], "required");
    assert_eq!(decided["qe"]["basis"], "diff");
    assert!(
        decided["qe"]["reason"]
            .as_str()
            .unwrap()
            .contains("no repository diff to score"),
        "{decided}"
    );
    let _ = core.cancel_run(sid);
}
