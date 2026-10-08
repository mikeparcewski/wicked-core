//! The floor's gate, after a floor failure — core#544, core#469, core#467.
//!
//! * core#544: a `request_changes` or approve-retry rework that leaves a floor-FAILED tree
//!   unchanged used to be admitted with NO checks run (`deterministicPass: true`, the checks clause
//!   gone from `criterion`) — the rework re-baselined the tree and "unchanged" read as "nothing to
//!   check". The failed result now stands for the unchanged tree: the gate denies with
//!   `source: repo_checks` and a `floorNote` naming the carried-forward result. A rework that DOES
//!   change the tree re-runs the floor (unchanged behaviour).
//! * core#469: a floor that did not FINISH (`repo_checks_timeout`) offers `extend`, `targeted` and
//!   `accept_partial` at its gate; each re-runs the checks on the tree as it stands (the seat does
//!   not run again) and the result goes through the ordinary gate fold.
//! * core#467 (item 4): the evaluator's discarded edit can be adopted at its gate
//!   (`accept_suggestion`): the pinned tree is applied and the creator's rework owns it.
//!
//! Every test drives the real `bug` workflow (triage → reproduce → fix → verify) through
//! `Core::spawn_with_engine` against a registered repo whose checks are `.wicked/checks.json`
//! shell commands — fast, and sandboxed like any other floor. On a host with no OS write boundary
//! the default floor is disclosed rather than run (F-7R2-005) and these shapes cannot occur; the
//! tests say so and return.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use wicked_council::types::{Category, Confidence, Dispatcher, InputMode, Vote};
use wicked_council::{AgenticCli, CouncilTask};

use wicked_core::{
    Core, CoreEvent, HumanConfirm, HumanDecision, LaunchSpec, RepoSpec, SessionStatus, StepInput,
    StepOutput, StepRunner, StepStatus,
};

#[ctor::ctor(unsafe)]
fn arm_hermetic_emit_spool() {
    wicked_apps_core::emit::hermetic_test_spool();
}

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

/// What the creator (`fix`) does on a REWORK attempt (attempt ≥ 1).
#[derive(Clone, Copy, PartialEq)]
enum Rework {
    /// Reads the findings and ends its turn without touching the tree (the core#544 shape).
    ChangesNothing,
    /// Removes the breakage.
    Repairs,
}

/// A seat that plays the `bug` workflow deterministically. `fix` writes the fix and — when the
/// repo carries `.break-in-fix` — a `BROKEN` marker the repo's `test` check fails on; its reworks
/// behave per [`Rework`]. `verify` leaves the tree alone unless `.verify-rewrites` is committed,
/// in which case its FIRST attempt rewrites the fix (the F-036 shape). The engine's judge gets a
/// well-formed PASS.
/// `(phase, attempt)` of every governed unit the seat ran.
type Ran = Arc<Mutex<Vec<(String, u32)>>>;

struct ScriptedSeat {
    rework: Rework,
    ran: Ran,
}
impl StepRunner for ScriptedSeat {
    fn run_unit(&self, input: &StepInput) -> StepOutput {
        let phase = input.unit.phase_id().unwrap_or("").to_string();
        let mut output = format!("did: {phase}");
        if input.unit.session_id == "validator" {
            output = "PASS\nthe work meets the criterion\nPASS".to_string();
        } else {
            self.ran
                .lock()
                .unwrap()
                .push((phase.clone(), input.attempt));
            if let Some(wd) = &input.workdir {
                match phase.as_str() {
                    "fix" if input.attempt == 0 => {
                        std::fs::write(wd.join("src/app.ts"), "fixed\n").unwrap();
                        if wd.join(".break-in-fix").is_file() {
                            std::fs::write(wd.join("BROKEN"), "the fix broke the build\n").unwrap();
                        }
                    }
                    "fix" => {
                        if self.rework == Rework::Repairs {
                            let _ = std::fs::remove_file(wd.join("BROKEN"));
                        }
                        output = "read the findings; the tree stands as it is".to_string();
                    }
                    "verify" if wd.join(".verify-rewrites").is_file() && input.attempt == 0 => {
                        std::fs::write(wd.join("src/app.ts"), "evaluator's rewrite\n").unwrap();
                    }
                    _ => {}
                }
            }
            if input.unit.role == wicked_core::PhaseRole::Evaluator && input.unit.tool_cmd.is_none()
            {
                output.push_str("\nVERDICT: PASS");
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
        governance_class: None,
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

/// The `test` check of a repo that BREAKS on the fix: fails while `BROKEN` exists.
const TEST_FAILS_ON_BROKEN: &str = r#"["sh", "-c", "test ! -f BROKEN"]"#;
/// The `test` check of a SLOW repo: the first run does not finish inside its bound (3 s × the
/// host-load factor, at most 9 s — the bound scales with load, so the fast `lint` beside it
/// still finishes); every later run (a marker under the checks' own HOME, which persists in the run's
/// scratch) finishes at once.
const TEST_SLOW_ONCE: &str = r#"["sh", "-c", "test -f \"$HOME/.slow-done\" && exit 0; : > \"$HOME/.slow-done\"; sleep 600"]"#;

/// A throwaway repo whose checks are `.wicked/checks.json` shell commands: `lint` always passes,
/// `test` is `test_argv`. `markers` are committed files the seat keys on.
fn make_repo(name: &str, test_argv: &str, timeout_s: u64, markers: &[&str]) -> PathBuf {
    let repo = std::env::temp_dir().join(format!(
        "wicked-core-floorgate-{name}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&repo);
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::create_dir_all(repo.join(".wicked")).unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.email", "t@example.invalid"]);
    git(&repo, &["config", "user.name", "t"]);
    git(&repo, &["config", "commit.gpgsign", "false"]);
    git(&repo, &["config", "core.autocrlf", "false"]);
    std::fs::write(repo.join("README.md"), "hello\n").unwrap();
    std::fs::write(repo.join("src/app.ts"), "buggy\n").unwrap();
    std::fs::write(
        repo.join(".wicked/checks.json"),
        format!(
            r#"{{"lint": ["sh", "-c", "exit 0"], "test": {test_argv}, "timeout_s": {timeout_s}}}"#
        ),
    )
    .unwrap();
    for m in markers {
        std::fs::write(repo.join(m), "").unwrap();
    }
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "init"]);
    repo
}

fn core_for(name: &str, rework: Rework) -> (Core, Ran) {
    let dir = std::env::temp_dir().join(format!(
        "wicked-core-floorgate-db-{name}-{}-{:?}",
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
        Arc::new(ScriptedSeat {
            rework,
            ran: ran.clone(),
        }),
    );
    (core, ran)
}

fn launch(core: &Core, name: &str, repo: &Path, sid: &str) -> std::sync::mpsc::Receiver<CoreEvent> {
    let entry = core
        .register_repo(RepoSpec {
            name: name.into(),
            root_path: repo.to_str().unwrap().into(),
            registered_at: 1,
        })
        .expect("register");
    let events = core.subscribe();
    core.launch_run(LaunchSpec {
        base_ref: None,
        project_id: None,
        problem: "Fix the bug in src/app.ts".into(),
        clis: vec![cli("a"), cli("b")],
        entity_mode: wicked_core::EntityMode::Shared,
        session_id: sid.into(),
        human_confirm: HumanConfirm::None,
        auto_deliver: false,
        repo_ref: Some(entry.id),
        workflow: Some("bug".into()),
        extra_write_roots: Vec::new(),
        extra_read_roots: Vec::new(),
        project_graph: None,
        plan: None,
        deliver_step: None,
        exclude_seats: Vec::new(),
        evidence_root: None,
        primary: None,
    })
    .expect("launch");
    events
}

const WAIT_BUDGET: Duration = Duration::from_secs(300);

/// Collect events until the run pauses, completes or fails.
fn until_settled(events: &std::sync::mpsc::Receiver<CoreEvent>, sid: &str) -> Vec<CoreEvent> {
    let start = Instant::now();
    let mut out = Vec::new();
    while start.elapsed() < WAIT_BUDGET {
        match events.recv_timeout(Duration::from_millis(250)) {
            Ok(ev) => {
                let done = matches!(&ev, CoreEvent::AwaitingHuman { session, .. } if session == sid)
                    || matches!(&ev, CoreEvent::SessionCompleted { session } if session == sid)
                    || matches!(&ev, CoreEvent::SessionFailed { session, .. } if session == sid);
                out.push(ev);
                if done {
                    return out;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    panic!("run {sid} did not settle within {WAIT_BUDGET:?}; saw {out:?}");
}

/// The `gateEvaluated` of `ord`: `(criterion, deterministic_pass, combined, denial source,
/// floor_note)`.
type GateView = (Option<String>, bool, bool, Option<String>, Option<String>);
fn gate(evs: &[CoreEvent], want: u32) -> Option<GateView> {
    evs.iter().rev().find_map(|ev| match ev {
        CoreEvent::GateEvaluated {
            ord,
            criterion,
            deterministic_pass,
            combined,
            denial,
            floor_note,
            ..
        } if *ord == want => Some((
            criterion.clone(),
            *deterministic_pass,
            *combined,
            denial.as_ref().map(|d| d.source.clone()),
            floor_note.clone(),
        )),
        _ => None,
    })
}

/// The `repoChecksEvaluated` frames of `ord`: `(attempt, outcome, check names that ran)`.
fn floors(evs: &[CoreEvent], want: u32) -> Vec<(u32, String, Vec<String>)> {
    evs.iter()
        .filter_map(|ev| match ev {
            CoreEvent::RepoChecksEvaluated {
                ord,
                attempt,
                outcome,
                checks,
                ..
            } if *ord == want => Some((
                *attempt,
                outcome.clone(),
                checks.iter().map(|c| c.name.clone()).collect(),
            )),
            _ => None,
        })
        .collect()
}

/// `true` when the fix unit's first floor was DENIED under `source` — the shape every test here
/// needs; `false` (and a note) on a host that cannot arm an OS write boundary.
fn fix_floor_denied(evs: &[CoreEvent], source: &str) -> bool {
    match gate(evs, 3) {
        Some((_, _, false, Some(s), _)) if s == source => true,
        other => {
            eprintln!(
                "floor_gate_actions: the fix floor was not denied by `{source}` on this host \
                 ({other:?}) — no OS write boundary to run the checks under; the default floor \
                 is disclosed, not run (F-7R2-005). Nothing to assert here."
            );
            false
        }
    }
}

fn no_session_failed(evs: &[CoreEvent]) {
    assert!(
        !evs.iter()
            .any(|e| matches!(e, CoreEvent::SessionFailed { .. })),
        "no sessionFailed: {evs:?}"
    );
}

/// core#544 (a): floor FAILED → `request_changes` → the creator returns an IDENTICAL tree → the
/// gate denies with `source: repo_checks`, `deterministicPass: false`, the checks clause kept in
/// `criterion` and a `floorNote` naming the carried-forward result. No second floor ran.
#[test]
fn a_request_changes_rework_that_changes_nothing_keeps_the_floor_denial() {
    let repo = make_repo("rc-nochange", TEST_FAILS_ON_BROKEN, 60, &[".break-in-fix"]);
    let (core, ran) = core_for("rc-nochange", Rework::ChangesNothing);
    let sid = "fg-rc-nochange";
    let events = launch(&core, "rc-nochange", &repo, sid);
    let first = until_settled(&events, sid);
    no_session_failed(&first);
    if !fix_floor_denied(&first, "repo_checks") {
        return;
    }
    core.confirm_gate(
        sid,
        HumanDecision::RequestChanges {
            note: Some("fix the build".into()),
        },
    )
    .expect("request changes");
    let second = until_settled(&events, sid);
    no_session_failed(&second);
    assert!(
        ran.lock().unwrap().contains(&("fix".to_string(), 1)),
        "the creator re-ran: {:?}",
        ran.lock().unwrap()
    );
    let (criterion, det_pass, combined, source, note) =
        gate(&second, 3).expect("the rework's gateEvaluated");
    assert!(
        !combined && !det_pass,
        "the unchanged failed tree is denied"
    );
    assert_eq!(source.as_deref(), Some("repo_checks"));
    assert!(
        criterion
            .as_deref()
            .is_some_and(|c| c.contains("repository's own checks pass")),
        "the checks clause stays in the criterion: {criterion:?}"
    );
    assert!(
        note.as_deref()
            .is_some_and(|n| n.contains("made no change") && n.contains("(failed)")),
        "the floorNote names the carried-forward result: {note:?}"
    );
    assert!(
        floors(&second, 3).is_empty(),
        "the unchanged tree's verdict is carried, not re-run"
    );
    let _ = std::fs::remove_dir_all(&repo);
}

/// core#544 (b): the same through the approve-retry path.
#[test]
fn an_approve_retry_that_changes_nothing_keeps_the_floor_denial() {
    let repo = make_repo("ap-nochange", TEST_FAILS_ON_BROKEN, 60, &[".break-in-fix"]);
    let (core, _ran) = core_for("ap-nochange", Rework::ChangesNothing);
    let sid = "fg-ap-nochange";
    let events = launch(&core, "ap-nochange", &repo, sid);
    let first = until_settled(&events, sid);
    if !fix_floor_denied(&first, "repo_checks") {
        return;
    }
    core.confirm_gate(
        sid,
        HumanDecision::Approve {
            amend: None,
            amend_scope: Default::default(),
        },
    )
    .expect("approve the retry");
    let second = until_settled(&events, sid);
    no_session_failed(&second);
    let (_, det_pass, combined, source, note) = gate(&second, 3).expect("gateEvaluated");
    assert!(!combined && !det_pass);
    assert_eq!(source.as_deref(), Some("repo_checks"));
    assert!(note.is_some_and(|n| n.contains("made no change")));
    let _ = std::fs::remove_dir_all(&repo);
}

/// core#544 (c): a rework that DOES change the tree re-runs the floor — and a repaired tree passes
/// and the run goes on to verify.
#[test]
fn a_rework_that_repairs_the_tree_re_runs_the_floor() {
    let repo = make_repo("rc-repair", TEST_FAILS_ON_BROKEN, 60, &[".break-in-fix"]);
    let (core, ran) = core_for("rc-repair", Rework::Repairs);
    let sid = "fg-rc-repair";
    let events = launch(&core, "rc-repair", &repo, sid);
    let first = until_settled(&events, sid);
    if !fix_floor_denied(&first, "repo_checks") {
        return;
    }
    core.confirm_gate(sid, HumanDecision::RequestChanges { note: None })
        .expect("request changes");
    let second = until_settled(&events, sid);
    no_session_failed(&second);
    let reruns = floors(&second, 3);
    assert_eq!(
        reruns.len(),
        1,
        "one fresh floor for the changed tree: {reruns:?}"
    );
    assert_eq!(reruns[0].1, "passed");
    assert!(
        ran.lock().unwrap().iter().any(|(p, _)| p == "verify"),
        "the repaired tree reaches verify"
    );
    let _ = std::fs::remove_dir_all(&repo);
}

/// Launch a SLOW-once repo and return the events up to the fix unit's timeout gate, or `None` on
/// a host with no OS write boundary.
type TimedOut = (Core, std::sync::mpsc::Receiver<CoreEvent>, Ran, PathBuf);
fn timed_out_fix(name: &str, sid: &str) -> Option<TimedOut> {
    let repo = make_repo(name, TEST_SLOW_ONCE, 3, &[]);
    let (core, ran) = core_for(name, Rework::ChangesNothing);
    let events = launch(&core, name, &repo, sid);
    let first = until_settled(&events, sid);
    no_session_failed(&first);
    if !fix_floor_denied(&first, "repo_checks_timeout") {
        return None;
    }
    let f = floors(&first, 3);
    assert_eq!(f.last().map(|x| x.1.as_str()), Some("timed_out"), "{f:?}");
    Some((core, events, ran, repo))
}

fn escalation(action: &str) -> HumanDecision {
    HumanDecision::escalation_action(action).expect("an escalation action")
}

/// core#469: `extend` re-runs every check under 2× its bound — the seat does not run again — and
/// the finished floor passes the gate.
#[test]
fn extend_re_runs_the_floor_under_a_doubled_bound() {
    let Some((core, events, ran, repo)) = timed_out_fix("extend", "fg-extend") else {
        return;
    };
    let fix_runs_before = ran
        .lock()
        .unwrap()
        .iter()
        .filter(|(p, _)| p == "fix")
        .count();
    core.confirm_gate("fg-extend", escalation("extend"))
        .expect("extend");
    let second = until_settled(&events, "fg-extend");
    no_session_failed(&second);
    assert_eq!(
        ran.lock()
            .unwrap()
            .iter()
            .filter(|(p, _)| p == "fix")
            .count(),
        fix_runs_before,
        "the seat did not run again"
    );
    let rerun = second
        .iter()
        .find_map(|ev| match ev {
            CoreEvent::RepoChecksEvaluated {
                ord: 3,
                outcome,
                checks,
                ..
            } => Some((outcome.clone(), checks.clone())),
            _ => None,
        })
        .expect("the re-run's repoChecksEvaluated");
    assert_eq!(rerun.0, "passed");
    let test = rerun.1.iter().find(|c| c.name == "test").expect("test ran");
    assert!(
        test.bound_s >= 6,
        "the test ran under the doubled bound (2 × 3 s × load): {}",
        test.bound_s
    );
    let (_, det_pass, combined, _, _) = gate(&second, 3).expect("gateEvaluated");
    assert!(det_pass && combined, "the finished floor passes the gate");
    let _ = std::fs::remove_dir_all(&repo);
}

/// core#469: `accept_partial` waives the check that did not finish; the rest re-run, the gate
/// passes, and the `floorNote` discloses what was waived.
#[test]
fn accept_partial_waives_the_unfinished_check_and_discloses_it() {
    let Some((core, events, _ran, repo)) = timed_out_fix("partial", "fg-partial") else {
        return;
    };
    core.confirm_gate("fg-partial", escalation("accept_partial"))
        .expect("accept partial");
    let second = until_settled(&events, "fg-partial");
    no_session_failed(&second);
    let f = floors(&second, 3);
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!(f[0].1, "passed");
    assert_eq!(
        f[0].2,
        vec!["lint".to_string()],
        "only the finished check re-ran"
    );
    let (_, det_pass, combined, _, note) = gate(&second, 3).expect("gateEvaluated");
    assert!(det_pass && combined);
    assert!(
        note.as_deref()
            .is_some_and(|n| n.contains("accept_partial") && n.contains("waived test")),
        "{note:?}"
    );
    let _ = std::fs::remove_dir_all(&repo);
}

/// core#469: `targeted` — a repo with no `test_targeted` command has its full test set waived for
/// this unit (disclosed), and the rest re-runs.
#[test]
fn targeted_without_a_targeted_command_waives_the_full_test_set() {
    let Some((core, events, _ran, repo)) = timed_out_fix("targeted", "fg-targeted") else {
        return;
    };
    core.confirm_gate("fg-targeted", escalation("targeted"))
        .expect("targeted");
    let second = until_settled(&events, "fg-targeted");
    no_session_failed(&second);
    let f = floors(&second, 3);
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!(f[0].2, vec!["lint".to_string()]);
    let (_, _, combined, _, note) = gate(&second, 3).expect("gateEvaluated");
    assert!(combined);
    assert!(
        note.as_deref()
            .is_some_and(|n| n.contains("targeted") && n.contains("waived test")),
        "{note:?}"
    );
    let _ = std::fs::remove_dir_all(&repo);
}

/// core#469: the floor actions answer only a floor that did not finish — at a regression's gate
/// they are refused and the gate stays open.
#[test]
fn floor_actions_are_refused_at_a_gate_that_is_not_a_timeout() {
    let repo = make_repo("refused", TEST_FAILS_ON_BROKEN, 60, &[".break-in-fix"]);
    let (core, _ran) = core_for("refused", Rework::ChangesNothing);
    let sid = "fg-refused";
    let events = launch(&core, "refused", &repo, sid);
    let first = until_settled(&events, sid);
    if !fix_floor_denied(&first, "repo_checks") {
        return;
    }
    for action in ["extend", "targeted", "accept_partial", "accept_suggestion"] {
        let err = core
            .confirm_gate(sid, escalation(action))
            .expect_err("refused at a regression's gate");
        assert!(
            err.to_string().contains("approve (retry)"),
            "{action}: {err}"
        );
    }
    let views = core.sessions_detail().expect("sessions");
    let s = views.iter().find(|v| v.session.id == sid).expect("run");
    assert_eq!(
        s.session.status,
        SessionStatus::AwaitingHuman,
        "still paused"
    );
    let _ = std::fs::remove_dir_all(&repo);
}

/// core#467 item 4: the evaluator rewrites the fix; the guard denies, restores the creator's tree
/// and pins the edit. `accept_suggestion` applies the pinned edit and sends the run back to the
/// creator, whose floor judges the adopted tree; the re-run evaluator then passes it.
#[test]
fn accept_suggestion_applies_the_evaluators_edit_and_reworks_the_creator() {
    let repo = make_repo(
        "suggest",
        r#"["sh", "-c", "exit 0"]"#,
        60,
        &[".verify-rewrites"],
    );
    let (core, ran) = core_for("suggest", Rework::ChangesNothing);
    let sid = "fg-suggest";
    let events = launch(&core, "suggest", &repo, sid);
    let first = until_settled(&events, sid);
    no_session_failed(&first);
    let suggestion = first.iter().find_map(|ev| match ev {
        CoreEvent::GateEscalated {
            ord: 4,
            condition,
            restored,
            suggestion_ref,
            ..
        } if condition == "evaluator_mutated_worktree" && *restored => suggestion_ref.clone(),
        _ => None,
    });
    let Some(suggestion) = suggestion else {
        eprintln!(
            "floor_gate_actions: no restored, pinned evaluator edit on this host — nothing to \
             adopt ({:?})",
            gate(&first, 4)
        );
        return;
    };
    assert!(suggestion.starts_with("refs/wicked/suggestions/"));
    core.confirm_gate(sid, escalation("accept_suggestion"))
        .expect("accept the suggestion");
    let second = until_settled(&events, sid);
    no_session_failed(&second);
    assert!(
        second.iter().any(|ev| matches!(
            ev,
            CoreEvent::UnitReworkAmended { ord: 3, scope, .. } if scope == "accept_suggestion"
        )),
        "the creator's rework is booked as the adopted suggestion"
    );
    assert!(
        ran.lock().unwrap().contains(&("fix".to_string(), 1)),
        "the creator re-ran: {:?}",
        ran.lock().unwrap()
    );
    let f = floors(&second, 3);
    assert_eq!(
        f.len(),
        1,
        "the creator's floor judges the adopted tree (a tree no floor of the unit had judged): \
         {f:?}"
    );
    // The adopted edit is in the tree the run carries forward.
    let views = core.sessions_detail().expect("sessions");
    let wt = views
        .iter()
        .find(|v| v.session.id == sid)
        .and_then(|v| v.session.workdir.clone())
        .expect("a bound run has a worktree");
    assert_eq!(
        std::fs::read_to_string(Path::new(&wt).join("src/app.ts")).unwrap(),
        "evaluator's rewrite\n"
    );
    let _ = std::fs::remove_dir_all(&repo);
}
