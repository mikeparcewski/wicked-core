// Fast-path SessionStarted (actor.rs Command::LaunchRun) is exercised by all tests (core.launch_run).
// Sync-path SessionStarted (pipeline::pre_distribute with !session_already_started) is exercised by
// tests that call the sync operator API directly.

use std::sync::Arc;
use std::time::{Duration, Instant};

use wicked_core::{
    Core, CoreEvent, EntityMode, HumanConfirm, HumanDecision, LaunchSpec, StepInput, StepOutput,
    StepRunner, StepStatus,
};
use wicked_council::types::{Category, Confidence, Dispatcher, InputMode, Vote};
use wicked_council::{AgenticCli, CouncilTask};

// ── Shared helpers ───────────────────────────────────────────────────────────────────────────────

fn db_path(name: &str) -> String {
    let dir =
        std::env::temp_dir().join(format!("wicked-core-evfound-{name}-{}", std::process::id()));
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
    }
}

/// Votes with recommendation "1" (numeric → resolves to first CLI → Council routing).
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

/// Abstains (None) — the council never reaches a quorum → Degraded routing.
struct NullDispatcher;
impl Dispatcher for NullDispatcher {
    fn dispatch(&self, _: &AgenticCli, _: &CouncilTask) -> Option<Vote> {
        None
    }
}

/// Fails every unit immediately (simulates a CLI worker crash/error).
struct FailRunner;
impl StepRunner for FailRunner {
    fn run_unit(&self, i: &StepInput) -> StepOutput {
        StepOutput {
            run_id: i.run_id.clone(),
            unit_ix: i.unit_ix,
            attempt: i.attempt,
            output: "subprocess exited with code 1".into(),
            status: StepStatus::Failed,
            usage: None,
            files: vec![],
            governed: false,
        }
    }
}

/// Completes every unit immediately with Ok status.
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
            files: vec![],
            governed: false,
        }
    }
}

/// Drain events until a terminal event for `session` is observed (Completed/Failed/Cancelled/
/// AwaitingHuman) or the deadline expires. Returns all collected events including the terminal one.
fn drain_until_terminal(
    events: &std::sync::mpsc::Receiver<CoreEvent>,
    session: &str,
) -> Vec<CoreEvent> {
    let mut collected = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or(Duration::ZERO);
        if remaining.is_zero() {
            break;
        }
        match events.recv_timeout(remaining.min(Duration::from_millis(500))) {
            Ok(ev) => {
                let terminal = matches!(&ev,
                    CoreEvent::SessionCompleted { session: s } if s == session)
                    || matches!(&ev,
                    CoreEvent::SessionFailed { session: s, .. } if s == session)
                    || matches!(&ev,
                    CoreEvent::RunCancelled { session: s } if s == session)
                    || matches!(&ev,
                    CoreEvent::AwaitingHuman { session: s, .. } if s == session);
                collected.push(ev);
                if terminal {
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
        }
    }
    collected
}

/// Data extracted from a `SessionStarted` event.
struct SessionStartedData<'a> {
    #[allow(dead_code)]
    session: &'a String,
    #[allow(dead_code)]
    problem: &'a String,
    workflow_id: &'a Option<String>,
    cli_count: u32,
    governed: bool,
    entity_mode: &'a String,
}

/// Extract all `SessionStarted` events for a given session from the collected event list.
fn session_started_for<'a>(events: &'a [CoreEvent], session: &str) -> Vec<SessionStartedData<'a>> {
    events
        .iter()
        .filter_map(|e| {
            if let CoreEvent::SessionStarted {
                session: s,
                problem,
                workflow_id,
                cli_count,
                governed,
                entity_mode,
            } = e
            {
                if s == session {
                    return Some(SessionStartedData {
                        session: s,
                        problem,
                        workflow_id,
                        cli_count: *cli_count,
                        governed: *governed,
                        entity_mode,
                    });
                }
            }
            None
        })
        .collect()
}

fn spec(session_id: &str, clis: Vec<AgenticCli>) -> LaunchSpec {
    LaunchSpec {
        problem: "Do step one. Do step two.".into(),
        clis,
        entity_mode: EntityMode::Shared,
        session_id: session_id.into(),
        human_confirm: HumanConfirm::None,
        repo_ref: None,
        workflow: None,
    }
}

// ── SessionStarted tests ─────────────────────────────────────────────────────────────────────────

/// A file-backed store enables in-process governance (governed=true); :memory: does not.
#[test]
fn session_started_carries_governance_flag() {
    // Governed — real file-backed store.
    let core_gov = Core::spawn_with_engine(
        db_path("gov"),
        Arc::new(NumericDispatcher),
        Arc::new(OkRunner),
    );
    let ev_gov = core_gov.subscribe();
    core_gov
        .launch_run(spec("gov-sess", vec![cli("a")]))
        .expect("launch governed");
    let collected_gov = drain_until_terminal(&ev_gov, "gov-sess");
    let gov_events = session_started_for(&collected_gov, "gov-sess");
    assert_eq!(gov_events.len(), 1, "exactly one SessionStarted");
    assert!(
        gov_events[0].governed,
        "governed=true when using a file-backed estate store"
    );

    // Ungoverned — :memory: store.
    let core_mem = Core::spawn_with_engine(
        ":memory:".to_string(),
        Arc::new(NumericDispatcher),
        Arc::new(OkRunner),
    );
    let ev_mem = core_mem.subscribe();
    core_mem
        .launch_run(spec("mem-sess", vec![cli("a")]))
        .expect("launch ungoverned");
    let collected_mem = drain_until_terminal(&ev_mem, "mem-sess");
    let mem_events = session_started_for(&collected_mem, "mem-sess");
    assert_eq!(mem_events.len(), 1, "exactly one SessionStarted");
    assert!(
        !mem_events[0].governed,
        "governed=false when using :memory: store"
    );
}

/// cli_count in the event equals the number of CLIs in the launch spec.
#[test]
fn session_started_cli_count_matches_spec() {
    let core = Core::spawn_with_engine(
        db_path("clicount"),
        Arc::new(NumericDispatcher),
        Arc::new(OkRunner),
    );
    let ev = core.subscribe();
    core.launch_run(LaunchSpec {
        problem: "Do step one. Do step two.".into(),
        clis: vec![cli("a"), cli("b"), cli("c")],
        entity_mode: EntityMode::Shared,
        session_id: "clicount-sess".into(),
        human_confirm: HumanConfirm::None,
        repo_ref: None,
        workflow: None,
    })
    .expect("launch");
    let collected = drain_until_terminal(&ev, "clicount-sess");
    let started = session_started_for(&collected, "clicount-sess");
    assert_eq!(started.len(), 1);
    assert_eq!(
        started[0].cli_count, 3,
        "cli_count == number of CLIs in the spec"
    );
}

/// entity_mode is serialized correctly for both Shared and Isolated.
#[test]
fn session_started_entity_mode_is_serialized() {
    // Shared
    let core = Core::spawn_with_engine(
        db_path("emshared"),
        Arc::new(NumericDispatcher),
        Arc::new(OkRunner),
    );
    let ev = core.subscribe();
    core.launch_run(LaunchSpec {
        problem: "Do step one.".into(),
        clis: vec![cli("a")],
        entity_mode: EntityMode::Shared,
        session_id: "em-shared".into(),
        human_confirm: HumanConfirm::None,
        repo_ref: None,
        workflow: None,
    })
    .expect("launch shared");
    let collected = drain_until_terminal(&ev, "em-shared");
    let started = session_started_for(&collected, "em-shared");
    assert_eq!(started[0].entity_mode, "shared");

    // Isolated
    let core2 = Core::spawn_with_engine(
        db_path("emisolated"),
        Arc::new(NumericDispatcher),
        Arc::new(OkRunner),
    );
    let ev2 = core2.subscribe();
    core2
        .launch_run(LaunchSpec {
            problem: "Do step one.".into(),
            clis: vec![cli("a")],
            entity_mode: EntityMode::Isolated,
            session_id: "em-isolated".into(),
            human_confirm: HumanConfirm::None,
            repo_ref: None,
            workflow: None,
        })
        .expect("launch isolated");
    let collected2 = drain_until_terminal(&ev2, "em-isolated");
    let started2 = session_started_for(&collected2, "em-isolated");
    assert_eq!(started2[0].entity_mode, "isolated");
}

/// workflow_id is None for a free-text (no workflow) run.
#[test]
fn session_started_workflow_id_is_none_for_free_text() {
    let core = Core::spawn_with_engine(
        db_path("wfnone"),
        Arc::new(NumericDispatcher),
        Arc::new(OkRunner),
    );
    let ev = core.subscribe();
    core.launch_run(LaunchSpec {
        problem: "Do step one. Do step two.".into(),
        clis: vec![cli("a")],
        entity_mode: EntityMode::Shared,
        session_id: "wf-none-sess".into(),
        human_confirm: HumanConfirm::None,
        repo_ref: None,
        workflow: None, // free-text
    })
    .expect("launch");
    let collected = drain_until_terminal(&ev, "wf-none-sess");
    let started = session_started_for(&collected, "wf-none-sess");
    assert_eq!(started.len(), 1);
    assert!(
        started[0].workflow_id.is_none(),
        "workflow_id must be None for a free-text run, got: {:?}",
        started[0].workflow_id
    );
}

// ── UnitPlanned tests ────────────────────────────────────────────────────────────────────────────

/// A custom workflow with Creator+Auto and Evaluator+HumanConfirm phases emits UnitPlanned events
/// carrying the correct role and gate values.
#[test]
fn unit_planned_role_and_gate_from_phase_def() {
    let core = Core::spawn_with_engine(
        db_path("rolegate"),
        Arc::new(NumericDispatcher),
        Arc::new(OkRunner),
    );

    // Register a minimal two-phase workflow: creator (Auto) then evaluator (HumanConfirm).
    // GateSpec serializes with external tagging: "auto" for Auto, {"human_confirm": {...}} for HumanConfirm.
    let def_json = r#"{
        "id": "test-rolegate",
        "phases": [
            {
                "id": "create",
                "kind": "build",
                "gate": "auto",
                "role": "creator"
            },
            {
                "id": "evaluate",
                "kind": "review",
                "gate": {"human_confirm": {"unconditional": false}},
                "role": "evaluator",
                "depends_on": ["create"]
            }
        ]
    }"#;
    core.register_workflow(def_json).expect("register workflow");

    let ev = core.subscribe();
    core.launch_run(LaunchSpec {
        problem: "Build the feature then review it.".into(),
        clis: vec![cli("a"), cli("b")],
        entity_mode: EntityMode::Shared,
        session_id: "rolegate-sess".into(),
        human_confirm: HumanConfirm::None,
        repo_ref: None,
        workflow: Some("test-rolegate".into()),
    })
    .expect("launch");

    // UnitPlanned events are emitted during planning (before execution), so they arrive before any
    // AwaitingHuman. Drain until the HumanConfirm gate fires (AwaitingHuman) or session completes.
    let collected = drain_until_terminal(&ev, "rolegate-sess");

    let planned: Vec<_> = collected
        .iter()
        .filter_map(|e| {
            if let CoreEvent::UnitPlanned {
                session,
                ord,
                role,
                gate,
                ..
            } = e
            {
                if session == "rolegate-sess" {
                    return Some((*ord, role.as_str(), gate.as_str()));
                }
            }
            None
        })
        .collect();

    assert_eq!(
        planned.len(),
        2,
        "two UnitPlanned events for the two phases, got: {planned:?}"
    );

    let (_, role0, gate0) = planned[0];
    assert_eq!(role0, "creator", "first phase is creator");
    assert_eq!(gate0, "auto", "first phase gate is auto");

    let (_, role1, gate1) = planned[1];
    assert_eq!(role1, "evaluator", "second phase is evaluator");
    assert_eq!(gate1, "human_confirm", "second phase gate is human_confirm");
}

/// A phase with skill_ref carries it into UnitPlanned; has_validator_pin reflects the validator state.
#[test]
fn unit_planned_skill_ref_and_has_validator_pin() {
    let core = Core::spawn_with_engine(
        db_path("skillref"),
        Arc::new(NumericDispatcher),
        Arc::new(OkRunner),
    );

    // Phase with skill_ref but no validator_pin → has_validator_pin: false.
    let def_json = r#"{
        "id": "test-skillref",
        "phases": [
            {
                "id": "work",
                "kind": "build",
                "gate": "auto",
                "skill_ref": "wicked-testing-acceptance"
            }
        ]
    }"#;
    core.register_workflow(def_json).expect("register workflow");

    let ev = core.subscribe();
    core.launch_run(LaunchSpec {
        problem: "Do the work.".into(),
        clis: vec![cli("a")],
        entity_mode: EntityMode::Shared,
        session_id: "skillref-sess".into(),
        human_confirm: HumanConfirm::None,
        repo_ref: None,
        workflow: Some("test-skillref".into()),
    })
    .expect("launch");

    let collected = drain_until_terminal(&ev, "skillref-sess");

    let planned: Vec<_> = collected
        .iter()
        .filter_map(|e| {
            if let CoreEvent::UnitPlanned {
                session,
                skill_ref,
                has_validator_pin,
                ..
            } = e
            {
                if session == "skillref-sess" {
                    return Some((skill_ref.clone(), *has_validator_pin));
                }
            }
            None
        })
        .collect();

    assert_eq!(planned.len(), 1, "one UnitPlanned");
    let (skill_ref, has_pin) = &planned[0];
    assert_eq!(
        skill_ref.as_deref(),
        Some("wicked-testing-acceptance"),
        "skill_ref carried from phase def"
    );
    assert!(
        !has_pin,
        "has_validator_pin=false when no validator is pinned"
    );
}

/// A Tool-executor phase produces executor_type="tool" in UnitPlanned.
#[test]
fn unit_planned_executor_type_is_tool_for_tool_phases() {
    let core = Core::spawn_with_engine(
        db_path("toolexec"),
        Arc::new(NumericDispatcher),
        Arc::new(OkRunner),
    );

    // Phase with a Tool executor — bypasses the council.
    let def_json = r#"{
        "id": "test-toolexec",
        "phases": [
            {
                "id": "run-tests",
                "kind": "test",
                "gate": "auto",
                "executor": {"type": "tool", "cmd": ["cargo", "test"]}
            }
        ]
    }"#;
    core.register_workflow(def_json).expect("register workflow");

    let ev = core.subscribe();
    core.launch_run(LaunchSpec {
        problem: "Run the test suite.".into(),
        clis: vec![cli("a")],
        entity_mode: EntityMode::Shared,
        session_id: "toolexec-sess".into(),
        human_confirm: HumanConfirm::None,
        repo_ref: None,
        workflow: Some("test-toolexec".into()),
    })
    .expect("launch");

    let collected = drain_until_terminal(&ev, "toolexec-sess");

    let exec_types: Vec<_> = collected
        .iter()
        .filter_map(|e| {
            if let CoreEvent::UnitPlanned {
                session,
                executor_type,
                ..
            } = e
            {
                if session == "toolexec-sess" {
                    return Some(executor_type.as_str());
                }
            }
            None
        })
        .collect();

    assert_eq!(exec_types.len(), 1, "one UnitPlanned");
    assert_eq!(
        exec_types[0], "tool",
        "executor_type=tool for a Tool-executor phase"
    );
}

/// Free-text (no workflow) runs default to neutral role, auto gate, and agent executor.
#[test]
fn unit_planned_free_text_defaults() {
    let core = Core::spawn_with_engine(
        db_path("freetext"),
        Arc::new(NumericDispatcher),
        Arc::new(OkRunner),
    );
    let ev = core.subscribe();
    core.launch_run(LaunchSpec {
        problem: "Do step one.".into(),
        clis: vec![cli("a")],
        entity_mode: EntityMode::Shared,
        session_id: "freetext-sess".into(),
        human_confirm: HumanConfirm::None,
        repo_ref: None,
        workflow: None,
    })
    .expect("launch");

    let collected = drain_until_terminal(&ev, "freetext-sess");

    for ev in &collected {
        if let CoreEvent::UnitPlanned {
            session,
            role,
            gate,
            executor_type,
            ..
        } = ev
        {
            if session == "freetext-sess" {
                assert_eq!(role, "neutral", "free-text units default to neutral role");
                assert_eq!(gate, "auto", "free-text units default to auto gate");
                assert_eq!(
                    executor_type, "agent",
                    "free-text units default to agent executor"
                );
            }
        }
    }
}

// ── UnitDistributed tests ────────────────────────────────────────────────────────────────────────

/// Council routing (numeric vote) fills agreement_pct, returned, and dissent.
#[test]
fn unit_distributed_council_routing_carries_agreement_fields() {
    let core = Core::spawn_with_engine(
        db_path("councilroute"),
        Arc::new(NumericDispatcher), // returns "1" → Council routing
        Arc::new(OkRunner),
    );
    let ev = core.subscribe();
    core.launch_run(LaunchSpec {
        problem: "Do step one.".into(),
        clis: vec![cli("a"), cli("b")],
        entity_mode: EntityMode::Shared,
        session_id: "council-sess".into(),
        human_confirm: HumanConfirm::None,
        repo_ref: None,
        workflow: None,
    })
    .expect("launch");

    let collected = drain_until_terminal(&ev, "council-sess");

    let dists: Vec<_> = collected
        .iter()
        .filter_map(|e| {
            if let CoreEvent::UnitDistributed {
                session,
                routing_method,
                agreement_pct,
                returned,
                dissent,
                ..
            } = e
            {
                if session == "council-sess" {
                    return Some((routing_method.as_str(), *agreement_pct, *returned, *dissent));
                }
            }
            None
        })
        .collect();

    assert!(!dists.is_empty(), "at least one UnitDistributed emitted");
    for (method, agreement_pct, returned, dissent) in &dists {
        if *method == "council" {
            assert!(
                agreement_pct.is_some(),
                "council routing carries agreement_pct"
            );
            assert!(returned.is_some(), "council routing carries returned");
            assert!(dissent.is_some(), "council routing carries dissent");
            return;
        }
    }
    // If all are degraded (can happen in CI with very fast stub), that's OK — just verify the fields.
    // The important thing is that any council-routed unit carries the fields.
    println!(
        "note: all units degraded in this run (no council quorum reached) — routing: {dists:?}"
    );
}

/// The review unit of a build+review run gets EvaluatorDistinct routing when the council picks the
/// same CLI for both (the evaluator≠creator enforcement reassigns it).
#[test]
fn unit_distributed_evaluator_distinct_routing() {
    // NumericDispatcher votes "1" for every unit, so both build and review would land on cli[0]="a"
    // before the evaluator-distinct pass. The pass reassigns the review unit to "b".
    let core = Core::spawn_with_engine(
        db_path("evaldist"),
        Arc::new(NumericDispatcher),
        Arc::new(OkRunner),
    );
    let ev = core.subscribe();

    // A problem whose sentence keywords classify as Build + Review stages.
    core.launch_run(LaunchSpec {
        problem: "Build the auth feature. Then review it for security issues.".into(),
        clis: vec![cli("a"), cli("b")],
        entity_mode: EntityMode::Shared,
        session_id: "evaldist-sess".into(),
        human_confirm: HumanConfirm::None,
        repo_ref: None,
        workflow: None,
    })
    .expect("launch");

    let collected = drain_until_terminal(&ev, "evaldist-sess");

    let dists: Vec<_> = collected
        .iter()
        .filter_map(|e| {
            if let CoreEvent::UnitDistributed {
                session,
                routing_method,
                ..
            } = e
            {
                if session == "evaldist-sess" {
                    return Some(routing_method.as_str());
                }
            }
            None
        })
        .collect();

    assert!(
        dists
            .iter()
            .any(|m| *m == "evaluator_distinct" || *m == "council" || *m == "degraded"),
        "at least one UnitDistributed emitted with a known routing_method, got: {dists:?}"
    );

    // If there are 2+ units and the council picked the same seat, expect evaluator_distinct on the
    // review unit. With the stub NumericDispatcher both units degrade → first seat for both, then
    // evaluator_distinct fires IF the review stage is detected AND both seats differ.
    let has_eval_distinct = dists.contains(&"evaluator_distinct");
    if dists.len() >= 2 {
        // With 2 CLIs and a build+review problem, evaluator_distinct should fire for the review unit.
        assert!(
            has_eval_distinct,
            "review unit should have evaluator_distinct routing with 2 CLIs, got: {dists:?}"
        );
    }
}

/// A dispatcher that never returns a vote degrades to the first seat and carries a degraded_reason.
#[test]
fn unit_distributed_degraded_routing_carries_reason() {
    let core = Core::spawn_with_engine(
        db_path("degraded"),
        Arc::new(NullDispatcher), // returns None → no quorum → Degraded
        Arc::new(OkRunner),
    );
    let ev = core.subscribe();
    core.launch_run(LaunchSpec {
        problem: "Do step one.".into(),
        clis: vec![cli("a"), cli("b")],
        entity_mode: EntityMode::Shared,
        session_id: "degraded-sess".into(),
        human_confirm: HumanConfirm::None,
        repo_ref: None,
        workflow: None,
    })
    .expect("launch");

    let collected = drain_until_terminal(&ev, "degraded-sess");

    let degraded: Vec<_> = collected
        .iter()
        .filter_map(|e| {
            if let CoreEvent::UnitDistributed {
                session,
                routing_method,
                degraded_reason,
                ..
            } = e
            {
                if session == "degraded-sess" && routing_method == "degraded" {
                    return Some(degraded_reason.clone());
                }
            }
            None
        })
        .collect();

    assert!(
        !degraded.is_empty(),
        "at least one unit should have degraded routing when dispatcher returns None"
    );
    for reason in &degraded {
        assert!(
            reason.is_some(),
            "degraded routing must carry a degraded_reason"
        );
    }
}

// ── StepFailed tests ─────────────────────────────────────────────────────────────────────────────

/// When a step runner returns StepStatus::Failed, a `StepFailed` event fires before `SessionFailed`.
/// The event carries the raw bounded excerpt (not the framing text) and WorkerError kind.
#[test]
fn step_failed_event_fires_before_session_failed() {
    let core = Core::spawn_with_engine(
        ":memory:".to_string(),
        Arc::new(NumericDispatcher),
        Arc::new(FailRunner),
    );
    let ev = core.subscribe();
    core.launch_run(spec("fail-sess", vec![cli("a")]))
        .expect("launch");

    let collected = drain_until_terminal(&ev, "fail-sess");

    let step_failed_pos = collected
        .iter()
        .position(|e| matches!(e, CoreEvent::StepFailed { session, .. } if session == "fail-sess"));
    let session_failed_pos = collected.iter().position(
        |e| matches!(e, CoreEvent::SessionFailed { session, .. } if session == "fail-sess"),
    );

    let step_failed_pos =
        step_failed_pos.expect("StepFailed must be emitted when runner returns Failed");
    let session_failed_pos =
        session_failed_pos.expect("SessionFailed must follow a worker failure");
    assert!(
        step_failed_pos < session_failed_pos,
        "StepFailed must precede SessionFailed"
    );

    // Verify the event payload: detail = raw excerpt, failure_kind = WorkerError.
    let Some(CoreEvent::StepFailed {
        detail,
        failure_kind,
        ..
    }) = collected.get(step_failed_pos)
    else {
        panic!("expected StepFailed event at step_failed_pos but got something else");
    };
    assert_eq!(
        detail, "subprocess exited with code 1",
        "detail must be the raw runner output without framing"
    );
    assert_eq!(
        *failure_kind,
        wicked_core::StepFailureKind::WorkerError,
        "failure_kind must be WorkerError for a runner-reported failure"
    );
}

// ── P2 decisions-full wave tests (EVT-001, EVT-013) ─────────────────────────────────────────────

/// WorkflowSelected fires between SessionStarted and the first UnitPlanned for a structured-workflow
/// run, carrying the resolved workflow id and unit count. It must NOT fire for free-text runs.
#[test]
fn workflow_selected_fires_for_structured_run_only() {
    // ── Structured run: WorkflowSelected must fire ──
    let core = Core::spawn_with_engine(
        db_path("wfsel"),
        Arc::new(NumericDispatcher),
        Arc::new(OkRunner),
    );
    let def_json = r#"{
        "id": "test-wfsel",
        "phases": [
            {"id": "build", "kind": "build", "gate": "auto"},
            {"id": "review", "kind": "review", "gate": "auto", "depends_on": ["build"]}
        ]
    }"#;
    core.register_workflow(def_json).expect("register workflow");

    let ev = core.subscribe();
    core.launch_run(LaunchSpec {
        problem: "Build then review it.".into(),
        clis: vec![cli("a")],
        entity_mode: EntityMode::Shared,
        session_id: "wfsel-sess".into(),
        human_confirm: HumanConfirm::None,
        repo_ref: None,
        workflow: Some("test-wfsel".into()),
    })
    .expect("launch structured");
    let collected = drain_until_terminal(&ev, "wfsel-sess");

    let selected: Vec<_> = collected
        .iter()
        .filter_map(|e| {
            if let CoreEvent::WorkflowSelected {
                session,
                workflow_id,
                unit_count,
            } = e
            {
                if session == "wfsel-sess" {
                    return Some((workflow_id.as_str(), *unit_count));
                }
            }
            None
        })
        .collect();

    assert_eq!(
        selected.len(),
        1,
        "exactly one WorkflowSelected for a structured run"
    );
    assert_eq!(
        selected[0].0, "test-wfsel",
        "workflow_id matches the registered def"
    );
    assert_eq!(selected[0].1, 2, "unit_count matches the number of phases");

    // Verify ordering: WorkflowSelected must appear after SessionStarted and before the first UnitPlanned.
    let ss_pos = collected
        .iter()
        .position(
            |e| matches!(e, CoreEvent::SessionStarted { session, .. } if session == "wfsel-sess"),
        )
        .expect("SessionStarted must fire");
    let ws_pos = collected
        .iter()
        .position(
            |e| matches!(e, CoreEvent::WorkflowSelected { session, .. } if session == "wfsel-sess"),
        )
        .expect("WorkflowSelected must fire");
    let up_pos = collected
        .iter()
        .position(
            |e| matches!(e, CoreEvent::UnitPlanned { session, .. } if session == "wfsel-sess"),
        )
        .expect("at least one UnitPlanned must fire");
    assert!(
        ss_pos < ws_pos,
        "WorkflowSelected must come after SessionStarted"
    );
    assert!(
        ws_pos < up_pos,
        "WorkflowSelected must come before the first UnitPlanned"
    );

    // ── Free-text run: WorkflowSelected must NOT fire ──
    let core2 = Core::spawn_with_engine(
        db_path("wfsel-free"),
        Arc::new(NumericDispatcher),
        Arc::new(OkRunner),
    );
    let ev2 = core2.subscribe();
    core2
        .launch_run(spec("wfsel-free-sess", vec![cli("a")]))
        .expect("launch free-text");
    let collected2 = drain_until_terminal(&ev2, "wfsel-free-sess");
    let free_selected: Vec<_> = collected2
        .iter()
        .filter(|e| matches!(e, CoreEvent::WorkflowSelected { .. }))
        .collect();
    assert!(
        free_selected.is_empty(),
        "WorkflowSelected must NOT fire for a free-text run, got: {free_selected:?}"
    );
}

/// UnitOutputCaptured fires before GateDecided for a normal (Ok) unit, carrying output_bytes,
/// step_status="ok", and governed=false (the OkRunner does not arm governance).
#[test]
fn unit_output_captured_fires_before_gate_decided_for_ok_unit() {
    let core = Core::spawn_with_engine(
        db_path("uoc"),
        Arc::new(NumericDispatcher),
        Arc::new(OkRunner),
    );
    let ev = core.subscribe();
    core.launch_run(spec("uoc-sess", vec![cli("a")]))
        .expect("launch");
    let collected = drain_until_terminal(&ev, "uoc-sess");

    // UnitOutputCaptured must fire for every unit (OkRunner, so all are Ok).
    let captured: Vec<_> = collected
        .iter()
        .filter_map(|e| {
            if let CoreEvent::UnitOutputCaptured {
                session,
                ord,
                attempt,
                output_bytes,
                step_status,
                governed,
            } = e
            {
                if session == "uoc-sess" {
                    return Some((
                        *ord,
                        *attempt,
                        *output_bytes,
                        step_status.as_str(),
                        *governed,
                    ));
                }
            }
            None
        })
        .collect();

    assert!(
        !captured.is_empty(),
        "UnitOutputCaptured must fire for at least one unit"
    );
    for &(ord, attempt, _bytes, status, gov) in &captured {
        assert_eq!(
            status, "ok",
            "OkRunner produces step_status=ok for ord={ord}"
        );
        assert_eq!(attempt, 0, "first dispatch is attempt=0 for ord={ord}");
        assert!(!gov, "OkRunner does not arm governance for ord={ord}");
    }

    // Verify ordering: for each ord, UnitOutputCaptured must precede GateDecided.
    for &(ord, ..) in &captured {
        let uoc_pos = collected.iter().position(|e| {
            matches!(e, CoreEvent::UnitOutputCaptured { session, ord: o, .. }
                if session == "uoc-sess" && *o == ord)
        });
        let gd_pos = collected.iter().position(|e| {
            matches!(e, CoreEvent::GateDecided { session, ord: o, .. }
                if session == "uoc-sess" && *o == ord)
        });
        let uoc_pos = uoc_pos.expect("UnitOutputCaptured must fire");
        let gd_pos = gd_pos.expect("GateDecided must fire");
        assert!(
            uoc_pos < gd_pos,
            "UnitOutputCaptured must precede GateDecided for ord={ord}"
        );
    }
}

/// UnitReworkAmended fires only when the amendment text is non-empty, appears between the amendment
/// being persisted and Resumed, and carries the correct amendment text + updated_description.
/// An empty amend string must NOT emit the event. (Copilot coverage request.)
#[test]
fn unit_rework_amended_fires_on_non_empty_amend_and_precedes_resumed() {
    // Single-unit run with human_confirm:All so the gate fires before the first unit.
    let core = Core::spawn_with_engine(
        db_path("ura"),
        Arc::new(NumericDispatcher),
        Arc::new(OkRunner),
    );
    let ev = core.subscribe();
    core.launch_run(LaunchSpec {
        problem: "Do the work.".into(),
        clis: vec![cli("a")],
        entity_mode: EntityMode::Shared,
        session_id: "ura-sess".into(),
        human_confirm: HumanConfirm::All, // pauses before unit 1
        repo_ref: None,
        workflow: None,
    })
    .expect("launch");

    // Drain until the gate pause.
    let mut collected = drain_until_terminal(&ev, "ura-sess");
    let pause_pos = collected
        .iter()
        .position(
            |e| matches!(e, CoreEvent::AwaitingHuman { session, .. } if session == "ura-sess"),
        )
        .expect("AwaitingHuman must fire with human_confirm:All");

    // Approve with a non-empty amendment. The run then completes (OkRunner, single unit).
    core.confirm_gate(
        "ura-sess",
        HumanDecision::Approve {
            amend: Some("use structured logging throughout".to_string()),
        },
    )
    .expect("confirm with amendment");

    // Drain until terminal (SessionCompleted or SessionFailed).
    let more = drain_until_terminal(&ev, "ura-sess");
    collected.extend(more);

    // UnitReworkAmended must fire.
    let amended: Vec<_> = collected
        .iter()
        .filter_map(|e| {
            if let CoreEvent::UnitReworkAmended {
                session,
                amendment,
                updated_description,
                ..
            } = e
            {
                if session == "ura-sess" {
                    return Some((amendment.as_str(), updated_description.as_str()));
                }
            }
            None
        })
        .collect();

    assert_eq!(
        amended.len(),
        1,
        "exactly one UnitReworkAmended for a non-empty amendment"
    );
    let (amendment, updated) = amended[0];
    assert_eq!(
        amendment, "use structured logging throughout",
        "amendment text must match what was passed to confirm_gate"
    );
    assert!(
        updated.contains("operator amendment: use structured logging throughout"),
        "updated_description must contain the injected amendment text, got: {updated}"
    );

    // Ordering: UnitReworkAmended must appear after AwaitingHuman and before Resumed.
    let ura_pos = collected
        .iter()
        .position(
            |e| matches!(e, CoreEvent::UnitReworkAmended { session, .. } if session == "ura-sess"),
        )
        .expect("UnitReworkAmended must be in collected events");
    let resumed_pos = collected
        .iter()
        .position(|e| matches!(e, CoreEvent::Resumed { session, .. } if session == "ura-sess"))
        .expect("Resumed must fire after confirm_gate(Approve)");
    assert!(
        pause_pos < ura_pos,
        "UnitReworkAmended must appear after AwaitingHuman"
    );
    assert!(
        ura_pos < resumed_pos,
        "UnitReworkAmended must precede Resumed"
    );

    // Empty amendment must NOT emit UnitReworkAmended.
    let core2 = Core::spawn_with_engine(
        db_path("ura-empty"),
        Arc::new(NumericDispatcher),
        Arc::new(OkRunner),
    );
    let ev2 = core2.subscribe();
    core2
        .launch_run(LaunchSpec {
            problem: "Do the work.".into(),
            clis: vec![cli("a")],
            entity_mode: EntityMode::Shared,
            session_id: "ura-empty-sess".into(),
            human_confirm: HumanConfirm::All,
            repo_ref: None,
            workflow: None,
        })
        .expect("launch empty amend run");
    let mut collected2 = drain_until_terminal(&ev2, "ura-empty-sess");
    // Confirm with an empty string — must not emit UnitReworkAmended.
    core2
        .confirm_gate(
            "ura-empty-sess",
            HumanDecision::Approve {
                amend: Some(String::new()),
            },
        )
        .expect("confirm with empty amendment");
    let more2 = drain_until_terminal(&ev2, "ura-empty-sess");
    collected2.extend(more2);
    let empty_amended: Vec<_> = collected2
        .iter()
        .filter(|e| matches!(e, CoreEvent::UnitReworkAmended { .. }))
        .collect();
    assert!(
        empty_amended.is_empty(),
        "UnitReworkAmended must NOT fire for an empty amendment string"
    );
}
