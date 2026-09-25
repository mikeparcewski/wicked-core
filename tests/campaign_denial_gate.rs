//! DES-L1 PR-1D (core #484) — the campaign's escalation-gate policy through the REAL engine
//! (`Core`, stub seats, no repo, temp store). An unattended campaign used to park forever when a
//! node's run hit the engine's escalation gate. `denial_gate: auto_reject` answers THAT gate with
//! Reject (the node cancels, `campaignNodeAwaitingHuman` then `runCancelled`); a run-level gate
//! the launch asked for (`human_confirm: all`) still HOLDS. `hold` (the default) parks both.
use std::sync::Arc;
use std::time::{Duration, Instant};

use wicked_core::{
    CampaignDef, CampaignNode, Core, CoreEvent, DenialGatePolicy, EntityMode, FailurePolicy,
    HumanConfirm, NodeStatus, PhaseRole, RunSpec, StepInput, StepOutput, StepRunner, StepStatus,
};
use wicked_council::types::{Category, Confidence, Dispatcher, InputMode, Vote};
use wicked_council::{AgenticCli, CouncilTask};

// ── Test-harness hygiene (core#311) — not a test ─────────────────────────────────────────────
/// Arm the hermetic emit spool BEFORE main (pre-main is single-threaded, so no test thread can
/// race it): engine paths under test fire coarse fire-and-forget `wicked.*` emissions, and with
/// no shared store configured those spool — which must land in a per-process temp file, never in
/// the operator's real emit-outbox replay queue under the wicked apps home. Every
/// binary in this suite carries this block; `harness_hygiene.rs` fails the suite if one is missing.
///
/// SAFETY (`ctor(unsafe)`): runs before `main` on one thread and only sets one process env var
/// via the std API — no allocator setup, no threads, no panics across the FFI boundary.
#[ctor::ctor(unsafe)]
fn arm_hermetic_emit_spool() {
    wicked_apps_core::emit::hermetic_test_spool();
}

fn db_path(name: &str) -> String {
    let dir = std::env::temp_dir().join(format!("wicked-core-cdg-{name}-{}", std::process::id()));
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
        acp: None,
        capabilities: None,
        login_invocation: None,
        health: None,
    }
}

struct StubDispatcher;
impl Dispatcher for StubDispatcher {
    fn dispatch(&self, c: &AgenticCli, _: &CouncilTask) -> Option<Vote> {
        Some(Vote {
            cli: c.key.clone(),
            recommendation: "x".into(),
            top_risk: "none".into(),
            change_my_mind: "no".into(),
            disqualifier: None,
            confidence: Confidence::default(),
            provenance: "stub".into(),
        })
    }
}

/// Every unit completes Ok; an Evaluator writes NO verdict line, so its fold denies into the
/// engine's escalation gate (PR-1A, D-9) — the shape #484 parks on.
struct NoVerdictSeat;
impl StepRunner for NoVerdictSeat {
    fn run_unit(&self, i: &StepInput) -> StepOutput {
        let output = if i.unit.session_id == "validator" {
            "PASS\nthe work meets the criterion\nPASS".to_string()
        } else if i.unit.role == PhaseRole::Evaluator {
            "reviewed the change; looks fine".to_string()
        } else {
            format!("did {}", i.unit.description)
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

/// A def whose review is an Evaluator agent unit — under `NoVerdictSeat` it opens the escalation gate.
const ESC_DEF: &str = r#"{
    "id": "esc-def",
    "phases": [
        { "id": "build", "kind": "build", "gate": "auto", "role": "creator" },
        { "id": "review", "kind": "review", "gate": "auto", "role": "evaluator", "depends_on": ["build"] }
    ]
}"#;

fn node(id: &str, hc: HumanConfirm, workflow_id: Option<&str>) -> CampaignNode {
    CampaignNode {
        node_id: id.to_string(),
        run_spec: RunSpec {
            problem: format!("do {id}"),
            clis: vec![cli("a"), cli("b")],
            entity_mode: EntityMode::Shared,
            human_confirm: hc,
            repo_ref: None,
            workflow_id: workflow_id.map(str::to_string),
        },
    }
}

fn campaign(id: &str, denial_gate: DenialGatePolicy) -> CampaignDef {
    CampaignDef {
        id: id.into(),
        name: "denial gate".into(),
        nodes: vec![
            // Parks at the ENGINE's escalation gate (the review writes no verdict line).
            node("esc", HumanConfirm::None, Some("esc-def")),
            // Parks at the RUN-LEVEL gate the launch asked for — never auto-answered.
            node("gate", HumanConfirm::All, None),
        ],
        edges: vec![],
        policy: FailurePolicy::ContinueIndependent,
        max_concurrency: 2,
        denial_gate,
    }
}

fn node_status(core: &Core, id: &str, node: &str) -> Option<NodeStatus> {
    core.campaign_detail(id)
        .ok()
        .flatten()
        .and_then(|c| c.node_status.get(node).copied())
}

fn wait_node(core: &Core, id: &str, node: &str, want: NodeStatus) -> bool {
    // Generous: the wait returns the moment the state is reached, and a loaded Windows runner
    // has been seen to land the node just past a 10 s deadline (core main b186c2f).
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if node_status(core, id, node) == Some(want) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

fn drain(ev: &std::sync::mpsc::Receiver<CoreEvent>) -> Vec<CoreEvent> {
    let mut seen = Vec::new();
    while let Ok(e) = ev.recv_timeout(Duration::from_millis(300)) {
        seen.push(e);
    }
    seen
}

#[test]
fn auto_reject_cancels_the_escalation_gated_node_and_holds_the_run_level_gate() {
    let core = Core::spawn_with_engine(
        db_path("auto"),
        Arc::new(StubDispatcher),
        Arc::new(NoVerdictSeat),
    );
    core.register_workflow(ESC_DEF)
        .expect("register the evaluator def");
    let ev = core.subscribe();
    core.launch_campaign(campaign("camp-auto", DenialGatePolicy::AutoReject))
        .expect("launch");

    assert!(
        wait_node(&core, "camp-auto", "esc", NodeStatus::Cancelled),
        "the escalation-gated node is auto-rejected → Cancelled: {:?}",
        node_status(&core, "camp-auto", "esc")
    );
    assert!(
        wait_node(&core, "camp-auto", "gate", NodeStatus::AwaitingHuman),
        "the run-level gate still HOLDS for a human: {:?}",
        node_status(&core, "camp-auto", "gate")
    );
    let evs = drain(&ev);
    let esc_run = "camp-auto:esc:a0";
    // The engine's gate opened as an ESCALATION on the review, the campaign disclosed the pause,
    // then answered it — the run cancelled; the gate node's run was never cancelled.
    assert!(evs.iter().any(|e| matches!(e, CoreEvent::GateEscalated { session, condition, denial_source, .. }
        if session == esc_run && condition == "verdict_not_pass" && denial_source == "evaluator_verdict")), "{evs:?}");
    let awaiting = evs
        .iter()
        .position(
            |e| matches!(e, CoreEvent::CampaignNodeAwaitingHuman { node, .. } if node == "esc"),
        )
        .expect("campaignNodeAwaitingHuman{esc}");
    let cancelled = evs
        .iter()
        .position(|e| matches!(e, CoreEvent::RunCancelled { session, .. } if session == esc_run))
        .expect("runCancelled for the esc run");
    assert!(awaiting < cancelled, "disclosed before answered: {evs:?}");
    assert!(!evs.iter().any(
        |e| matches!(e, CoreEvent::RunCancelled { session, .. } if session == "camp-auto:gate:a0")
    ));
    assert!(
        !evs.iter()
            .any(|e| matches!(e, CoreEvent::SessionFailed { .. })),
        "never sessionFailed: {evs:?}"
    );
}

#[test]
fn hold_parks_both_gates_for_a_human() {
    let core = Core::spawn_with_engine(
        db_path("hold"),
        Arc::new(StubDispatcher),
        Arc::new(NoVerdictSeat),
    );
    core.register_workflow(ESC_DEF)
        .expect("register the evaluator def");
    let ev = core.subscribe();
    core.launch_campaign(campaign("camp-hold", DenialGatePolicy::Hold))
        .expect("launch");

    assert!(wait_node(
        &core,
        "camp-hold",
        "esc",
        NodeStatus::AwaitingHuman
    ));
    assert!(wait_node(
        &core,
        "camp-hold",
        "gate",
        NodeStatus::AwaitingHuman
    ));
    let evs = drain(&ev);
    assert!(
        !evs.iter()
            .any(|e| matches!(e, CoreEvent::RunCancelled { .. })),
        "hold never answers a gate: {evs:?}"
    );
    assert_eq!(
        node_status(&core, "camp-hold", "esc"),
        Some(NodeStatus::AwaitingHuman),
        "still parked"
    );
}
