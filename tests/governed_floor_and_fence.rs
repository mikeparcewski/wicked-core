//! Wave 6 (F-7R2-005 / F-7R2-013) — the DEFAULT floor and the retained run branch, through the
//! real actor against a real registered repo + linked worktree.
//!
//! The Phase 7 re-run (run b86c14c1) passed seven vacuous gates: every free-text unit evaluated
//! as `hasDeterministicFloor:false, judgeCli:null, evaluatorPolicies:[]` — including the one that
//! wrote 298 lines of tests — and the worktree was reaped seconds after `sessionCompleted`, so
//! the run page had no files view. These tests pin what the engine must now do for a bound
//! PROSE-PLANNED run (no workflow def, no pinned validator):
//!
//! 1. a unit that CHANGES the worktree tree gets the default repo-checks floor
//!    (`repoChecksEvaluated` for the unit — the checks ran, or the host could not arm an OS
//!    sandbox and the record says so) and a judge distinct from the creator, so its gate is
//!    NOT `ungated`;
//! 2. a unit that leaves the tree alone is honestly `ungated: true`, with the reason on the wire;
//! 3. `runBaseResolved.runBranch` and the session's `run_branch` / `base_commit` /
//!    `finished_at` are recorded, and the completed run's worktree is RETAINED until the run is
//!    archived — then reaped (clean-only).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use wicked_council::types::{Category, Confidence, Dispatcher, InputMode, Vote};
use wicked_council::{AgenticCli, CouncilTask};

use wicked_core::{
    Core, CoreEvent, HumanConfirm, LaunchSpec, RepoSpec, SessionStatus, StepInput, StepOutput,
    StepRunner, StepStatus,
};

/// Route every fire-and-forget `wicked.*` emission this binary triggers to a per-process temp spool
/// instead of the operator's real replay queue (core#311). Every binary in this suite carries this
/// block; `harness_hygiene.rs` fails the suite if one is missing.
///
/// SAFETY (`ctor(unsafe)`): runs before `main` on one thread and only sets one process env var
/// via the std API — no allocator setup, no threads, no panics across the FFI boundary.
#[ctor::ctor(unsafe)]
fn arm_hermetic_emit_spool() {
    wicked_apps_core::emit::hermetic_test_spool();
}

struct StubDispatcher;
impl Dispatcher for StubDispatcher {
    fn dispatch(&self, cli: &AgenticCli, _t: &CouncilTask) -> Option<Vote> {
        Some(Vote {
            cli: cli.key.clone(),
            recommendation: "1 — fit".into(),
            top_risk: "none".into(),
            change_my_mind: "no".into(),
            disqualifier: None,
            confidence: Confidence::default(),
            provenance: "stub".into(),
        })
    }
}

/// `(ord, assigned seat)` of every run unit the seat executed.
type Ran = Arc<Mutex<Vec<(u32, String)>>>;

/// A seat that COMMITS a new file on the run's first unit, touches nothing on any later unit,
/// and answers the engine's judge sessions (`session_id == "validator"`) with a well-formed PASS.
struct ScriptedSeat {
    ran: Ran,
}
impl StepRunner for ScriptedSeat {
    fn run_unit(&self, input: &StepInput) -> StepOutput {
        let mut output = format!("did unit {}", input.unit.ord);
        if input.unit.session_id == "validator" {
            output = "PASS\nthe work meets the criterion\nPASS".to_string();
        } else {
            self.ran.lock().unwrap().push((
                input.unit.ord,
                input.unit.assigned_cli.clone().unwrap_or_default(),
            ));
            if let (1, Some(wd)) = (input.unit.ord, &input.workdir) {
                std::fs::create_dir_all(wd.join("src")).unwrap();
                std::fs::write(wd.join("src/note.txt"), "a note\n").unwrap();
                git(wd, &["add", "-A"]);
                git(wd, &["commit", "-qm", "add the note"]);
            }
        }
        StepOutput {
            run_id: input.run_id.clone(),
            unit_ix: input.unit_ix,
            attempt: input.attempt,
            output,
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
        headless_invocation: format!("{key} -p {{PROMPT}}"),
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

fn git(repo: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .expect("git runs");
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

/// A throwaway repo with one commit and NO detectable checks (no Cargo.toml, no package.json), so
/// the floor's detection is empty wherever it can run — the test is about the floor ARMING.
fn make_git_repo(name: &str) -> PathBuf {
    let repo = std::env::temp_dir().join(format!(
        "wicked-core-w6floor-{name}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&repo);
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.email", "t@example.invalid"]);
    git(&repo, &["config", "user.name", "t"]);
    git(&repo, &["config", "commit.gpgsign", "false"]);
    // The Windows runner checks out with `core.autocrlf=true`; pinned off BEFORE the first add so
    // the tree ids the floor compares are byte-exact on every host.
    git(&repo, &["config", "core.autocrlf", "false"]);
    std::fs::write(repo.join("README.md"), "hello\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "init"]);
    repo
}

fn core_for(name: &str) -> (Core, Ran) {
    let dir = std::env::temp_dir().join(format!(
        "wicked-core-w6floor-db-{name}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("estate.db").to_str().unwrap().to_string();
    let ran = Arc::new(Mutex::new(Vec::new()));
    let core = Core::spawn_with_engine(
        db,
        Arc::new(StubDispatcher),
        Arc::new(ScriptedSeat { ran: ran.clone() }),
    );
    (core, ran)
}

const WAIT_BUDGET: Duration = Duration::from_secs(180);

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
        std::thread::sleep(Duration::from_millis(25));
    }
    eprintln!(
        "wait_status({run_id}): timed out after {:?} waiting for {want:?}; last observed {last:?}",
        start.elapsed()
    );
    false
}

fn drain(events: &std::sync::mpsc::Receiver<CoreEvent>) -> Vec<CoreEvent> {
    let mut out = Vec::new();
    while let Ok(ev) = events.recv_timeout(Duration::from_millis(400)) {
        out.push(ev);
    }
    out
}

/// The `gateEvaluated` record for `ord`, destructured into what this test asserts on.
struct Gate {
    has_floor: bool,
    agent_verdict: Option<String>,
    judge_cli: Option<String>,
    combined: bool,
    ungated: bool,
    ungated_reason: Option<String>,
    floor_note: Option<String>,
}

fn gate_for(events: &[CoreEvent], want_ord: u32) -> Gate {
    events
        .iter()
        .find_map(|ev| match ev {
            CoreEvent::GateEvaluated {
                ord,
                has_deterministic_floor,
                agent_verdict,
                judge_cli,
                combined,
                ungated,
                ungated_reason,
                floor_note,
                ..
            } if *ord == want_ord => Some(Gate {
                has_floor: *has_deterministic_floor,
                agent_verdict: agent_verdict.clone(),
                judge_cli: judge_cli.clone(),
                combined: *combined,
                ungated: *ungated,
                ungated_reason: ungated_reason.clone(),
                floor_note: floor_note.clone(),
            }),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no gateEvaluated for ord {want_ord}"))
}

#[test]
fn a_changed_tree_gets_the_default_floor_and_judge_an_unchanged_one_is_honestly_ungated() {
    let repo = make_git_repo("floor");
    let (core, ran) = core_for("floor");
    let entry = core
        .register_repo(RepoSpec {
            name: "floor".into(),
            root_path: repo.to_str().unwrap().into(),
            registered_at: 1,
        })
        .expect("register");
    let events = core.subscribe();
    let run_id = "r-w6-floor";
    core.launch_run(LaunchSpec {
        project_id: None,
        // Two sentences ⇒ the prose planner's two units: the first writes, the second reads.
        problem: "Add a note file under src. Confirm the note reads well.".into(),
        clis: vec![cli("a"), cli("b")],
        entity_mode: wicked_core::EntityMode::Shared,
        session_id: run_id.into(),
        human_confirm: HumanConfirm::None,
        repo_ref: Some(entry.id.clone()),
        workflow: None,
        extra_write_roots: Vec::new(),
        extra_read_roots: Vec::new(),
        project_graph: None,
    })
    .expect("launch");
    assert!(
        wait_status(&core, run_id, SessionStatus::Completed),
        "a bound prose run with a committing seat and a passing judge completes"
    );
    let evs = drain(&events);
    let ran = ran.lock().unwrap().clone();
    let last_ord = ran.iter().map(|(o, _)| *o).max().expect("units ran");
    assert!(last_ord >= 2, "two prose units expected, ran {ran:?}");

    // (1) The unit that CHANGED the tree: the default floor armed — `repoChecksEvaluated` exists
    // for it (checks ran, or the host could not arm an OS sandbox and the record says so) — and
    // a judge distinct from the creator rendered the verdict. Never `ungated`.
    let checks_for_1 = evs.iter().find_map(|ev| match ev {
        CoreEvent::RepoChecksEvaluated {
            ord,
            passed,
            checks,
            sandbox_error,
            ..
        } if *ord == 1 => Some((*passed, checks.len(), sandbox_error.clone())),
        _ => None,
    });
    assert!(
        checks_for_1.is_some(),
        "the default repo-checks floor ran (or disclosed it could not) for the unit that changed \
         the tree; events: {}",
        evs.len()
    );
    let g1 = gate_for(&evs, 1);
    assert!(
        !g1.ungated,
        "a unit that changed the tree is never UNGATED: {:?}",
        g1.ungated_reason
    );
    assert_eq!(
        g1.agent_verdict.as_deref(),
        Some("pass"),
        "the default judge rendered the verdict"
    );
    let judge = g1.judge_cli.expect("the judge seat is named on the gate");
    let creator = ran
        .iter()
        .find(|(o, _)| *o == 1)
        .map(|(_, c)| c.clone())
        .unwrap_or_default();
    assert_ne!(
        judge, creator,
        "evaluator ≠ creator: judge {judge}, creator {creator}"
    );
    assert!(g1.combined, "floor + judge both passed");
    // The floor COUNTS as deterministic when it ran; when the host could not arm a sandbox it is
    // disclosed, not counted — and never a denial for the default floor.
    match checks_for_1 {
        Some((true, _, _)) => {
            assert!(g1.has_floor, "checks ran ⇒ a deterministic floor");
            assert!(g1.floor_note.is_none(), "{:?}", g1.floor_note);
        }
        Some((false, 0, sandbox_error)) => {
            eprintln!(
                "governed_floor_and_fence: no OS-sandbox tool on this host — the default floor \
                 is disclosed (not counted, not denied)"
            );
            assert!(!g1.has_floor);
            // Review of #449, FL-1: the CAUSE is on the wire — on the checks frame and on the
            // gate — even though a judge ran and the gate is not `ungated`.
            let sandbox_error =
                sandbox_error.expect("repoChecksEvaluated.sandboxError names the cause");
            assert!(
                sandbox_error.contains("no OS write boundary"),
                "{sandbox_error}"
            );
            let note = g1
                .floor_note
                .as_deref()
                .expect("gateEvaluated.floorNote names the absent floor's cause");
            assert!(
                note.contains("could not run") && note.contains("sandbox"),
                "{note}"
            );
        }
        other => panic!("unexpected repoChecksEvaluated shape for ord 1: {other:?}"),
    }

    // (2) The unit that left the tree alone: honestly UNGATED, with the reason per absent layer.
    let g2 = gate_for(&evs, last_ord);
    assert!(
        g2.ungated,
        "an unchanged tree with no judge is UNGATED, never 'pass'"
    );
    let why = g2
        .ungated_reason
        .expect("ungatedReason rides beside ungated");
    assert!(
        why.contains("unchanged") && why.contains("no judge"),
        "the reason names the absent layers: {why}"
    );
    assert!(g2.judge_cli.is_none() && !g2.has_floor);
    assert!(
        g2.floor_note
            .as_deref()
            .is_some_and(|n| n.contains("unchanged")),
        "floorNote rides every gate without a deterministic floor: {:?}",
        g2.floor_note
    );
    assert!(
        !evs.iter()
            .any(|ev| matches!(ev, CoreEvent::RepoChecksEvaluated { ord, .. } if *ord == last_ord)),
        "no checks ran for a unit that changed nothing — the narrator must never say they did"
    );

    // (3) The run branch and its base are recorded — on the event and, durably, on the session.
    let expected_branch = format!("wicked/{run_id}");
    let run_branch_event = evs
        .iter()
        .find_map(|ev| match ev {
            CoreEvent::RunBaseResolved {
                run_branch,
                base_commit,
                ..
            } => Some((run_branch.clone(), base_commit.clone())),
            _ => None,
        })
        .expect("runBaseResolved emitted for a fresh worktree");
    assert_eq!(run_branch_event.0, expected_branch);
    let view = core
        .sessions_detail()
        .unwrap()
        .into_iter()
        .find(|v| v.session.id == run_id)
        .expect("the run's view");
    assert_eq!(
        view.session.run_branch.as_deref(),
        Some(expected_branch.as_str())
    );
    assert_eq!(
        view.session.base_commit.as_deref(),
        Some(run_branch_event.1.as_str()),
        "the session's base_commit is the resolved base"
    );
    assert!(
        view.session
            .base_commit
            .as_deref()
            .is_some_and(|c| c.len() == 40 && c.chars().all(|ch| ch.is_ascii_hexdigit())),
        "a full commit id: {:?}",
        view.session.base_commit
    );
    assert!(
        view.session.finished_at.is_some(),
        "finished_at stamped on completion"
    );

    // The completed run's worktree is RETAINED (the default 14-day window)…
    let workdir = PathBuf::from(
        view.session
            .workdir
            .clone()
            .expect("a bound run has a workdir"),
    );
    assert!(
        workdir.join("src/note.txt").is_file(),
        "the worktree at {} is kept after completion — the files view needs it",
        workdir.display()
    );
    // …until the operator ARCHIVES the run: the clean tree is reaped, the branch stays.
    assert!(core.archive_run(run_id, true, None).expect("archive"));
    let start = Instant::now();
    while workdir.exists() && start.elapsed() < Duration::from_secs(20) {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !workdir.exists(),
        "archiving reaps the retained (clean) worktree at {}",
        workdir.display()
    );
    let branches = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["branch", "--list", &expected_branch])
        .output()
        .expect("git runs");
    assert!(
        String::from_utf8_lossy(&branches.stdout).contains(&expected_branch),
        "the run branch is the record and survives the reap"
    );

    drop(core);
    let _ = std::fs::remove_dir_all(&repo);
}
