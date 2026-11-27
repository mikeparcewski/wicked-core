//! core#2005: the demo review loop's "Record it again" is a `request_changes` at the TERMINAL gate,
//! the gate that opens after the final unit with the cursor past it. The engine used to refuse it
//! ("no creator phase precedes unit 0": the pre-check read no cursor, so ord 0) and the gate never
//! offered it. Through the REAL engine (`Core`, stub seats, temp store), on the demo presets' own
//! shapes (`src/catalog.rs` `demo_preset` / `demo_human_review_preset`: a gated creator `plan`, a
//! neutral `record`, and for `demo` an evaluator `review` whose gate is the terminal one): the gate
//! offers the send-back, the plan re-runs with the note, its gate is approved, a new take is
//! recorded (and reviewed), the terminal gate opens again and its approve completes the run.
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use wicked_core::{
    Core, CoreEvent, EntityMode, GateExpect, HumanConfirm, HumanDecision, LaunchSpec, PhaseRole,
    StepInput, StepOutput, StepRunner, StepStatus, GATE_CHANGED,
};
use wicked_council::types::{Category, Confidence, Dispatcher, InputMode, Vote};
use wicked_council::{AgenticCli, CouncilTask};

#[ctor::ctor(unsafe)]
fn arm_hermetic_emit_spool() {
    wicked_apps_core::emit::hermetic_test_spool();
}

fn db_path(name: &str) -> String {
    let dir = std::env::temp_dir().join(format!("wicked-core-tsb-{name}-{}", std::process::id()));
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

/// Votes "1" — resolves to the first CLI (council routing).
struct NumericDispatcher;
impl Dispatcher for NumericDispatcher {
    fn dispatch(&self, c: &AgenticCli, _: &CouncilTask) -> Option<Vote> {
        Some(Vote {
            cli: c.key.clone(),
            recommendation: "1".into(),
            top_risk: "none".into(),
            change_my_mind: "no".into(),
            disqualifier: None,
            confidence: Confidence::default(),
            provenance: "numeric".into(),
        })
    }
}

/// Every seat records `(ord, attempt, rework amendment)`; a review always passes, a judge too.
type Handed = Arc<Mutex<Vec<(u32, u32, Option<String>)>>>;
struct Seat {
    handed: Handed,
}
impl StepRunner for Seat {
    fn run_unit(&self, i: &StepInput) -> StepOutput {
        if i.unit.session_id != "validator" {
            self.handed.lock().unwrap().push((
                i.unit.ord,
                i.attempt,
                i.unit.rework_amendment.clone(),
            ));
        }
        let output = if i.unit.session_id == "validator" {
            "PASS\nthe work meets the criterion\nPASS".to_string()
        } else if i.unit.role == PhaseRole::Evaluator {
            "the take shows every chapter\nVERDICT: PASS".to_string()
        } else {
            format!("done (attempt {})", i.attempt)
        };
        StepOutput {
            run_id: i.run_id.clone(),
            unit_ix: i.unit_ix,
            attempt: i.attempt,
            output,
            status: StepStatus::Ok,
            usage: None,
            files: Vec::new(),
            tools: Vec::new(),
            governed: false,
        }
    }
}

/// `demo`: probe is left out (a Tool on the host); plan, record and review keep their roles and gates.
const DEMO: &str = r#"{
    "id": "tsb-demo",
    "phases": [
        { "id": "plan", "kind": "build", "role": "creator", "gate": {"human_confirm": {"unconditional": false}} },
        { "id": "record", "kind": "recon", "role": "neutral", "gate": "auto", "depends_on": ["plan"] },
        { "id": "review", "kind": "review", "role": "evaluator", "gate": {"human_confirm": {"unconditional": false}}, "depends_on": ["record"] }
    ]
}"#;

/// `demo-human-review`: the person reviews the take at the record step's own (terminal) gate.
const DEMO_HUMAN: &str = r#"{
    "id": "tsb-demo-human",
    "phases": [
        { "id": "plan", "kind": "build", "role": "creator", "gate": {"human_confirm": {"unconditional": false}} },
        { "id": "record", "kind": "recon", "role": "neutral", "gate": {"human_confirm": {"unconditional": false}}, "depends_on": ["plan"] }
    ]
}"#;

/// No creator anywhere: the terminal gate has nothing to send back to.
const NO_CREATOR: &str = r#"{
    "id": "tsb-no-creator",
    "phases": [
        { "id": "look", "kind": "recon", "role": "neutral", "gate": {"human_confirm": {"unconditional": false}} }
    ]
}"#;

fn launch(core: &Core, run: &str, workflow: &str) {
    core.launch_run(LaunchSpec {
        scope: None,
        base_ref: None,
        project_id: None,
        problem: "record a demo of the app".into(),
        clis: vec![cli("a"), cli("b")],
        entity_mode: EntityMode::Shared,
        session_id: run.into(),
        human_confirm: HumanConfirm::None,
        auto_deliver: false,
        repo_ref: None,
        workflow: Some(workflow.into()),
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
    })
    .expect("launch");
}

/// Drain until the run pauses or ends (or 20 s); the last event is the stop.
fn next_stop(ev: &std::sync::mpsc::Receiver<CoreEvent>, session: &str) -> Vec<CoreEvent> {
    let mut seen = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if let Ok(e) = ev.recv_timeout(Duration::from_millis(200)) {
            if e.to_json()["session"].as_str() == Some(session) {
                let stop = matches!(
                    e,
                    CoreEvent::AwaitingHuman { .. }
                        | CoreEvent::SessionCompleted { .. }
                        | CoreEvent::SessionFailed { .. }
                        | CoreEvent::RunCancelled { .. }
                );
                seen.push(e);
                if stop {
                    return seen;
                }
            }
        }
    }
    panic!("timed out waiting for a pause or the end; saw: {seen:?}");
}

/// The gate the run paused at: `(ord, reviewing_ord, gate_kind, gate_seq)`.
fn paused_at(seen: &[CoreEvent]) -> (u32, Option<u32>, String, u32) {
    match seen.last() {
        Some(CoreEvent::AwaitingHuman {
            ord,
            reviewing_ord,
            gate_kind,
            gate_seq,
            ..
        }) => (*ord, *reviewing_ord, gate_kind.clone(), *gate_seq),
        other => panic!("the run did not pause at a gate: {other:?} in {seen:?}"),
    }
}

/// The plan's own gate: a def gate after unit 1, before unit 2 runs.
fn assert_plan_gate(seen: &[CoreEvent]) {
    let (ord, reviewing, kind, _) = paused_at(seen);
    assert_eq!(
        (ord, reviewing, kind.as_str()),
        (2, Some(1), "def"),
        "the plan's own gate: {seen:?}"
    );
}

fn approve() -> HumanDecision {
    HumanDecision::Approve {
        amend: None,
        amend_scope: Default::default(),
    }
}

fn offered(core: &Core, run: &str) -> Vec<String> {
    core.gate_actions(run)
        .expect("gate actions")
        .expect("a gate is open")
        .actions
        .into_iter()
        .map(|a| a.action)
        .collect()
}

const NOTE: &str = "chapter 2 timed out: wait for the table to load";

/// Approve the plan, then run to the terminal gate; returns its `(ord, gate_seq)`.
fn to_terminal(
    core: &Core,
    ev: &std::sync::mpsc::Receiver<CoreEvent>,
    run: &str,
    last: u32,
) -> (u32, u32) {
    assert_plan_gate(&next_stop(ev, run));
    core.confirm_gate(run, approve()).expect("approve the plan");
    let (ord, _, kind, seq) = paused_at(&next_stop(ev, run));
    assert_eq!(
        (ord, kind.as_str()),
        (last, "terminal"),
        "the review gate after the last unit"
    );
    (ord, seq)
}

/// Send back at the terminal gate, then the full retake: plan again (with the note), its gate,
/// record (and review) again, the terminal gate again, approve, completed.
fn retake(
    core: &Core,
    ev: &std::sync::mpsc::Receiver<CoreEvent>,
    run: &str,
    last: u32,
    first_seq: u32,
    handed: &Handed,
) {
    let actions = offered(core, run);
    assert!(
        actions.iter().any(|a| a == "request_changes"),
        "the terminal gate offers the send-back (studio's Record it again): {actions:?}"
    );
    core.confirm_gate(
        run,
        HumanDecision::RequestChanges {
            note: Some(NOTE.into()),
        },
    )
    .expect("Record it again is taken at the terminal gate");
    let back = next_stop(ev, run);
    assert!(
        back.iter().any(
            |e| matches!(e, CoreEvent::UnitReworkAmended { ord: 1, scope, amendment, .. }
            if scope == "request_changes" && amendment.ends_with(NOTE))
        ),
        "the send-back goes to the plan with the note: {back:?}"
    );
    // The re-run plan waits for its approval before a new take is recorded.
    assert_plan_gate(&back);
    core.confirm_gate(run, approve())
        .expect("approve the new plan");
    let again = next_stop(ev, run);
    let (ord, _, kind, seq) = paused_at(&again);
    assert_eq!((ord, kind.as_str()), (last, "terminal"), "{again:?}");
    // IDENTITY REUSE: the terminal gate re-opened at the same ord is another opening; an answer
    // sent for the first one is refused, and the gate stays open.
    assert_ne!(seq, first_seq, "a new opening: {again:?}");
    let stale = core
        .confirm_gate_expecting(
            run,
            approve(),
            None,
            Some(GateExpect {
                ord: Some(last),
                plan_rev: None,
                gate_seq: Some(first_seq),
            }),
        )
        .expect_err("an answer for the first terminal opening");
    assert!(format!("{stale:#}").contains(GATE_CHANGED), "{stale:#}");
    core.confirm_gate(run, approve())
        .expect("accept the new take");
    let end = next_stop(ev, run);
    assert!(
        matches!(end.last(), Some(CoreEvent::SessionCompleted { .. })),
        "{end:?}"
    );
    // The seats' own record: the plan ran twice, the re-run with the note; every later unit twice.
    let handed = handed.lock().unwrap().clone();
    let plans: Vec<_> = handed.iter().filter(|(o, _, _)| *o == 1).collect();
    assert_eq!(plans.len(), 2, "{handed:?}");
    assert!(plans[0].2.is_none(), "{handed:?}");
    assert!(
        plans[1].2.as_deref().is_some_and(|a| a.ends_with(NOTE)),
        "the second plan is handed the note: {handed:?}"
    );
    for o in 2..=last {
        assert_eq!(
            handed.iter().filter(|(u, _, _)| *u == o).count(),
            2,
            "unit {o} ran again from the new plan: {handed:?}"
        );
    }
}

#[test]
fn demo_record_it_again_at_the_terminal_review_gate_sends_the_run_back_to_the_plan() {
    let handed: Handed = Arc::new(Mutex::new(Vec::new()));
    let core = Core::spawn_with_engine(
        db_path("demo"),
        Arc::new(NumericDispatcher),
        Arc::new(Seat {
            handed: Arc::clone(&handed),
        }),
    );
    core.register_workflow(DEMO).expect("register");
    let ev = core.subscribe();
    let run = "tsb-demo-run";
    launch(&core, run, "tsb-demo");
    let (_, seq) = to_terminal(&core, &ev, run, 3);
    retake(&core, &ev, run, 3, seq, &handed);
}

#[test]
fn demo_human_review_record_it_again_at_the_record_gate_sends_the_run_back_to_the_plan() {
    let handed: Handed = Arc::new(Mutex::new(Vec::new()));
    let core = Core::spawn_with_engine(
        db_path("human"),
        Arc::new(NumericDispatcher),
        Arc::new(Seat {
            handed: Arc::clone(&handed),
        }),
    );
    core.register_workflow(DEMO_HUMAN).expect("register");
    let ev = core.subscribe();
    let run = "tsb-human-run";
    launch(&core, run, "tsb-demo-human");
    let (_, seq) = to_terminal(&core, &ev, run, 2);
    retake(&core, &ev, run, 2, seq, &handed);
}

/// PARTIAL FAILURE: with no creator to send back to, the terminal gate neither offers the send-back
/// nor takes it, and it refuses BEFORE the row resolves, naming the reviewed unit (never "unit 0").
#[test]
fn a_terminal_gate_with_no_creator_refuses_the_send_back_before_resolving_and_names_its_unit() {
    let handed: Handed = Arc::new(Mutex::new(Vec::new()));
    let core = Core::spawn_with_engine(
        db_path("none"),
        Arc::new(NumericDispatcher),
        Arc::new(Seat { handed }),
    );
    core.register_workflow(NO_CREATOR).expect("register");
    let ev = core.subscribe();
    let run = "tsb-none-run";
    launch(&core, run, "tsb-no-creator");
    let (ord, _, kind, _) = paused_at(&next_stop(&ev, run));
    assert_eq!((ord, kind.as_str()), (1, "terminal"));
    assert!(!offered(&core, run).iter().any(|a| a == "request_changes"));
    let err = core
        .confirm_gate(
            run,
            HumanDecision::RequestChanges {
                note: Some(NOTE.into()),
            },
        )
        .expect_err("nothing to send back to");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("unit 1") && msg.contains("approve to complete") && !msg.contains("unit 0"),
        "{msg}"
    );
    // The gate is still open and its approve completes the run.
    assert!(offered(&core, run).iter().any(|a| a == "approve"));
    core.confirm_gate(run, approve())
        .expect("approve completes");
    assert!(matches!(
        next_stop(&ev, run).last(),
        Some(CoreEvent::SessionCompleted { .. })
    ));
}
