//! (core#1390) A dispatch's worktree reads run off the actor: while a unit's probe is out, the
//! actor answers; when it lands, the unit runs on the tree it read; a probe whose run moved on is
//! dropped.

use super::HOLD;
use crate::workflow::{StepInput, StepOutput, StepRunner, StepStatus};
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct NoCouncil;
impl wicked_council::types::Dispatcher for NoCouncil {
    fn dispatch(
        &self,
        cli: &crate::AgenticCli,
        _t: &wicked_council::CouncilTask,
    ) -> Option<wicked_council::types::Vote> {
        Some(wicked_council::types::Vote {
            cli: cli.key.clone(),
            recommendation: "x".into(),
            top_risk: "none".into(),
            change_my_mind: "no".into(),
            disqualifier: None,
            confidence: Default::default(),
            provenance: "stub".into(),
        })
    }
}

/// Records the units it ran and the baseline each was handed; answers with prose.
#[derive(Default)]
struct Seat {
    ran: Mutex<Vec<(u32, Option<String>)>>,
}
impl StepRunner for Seat {
    fn run_unit(&self, i: &StepInput) -> StepOutput {
        self.ran.lock().unwrap().push((
            i.unit.ord,
            i.unit.worktree_baseline.as_ref().map(|b| b.tree.clone()),
        ));
        StepOutput {
            run_id: i.run_id.clone(),
            unit_ix: i.unit_ix,
            attempt: i.attempt,
            output: "Did the step as asked; nothing else to report. ".repeat(8),
            status: StepStatus::Ok,
            usage: None,
            files: Vec::new(),
            tools: Vec::new(),
            governed: false,
        }
    }
}

fn git(dir: &std::path::Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git runs");
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn cli(key: &str) -> crate::AgenticCli {
    use wicked_council::types::{Category, Confidence, InputMode};
    crate::AgenticCli {
        key: key.into(),
        display_name: key.into(),
        binary: "/bin/true".into(),
        headless_invocation: "/bin/true {PROMPT}".into(),
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

const ONE_STEP: &str = r#"{"id":"probe-one","phases":[
  {"id":"one","kind":"build","gate":"auto","role":"neutral"}]}"#;

struct Fixture {
    dir: std::path::PathBuf,
    core: crate::Core,
    seat: Arc<Seat>,
    repo_ref: String,
}

fn fixture(tag: &str) -> Fixture {
    let dir = std::env::temp_dir().join(format!(
        "wicked-core-probe-{tag}-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    let repo = dir.join("repo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.email", "t@example.invalid"]);
    git(&repo, &["config", "user.name", "t"]);
    git(&repo, &["config", "commit.gpgsign", "false"]);
    std::fs::write(repo.join("src/a.ts"), "export const a = 1;\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "init"]);
    let seat = Arc::new(Seat::default());
    let core = crate::Core::spawn_with_engine(
        dir.join("estate.db").to_string_lossy().into_owned(),
        Arc::new(NoCouncil),
        seat.clone(),
    );
    core.register_workflow(ONE_STEP).expect("def");
    let entry = core
        .register_repo(crate::RepoSpec {
            name: format!("probe-{tag}"),
            root_path: repo.to_string_lossy().into_owned(),
            registered_at: 1,
        })
        .expect("register");
    Fixture {
        dir,
        core,
        seat,
        repo_ref: entry.id,
    }
}

fn launch(f: &Fixture, run: &str) {
    launch_with(f, run, crate::HumanConfirm::None)
}

fn launch_with(f: &Fixture, run: &str, human_confirm: crate::HumanConfirm) {
    f.core
        .launch_run(crate::LaunchSpec {
            base_ref: None,
            project_id: None,
            problem: "Do the one step".into(),
            clis: vec![cli("a"), cli("b")],
            entity_mode: crate::EntityMode::Shared,
            session_id: run.into(),
            human_confirm,
            auto_deliver: false,
            repo_ref: Some(f.repo_ref.clone()),
            workflow: Some("probe-one".into()),
            extra_write_roots: Vec::new(),
            extra_read_roots: Vec::new(),
            project_graph: None,
            plan: None,
            deliver_step: None,
            exclude_seats: Vec::new(),
            evidence_root: None,
            primary: None,
            reduced_assurance: true,
            power: false,
            voice: None,
            deliverables: Vec::new(),
            qe_acceptance: Default::default(),
        })
        .expect("launch");
}

/// Park the next probe of `run`; returns (entered, release).
fn hold(run: &str) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
    let (entered_tx, entered) = channel();
    let (release, release_rx) = channel();
    HOLD.lock()
        .unwrap()
        .get_or_insert_with(Default::default)
        .insert(run.to_string(), (entered_tx, release_rx));
    (entered, release)
}

fn status(f: &Fixture, run: &str) -> Option<crate::SessionStatus> {
    f.core
        .sessions_detail()
        .ok()?
        .into_iter()
        .find(|v| v.session.id == run)
        .map(|v| v.session.status)
}

#[test]
fn the_actor_answers_while_a_dispatch_reads_its_worktree_and_the_unit_runs_on_that_read() {
    let f = fixture("held");
    let run = format!("probe-held-{}", std::process::id());
    let (entered, release) = hold(&run);
    launch(&f, &run);
    entered
        .recv_timeout(Duration::from_secs(60))
        .expect("the dispatch hands its worktree read to the probe thread");
    // While the probe is held, the tree changes: the baseline must be the tree as the probe reads
    // it after release, not a tree read before.
    let view = f
        .core
        .sessions_detail()
        .unwrap()
        .into_iter()
        .find(|v| v.session.id == run)
        .expect("the run");
    let wt = std::path::PathBuf::from(view.session.workdir.clone().expect("a worktree"));
    assert_eq!(
        view.units[0].last_attempt, None,
        "the first pass persists nothing of the dispatch"
    );
    std::fs::write(wt.join("src/while-held.ts"), "export const h = 1;\n").unwrap();

    // The probe is out — on the actor's old path this was a snapshot on the actor thread, and
    // every command waited behind it.
    let t = Instant::now();
    f.core.ping();
    assert!(
        t.elapsed() < Duration::from_secs(2),
        "the actor answers while the probe reads ({:?})",
        t.elapsed()
    );
    assert!(
        f.seat.ran.lock().unwrap().is_empty(),
        "nothing runs before the probe lands"
    );

    release.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    while f.seat.ran.lock().unwrap().is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let ran = f.seat.ran.lock().unwrap().clone();
    assert_eq!(ran.len(), 1, "the unit runs once the probe lands: {ran:?}");
    let expected = crate::worktree_guard::snapshot(&wt, &f.dir.join("repo"))
        .expect("snapshot")
        .tree;
    assert_eq!(
        ran[0].1.as_deref(),
        Some(expected.as_str()),
        "it ran on the baseline the probe read after release (with the file written meanwhile)"
    );
    let stats = f.core.actor_stats();
    assert!(
        stats.commands.iter().any(|c| c.command == "DispatchProbed"),
        "the dispatch finished on the probe's answer: {stats:?}"
    );
    drop(f.core);
    let _ = std::fs::remove_dir_all(&f.dir);
}

#[test]
fn a_probe_whose_run_was_cancelled_meanwhile_is_dropped() {
    let f = fixture("cancel");
    let run = format!("probe-cancel-{}", std::process::id());
    let (entered, release) = hold(&run);
    launch(&f, &run);
    entered
        .recv_timeout(Duration::from_secs(60))
        .expect("the probe is out");
    f.core.cancel_run(&run).expect("cancel");
    release.send(()).unwrap();
    // The probe lands on a cancelled run: nothing is dispatched.
    let deadline = Instant::now() + Duration::from_secs(30);
    let answered = || {
        f.core
            .actor_stats()
            .commands
            .iter()
            .any(|c| c.command == "DispatchProbed")
    };
    while Instant::now() < deadline && !answered() {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(answered(), "the probe's answer landed (and was dropped)");
    std::thread::sleep(Duration::from_millis(200));
    assert!(f.seat.ran.lock().unwrap().is_empty(), "no unit ran");
    assert_eq!(status(&f, &run), Some(crate::SessionStatus::Cancelled));
    drop(f.core);
    let _ = std::fs::remove_dir_all(&f.dir);
}

/// (review of #1411, P1) A probe of a cancelled life of a run id never dispatches into a fresh
/// launch of the same id — not even one paused at a human gate on the same attempt and unit.
#[test]
fn a_probe_from_a_cancelled_life_never_dispatches_into_a_relaunch() {
    let f = fixture("relaunch");
    let run = format!("probe-relaunch-{}", std::process::id());
    let (entered, release) = hold(&run);
    launch(&f, &run);
    entered
        .recv_timeout(Duration::from_secs(60))
        .expect("the probe is out");
    f.core.cancel_run(&run).expect("cancel");
    launch_with(&f, &run, crate::HumanConfirm::All);
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline && status(&f, &run) != Some(crate::SessionStatus::AwaitingHuman)
    {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        status(&f, &run),
        Some(crate::SessionStatus::AwaitingHuman),
        "the relaunch waits at its human gate"
    );
    release.send(()).unwrap();
    let answered = || {
        f.core
            .actor_stats()
            .commands
            .iter()
            .any(|c| c.command == "DispatchProbed")
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline && !answered() {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(answered(), "the old probe's answer landed");
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        f.seat.ran.lock().unwrap().is_empty(),
        "nothing ran past the relaunch's human gate"
    );
    assert_eq!(status(&f, &run), Some(crate::SessionStatus::AwaitingHuman));
    drop(f.core);
    let _ = std::fs::remove_dir_all(&f.dir);
}
