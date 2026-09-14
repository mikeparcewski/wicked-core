//! DES-L9 F1 arm (BC-58; crew #549 / #550) through the REAL engine: a `deliver` Tool unit that
//! REFUSES (exit 1 with the script's own `deliver: …` line) parks the run at an `escalation` gate on
//! that unit — with `human_confirm: None`, the API default that used to fall straight through to
//! `sessionFailed` and reap the tree — and Approve re-runs the phase (no second deliver gate); once
//! the cause is fixed the run completes. And the additive `LaunchSpec.base_ref` (BC-59): the mint
//! bases a fresh worktree on `origin/<base_ref>` and says so on `runBaseResolved`; an unresolvable
//! ref fails the launch loudly by name.
//!
//! Nothing stubbed but the seat (no agent unit runs) and GitHub (the deliver phase is a bash script
//! that stands in for crew's — it refuses until a flag file exists). POSIX only: the phases are
//! shell scripts (the suite's idiom — `tool_only_plan_needs_no_seat`, `domain_extraction_e2e`).

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use wicked_core::{
    Core, CoreEvent, EntityMode, HumanConfirm, HumanDecision, LaunchSpec, RepoSpec, SessionStatus,
    StepInput, StepOutput, StepRunner, StepStatus,
};
use wicked_council::types::{Confidence, Dispatcher, Vote};
use wicked_council::{AgenticCli, CouncilTask};

/// Pre-main: the hermetic emit spool (core#311) — this binary must never write the operator's
/// real emit spool. SAFETY (`ctor(unsafe)`): single-threaded, std env/fs only.
#[ctor::ctor(unsafe)]
fn arm_hermetic_emit_spool() {
    wicked_apps_core::emit::hermetic_test_spool();
}

/// A tool-only plan convenes no council — but the trait needs an answer if it ever did.
struct IdleDispatcher;
impl Dispatcher for IdleDispatcher {
    fn dispatch(&self, c: &AgenticCli, _: &CouncilTask) -> Option<Vote> {
        Some(Vote {
            cli: c.key.clone(),
            recommendation: "1".into(),
            top_risk: "none".into(),
            change_my_mind: "no".into(),
            disqualifier: None,
            confidence: Confidence::default(),
            provenance: "test".into(),
        })
    }
}

/// A tool-only plan dispatches no worker turn; answering `Ok` keeps a misrouted unit from wedging.
struct IdleRunner;
impl StepRunner for IdleRunner {
    fn run_unit(&self, i: &StepInput) -> StepOutput {
        StepOutput {
            run_id: i.run_id.clone(),
            unit_ix: i.unit_ix,
            attempt: i.attempt,
            output: "no agent unit in this plan".into(),
            status: StepStatus::Ok,
            usage: None,
            files: vec![],
            tools: Vec::new(),
            governed: false,
        }
    }
}

fn fixture_root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "wicked-core-l9-{name}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn sh(cwd: &Path, args: &[&str]) -> String {
    // spawn-audit: test-only — a git fixture building the layout under test; reads no engine state.
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("git runs");
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn identity(repo: &Path) {
    sh(repo, &["config", "user.email", "t@example.invalid"]);
    sh(repo, &["config", "user.name", "t"]);
    sh(repo, &["config", "commit.gpgsign", "false"]);
    sh(repo, &["config", "core.autocrlf", "false"]);
}

/// A throwaway repo with one commit on `main` — what `register_repo` validates.
fn make_repo(dir: &Path) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    sh(dir, &["init", "-q", "-b", "main", "."]);
    identity(dir);
    std::fs::write(dir.join("README.md"), "hello\n").unwrap();
    sh(dir, &["add", "-A"]);
    sh(dir, &["commit", "-qm", "init"]);
    dir.to_path_buf()
}

fn core_at(root: &Path) -> Core {
    let db = root.join("estate.db").to_str().unwrap().to_string();
    Core::spawn_with_engine(db, Arc::new(IdleDispatcher), Arc::new(IdleRunner))
}

fn spec(
    session_id: &str,
    workflow: &str,
    repo_ref: String,
    auto_deliver: bool,
    base_ref: Option<&str>,
) -> LaunchSpec {
    LaunchSpec {
        base_ref: base_ref.map(str::to_string),
        project_id: None,
        problem: format!("Revise the pull request: {session_id}"),
        // Tool-only: no seat at all (wicked-crew#533).
        clis: Vec::new(),
        entity_mode: EntityMode::Shared,
        session_id: session_id.into(),
        // The API default — the arm must not depend on a human posture.
        human_confirm: HumanConfirm::None,
        auto_deliver,
        repo_ref: Some(repo_ref),
        workflow: Some(workflow.into()),
        extra_write_roots: Vec::new(),
        extra_read_roots: Vec::new(),
        project_graph: None,
    }
}

const WAIT: Duration = Duration::from_secs(180);

fn wait_status(core: &Core, run_id: &str, want: SessionStatus) -> bool {
    let start = Instant::now();
    let mut last = None;
    while start.elapsed() < WAIT {
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
    eprintln!("wait_status({run_id}): timed out waiting for {want:?}; last {last:?}");
    false
}

/// Drain until `stop` matches an event (that event included) or the budget runs out.
fn drain_until(
    events: &std::sync::mpsc::Receiver<CoreEvent>,
    stop: impl Fn(&CoreEvent) -> bool,
) -> Vec<CoreEvent> {
    let mut out = Vec::new();
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline {
        match events.recv_timeout(Duration::from_millis(500)) {
            Ok(ev) => {
                let done = stop(&ev);
                out.push(ev);
                if done {
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    out
}

fn is_terminal(ev: &CoreEvent, sid: &str) -> bool {
    matches!(ev, CoreEvent::SessionCompleted { session } if session == sid)
        || matches!(ev, CoreEvent::SessionFailed { session, .. } if session == sid)
        || matches!(ev, CoreEvent::RunCancelled { session } if session == sid)
}

/// DES-L9 §4 — the script's identity refusal (D-18), verbatim.
const REFUSAL: &str = "deliver: identity mismatch — GH_ACCOUNT is release-bot but gh's active \
login is someone-else; nothing was staged, committed or pushed. Fix the daemon's gh login (switch \
gh's active account, or export GH_TOKEN in the daemon environment) and approve to retry the \
deliver phase";

/// A one-phase def whose ONLY phase is a `deliver` Tool unit (crew's `DELIVER_PHASE_ID`) that
/// refuses with the identity text until `flag` exists, then "pushes" and prints a PR URL last.
fn deliver_def(id: &str, flag: &Path) -> String {
    let script = format!(
        "if [ ! -f '{}' ]; then echo \"{REFUSAL}\"; exit 1; fi; echo 'deliver: pushing as \
         release-bot (GH_ACCOUNT pinned by GH_TOKEN)'; echo https://github.com/o/r/pull/7",
        flag.display()
    );
    serde_json::json!({
        "id": id,
        "phases": [
            { "id": "deliver", "kind": "build",
              "executor": { "type": "tool", "cmd": ["bash", "-lc", script] } }
        ]
    })
    .to_string()
}

#[test]
fn a_deliver_refusal_parks_at_an_escalation_gate_and_approve_re_runs_the_phase() {
    let root = fixture_root("refusal");
    let repo = make_repo(&root.join("repo"));
    let flag = root.join("identity-fixed");
    let core = core_at(&root);
    let entry = core
        .register_repo(RepoSpec {
            name: "l9".into(),
            root_path: repo.to_str().unwrap().into(),
            registered_at: 1,
        })
        .expect("register");
    core.register_workflow(deliver_def("l9-deliver-refusal", &flag))
        .expect("register def");
    let events = core.subscribe();
    let sid = "l9-deliver-refusal-run";
    core.launch_run(spec(
        sid,
        "l9-deliver-refusal",
        entry.id.clone(),
        false,
        None,
    ))
    .expect("launch");

    // (1) The deliver GATE first — `human_confirm: None` never silences it (F-E2E-030).
    let evs = drain_until(&events, |ev| {
        matches!(ev, CoreEvent::AwaitingHuman { session, .. } if session == sid)
            || is_terminal(ev, sid)
    });
    let gate = evs.iter().find_map(|ev| match ev {
        CoreEvent::AwaitingHuman { gate_kind, ord, .. } => Some((gate_kind.clone(), *ord)),
        _ => None,
    });
    assert_eq!(gate, Some(("deliver".to_string(), 1)), "events: {evs:?}");

    // (2) Approve → the script REFUSES → the arm parks at `escalation` on the deliver unit.
    core.confirm_gate(sid, HumanDecision::Approve { amend: None })
        .expect("approve the deliver gate");
    let evs = drain_until(&events, |ev| {
        matches!(ev, CoreEvent::AwaitingHuman { session, .. } if session == sid)
            || is_terminal(ev, sid)
    });
    let failed = evs.iter().find_map(|ev| match ev {
        CoreEvent::StepFailed {
            ord,
            detail,
            failure_kind,
            ..
        } => Some((
            *ord,
            detail.clone(),
            matches!(failure_kind, wicked_core::StepFailureKind::WorkerError),
        )),
        _ => None,
    });
    let (f_ord, f_detail, f_worker) = failed.expect("stepFailed for the refusal");
    assert_eq!(f_ord, 1);
    assert!(f_worker);
    // On a host with NO OS-sandbox tool (bwrap / sandbox-exec) the deliver unit's own pre-run
    // RE-VERIFY refuses fail-closed BEFORE the script runs (`deliver: the run recorded no verified
    // tree, and the repository's own checks FAILED on it: checks not run: no OS write boundary
    // could be armed …`) — an ENGINE-authored deliver refusal, which the arm must park exactly like
    // the script's. The gate shape below is asserted either way; the script-refusal → fix →
    // approve → completed leg needs armable checks and is skipped (printed) where they are not —
    // never a silent pass.
    let engine_floor_refusal = f_detail.contains("no OS write boundary could be armed");
    if !engine_floor_refusal {
        assert!(
            f_detail.contains("deliver: identity mismatch"),
            "{f_detail}"
        );
    }
    let parked = evs
        .iter()
        .find_map(|ev| match ev {
            CoreEvent::AwaitingHuman {
                ord,
                reviewing_ord,
                gate_kind,
                prompt,
                ..
            } => Some((*ord, *reviewing_ord, gate_kind.clone(), prompt.clone())),
            _ => None,
        })
        .expect("the refusal parks the run");
    assert_eq!(
        (parked.0, parked.1, parked.2.as_str()),
        (1, Some(1), "escalation"),
        "events: {evs:?}"
    );
    let expected_head = if engine_floor_refusal {
        "The deliver phase refused: deliver: the run recorded no verified tree"
    } else {
        "The deliver phase refused: deliver: identity mismatch"
    };
    assert!(
        parked.3.starts_with(expected_head) && parked.3.contains("no second deliver gate"),
        "{}",
        parked.3
    );
    assert!(
        !evs.iter().any(|ev| is_terminal(ev, sid)),
        "no sessionFailed: a refusal is parked, not terminal"
    );
    assert!(
        !evs.iter()
            .any(|ev| matches!(ev, CoreEvent::FailureTriaged { .. })),
        "no judge reads a deterministic refusal"
    );
    let view = core
        .sessions_detail()
        .unwrap()
        .into_iter()
        .find(|v| v.session.id == sid)
        .expect("the run is on the store");
    assert_eq!(view.session.status, SessionStatus::AwaitingHuman);
    let unit = view
        .units
        .iter()
        .find(|u| u.ord == 1)
        .expect("the deliver unit");
    assert_eq!(
        unit.denial.as_ref().map(|d| d.source.as_str()),
        Some("deliver_refusal")
    );
    assert!(
        unit.denial_reason
            .as_deref()
            .is_some_and(|r| r.starts_with("deliver refused on unit 1: deliver: ")),
        "{:?}",
        unit.denial_reason
    );
    let workdir = view
        .session
        .workdir
        .clone()
        .expect("a repo-scoped run has a worktree");
    assert!(
        Path::new(&workdir).is_dir(),
        "the worktree is KEPT while the run is parked: {workdir}"
    );
    if engine_floor_refusal {
        eprintln!(
            "deliver_refusal_gate: no OS-sandbox tool on this host — the engine's own re-verify \
             refusal parked the run at the escalation gate (proven above); the script-refusal → \
             fix → approve → completed leg needs armable checks and is skipped here"
        );
        let _ = core.cancel_run(sid);
        let _ = std::fs::remove_dir_all(&root);
        return;
    }

    // (3) Fix the identity, approve → the phase re-runs (attempt 1) and the run completes.
    std::fs::write(&flag, "fixed\n").unwrap();
    core.confirm_gate(sid, HumanDecision::Approve { amend: None })
        .expect("approve the escalation gate");
    assert!(
        wait_status(&core, sid, SessionStatus::Completed),
        "the re-run deliver phase completes the run"
    );
    let evs = drain_until(&events, |ev| is_terminal(ev, sid));
    assert!(
        evs.iter().any(|ev| matches!(
            ev,
            CoreEvent::UnitDispatched { ord: 1, attempt, .. } if *attempt >= 1
        )),
        "the deliver unit was RE-dispatched (attempt bumped): {evs:?}"
    );
    assert!(evs
        .iter()
        .any(|ev| matches!(ev, CoreEvent::SessionCompleted { session } if session == sid)));
    assert!(!evs
        .iter()
        .any(|ev| matches!(ev, CoreEvent::SessionFailed { .. })));
    let _ = std::fs::remove_dir_all(&root);
}

/// A bare origin + a registered clone + a PR-shaped branch `wicked/prior-run` (one commit on top
/// of main) pushed from elsewhere. Returns (clone, PR head sha).
fn origin_with_pr_branch(root: &Path) -> (PathBuf, String) {
    let seed = make_repo(&root.join("seed"));
    let origin = root.join("origin.git");
    sh(
        root,
        &[
            "-c",
            "core.autocrlf=false",
            "clone",
            "-q",
            "--bare",
            seed.to_str().unwrap(),
            origin.to_str().unwrap(),
        ],
    );
    sh(&origin, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    let clone = root.join("clone");
    sh(
        root,
        &[
            "-c",
            "core.autocrlf=false",
            "clone",
            "-q",
            origin.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    );
    identity(&clone);
    let other = root.join("other");
    sh(
        root,
        &[
            "-c",
            "core.autocrlf=false",
            "clone",
            "-q",
            origin.to_str().unwrap(),
            other.to_str().unwrap(),
        ],
    );
    identity(&other);
    sh(&other, &["checkout", "-q", "-b", "wicked/prior-run"]);
    std::fs::write(other.join("pr.txt"), "the prior run's fix\n").unwrap();
    sh(&other, &["add", "-A"]);
    sh(&other, &["commit", "-qm", "prior run"]);
    sh(&other, &["push", "-q", "origin", "wicked/prior-run"]);
    let pr_head = sh(&other, &["rev-parse", "HEAD"]);
    (clone, pr_head)
}

#[test]
fn an_explicit_base_ref_reaches_the_mint_and_an_unresolvable_one_fails_the_launch_by_name() {
    let root = fixture_root("baseref");
    let (clone, pr_head) = origin_with_pr_branch(&root);
    let core = core_at(&root);
    let entry = core
        .register_repo(RepoSpec {
            name: "l9-base".into(),
            root_path: clone.to_str().unwrap().into(),
            registered_at: 1,
        })
        .expect("register");
    // One Tool phase that touches nothing — the run's only job is to show which base it got.
    core.register_workflow(
        serde_json::json!({
            "id": "l9-base-only",
            "phases": [
                { "id": "note", "kind": "recon",
                  "executor": { "type": "tool", "cmd": ["bash", "-lc", "cat pr.txt"] } }
            ]
        })
        .to_string(),
    )
    .expect("register def");
    let events = core.subscribe();

    // (a) The explicit base: the worktree is minted from the PR head and the wire says so.
    let sid = "l9-revision";
    core.launch_run(spec(
        sid,
        "l9-base-only",
        entry.id.clone(),
        true,
        Some("wicked/prior-run"),
    ))
    .expect("launch");
    let evs = drain_until(&events, |ev| is_terminal(ev, sid));
    let based = evs.iter().find_map(|ev| match ev {
        CoreEvent::RunBaseResolved {
            session,
            base_ref,
            base_commit,
            lifted,
            behind,
            note,
            run_branch,
            ..
        } if session == sid => Some((
            base_ref.clone(),
            base_commit.clone(),
            *lifted,
            *behind,
            note.clone(),
            run_branch.clone(),
        )),
        _ => None,
    });
    let (base_ref, base_commit, lifted, behind, note, run_branch) =
        based.expect("runBaseResolved on the wire");
    assert_eq!(base_ref.as_deref(), Some("origin/wicked/prior-run"));
    assert_eq!(base_commit, pr_head, "the base IS the PR head");
    assert!(!lifted && behind == 0);
    assert!(
        note.as_deref()
            .is_some_and(|n| n.contains("explicit base") && n.contains("revises a pull request")),
        "{note:?}"
    );
    assert_eq!(run_branch, format!("wicked/{sid}"));
    assert!(
        evs.iter()
            .any(|ev| matches!(ev, CoreEvent::SessionCompleted { session } if session == sid)),
        "the tool phase read the PR's file from the tree it was based on: {evs:?}"
    );
    let view = core
        .sessions_detail()
        .unwrap()
        .into_iter()
        .find(|v| v.session.id == sid)
        .unwrap();
    assert_eq!(view.session.base_commit.as_deref(), Some(pr_head.as_str()));

    // (b) An unresolvable base fails the launch loudly, naming the ref — never a fall-back to main.
    let sid2 = "l9-revision-gone";
    core.launch_run(spec(
        sid2,
        "l9-base-only",
        entry.id.clone(),
        true,
        Some("wicked/no-such-run"),
    ))
    .expect("the fast path accepts the launch; the mint refuses it");
    let evs = drain_until(&events, |ev| is_terminal(ev, sid2));
    assert!(
        evs.iter()
            .any(|ev| matches!(ev, CoreEvent::SessionFailed { session, .. } if session == sid2)),
        "{evs:?}"
    );
    let named = evs.iter().any(|ev| match ev {
        CoreEvent::Error { session, message } => {
            session.as_deref() == Some(sid2)
                && message.contains("origin/wicked/no-such-run does not resolve")
        }
        _ => false,
    });
    assert!(named, "the failure names the ref: {evs:?}");
    assert!(!evs.iter().any(|ev| matches!(
        ev,
        CoreEvent::RunBaseResolved { session, .. } if session == sid2
    )));
    let _ = std::fs::remove_dir_all(&root);
}
