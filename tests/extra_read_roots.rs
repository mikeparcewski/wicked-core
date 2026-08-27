//! core#294 — `extraReadRoots`, the launch-declared READ-ONLY mirror of `extraWriteRoots`.
//!
//! The constraint space this closes (crew#311 → core#293): a run that must be GROUNDED in repo
//! content without binding its worktree had exactly three options, and all three were wrong —
//! `repoRef` killed the ACP permission stream, unbound absolute reads were denied by the boundary,
//! and `extraWriteRoots` on the repo root worked only by handing a doc-draft worker WRITE access
//! to the tree it was supposed to read.
//!
//! What must hold at the LAUNCH seam (the boundary semantics themselves are proven in
//! `execute_wrapped`'s unit tests, against the same two helpers the launcher calls):
//! 1. An unusable read root REJECTS the launch — synchronously, with NO session persisted. The
//!    same contract the write half carries: an invalid boundary declaration must never become a
//!    live run whose grounding silently did not happen.
//! 2. A valid one is PERSISTED on the session, so a resume/redrive re-arms the same grounding
//!    rather than depending on the daemon's memory of the launch.
//! 3. Declaring read roots widens NOTHING on the write side.

use std::sync::Arc;

use wicked_council::types::{Category, Confidence, Dispatcher, InputMode, Vote};
use wicked_council::{AgenticCli, CouncilTask};

use wicked_core::{Core, HumanConfirm, LaunchSpec, StepInput, StepOutput, StepRunner, StepStatus};

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

struct OkRunner;
impl StepRunner for OkRunner {
    fn run_unit(&self, input: &StepInput) -> StepOutput {
        StepOutput {
            run_id: input.run_id.clone(),
            unit_ix: input.unit_ix,
            attempt: input.attempt,
            output: "ok".into(),
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

fn spec(session_id: &str, read_roots: Vec<String>) -> LaunchSpec {
    LaunchSpec {
        problem: "Draft the overview. Review the draft".into(),
        clis: vec![cli("a")],
        entity_mode: wicked_core::EntityMode::Shared,
        session_id: session_id.into(),
        human_confirm: HumanConfirm::All, // pause immediately; this test is about the LAUNCH seam
        repo_ref: None,
        workflow: None,
        project_id: None,
        extra_write_roots: Vec::new(),
        extra_read_roots: read_roots,
        project_graph: None,
    }
}

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("wicked-core-xrr-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::canonicalize(&dir).unwrap()
}

fn spawn(db: &std::path::Path) -> Core {
    Core::spawn_with_engine(
        db.to_str().unwrap().to_string(),
        Arc::new(StubDispatcher),
        Arc::new(OkRunner),
    )
}

/// `$HOME` is the prerequisite the boundary validation fails CLOSED without — and it is unset on
/// Windows, where the shell spells it `USERPROFILE`. Give the process one (only when it has none)
/// so the accept-path assertions below judge the ROOT, not the environment.
fn ensure_home() {
    if std::env::var_os("HOME").is_none() {
        std::env::set_var("HOME", scratch("home"));
    }
}

/// (1) + (3). A relative read root names no tree anyone declared — it would bind to whatever cwd
/// the launcher happened to have — so the launch is refused before a session exists.
#[test]
fn an_unusable_read_root_refuses_the_launch_with_no_session() {
    let db = scratch("reject").join("estate.db");
    let core = spawn(&db);

    let err = core
        .launch_run(spec("run-bad-read", vec!["relative/reference".into()]))
        .expect_err("a relative read root must refuse the launch");
    assert!(
        err.to_string().contains("read root"),
        "the failure must name WHICH declaration was refused, got: {err}"
    );
    assert!(
        !core
            .sessions()
            .unwrap()
            .contains(&"run-bad-read".to_string()),
        "a refused launch must persist NO session — a live run whose grounding silently did not \
         happen is worse than no run"
    );
}

/// (2) + (3). A valid read root rides the session, and touches the write half not at all.
#[test]
fn a_valid_read_root_is_persisted_on_the_session_and_widens_no_writes() {
    ensure_home();
    let db = scratch("accept").join("estate.db");
    let reference = scratch("reference-repo");
    let core = spawn(&db);

    let declared = reference.to_string_lossy().into_owned();
    core.launch_run(spec("run-grounded", vec![declared.clone()]))
        .expect("an absolute reference root outside the pin tree must be admitted");

    let views = core.sessions_detail().expect("sessions_detail");
    let session = &views
        .iter()
        .find(|v| v.session.id == "run-grounded")
        .expect("the launched run is persisted")
        .session;

    assert_eq!(
        session.extra_read_roots,
        vec![declared],
        "the declared grounding must survive on the session, so a resume re-arms the SAME \
         boundary rather than trusting the daemon's memory of the launch"
    );
    assert!(
        session.extra_write_roots.is_empty(),
        "declaring READ roots must widen nothing on the write side — that inversion is the whole \
         defect core#294 closes"
    );
}
