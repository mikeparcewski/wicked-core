// Integration tests for the P2 governance-deep event wave:
//   EVT-009 ValidationPinAttached — fires at plan time when a pinned validator is attached to a unit
//   EVT-011 ToolExecutorDispatched — fires when a unit with a tool_cmd is dispatched
//
// EVT-008 (GovernanceHookFired), EVT-010 (GateEscalated), and EVT-016 (GovernanceContextArmed) require
// a live subprocess invocation and/or gate escalation, which exceed the offline-only test budget. They
// are covered structurally by the NAPI drift test and by manual governance e2e runs.

use std::sync::Arc;
use std::time::{Duration, Instant};

use wicked_core::{
    Core, CoreEvent, EntityMode, HumanConfirm, LaunchSpec, StepInput, StepOutput, StepRunner,
    StepStatus,
};
use wicked_council::types::{Category, Confidence, Dispatcher, InputMode, Vote};
use wicked_council::{AgenticCli, CouncilTask};

// ── Shared helpers ───────────────────────────────────────────────────────────────────────────────

fn db_path(name: &str) -> String {
    let dir = std::env::temp_dir().join(format!("wicked-core-evgov-{name}-{}", std::process::id()));
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

/// Drain events until a terminal event for `session` is observed or the deadline expires.
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

// ── EVT-009 ValidationPinAttached ───────────────────────────────────────────────────────────────

/// A workflow phase with an approved pinned validator emits `ValidationPinAttached` at plan time.
/// Tests the full producer path: vault → attach → emit.
#[test]
fn validation_pin_attached_fires_for_a_pinned_validator_unit() {
    use wicked_apps_core::open_store;
    use wicked_core::{store_validator, DeterministicValidator};

    // Build a store with an approved validator already vaulted.
    let db = db_path("pinattach");
    let mut store = open_store(Some(&db)).unwrap();

    let validator = DeterministicValidator {
        criterion: "README.md must exist".into(),
        script: "test -f README.md".into(),
        approved: true,
    };
    let p = store_validator(&mut store, &validator).unwrap();
    drop(store); // release the store before opening the Core

    // A 1-phase workflow def that pins the approved validator.
    let def_json = serde_json::json!({
        "id": "gated-test",
        "phases": [ { "id": "build", "kind": "build", "validator_pin": p } ]
    })
    .to_string();

    // Register the workflow and launch a run.
    let core = Core::spawn_with_engine(db, Arc::new(NumericDispatcher), Arc::new(OkRunner));
    let ev = core.subscribe();

    // Register the workflow so the actor's registry knows it.
    core.register_workflow(&def_json).unwrap();

    core.launch_run(LaunchSpec {
        problem: "Do the thing.".into(),
        clis: vec![cli("stub")],
        entity_mode: EntityMode::Shared,
        session_id: "pin-sess".into(),
        human_confirm: HumanConfirm::None,
        repo_ref: None,
        workflow: Some("gated-test".into()),
    })
    .expect("launch");

    let collected = drain_until_terminal(&ev, "pin-sess");

    // Find ValidationPinAttached events for this session.
    let pin_events: Vec<_> = collected
        .iter()
        .filter_map(|e| {
            if let CoreEvent::ValidationPinAttached {
                session,
                ord,
                pin: event_pin,
                criterion,
            } = e
            {
                if session == "pin-sess" {
                    return Some((*ord, event_pin.as_str(), criterion.as_str()));
                }
            }
            None
        })
        .collect();

    assert_eq!(
        pin_events.len(),
        1,
        "exactly one ValidationPinAttached for the single pinned phase; got: {pin_events:?}"
    );
    let (ord, event_pin, criterion) = pin_events[0];
    assert_eq!(ord, 1, "ord 1 (the only phase)");
    assert_eq!(
        event_pin, p,
        "pin matches the vaulted validator's content-hash"
    );
    assert!(
        criterion.contains("README.md"),
        "criterion carries the validator's acceptance text: {criterion}"
    );
}

// ── EVT-011 ToolExecutorDispatched ──────────────────────────────────────────────────────────────

/// A workflow phase with a Tool executor emits `ToolExecutorDispatched` before the command spawns.
#[test]
fn tool_executor_dispatched_fires_for_a_tool_phase() {
    // A 1-phase workflow with a Tool executor (echo-based — always succeeds in the test env).
    let def_json = serde_json::json!({
        "id": "tool-test",
        "phases": [ {
            "id": "run",
            "kind": "build",
            "executor": { "type": "tool", "cmd": ["echo", "tool-done"] }
        } ]
    })
    .to_string();

    let core = Core::spawn_with_engine(
        db_path("tooldispatch"),
        Arc::new(NumericDispatcher),
        Arc::new(OkRunner),
    );
    let ev = core.subscribe();

    core.register_workflow(&def_json).unwrap();
    core.launch_run(LaunchSpec {
        problem: "Run the tool.".into(),
        clis: vec![cli("stub")],
        entity_mode: EntityMode::Shared,
        session_id: "tool-sess".into(),
        human_confirm: HumanConfirm::None,
        repo_ref: None,
        workflow: Some("tool-test".into()),
    })
    .expect("launch");

    let collected = drain_until_terminal(&ev, "tool-sess");

    // Exactly one ToolExecutorDispatched for the single tool phase.
    let dispatched: Vec<_> = collected
        .iter()
        .filter_map(|e| {
            if let CoreEvent::ToolExecutorDispatched {
                session, ord, cmd, ..
            } = e
            {
                if session == "tool-sess" {
                    return Some((*ord, cmd.clone()));
                }
            }
            None
        })
        .collect();

    assert_eq!(
        dispatched.len(),
        1,
        "exactly one ToolExecutorDispatched; got: {dispatched:?}"
    );
    let (ord, cmd) = &dispatched[0];
    assert_eq!(*ord, 1, "ord 1 (the only phase)");
    assert!(
        cmd.first().map(|s| s == "echo").unwrap_or(false),
        "cmd carries the tool binary: {cmd:?}"
    );
    assert!(
        cmd.contains(&"tool-done".to_string()),
        "cmd carries the tool arguments: {cmd:?}"
    );

    // The session must complete (the tool path goes through apply_and_finish_unit like the agent path).
    let completed = collected.iter().any(|e| {
        matches!(e, CoreEvent::SessionCompleted { session } if session == "tool-sess")
            || matches!(e, CoreEvent::SessionFailed { session, .. } if session == "tool-sess")
    });
    assert!(
        completed,
        "session must reach a terminal state; events: {collected:?}"
    );
}
