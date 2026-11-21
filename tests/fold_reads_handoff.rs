//! core#1418 (coord review of #1739, F3): the fold's pinned validator runs off the actor, and a
//! result whose run was relaunched while its validator ran never folds into the new life.
//!
//! Through the whole engine: a real git repository, a one-phase workflow pinning an approved
//! validator, and a runner whose second creator call (the relaunched life's) is held. The
//! validator waits in the worktree until the test lets it go, so the cancel and the relaunch are
//! both processed while it runs.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use wicked_core::{
    store_validator, Core, DeterministicValidator, EntityMode, HumanConfirm, LaunchSpec, RepoSpec,
    SessionStatus, StepInput, StepOutput, StepRunner, StepStatus,
};
use wicked_council::types::{Category, Confidence, Dispatcher, InputMode, Vote};
use wicked_council::{AgenticCli, CouncilTask};

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

/// Answers every step `Ok`. The second creator call (the relaunched life's) is held until
/// `release` is set, so only the earlier life's result could complete the run meanwhile.
struct HeldSecondRunner {
    creator_calls: Arc<AtomicUsize>,
    release: Arc<AtomicBool>,
}
impl StepRunner for HeldSecondRunner {
    fn run_unit(&self, input: &StepInput) -> StepOutput {
        let ok = |output: &str| StepOutput {
            run_id: input.run_id.clone(),
            unit_ix: input.unit_ix,
            attempt: input.attempt,
            output: output.into(),
            status: StepStatus::Ok,
            usage: None,
            files: Vec::new(),
            tools: Vec::new(),
            governed: false,
        };
        // The engine's judge session gets a well-formed PASS.
        if input.unit.session_id == "validator" {
            return ok("PASS\nthe work meets the criterion\nPASS");
        }
        if self.creator_calls.fetch_add(1, Ordering::SeqCst) >= 1 {
            let deadline = Instant::now() + Duration::from_secs(120);
            while !self.release.load(Ordering::SeqCst) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        ok("ok")
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

fn make_git_repo(name: &str) -> PathBuf {
    let repo =
        std::env::temp_dir().join(format!("wicked-core-handoff-{name}-{}", std::process::id()));
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

fn spec(session_id: &str, repo_ref: &str) -> LaunchSpec {
    LaunchSpec {
        base_ref: None,
        project_id: None,
        problem: "Do the one task".into(),
        clis: vec![cli("a")],
        entity_mode: EntityMode::Shared,
        session_id: session_id.into(),
        human_confirm: HumanConfirm::None,
        auto_deliver: false,
        repo_ref: Some(repo_ref.into()),
        workflow: Some("handoff-pinned".into()),
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
        voice: None,
        deliverables: Vec::new(),
        qe_acceptance: Default::default(),
    }
}

/// The run's state a fold changes: its status, cursor and attempt, and each unit's status.
fn state_of(core: &Core, run_id: &str) -> String {
    core.sessions_detail()
        .unwrap()
        .into_iter()
        .find(|v| v.session.id == run_id)
        .map(|v| {
            format!(
                "{:?} unit_ix={} attempt={} units={:?}",
                v.session.status,
                v.session.unit_ix,
                v.session.attempt,
                v.units.iter().map(|u| u.status).collect::<Vec<_>>()
            )
        })
        .unwrap_or_default()
}

/// Poll `cond` until it holds; the bound only fails a run that never gets there.
fn wait_until(what: &str, cond: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(120);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn file(dir: &Path, name: &str) -> PathBuf {
    dir.join(name)
}

#[test]
fn a_result_whose_run_was_relaunched_while_its_validator_ran_never_folds() {
    let repo = make_git_repo("relaunch");
    let dir = std::env::temp_dir().join(format!("wicked-core-handoff-db-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("estate.db").to_str().unwrap().to_string();

    // The validator says it started, then waits in the worktree until the test lets it go, and
    // says it ended. Relative paths: it runs with the run's worktree as its working directory. No
    // `>` or `$(`: the validator denylist refuses them (`validator::looks_dangerous`). The engine's
    // validator timeout bounds the wait.
    let pin = {
        let mut store = wicked_apps_core::open_store(Some(&db)).unwrap();
        let pin = store_validator(
            &mut store,
            &DeterministicValidator {
                criterion: "the work is checked".into(),
                script:
                    "touch started; while [ ! -f go ]; do sleep 0.05; done; touch ended; exit 0"
                        .into(),
                approved: true,
            },
        )
        .unwrap();
        drop(store);
        pin
    };
    let def = serde_json::json!({
        "id": "handoff-pinned",
        "phases": [ { "id": "build", "kind": "build", "validator_pin": pin } ]
    })
    .to_string();

    let creator_calls = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(AtomicBool::new(false));
    let core = Core::spawn_with_engine(
        db,
        Arc::new(StubDispatcher),
        Arc::new(HeldSecondRunner {
            creator_calls: creator_calls.clone(),
            release: release.clone(),
        }),
    );
    core.register_workflow(&def).unwrap();
    let entry = core
        .register_repo(RepoSpec {
            name: "handoff".into(),
            root_path: repo.to_str().unwrap().into(),
            registered_at: 0,
        })
        .unwrap();
    let wt = repo.join("wicked-worktrees").join("r-handoff");

    core.launch_run(spec("r-handoff", &entry.id))
        .expect("launch");
    wait_until("the first life's validator to start", || {
        file(&wt, "started").is_file()
    });

    // The validator is running; the actor is not inside it (#1418): the cancel answers at once.
    // Before, the actor ran the validator itself, so the cancel waited for it to end.
    let t = Instant::now();
    assert_eq!(
        core.cancel_run("r-handoff").unwrap(),
        SessionStatus::Cancelled
    );
    assert!(
        t.elapsed() < Duration::from_secs(20),
        "the cancel waited {:?} behind the pinned validator",
        t.elapsed()
    );

    // Relaunch the same id while the first life's validator still runs. Its creator call is held.
    core.launch_run(spec("r-handoff", &entry.id))
        .expect("relaunch");
    wait_until("the relaunched life's creator call", || {
        creator_calls.load(Ordering::SeqCst) >= 2
    });
    assert!(
        !file(&wt, "ended").exists(),
        "the first validator is still running"
    );
    let relaunched = state_of(&core, "r-handoff");

    // Let the first life's validator end. Its result comes back to a run that was relaunched.
    std::fs::write(file(&wt, "go"), "").unwrap();
    wait_until("the first life's validator to end", || {
        file(&wt, "ended").is_file()
    });
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(
        state_of(&core, "r-handoff"),
        relaunched,
        "the earlier life's result folded into the relaunched run"
    );

    release.store(true, Ordering::SeqCst);
    let _ = core.cancel_run("r-handoff");
    drop(core);
    let _ = std::fs::remove_dir_all(&repo);
    let _ = std::fs::remove_dir_all(&dir);
}
