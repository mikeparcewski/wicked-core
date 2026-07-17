//! P3 proving test — the repo registry + git-worktree isolation.
//!
//! Register a real git repo, launch a run targeting it, and prove: a worktree is created at
//! `<repo>/.wicked/worktrees/<run_id>`, the worker receives that path as its `workdir`, the run's
//! work lands there, a COMPLETED run keeps its worktree (for review), and a CANCELLED run discards it.

use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use wicked_council::types::{Category, Confidence, Dispatcher, InputMode, Vote};
use wicked_council::{AgenticCli, CouncilTask};

use wicked_core::{
    Core, HumanConfirm, LaunchSpec, RepoSpec, SessionStatus, StepInput, StepOutput, StepRunner,
    StepStatus,
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

/// Records the workdir each unit received, and (proof the run really executes there) writes a file
/// into the workdir so the test can confirm work landed in the worktree.
struct WorkdirRunner {
    seen: Arc<Mutex<Vec<Option<PathBuf>>>>,
}
impl StepRunner for WorkdirRunner {
    fn run_unit(&self, input: &StepInput) -> StepOutput {
        self.seen.lock().unwrap().push(input.workdir.clone());
        if let Some(dir) = &input.workdir {
            let _ = std::fs::write(dir.join(format!("unit-{}.txt", input.unit.ord)), "done");
        }
        StepOutput {
            run_id: input.run_id.clone(),
            unit_ix: input.unit_ix,
            attempt: input.attempt,
            output: "ok".into(),
            status: StepStatus::Ok,
            usage: None,
            files: Vec::new(),
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
    }
}

/// Create a throwaway git repo with one commit; returns its absolute path.
fn make_git_repo(name: &str) -> PathBuf {
    let repo = std::env::temp_dir().join(format!("wicked-core-p3-{name}"));
    let _ = std::fs::remove_dir_all(&repo);
    std::fs::create_dir_all(&repo).unwrap();
    let git = |args: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(args)
            .output()
            .unwrap()
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "t@t"]);
    git(&["config", "user.name", "t"]);
    git(&["config", "commit.gpgsign", "false"]);
    std::fs::write(repo.join("README.md"), "hello").unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "init"]);
    repo
}

fn core_for(name: &str) -> (Core, Arc<Mutex<Vec<Option<PathBuf>>>>) {
    let dir = std::env::temp_dir().join(format!("wicked-core-p3db-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("estate.db").to_str().unwrap().to_string();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let core = Core::spawn_with_engine(
        db,
        Arc::new(StubDispatcher),
        Arc::new(WorkdirRunner { seen: seen.clone() }),
    );
    (core, seen)
}

fn spec(session_id: &str, repo_ref: Option<String>) -> LaunchSpec {
    LaunchSpec {
        problem: "Do the one task".into(),
        clis: vec![cli("a")],
        entity_mode: wicked_core::EntityMode::Shared,
        session_id: session_id.into(),
        human_confirm: HumanConfirm::None,
        repo_ref,
        workflow: None,
    }
}

fn wait_status(core: &Core, run_id: &str, want: SessionStatus) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Ok(views) = core.sessions_detail() {
            if let Some(v) = views.iter().find(|v| v.session.id == run_id) {
                if v.session.status == want {
                    return true;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    false
}

#[test]
fn register_validates_git_repo_and_lists_it() {
    let repo = make_git_repo("reg");
    let (core, _) = core_for("reg");

    let entry = core
        .register_repo(RepoSpec {
            name: "My Demo Repo".into(),
            root_path: repo.to_str().unwrap().into(),
            registered_at: 100,
        })
        .expect("register a valid git repo");
    assert_eq!(entry.id, "my-demo-repo", "id is a slug of the name");
    assert!(!entry.default_branch.is_empty());

    let repos = core.list_repos().unwrap();
    assert_eq!(repos.len(), 1);
    assert_eq!(repos[0].root_path, repo.to_str().unwrap());

    // A non-git path is rejected.
    let bad = std::env::temp_dir().join("wicked-core-p3-not-git");
    let _ = std::fs::create_dir_all(&bad);
    assert!(
        core.register_repo(RepoSpec {
            name: "nope".into(),
            root_path: bad.to_str().unwrap().into(),
            registered_at: 0,
        })
        .is_err(),
        "registering a non-git path must fail"
    );
}

#[test]
fn launch_in_repo_creates_worktree_and_passes_workdir() {
    let repo = make_git_repo("launch");
    let (core, seen) = core_for("launch");
    let entry = core
        .register_repo(RepoSpec {
            name: "proj".into(),
            root_path: repo.to_str().unwrap().into(),
            registered_at: 0,
        })
        .unwrap();

    core.launch_run(spec("run1", Some(entry.id.clone())))
        .expect("launch in repo");
    assert!(wait_status(&core, "run1", SessionStatus::Completed));

    let expected_wt = repo.join(".wicked").join("worktrees").join("run1");
    assert!(
        expected_wt.is_dir(),
        "worktree created at <repo>/.wicked/worktrees/<run_id>"
    );
    // The worker ran in that worktree and its work landed there.
    assert_eq!(
        seen.lock().unwrap().first().cloned().flatten(),
        Some(expected_wt.clone()),
        "the worker received the worktree as its workdir"
    );
    assert!(
        expected_wt.join("unit-1.txt").is_file(),
        "the unit's work landed in the worktree"
    );
    // A COMPLETED run keeps its worktree for review.
    assert!(
        expected_wt.is_dir(),
        "a completed run's worktree is preserved (not auto-removed)"
    );
    // And the session records its workdir.
    let views = core.sessions_detail().unwrap();
    let v = views.iter().find(|v| v.session.id == "run1").unwrap();
    assert_eq!(v.session.workdir.as_deref(), expected_wt.to_str());
    assert_eq!(v.session.repo_ref.as_deref(), Some(entry.id.as_str()));
}

#[test]
fn cancel_discards_the_worktree() {
    let repo = make_git_repo("cancel");
    let (core, _) = core_for("cancel");
    let entry = core
        .register_repo(RepoSpec {
            name: "proj".into(),
            root_path: repo.to_str().unwrap().into(),
            registered_at: 0,
        })
        .unwrap();

    // Pause before the first unit so the run is parked while we cancel it.
    let mut s = spec("run2", Some(entry.id.clone()));
    s.human_confirm = HumanConfirm::Before(1);
    core.launch_run(s).expect("launch");
    assert!(wait_status(&core, "run2", SessionStatus::AwaitingHuman));

    let wt = repo.join(".wicked").join("worktrees").join("run2");
    assert!(wt.is_dir(), "worktree exists while the run is parked");

    assert_eq!(core.cancel_run("run2").unwrap(), SessionStatus::Cancelled);
    // Give the (synchronous) cancel cleanup a beat, then assert the worktree is gone.
    assert!(
        !wt.is_dir(),
        "cancelling a run discards its worktree (work abandoned)"
    );
}
