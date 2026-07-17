//! P4a proving test — the FIRST demonstrably-functional end-to-end slice.
//!
//! Register a real git repo, launch a run that targets it, and let the REAL wrapped-CLI runner
//! (`WrappedCliStepRunner`) execute an actual CLI (`echo`) in the run's worktree. Prove the real
//! stdout flows through the governance gate into the persisted `work_output`, and the run completes.
//! (Unix-only: uses the `echo` binary. The stub dispatcher avoids the council's subprocess voting.)

#![cfg(unix)]

use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use wicked_council::types::{Category, Confidence, Dispatcher, InputMode, Vote};
use wicked_council::{AgenticCli, CouncilTask};

use wicked_core::{Core, HumanConfirm, LaunchSpec, SessionStatus, WrappedCliStepRunner};

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

/// A council seat keyed `echo` — the wrapped runner resolves it to `echo {PROMPT}` (not registered ⇒
/// key-as-binary fallback) and runs the real `echo`.
fn echo_cli() -> AgenticCli {
    AgenticCli {
        key: "echo".into(),
        display_name: "echo".into(),
        binary: "echo".into(),
        headless_invocation: "echo {PROMPT}".into(),
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

fn make_git_repo(name: &str) -> std::path::PathBuf {
    let repo = std::env::temp_dir().join(format!("wicked-core-p4a-{name}"));
    let _ = std::fs::remove_dir_all(&repo);
    std::fs::create_dir_all(&repo).unwrap();
    let git = |a: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(a)
            .output()
            .unwrap()
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "t@t"]);
    git(&["config", "user.name", "t"]);
    git(&["config", "commit.gpgsign", "false"]);
    std::fs::write(repo.join("README.md"), "x").unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "init"]);
    repo
}

fn wait_status(core: &Core, run_id: &str, want: SessionStatus) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Ok(views) = core.sessions_detail() {
            if let Some(v) = views.iter().find(|v| v.session.id == run_id) {
                if v.session.status == want {
                    return true;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

#[test]
fn real_cli_runs_in_the_worktree_and_output_is_governed_and_persisted() {
    let repo = make_git_repo("e2e");
    let dir = std::env::temp_dir().join("wicked-core-p4a-db");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("estate.db").to_str().unwrap().to_string();

    // REAL wrapped-CLI runner + a stub dispatcher (so distribution doesn't spawn council subprocesses).
    let core = Core::spawn_with_engine(
        db,
        Arc::new(StubDispatcher),
        Arc::new(WrappedCliStepRunner::default()),
    );

    let entry = core
        .register_repo(wicked_core::RepoSpec {
            name: "e2e".into(),
            root_path: repo.to_str().unwrap().into(),
            registered_at: 0,
        })
        .expect("register repo");

    // One unit: the planner produces a single unit whose description becomes the echo prompt.
    core.launch_run(LaunchSpec {
        problem: "wicked-orchestrator-marker".into(),
        clis: vec![echo_cli()],
        entity_mode: wicked_core::EntityMode::Shared,
        session_id: "run".into(),
        human_confirm: HumanConfirm::None,
        repo_ref: Some(entry.id.clone()),
        workflow: None,
    })
    .expect("launch");

    assert!(
        wait_status(&core, "run", SessionStatus::Completed),
        "the real-CLI run reaches Completed"
    );

    // The unit's persisted work_output is the REAL stdout of `echo`, containing the prompt text.
    let views = core.sessions_detail().unwrap();
    let v = views.iter().find(|v| v.session.id == "run").unwrap();
    let unit = &v.units[0];
    assert_eq!(
        unit.status,
        wicked_core::UnitStatus::Done,
        "the unit was governance-approved"
    );
    let out = core
        .work_output(&unit.id)
        .expect("the approved unit has a persisted work_output");
    assert!(
        out.contains("wicked-orchestrator-marker"),
        "the work output is the real echo stdout (got: {out:?})"
    );

    // The run executed in the repo's isolated worktree.
    assert!(
        repo.join(".wicked").join("worktrees").join("run").is_dir(),
        "the run used the repo's worktree"
    );
    assert!(
        v.session.workdir.is_some(),
        "the session records its workdir"
    );
}
