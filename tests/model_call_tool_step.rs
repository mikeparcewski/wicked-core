//! (DES-j6-build D1, seam C0; coord fold 1) A repo-defined Tool step cannot spend seat model calls.
//!
//! The model-call channel is armed only for a declared step of a SHIPPED preset
//! (`model_call::MODEL_CALL_STEPS`, the built-in step's command byte-for-byte; unit-tested in
//! `model_call.rs`). Here a registered workflow's Tool phase runs the real `wicked-core model-call`
//! client: the engine arms no channel for it, so the client refuses (`WICKED_MODEL_CALL_SOCK is
//! not set`), no seat runs and no answer is written.
//!
//! Were the channel armed, the seat registry would be the engine's own (built-ins plus the user
//! `clis.toml` under `$HOME`), so run this test with `HOME` pointed at a scratch directory.
#![cfg(unix)]

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use wicked_core::{
    Core, CoreEvent, EntityMode, HumanConfirm, LaunchSpec, StepInput, StepOutput, StepRunner,
    StepStatus,
};
use wicked_council::types::{Category, Confidence, Dispatcher, InputMode, Vote};
use wicked_council::{AgenticCli, CouncilTask};

const BIN: &str = env!("CARGO_BIN_EXE_wicked-core");

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("wicked-core-mc-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
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

struct NoCouncil;
impl Dispatcher for NoCouncil {
    fn dispatch(&self, _: &AgenticCli, _: &CouncilTask) -> Option<Vote> {
        None
    }
}

/// The seats: each answers `<seat>: <prompt>` and records the seat, the asking run and the prompt.
#[derive(Default)]
struct Seats {
    seen: Mutex<Vec<(Option<String>, Option<String>, String)>>,
}

impl StepRunner for Seats {
    fn run_unit(&self, i: &StepInput) -> StepOutput {
        self.seen.lock().unwrap().push((
            i.unit.assigned_cli.clone(),
            i.unit.parent_run_id.clone(),
            i.unit.description.clone(),
        ));
        StepOutput {
            run_id: i.run_id.clone(),
            unit_ix: i.unit_ix,
            attempt: i.attempt,
            output: format!(
                "{}: {}",
                i.unit.assigned_cli.clone().unwrap_or_default(),
                i.unit.description
            ),
            status: StepStatus::Ok,
            usage: None,
            files: vec![],
            tools: Vec::new(),
            governed: false,
        }
    }
}

fn wait_terminal(events: &std::sync::mpsc::Receiver<CoreEvent>, session: &str) -> CoreEvent {
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        match events.recv_timeout(Duration::from_millis(500)) {
            Ok(ev) => {
                let terminal = matches!(&ev, CoreEvent::SessionCompleted { session: s } if s == session)
                    || matches!(&ev, CoreEvent::SessionFailed { session: s, .. } if s == session)
                    || matches!(&ev, CoreEvent::RunCancelled { session: s, .. } if s == session)
                    || matches!(&ev, CoreEvent::AwaitingHuman { session: s, .. } if s == session);
                if terminal {
                    return ev;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(e) => panic!("event stream closed: {e}"),
        }
    }
    panic!("run {session} reached no terminal state in 60 s");
}

#[test]
fn a_repo_defined_tool_step_gets_no_model_call_channel() {
    let dir = scratch("repo");
    let seats = Arc::new(Seats::default());
    let core = Core::spawn_with_engine(
        dir.join("estate.db").to_str().unwrap().to_string(),
        Arc::new(NoCouncil),
        seats.clone(),
    );
    std::fs::write(dir.join("write.txt"), "Write one sentence.").unwrap();
    let call = |family: &str, out: &str| {
        serde_json::json!([
            BIN,
            "model-call",
            "--family",
            family,
            "--in",
            dir.join("write.txt").to_str().unwrap(),
            "--out",
            dir.join(out).to_str().unwrap(),
            "--timeout-s",
            "30"
        ])
    };
    let def = serde_json::json!({
        "id": "test-model-call",
        "phases": [
            {"id": "write", "kind": "test", "gate": "auto",
             "executor": {"type": "tool", "cmd": call("claude", "write.out")}},
            {"id": "check", "kind": "test", "gate": "auto",
             "executor": {"type": "tool", "cmd": call("codex", "check.out")}}
        ]
    });
    core.register_workflow(&def.to_string())
        .expect("register workflow");
    let ev = core.subscribe();
    core.launch_run(LaunchSpec {
        base_ref: None,
        project_id: None,
        problem: "Make two model calls.".into(),
        clis: vec![cli("claude"), cli("codex")],
        entity_mode: EntityMode::Shared,
        session_id: "mc-sess".into(),
        human_confirm: HumanConfirm::None,
        auto_deliver: false,
        repo_ref: None,
        workflow: Some("test-model-call".into()),
        extra_write_roots: Vec::new(),
        extra_read_roots: Vec::new(),
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
    let end = wait_terminal(&ev, "mc-sess");
    assert!(
        !matches!(end, CoreEvent::SessionCompleted { .. }),
        "the client refuses, so the step fails: {end:?}"
    );
    assert!(!dir.join("write.out").exists(), "no answer was written");
    assert!(!dir.join("check.out").exists(), "no answer was written");
    assert!(
        seats.seen.lock().unwrap().is_empty(),
        "no seat ran a model call for a repo-defined step"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
