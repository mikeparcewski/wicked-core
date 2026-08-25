//! DES-PROJECT-001 proving test — the Project model as ENGINE behavior.
//!
//! What must hold (the engine's slice of the ADR §8 e2e):
//! 1. `LaunchSpec.project_id` attaches the `crew.run` membership ATOMICALLY with the launch
//!    record; an unknown or archived project rejects the launch with NO session persisted.
//! 2. A human gate writes a DURABLE open `interaction_request` in the same batch as the
//!    `AwaitingHuman` transition — and it SURVIVES an engine restart (drop the Core, re-spawn on
//!    the same db): the ephemeral-GateCache fix, proven at the store.
//! 3. `confirm_gate` resolves the request `answered` (with the decision payload); `cancel_run`
//!    resolves a still-open one `cancelled`.
//! 4. A project-bound run's terminal outcome memory lands under `project:<id>/run:<run_id>`
//!    (the foundation record, §3.2).

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use wicked_council::types::{Category, Confidence, Dispatcher, InputMode, Vote};
use wicked_council::{AgenticCli, CouncilTask};

use wicked_core::{
    Core, HumanConfirm, HumanDecision, InteractionStatus, LaunchSpec, ProjectPatch, ProjectStatus,
    SessionStatus, StepInput, StepOutput, StepRunner, StepStatus, MEMBER_KIND_RUN,
};

struct StubDispatcher;
impl Dispatcher for StubDispatcher {
    fn dispatch(&self, cli: &AgenticCli, _t: &CouncilTask) -> Option<Vote> {
        Some(Vote {
            cli: cli.key.clone(),
            recommendation: "x".into(),
            top_risk: "none".into(),
            change_my_mind: "no".into(),
            disqualifier: None,
            confidence: Confidence::default(),
            provenance: "stub".into(),
        })
    }
}

struct OkRunner {
    ran: Arc<Mutex<Vec<usize>>>,
}
impl StepRunner for OkRunner {
    fn run_unit(&self, input: &StepInput) -> StepOutput {
        self.ran.lock().unwrap().push(input.unit_ix);
        StepOutput {
            run_id: input.run_id.clone(),
            unit_ix: input.unit_ix,
            attempt: input.attempt,
            output: format!("did: {}", input.unit.description),
            status: StepStatus::Ok,
            usage: None,
            files: Vec::new(),
            tools: Vec::new(),
            governed: false,
        }
    }
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
    }
}

fn spec(session_id: &str, hc: HumanConfirm, project_id: Option<String>) -> LaunchSpec {
    LaunchSpec {
        problem: "Do step one. Do step two".into(),
        clis: vec![cli("a"), cli("b")],
        entity_mode: wicked_core::EntityMode::Shared,
        session_id: session_id.into(),
        human_confirm: hc,
        repo_ref: None,
        workflow: None,
        project_id,
        extra_write_roots: Vec::new(),
        project_graph: None,
    }
}

/// A fresh temp db path (NOT `:memory:` — restart survival needs a file to re-open).
fn fresh_db(name: &str) -> String {
    let dir =
        std::env::temp_dir().join(format!("wicked-core-project-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("estate.db").to_str().unwrap().to_string()
}

fn spawn(db: &str) -> Core {
    Core::spawn_with_engine(
        db.to_string(),
        Arc::new(StubDispatcher),
        Arc::new(OkRunner {
            ran: Arc::new(Mutex::new(Vec::new())),
        }),
    )
}

const WAIT_BUDGET: Duration = Duration::from_secs(20);

fn wait_status(core: &Core, run_id: &str, want: SessionStatus) -> bool {
    let start = Instant::now();
    let mut last: Option<SessionStatus> = None;
    while start.elapsed() < WAIT_BUDGET {
        if let Ok(views) = core.sessions_detail() {
            if let Some(v) = views.iter().find(|v| v.session.id == run_id) {
                if v.session.status == want {
                    return true;
                }
                last = Some(v.session.status);
            }
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    eprintln!("wait_status({run_id}): timed out waiting for {want:?}; last observed {last:?}");
    false
}

/// Read helper: open the SAME db read-only (what the napi read surface does).
fn ro(db: &str) -> wicked_apps_core::SqliteStore {
    wicked_apps_core::open_store_ro(Some(db)).expect("read-only store opens")
}

#[test]
fn launch_with_project_attaches_membership_atomically() {
    let db = fresh_db("attach");
    let core = spawn(&db);
    let project = core.project_create("keystone", None).expect("create");
    assert_eq!(project.status, ProjectStatus::Active);
    assert_eq!(project.scope, format!("project:{}", project.id));

    core.launch_run(spec("run-a", HumanConfirm::None, Some(project.id.clone())))
        .expect("launch");
    // The membership is written in the SAME batch as the launch stub — visible immediately.
    let members = wicked_core::list_members(&ro(&db), &project.id).expect("members");
    assert_eq!(members.len(), 1);
    assert_eq!(members[0].member_kind, MEMBER_KIND_RUN);
    assert_eq!(members[0].member_ref, "run-a");
    assert!(wait_status(&core, "run-a", SessionStatus::Completed));
}

#[test]
fn launch_into_unknown_or_archived_project_fails_with_no_session() {
    let db = fresh_db("fail-closed");
    let core = spawn(&db);

    // Unknown project → synchronous Err, no session persisted.
    let err = core
        .launch_run(spec("run-x", HumanConfirm::None, Some("proj_nope".into())))
        .unwrap_err();
    assert!(err.to_string().contains("not registered"), "got: {err}");
    assert!(
        !core.sessions().unwrap().contains(&"run-x".to_string()),
        "a refused launch must persist NO session"
    );

    // Archived project → blocked the same way (ADR §1.3: archive blocks new attachments).
    let project = core.project_create("done-pile", None).expect("create");
    core.project_update(
        &project.id,
        ProjectPatch {
            status: Some(ProjectStatus::Archived),
            ..Default::default()
        },
    )
    .expect("archive");
    let err = core
        .launch_run(spec("run-y", HumanConfirm::None, Some(project.id.clone())))
        .unwrap_err();
    assert!(err.to_string().contains("archived"), "got: {err}");
    assert!(!core.sessions().unwrap().contains(&"run-y".to_string()));
}

#[test]
fn gate_prompt_is_durable_survives_restart_and_resolves_answered() {
    let db = fresh_db("durable-gate");
    let project_id;
    {
        let core = spawn(&db);
        let project = core.project_create("keystone", None).expect("create");
        project_id = project.id.clone();
        core.launch_run(spec(
            "run-g",
            HumanConfirm::Before(1),
            Some(project.id.clone()),
        ))
        .expect("launch");
        assert!(wait_status(&core, "run-g", SessionStatus::AwaitingHuman));

        let open =
            wicked_core::list_interactions(&ro(&db), Some("run-g"), Some(InteractionStatus::Open))
                .expect("interactions");
        assert_eq!(open.len(), 1, "the pause wrote ONE durable open request");
        assert_eq!(open[0].ord, Some(1));
        assert!(!open[0].prompt.is_empty(), "the prompt text is durable");
        // Drop the Core → actor shuts down → "daemon restart".
    }
    {
        // ── RESTART SURVIVAL: a fresh engine on the same db still has the open prompt. ──
        let core = spawn(&db);
        let open =
            wicked_core::list_interactions(&ro(&db), Some("run-g"), Some(InteractionStatus::Open))
                .expect("interactions after restart");
        assert_eq!(
            open.len(),
            1,
            "the open prompt must survive an engine restart (the ephemeral-GateCache fix)"
        );

        // Answer it from the fresh process — the run resumes and completes.
        let status = core
            .confirm_gate("run-g", HumanDecision::Approve { amend: None })
            .expect("confirm after restart");
        assert_eq!(status, SessionStatus::Executing);
        assert!(wait_status(&core, "run-g", SessionStatus::Completed));

        let all = wicked_core::list_interactions(&ro(&db), Some("run-g"), None).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].status, InteractionStatus::Answered);
        let answer: serde_json::Value =
            serde_json::from_str(all[0].answer.as_deref().unwrap()).unwrap();
        assert_eq!(answer["approve"], serde_json::Value::Bool(true));
        assert!(all[0].resolved_at.is_some());
        assert!(
            wicked_core::list_interactions(&ro(&db), None, Some(InteractionStatus::Open))
                .unwrap()
                .is_empty(),
            "nothing is left open once answered"
        );

        // §3.2: the project-bound run's outcome memory carries the project scope.
        let scoped = core
            .list_memories(&format!("project:{project_id}"), 10)
            .expect("list_memories");
        assert!(
            scoped.iter().any(|m| m.content.contains("run-g")),
            "the run outcome must be captured under project:{project_id} (got {scoped:?})"
        );
    }
}

#[test]
fn cancel_resolves_open_prompt_cancelled() {
    let db = fresh_db("cancel-gate");
    let core = spawn(&db);
    core.launch_run(spec("run-c", HumanConfirm::Before(1), None))
        .expect("launch");
    assert!(wait_status(&core, "run-c", SessionStatus::AwaitingHuman));
    core.cancel_run("run-c").expect("cancel");
    let all = wicked_core::list_interactions(&ro(&db), Some("run-c"), None).unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(
        all[0].status,
        InteractionStatus::Cancelled,
        "a cancelled run's open prompt must not stay renderable"
    );
}
