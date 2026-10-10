//! core#850 (codex audit EX-01) — through the REAL engine (`Core::launch_run` → plan →
//! distribute): a run whose assurance contract requires a distinct evaluator refuses to grade on
//! its creator's seat; the explicit `reducedAssurance` opt-in keeps the disclosed fallback and
//! says so on `sessionStarted` and every gate; a workflow's `required_instruments` is the
//! contract, and an unknown instrument refuses the def.

use std::sync::Arc;
use std::time::{Duration, Instant};

use wicked_core::{
    Core, CoreEvent, EntityMode, HumanConfirm, LaunchSpec, StepInput, StepOutput, StepRunner,
    StepStatus,
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
        "wicked-core-assurance-{name}-{}",
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

/// The 2-phase build→review def: the review must run on a seat that did not build.
const BUILD_REVIEW: &str = r#"{"id":"assurance-build-review","phases":[
  {"id":"build","kind":"build","gate":"auto"},
  {"id":"review","kind":"review","gate":"auto","depends_on":["build"]}]}"#;

/// The same def declaring a contract that does not require a distinct evaluator.
const JUDGE_ONLY: &str = r#"{"id":"assurance-judge-only","required_instruments":["judge"],"phases":[
  {"id":"build","kind":"build","gate":"auto"},
  {"id":"review","kind":"review","gate":"auto","depends_on":["build"]}]}"#;

fn spec(sid: &str, workflow: &str, reduced: bool) -> LaunchSpec {
    LaunchSpec {
        base_ref: None,
        project_id: None,
        problem: "Build the thing.".into(),
        clis: vec![cli("codex")],
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
        reduced_assurance: reduced,
    }
}

fn engine(name: &str) -> Core {
    let core = Core::spawn_with_engine(db_path(name), Arc::new(NoBallots), Arc::new(OkRunner));
    core.register_workflow(BUILD_REVIEW)
        .expect("register build→review");
    core.register_workflow(JUDGE_ONLY)
        .expect("register judge-only");
    core
}

fn json_of(evs: &[CoreEvent], ty: &str) -> Vec<serde_json::Value> {
    evs.iter()
        .map(|e| e.to_json())
        .filter(|j| j["type"] == ty)
        .collect()
}

fn ends(sid: &str) -> impl Fn(&CoreEvent) -> bool + '_ {
    move |e| {
        matches!(e,
            CoreEvent::AwaitingHuman { session, .. }
            | CoreEvent::SessionCompleted { session }
            | CoreEvent::SessionFailed { session, .. } if session == sid)
    }
}

/// EX-01 reproduced and fixed at the entry point: a one-seat build→review run, launched without
/// the opt-in, never reviews on the builder's seat — distribution refuses (`NoEligibleSeat`, the
/// dead-seat gate: sign a seat in or relaunch reduced), naming the requirement. Before core#850
/// this run completed with the review on `codex`, disclosed only as `creator_seat`.
#[test]
fn ex01_a_full_assurance_one_seat_run_never_reviews_on_its_creators_seat() {
    let sid = "assure-full";
    let core = engine("full");
    let ev = core.subscribe();
    core.launch_run(spec(sid, "assurance-build-review", false))
        .expect("launch");
    let evs = collect_until(&ev, Duration::from_secs(30), ends(sid));
    let started = json_of(&evs, "sessionStarted");
    assert_eq!(started[0]["assurance"]["mode"], "full", "{started:?}");
    assert_eq!(
        started[0]["assurance"]["required"],
        serde_json::json!(["distinct_evaluator", "judge"])
    );
    let escalated = json_of(&evs, "gateEscalated");
    let summary = escalated
        .first()
        .and_then(|g| g["verdictSummary"].as_str())
        .unwrap_or_else(|| panic!("the refusal parks at a gate: {evs:?}"));
    assert!(
        summary.contains("distinct_evaluator") && summary.contains("reduced assurance"),
        "{summary}"
    );
    assert!(
        json_of(&evs, "gateEvaluated").is_empty(),
        "nothing was graded on the creator's seat: {evs:?}"
    );
}

/// The explicit opt-in: the same run, `reducedAssurance: true`, reviews on the creator's seat —
/// disclosed on `sessionStarted`, `unitDistributed.distinctnessFallback` and the review gate's
/// receipt (`distinct_evaluator` skipped, `reduced_assurance`).
#[test]
fn ex01_a_reduced_run_keeps_the_fallback_and_discloses_it_on_every_receipt() {
    let sid = "assure-reduced";
    let core = engine("reduced");
    let ev = core.subscribe();
    core.launch_run(spec(sid, "assurance-build-review", true))
        .expect("launch");
    let evs = collect_until(&ev, Duration::from_secs(60), ends(sid));
    assert!(
        evs.iter()
            .any(|e| matches!(e, CoreEvent::SessionCompleted { session } if session == sid)),
        "the reduced run completes: {evs:?}"
    );
    assert_eq!(
        json_of(&evs, "sessionStarted")[0]["assurance"]["mode"],
        "reduced"
    );
    let review = json_of(&evs, "unitDistributed")
        .into_iter()
        .find(|d| d["ord"] == 2)
        .expect("the review was seated");
    assert_eq!(review["distinctnessFallback"], "creator_seat");
    let gates = json_of(&evs, "gateEvaluated");
    let review_gate = gates.iter().find(|g| g["ord"] == 2).expect("review gate");
    let r = &review_gate["assurance"];
    assert_eq!(r["mode"], "reduced", "{r}");
    assert_eq!(r["creator"], "codex");
    assert_eq!(r["evaluator"], "codex");
    let skip = r["skipped"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["instrument"] == "distinct_evaluator")
        .unwrap_or_else(|| panic!("distinct_evaluator skipped: {r}"));
    assert_eq!(skip["reason"], "reduced_assurance");
    assert!(gates.iter().all(|g| g["assurance"]["attempt"] == 0));
}

/// A workflow's `required_instruments` IS the contract: one that does not require a distinct
/// evaluator runs a one-seat review without the opt-in (still `full`, still disclosed); an
/// unknown instrument refuses the def at registration.
#[test]
fn the_workflow_declares_its_required_instruments() {
    let sid = "assure-declared";
    let core = engine("declared");
    let ev = core.subscribe();
    core.launch_run(spec(sid, "assurance-judge-only", false))
        .expect("launch");
    let evs = collect_until(&ev, Duration::from_secs(60), ends(sid));
    let started = json_of(&evs, "sessionStarted");
    assert_eq!(
        started[0]["assurance"]["required"],
        serde_json::json!(["judge"])
    );
    assert_eq!(started[0]["assurance"]["mode"], "full");
    assert!(
        evs.iter()
            .any(|e| matches!(e, CoreEvent::SessionCompleted { session } if session == sid)),
        "{evs:?}"
    );
    let err = core
        .register_workflow(
            r#"{"id":"assurance-bad","required_instruments":["vibes"],"phases":[
              {"id":"build","kind":"build","gate":"auto"}]}"#,
        )
        .expect_err("an unknown instrument refuses the def");
    assert!(
        format!("{err:#}").contains("unknown instrument `vibes`"),
        "{err:#}"
    );
}
