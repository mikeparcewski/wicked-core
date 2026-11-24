//! DES-STANDIN-001 §3.4 T-R6 (core#1863): what reaches a stand-in, through the REAL dispatch
//! (`dispatch_unit` → `StepInput.prior_outputs`, what the runner renders into the prompt) on a
//! seeded store. Each route carries a marker; the control is the same run with the review on a
//! real seat, which is handed the marker by the same route, so every check can fail.

use super::*;
use crate::domain::{HumanConfirm, UnitStatus, WorkUnit};
use crate::scope::EntityMode;
use std::sync::mpsc::channel;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use wicked_apps_core::open_store;

const BUILT: &str = "BUILT-WORK-MARKER-4b1d";
const TEAM: &str = "TEAM-LEDGER-MARKER-90ce";
const SAID: &str = "OPERATOR-MESSAGE-MARKER-2f7a";
const VOICE: &str = "VOICE-BLOCK-MARKER-c3e8";

fn session(run_id: &str) -> AgentSession {
    AgentSession {
        intent_amendments: Vec::new(),
        id: run_id.into(),
        workflow_id: format!("wf-{run_id}"),
        problem: "p".into(),
        entity_mode: EntityMode::Shared,
        collection_scope: None,
        clis: vec!["codex".into(), "claude".into()],
        status: SessionStatus::Executing,
        human_confirm: HumanConfirm::None,
        auto_deliver: false,
        unit_ix: 1,
        attempt: 0,
        workdir: None,
        repo_ref: None,
        extra_write_roots: Vec::new(),
        extra_read_roots: Vec::new(),
        project_graph: None,
        project_id: None,
        archived_at: None,
        archive_note: None,
        verified_tree: None,
        run_branch: None,
        base_commit: None,
        finished_at: None,
        seat_failures: Vec::new(),
        benched_seats: Vec::new(),
        team: None,
        team_plan: None,
        exclude_seats: Vec::new(),
        evidence_root: None,
        assurance: Default::default(),
        power: None,
        run_context: None,
    }
}

/// One dispatch as the runner saw it: the unit's ord, its context labels and texts.
type Handed = (u32, Vec<String>, Vec<String>);

/// Every dispatched unit's handed context, as the runner sees it.
#[derive(Default)]
struct Recorder(Mutex<Vec<Handed>>);
impl StepRunner for Recorder {
    fn run_unit(&self, i: &StepInput) -> crate::workflow::StepOutput {
        self.0.lock().unwrap().push((
            i.unit.ord,
            i.prior_outputs.iter().map(|p| p.label.clone()).collect(),
            i.prior_outputs.iter().map(|p| p.output.clone()).collect(),
        ));
        crate::workflow::StepOutput {
            run_id: i.run_id.clone(),
            unit_ix: i.unit_ix,
            attempt: i.attempt,
            output: "VERDICT: PASS".into(),
            status: crate::workflow::StepStatus::Ok,
            usage: None,
            files: vec![],
            tools: Vec::new(),
            governed: false,
        }
    }
}

/// A build (ord 1, done on `codex`, its output and its team's ledger stored) then a review (ord 2)
/// seated on `reviewer`, with an operator message queued for the review: dispatch the review and
/// return the labels and texts it was handed.
fn review_context(run_id: &str, reviewer: &str) -> (Vec<String>, Vec<String>) {
    let mut store = open_store(Some(":memory:")).unwrap();
    let mut build = WorkUnit::pending(format!("{run_id}:build"), run_id, 1, "build");
    build.role = crate::workflow::PhaseRole::Creator;
    build.status = UnitStatus::Done;
    build.last_attempt = Some(0);
    build.assigned_cli = Some("codex".into());
    let mut snap =
        crate::domain::UnitTeamSnapshot::stamped(crate::team::events::Transport::Bus, None, None);
    snap.ledger = Some(crate::team::TeamLedger::new(
        crate::team::FinalPass::Completed,
        Vec::new(),
        Vec::new(),
        Default::default(),
    ));
    snap.ledger_ref = Some(TEAM.into());
    build.team = Some(snap);
    put_node(&mut store, build.to_node()).unwrap();
    let mut out = wicked_apps_core::Node::new(
        wicked_apps_core::synthetic_symbol(crate::execute::WORK_OUTPUT, &build.id),
        wicked_apps_core::NodeKind::Other(crate::execute::WORK_OUTPUT.to_string()),
        build.id.clone(),
        wicked_apps_core::Language::new(wicked_apps_core::SYMBOL_SCHEME),
        wicked_apps_core::Location::new("work_output/x", wicked_apps_core::Span::ZERO),
    );
    out.metadata.insert(
        "output".into(),
        serde_json::Value::String(format!("built it {BUILT}")),
    );
    put_node(&mut store, out).unwrap();
    let mut review = WorkUnit::pending(format!("{run_id}:review"), run_id, 2, "review");
    review.role = crate::workflow::PhaseRole::Evaluator;
    review.status = UnitStatus::Distributed;
    review.assigned_cli = Some(reviewer.into());
    put_node(&mut store, review.to_node()).unwrap();
    put_node(&mut store, session(run_id).to_node()).unwrap();
    crate::power::queue_operator_message(run_id, "all", SAID, Some(2));
    let (tx, _rx) = channel::<Command>();
    let mut subs = crate::event_log::EventSink::default();
    let rec = Arc::new(Recorder::default());
    let runner: Arc<dyn StepRunner> = rec.clone();
    assert!(dispatch_unit(
        &mut store,
        &mut subs,
        &runner,
        &tx,
        run_id,
        1,
        &None,
        &None,
        uuid::Uuid::nil(),
        false,
    )
    .expect("the review dispatches"));
    let until = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some((_, labels, texts)) = rec.0.lock().unwrap().iter().find(|s| s.0 == 2) {
            return (labels.clone(), texts.clone());
        }
        assert!(Instant::now() < until, "the review reached its runner");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn holds(texts: &[String], marker: &str) -> bool {
    texts.iter().any(|t| t.contains(marker))
}

/// T-R6, the team ledger and operator-message routes: a real reviewer is handed the creator's
/// team ledger and the operator's message queued for it; a stand-in is handed the work only (the
/// message stays queued for the seats).
#[test]
fn a_stand_in_never_hears_the_team_ledger_or_an_operator_message() {
    let (labels, texts) = review_context("tr6-real", "claude");
    assert!(
        holds(&texts, BUILT),
        "the control is handed the work: {labels:?}"
    );
    assert!(
        holds(&texts, TEAM),
        "the control hears the team ledger: {labels:?}"
    );
    assert!(
        holds(&texts, SAID),
        "the control hears the operator: {labels:?}"
    );

    let (labels, texts) = review_context("tr6-stand-in", "claude~1");
    assert_eq!(
        labels.first().map(String::as_str),
        Some(crate::standin::PREFACE_LABEL),
        "{labels:?}"
    );
    assert!(
        holds(&texts, BUILT),
        "the stand-in is handed the work: {labels:?}"
    );
    assert!(!holds(&texts, TEAM), "never the team ledger: {labels:?}");
    assert!(
        !holds(&texts, SAID),
        "never an operator message: {labels:?}"
    );
    assert_eq!(
        crate::power::take_operator_messages("tr6-stand-in", "claude", 2).len(),
        1,
        "the message stays queued for the seats"
    );
}

/// T-R6, the voice route (core#1216): the person's voice rides the orchestrator's build brief and
/// nothing else, and never a stand-in's, even one seated on a build (a stand-in is never minted
/// for a creator; this is the defence in depth).
#[test]
fn a_stand_in_never_hears_the_voice() {
    let mut store = open_store(Some(":memory:")).unwrap();
    let mut subs = crate::event_log::EventSink::default();
    let brief = |store: &mut dyn GraphStore,
                 subs: &mut crate::event_log::EventSink,
                 run_id: &str,
                 seat: &str,
                 catalog: &str,
                 role: crate::workflow::PhaseRole| {
        let mut s = session(run_id);
        let mut power = crate::power::PowerState::launched(true, 0);
        power.voice = Some(format!("## Voice\n\n- {VOICE}."));
        s.power = Some(power);
        put_node(store, s.to_node()).unwrap();
        let mut unit = WorkUnit::pending(format!("{run_id}:{catalog}"), run_id, 1, catalog);
        unit.catalog = Some(catalog.into());
        unit.role = role;
        unit.assigned_cli = Some(seat.into());
        power_dispatch(store, subs, &s, unit, false)
            .unwrap()
            .description
    };
    use crate::workflow::PhaseRole::{Creator, Evaluator};
    let control = brief(
        &mut store,
        &mut subs,
        "voice-real",
        "claude",
        "build",
        Creator,
    );
    assert!(
        control.contains(VOICE),
        "the orchestrator hears the voice: {control}"
    );
    for (run, catalog, role) in [
        ("voice-build", "build", Creator),
        ("voice-review", "review", Evaluator),
        ("voice-critique", "critique", Evaluator),
    ] {
        let d = brief(&mut store, &mut subs, run, "claude~1", catalog, role);
        assert!(
            !d.contains(VOICE),
            "{catalog}: a stand-in never hears the voice: {d}"
        );
    }
}
