//! (QE acceptance, engine-originated launches) — through the REAL engine: the launches the engine
//! makes on its own carry the same QE word as `launch_run`. A campaign node's `run_spec` and a bus
//! `wicked.crew.run.requested`'s `args` take `skip_qe_acceptance_reason` / `force_qe_acceptance`,
//! and the host's standing "QE acceptance off" (`Core::set_qe_acceptance_off`, crew's daemon
//! setting) skips a node or request that gives no word of its own — only when its contract requires
//! QE acceptance. The skip is labelled on the run and on every gate receipt; a word that cannot
//! apply is refused (a campaign node fails at its launch; a bus request is dropped, never retried).

use std::sync::Arc;
use std::time::{Duration, Instant};

use std::collections::BTreeMap;

use wicked_apps_core::{open_store, ToNode};
use wicked_core::assurance::QeOverride;
use wicked_core::{
    put_node, BusDb, BusEmit, Campaign, CampaignDef, CampaignEdge, CampaignGateDecision,
    CampaignNode, CampaignStatus, Core, CoreEvent, DenialGatePolicy, EdgeCondition, EntityMode,
    FailurePolicy, HumanConfirm, LaunchSpec, NodeStatus, PlanStep, PresetSpec, RunSpec, StepInput,
    StepOutput, StepRunner, StepStatus,
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
        "wicked-core-qe-engine-{name}-{}",
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

/// One build step under the full contract, QE acceptance included: completes at once on `OkRunner`,
/// so its gate receipt is observable.
const QE_ONE: &str = r#"{"id":"qe-one","required_instruments":["distinct_evaluator","judge","qe_acceptance"],"phases":[
  {"id":"build","kind":"build","gate":"auto"}]}"#;
const NO_QE: &str = r#"{"id":"qe-none","phases":[
  {"id":"build","kind":"build","gate":"auto"}]}"#;

const OFF: &str = "QE acceptance is off on this daemon";

fn engine(name: &str) -> Core {
    let core = Core::spawn_with_engine(db_path(name), Arc::new(NoBallots), Arc::new(OkRunner));
    core.register_workflow(QE_ONE).expect("register qe-one");
    core.register_workflow(NO_QE).expect("register qe-none");
    core
}

fn node(id: &str, workflow: &str, skip: Option<&str>, force: bool) -> CampaignNode {
    CampaignNode {
        node_id: id.into(),
        run_spec: RunSpec {
            problem: "Fix the thing.".into(),
            clis: vec![cli("codex"), cli("claude")],
            entity_mode: EntityMode::Shared,
            human_confirm: HumanConfirm::None,
            repo_ref: None,
            workflow_id: Some(workflow.into()),
            skip_qe_acceptance_reason: skip.map(str::to_string),
            force_qe_acceptance: force,
        },
    }
}

fn campaign(id: &str, nodes: Vec<CampaignNode>) -> CampaignDef {
    CampaignDef {
        id: id.into(),
        name: id.into(),
        nodes,
        edges: vec![],
        policy: FailurePolicy::ContinueIndependent,
        max_concurrency: 8,
        denial_gate: DenialGatePolicy::default(),
    }
}

/// Every event until each of `sids` has ended (or `within`), as JSON.
fn events_until_done(
    ev: &std::sync::mpsc::Receiver<CoreEvent>,
    sids: &[&str],
    within: Duration,
) -> Vec<serde_json::Value> {
    let done = std::cell::RefCell::new(std::collections::BTreeSet::<String>::new());
    collect_until(ev, within, |e| {
        if let CoreEvent::SessionCompleted { session } | CoreEvent::SessionFailed { session, .. } =
            e
        {
            done.borrow_mut().insert(session.clone());
        }
        sids.iter().all(|s| done.borrow().contains(*s))
    })
    .iter()
    .map(|e| e.to_json())
    .collect()
}

fn of<'a>(evs: &'a [serde_json::Value], sid: &str) -> Vec<&'a serde_json::Value> {
    evs.iter().filter(|j| j["session"] == sid).collect()
}

fn started_qe(evs: &[serde_json::Value], sid: &str) -> serde_json::Value {
    of(evs, sid)
        .into_iter()
        .find(|j| j["type"] == "sessionStarted")
        .unwrap_or_else(|| panic!("sessionStarted for {sid}: {evs:?}"))["assurance"]["qe"]
        .clone()
}

/// The QE decision on every gate receipt of `sid` (there is at least one).
fn gate_qe(evs: &[serde_json::Value], sid: &str) -> Vec<serde_json::Value> {
    let gates: Vec<serde_json::Value> = of(evs, sid)
        .into_iter()
        .filter(|j| j["type"] == "gateEvaluated")
        .map(|j| j["assurance"]["qe"].clone())
        .collect();
    assert!(!gates.is_empty(), "{sid} evaluated a gate: {evs:?}");
    gates
}

fn skipped_with(qe: &serde_json::Value, reason: &str) {
    assert_eq!(
        (
            qe["status"].as_str(),
            qe["basis"].as_str(),
            qe["reason"].as_str()
        ),
        (
            Some("skipped"),
            Some("operator"),
            Some(format!("QE acceptance skipped by operator: {reason}").as_str())
        ),
        "{qe}"
    );
}

/// Campaign nodes: with the host's setting off, a QE node with no word is skipped with the
/// setting's reason (on the run and on every gate); a node that does not require QE acceptance
/// gets no decision at all; a node's own force and own skip win over the setting.
#[test]
fn campaign_nodes_take_their_own_word_or_the_hosts_standing_skip() {
    let core = engine("camp");
    core.set_qe_acceptance_off(Some(OFF.into())).unwrap();
    assert_eq!(core.qe_acceptance_off().as_deref(), Some(OFF));
    let ev = core.subscribe();
    core.launch_campaign(campaign(
        "qc",
        vec![
            node("off", "qe-one", None, false),
            node("plain", "qe-none", None, false),
            node("forced", "qe-one", None, true),
            node("own", "qe-one", Some("docs-only hotfix"), false),
        ],
    ))
    .expect("launch the campaign");
    let (off, plain, forced, own) = ("qc:off:a0", "qc:plain:a0", "qc:forced:a0", "qc:own:a0");
    let evs = events_until_done(&ev, &[off, plain, forced, own], Duration::from_secs(60));

    skipped_with(&started_qe(&evs, off), OFF);
    for qe in gate_qe(&evs, off) {
        skipped_with(&qe, OFF);
    }
    assert!(
        started_qe(&evs, plain).is_null(),
        "no QE decision on a run that does not require it"
    );
    let f = started_qe(&evs, forced);
    assert_eq!(
        (f["status"].as_str(), f["basis"].as_str()),
        (Some("required"), Some("operator")),
        "the node's own force wins over the setting: {f}"
    );
    skipped_with(&started_qe(&evs, own), "docs-only hotfix");
    for qe in gate_qe(&evs, own) {
        skipped_with(&qe, "docs-only hotfix");
    }
}

/// The setting is read as each node LAUNCHES: back on, a later campaign's QE node is required.
#[test]
fn a_campaign_node_reads_the_setting_when_it_launches() {
    let core = engine("camp-on");
    core.set_qe_acceptance_off(Some(OFF.into())).unwrap();
    core.set_qe_acceptance_off(None).unwrap();
    assert_eq!(core.qe_acceptance_off(), None);
    let ev = core.subscribe();
    core.launch_campaign(campaign("qn", vec![node("n", "qe-one", None, false)]))
        .expect("launch");
    let evs = events_until_done(&ev, &["qn:n:a0"], Duration::from_secs(60));
    let qe = started_qe(&evs, "qn:n:a0");
    assert_eq!(
        (qe["status"].as_str(), qe["basis"].as_str()),
        (Some("required"), Some("plan")),
        "{qe}"
    );
    assert!(core
        .set_qe_acceptance_off(Some("  ".into()))
        .unwrap_err()
        .contains("needs a reason"));
}

/// A node's word that cannot apply: both at once (or a blank reason) refuses the campaign at define
/// time, nothing persisted; a skip on a node whose workflow does not require QE acceptance fails
/// that node at its launch, while an independent node runs on.
#[test]
fn a_campaign_nodes_word_that_cannot_apply_is_refused() {
    let core = engine("camp-refuse");
    let err = core
        .launch_campaign(campaign("qb", vec![node("n", "qe-one", Some("x"), true)]))
        .expect_err("skip and force at once");
    assert!(format!("{err:#}").contains("mutually exclusive"), "{err:#}");
    let err = core
        .launch_campaign(campaign("qb2", vec![node("n", "qe-one", Some(" "), false)]))
        .expect_err("a blank reason");
    assert!(format!("{err:#}").contains("needs a reason"), "{err:#}");
    assert!(core.campaign_status("qb").unwrap().is_none());

    let ev = core.subscribe();
    core.launch_campaign(campaign(
        "qr",
        vec![
            node("bad", "qe-none", Some("why"), false),
            node("good", "qe-one", None, false),
        ],
    ))
    .expect("the def is well formed");
    let evs = events_until_done(&ev, &["qr:good:a0"], Duration::from_secs(60));
    assert!(
        of(&evs, "qr:bad:a0")
            .iter()
            .all(|j| j["type"] != "sessionStarted"),
        "the refused node never started a run"
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let c = core.campaign_detail("qr").unwrap().expect("the campaign");
        let st = serde_json::to_value(&c).unwrap();
        if st["status"] == "partially_completed" {
            assert_eq!(st["node_status"]["bad"], "failed", "{st}");
            assert_eq!(st["node_status"]["good"], "completed", "{st}");
            break;
        }
        assert!(Instant::now() < deadline, "the campaign settles: {st}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Bus requests: with the setting off, a QE request with no word is skipped with the setting's
/// reason; its own force wins; a request whose word cannot apply (a skip on a workflow that does
/// not require QE acceptance) is dropped — and the bridge goes on to launch the next request.
#[test]
fn bus_requests_take_their_own_word_or_the_hosts_and_a_refused_word_is_dropped() {
    let dir = std::path::PathBuf::from(db_path("bus"))
        .parent()
        .unwrap()
        .to_path_buf();
    let bus_db = dir.join("bus.db").to_str().unwrap().to_string();
    let core = engine("bus");
    core.set_qe_acceptance_off(Some(OFF.into())).unwrap();
    let ev = core.subscribe();
    let bridge = core.connect_bus(bus_db.clone(), vec![cli("codex"), cli("claude")]);
    let bus = BusDb::open(&bus_db).unwrap();
    let request = |workflow: &str, args: serde_json::Value| {
        bus.emit(&BusEmit::new(
            wicked_core::RUN_REQUESTED,
            "wicked-cli",
            "cli.run",
            serde_json::json!({ "workflow": workflow, "problem": "Fix the thing.", "args": args }),
        ))
        .unwrap();
    };
    request("qe-one", serde_json::json!({ "session_id": "b-off" }));
    request(
        "qe-none",
        serde_json::json!({ "session_id": "b-bad", "skip_qe_acceptance_reason": "why" }),
    );
    request(
        "qe-one",
        serde_json::json!({ "session_id": "b-forced", "force_qe_acceptance": true }),
    );
    request("qe-none", serde_json::json!({ "session_id": "b-plain" }));
    let evs = events_until_done(
        &ev,
        &["b-off", "b-forced", "b-plain"],
        Duration::from_secs(60),
    );
    skipped_with(&started_qe(&evs, "b-off"), OFF);
    for qe in gate_qe(&evs, "b-off") {
        skipped_with(&qe, OFF);
    }
    let f = started_qe(&evs, "b-forced");
    assert_eq!(
        (f["status"].as_str(), f["basis"].as_str()),
        (Some("required"), Some("operator")),
        "{f}"
    );
    assert!(started_qe(&evs, "b-plain").is_null());
    assert!(
        of(&evs, "b-bad").is_empty(),
        "the refused request launched nothing"
    );
    assert!(core.sessions().unwrap().iter().all(|s| s != "b-bad"));
    bridge.stop();
}

/// Every event until each of `sids` has STARTED (or `within`), as JSON — for runs that park (a
/// preset's plan approval) rather than end.
fn events_until_started(
    ev: &std::sync::mpsc::Receiver<CoreEvent>,
    sids: &[&str],
    within: Duration,
) -> Vec<serde_json::Value> {
    let seen = std::cell::RefCell::new(std::collections::BTreeSet::<String>::new());
    collect_until(ev, within, |e| {
        if let CoreEvent::SessionStarted { session, .. } = e {
            seen.borrow_mut().insert(session.clone());
        }
        sids.iter().all(|s| seen.borrow().contains(*s))
    })
    .iter()
    .map(|e| e.to_json())
    .collect()
}

fn node_status_of(core: &Core, campaign: &str, node: &str) -> Option<NodeStatus> {
    core.campaign_detail(campaign)
        .ok()
        .flatten()
        .and_then(|c| c.node_status.get(node).copied())
}

fn wait_node(core: &Core, campaign: &str, node: &str, want: NodeStatus) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while node_status_of(core, campaign, node) != Some(want) {
        assert!(
            Instant::now() < deadline,
            "{campaign}/{node} never reached {want:?} (at {:?})",
            node_status_of(core, campaign, node)
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// codex r1: the setting is read when a node DISPATCHES, not when its campaign launches. Node `a`
/// launches with the setting on (required) and parks at its gate; the setting goes off; once `a` is
/// approved and completes, its dependent `b` dispatches under the standing skip.
#[test]
fn a_dependent_node_reads_the_setting_when_it_dispatches() {
    let core = engine("camp-later");
    let ev = core.subscribe();
    let mut a = node("a", "qe-one", None, false);
    a.run_spec.human_confirm = HumanConfirm::All;
    let mut def = campaign("ql", vec![a, node("b", "qe-one", None, false)]);
    def.edges = vec![CampaignEdge {
        from: "a".into(),
        to: "b".into(),
        condition: EdgeCondition::OnSuccess,
    }];
    core.launch_campaign(def).expect("launch");
    wait_node(&core, "ql", "a", NodeStatus::AwaitingHuman);
    core.set_qe_acceptance_off(Some(OFF.into())).unwrap();
    core.confirm_campaign_gate("ql", "a", CampaignGateDecision::Approve { amend: None })
        .expect("approve a");
    let evs = events_until_done(&ev, &["ql:a:a0", "ql:b:a0"], Duration::from_secs(60));
    let qa = started_qe(&evs, "ql:a:a0");
    assert_eq!(
        (qa["status"].as_str(), qa["basis"].as_str()),
        (Some("required"), Some("plan")),
        "a launched while the setting was on: {qa}"
    );
    skipped_with(&started_qe(&evs, "ql:b:a0"), OFF);
}

/// The crash artifact `dispatch` can leave: `node` persisted Running under its derived run id, with
/// no session written (the p13 SC-C6/F1b window).
fn crashed_at(def: CampaignDef, node: &str) -> Campaign {
    let cid = def.id.clone();
    let node_status = def
        .nodes
        .iter()
        .map(|n| {
            let s = if n.node_id == node {
                NodeStatus::Running
            } else {
                NodeStatus::Pending
            };
            (n.node_id.clone(), s)
        })
        .collect();
    let mut node_run_id = BTreeMap::new();
    node_run_id.insert(node.to_string(), format!("{cid}:{node}:a0"));
    Campaign {
        id: cid.clone(),
        def_id: cid,
        status: CampaignStatus::Running,
        def,
        node_status,
        node_run_id,
        node_attempt: BTreeMap::new(),
        pending_decision: BTreeMap::new(),
        pending_decision_amend: BTreeMap::new(),
        pending_failure_gates: Vec::new(),
        fail_fast_tripped: false,
    }
}

/// codex r1: a node relaunched on resume (it crashed before its session was written) takes the host
/// word CURRENT at the relaunch, and a node's own word survives the relaunch.
#[test]
fn a_node_relaunched_on_resume_reads_the_current_setting_and_keeps_its_own_word() {
    for (cid, own_force) in [("qx", false), ("qy", true)] {
        let db = db_path(&format!("camp-resume-{cid}"));
        {
            let mut store = open_store(Some(&db)).expect("open store");
            let def = campaign(cid, vec![node("x", "qe-one", None, own_force)]);
            put_node(&mut store, crashed_at(def, "x").to_node()).expect("persist the artifact");
        }
        let core = Core::spawn_with_engine(db, Arc::new(NoBallots), Arc::new(OkRunner));
        core.register_workflow(QE_ONE).expect("register qe-one");
        core.set_qe_acceptance_off(Some(OFF.into())).unwrap();
        let ev = core.subscribe();
        let run = format!("{cid}:x:a0");
        core.resume_campaign(cid).expect("resume");
        let evs = events_until_started(&ev, &[run.as_str()], Duration::from_secs(30));
        let qe = started_qe(&evs, &run);
        if own_force {
            assert_eq!(
                (qe["status"].as_str(), qe["basis"].as_str()),
                (Some("required"), Some("operator")),
                "the node's own force survives the relaunch: {qe}"
            );
        } else {
            skipped_with(&qe, OFF);
        }
    }
}

fn step(catalog: &str, id: &str, depends_on: &[&str]) -> PlanStep {
    PlanStep {
        catalog: catalog.into(),
        id: id.into(),
        depends_on: if depends_on.is_empty() {
            None
        } else {
            Some(depends_on.iter().map(|s| s.to_string()).collect())
        },
        ..Default::default()
    }
}

/// codex r1: the PRESET the engine resolves decides whether the standing skip applies, on both
/// engine-originated paths: the built-in `bug` preset requires QE acceptance (the engine's own
/// table), so a campaign node and a bus request naming it are skipped; a saved preset that declares
/// no `qe_acceptance` requires none, so the same requests naming it get no decision (and are not
/// refused). (A saved preset cannot shadow a built-in globally, and these launches are unfiled, so
/// no project shadow reaches them.)
#[test]
fn the_resolved_preset_decides_where_the_standing_skip_applies() {
    for saved in [false, true] {
        let tag = if saved { "saved" } else { "builtin" };
        let workflow = if saved { "my-flow" } else { "bug" };
        let dir = std::path::PathBuf::from(db_path(&format!("preset-{tag}")))
            .parent()
            .unwrap()
            .to_path_buf();
        let core = engine(&format!("preset-{tag}"));
        if saved {
            core.put_preset(PresetSpec {
                name: "my-flow".into(),
                project_id: None,
                steps: vec![
                    step("build", "make", &[]),
                    step("review", "check", &["make"]),
                ],
                created_by: "api".into(),
            })
            .expect("save my-flow");
        }
        core.set_qe_acceptance_off(Some(OFF.into())).unwrap();
        let ev = core.subscribe();
        let bus_db = dir.join("bus.db").to_str().unwrap().to_string();
        let bridge = core.connect_bus(bus_db.clone(), vec![cli("codex"), cli("claude")]);
        let cid = format!("qp{tag}");
        core.launch_campaign(campaign(&cid, vec![node("n", workflow, None, false)]))
            .expect("launch");
        let bus_run = format!("bp-{tag}");
        BusDb::open(&bus_db)
            .unwrap()
            .emit(&BusEmit::new(
                wicked_core::RUN_REQUESTED,
                "wicked-cli",
                "cli.run",
                serde_json::json!({ "workflow": workflow, "problem": "Fix the thing.",
                    "args": { "session_id": bus_run } }),
            ))
            .unwrap();
        let node_run = format!("{cid}:n:a0");
        let evs = events_until_started(
            &ev,
            &[node_run.as_str(), bus_run.as_str()],
            Duration::from_secs(60),
        );
        for run in [&node_run, &bus_run] {
            let qe = started_qe(&evs, run);
            if saved {
                assert!(qe.is_null(), "{run}: the saved preset requires no QE: {qe}");
            } else {
                skipped_with(&qe, OFF);
            }
        }
        bridge.stop();
        let _ = core.cancel_campaign(&cid);
        let _ = core.cancel_run(&bus_run);
    }
}

/// codex r2: the straight-through `Core::launch` records no assurance contract, so it refuses ANY QE
/// word — an explicit skip, a force, or the host's standing skip — rather than dropping it.
#[test]
fn the_straight_through_path_refuses_every_qe_word() {
    let core = engine("straight");
    let events = core.subscribe();
    for (i, word) in [
        QeOverride::Skip("why".into()),
        QeOverride::Force,
        QeOverride::Standing(OFF.into()),
    ]
    .into_iter()
    .enumerate()
    {
        let sid = format!("straight-{i}");
        core.launch(LaunchSpec {
            base_ref: None,
            project_id: None,
            problem: "Fix the thing.".into(),
            clis: vec![cli("codex"), cli("claude")],
            entity_mode: EntityMode::Shared,
            session_id: sid.clone(),
            human_confirm: HumanConfirm::None,
            auto_deliver: false,
            repo_ref: None,
            workflow: Some("qe-one".into()),
            extra_write_roots: Vec::new(),
            extra_read_roots: Vec::new(),
            project_graph: None,
            plan: None,
            deliver_step: None,
            exclude_seats: Vec::new(),
            evidence_root: None,
            primary: None,
            reduced_assurance: false,
            power: false,
            deliverables: Vec::new(),
            qe_acceptance: word.clone(),
        });
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            assert!(Instant::now() < deadline, "{word:?}: no refusal");
            match events.recv_timeout(Duration::from_secs(1)) {
                Ok(CoreEvent::Error { message, .. }) => {
                    assert!(
                        message.contains("QE acceptance word") && message.contains("launch_run"),
                        "{word:?}: {message}"
                    );
                    break;
                }
                Ok(CoreEvent::SessionStarted { session, .. }) if session == sid => {
                    panic!("{word:?}: the straight-through path started a run it cannot label")
                }
                _ => continue,
            }
        }
        assert!(
            core.sessions().unwrap().iter().all(|s| *s != sid),
            "{word:?}: nothing persisted"
        );
    }
}
