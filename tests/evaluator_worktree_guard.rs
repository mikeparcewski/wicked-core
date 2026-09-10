//! F-036 / F-039 proving tests — evaluator ≠ creator held STRUCTURALLY, and `verify` re-derives
//! "done" by running the repo's own checks.
//!
//! The acceptance run this closes: a `bug` run's `verify` phase (role evaluator, `executes_code:
//! false`) landed on an unchecked seat, REWROTE the fix it was reviewing, and its gate PASSED on
//! the one criterion "the run left a change in its worktree" — while the `fix` gate before it had
//! passed with nothing evaluated at all. These tests drive the same `bug` workflow through the real
//! actor (`Core::spawn_with_engine`) against a real registered repo + linked worktree, with a fake
//! seat that behaves exactly like that evaluator, and pin what the engine must now do:
//!
//! 1. an evaluator phase whose seat edits a file is DENIED at its gate, the denial names the path,
//!    `evaluatorMutatedWorktree` carries it, and the run escalates to a human rather than
//!    certifying the rewrite;
//! 2. the `fix` gate has a deterministic floor AND a distinct-seat verdict (never `combined: true`
//!    with nothing evaluated);
//! 3. a clean evaluator passes, and the engine runs the repository's own checks (`cargo test` on
//!    a Cargo fixture) as a deterministic floor whose exit code is attached to the gate;
//! 4. a failing check DENIES the verify gate with the exit code and output tail as evidence.

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
            recommendation: "x".into(),
            top_risk: "none".into(),
            change_my_mind: "no".into(),
            disqualifier: None,
            confidence: Confidence::default(),
            provenance: "stub".into(),
        })
    }
}

/// What the fake seat does when it runs the `verify` phase.
#[derive(Clone, Copy)]
enum VerifyBehaviour {
    /// The F-036 shape: rewrite the creator's fix while "reviewing" it.
    RewritesTheFix,
    /// A well-behaved evaluator: reads, runs things, writes nothing.
    LeavesTreeAlone,
}

/// A seat that plays every role of the `bug` workflow deterministically:
/// * `fix` (the Creator) writes the fix into the worktree — a real diff, so the evidence floors pass;
/// * `verify` (the Evaluator) either rewrites that fix or leaves the tree alone;
/// * the engine's own JUDGE sessions (`session_id == "validator"`) get a well-formed PASS, so the
///   distinct-seat layer-2 verdict is exercised without an LLM;
/// * everything else reports prose.
struct ScriptedSeat {
    verify: VerifyBehaviour,
    ran: Arc<Mutex<Vec<String>>>,
}
impl StepRunner for ScriptedSeat {
    fn run_unit(&self, input: &StepInput) -> StepOutput {
        let phase = input.unit.phase_id().unwrap_or("").to_string();
        self.ran.lock().unwrap().push(phase.clone());
        let mut output = format!("did: {phase}");
        if input.unit.session_id == "validator" {
            // The agent judge's contract: the SAME verdict word on the first and the last line.
            output = "PASS\nthe work meets the criterion\nPASS".to_string();
        } else if let Some(wd) = &input.workdir {
            match phase.as_str() {
                "fix" => {
                    std::fs::write(wd.join("src/app.ts"), "fixed\n").unwrap();
                    std::fs::write(wd.join("src/fix.ts"), "the fix\n").unwrap();
                }
                "verify" => {
                    if let VerifyBehaviour::RewritesTheFix = self.verify {
                        // Exactly F-036: the evaluator "improves" the fix under review.
                        std::fs::write(wd.join("src/app.ts"), "evaluator's rewrite\n").unwrap();
                        std::fs::remove_file(wd.join("src/fix.ts")).unwrap();
                    }
                }
                _ => {}
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

/// A throwaway repo with one commit: `src/app.ts` (what the fix edits) and, when `cargo_test`
/// is given, a minimal crate whose single test passes or fails — the repository's OWN check.
fn make_git_repo(name: &str, cargo_test: Option<bool>) -> PathBuf {
    let repo = std::env::temp_dir().join(format!(
        "wicked-core-wtguard-{name}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&repo);
    std::fs::create_dir_all(repo.join("src")).unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.email", "t@example.invalid"]);
    git(&repo, &["config", "user.name", "t"]);
    git(&repo, &["config", "commit.gpgsign", "false"]);
    std::fs::write(repo.join("README.md"), "hello\n").unwrap();
    std::fs::write(repo.join("src/app.ts"), "buggy\n").unwrap();
    if let Some(passes) = cargo_test {
        std::fs::write(
            repo.join("Cargo.toml"),
            "[package]\nname = \"wtguard_fixture\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\
             [lib]\npath = \"src/lib.rs\"\n[workspace]\n",
        )
        .unwrap();
        let body = if passes {
            "#[cfg(test)]\nmod t {\n    #[test]\n    fn ok() {}\n}\n"
        } else {
            "#[cfg(test)]\nmod t {\n    #[test]\n    fn boom() {\n        assert!(false, \
             \"REPO CHECK BOOM\");\n    }\n}\n"
        };
        std::fs::write(repo.join("src/lib.rs"), body).unwrap();
        // `cargo test` generates `Cargo.lock` in a fixture that ships none — ignored, as a library
        // crate's `.gitignore` would. Build artifacts go to the floor's scratch (`CARGO_TARGET_DIR`),
        // so `target/` needs no ignore; anything else the check wrote would (correctly) trip the
        // worktree guard's FINAL comparison.
        std::fs::write(repo.join(".gitignore"), "Cargo.lock\n").unwrap();
    }
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "init"]);
    repo
}

fn core_for(name: &str, verify: VerifyBehaviour) -> (Core, Arc<Mutex<Vec<String>>>) {
    let dir = std::env::temp_dir().join(format!(
        "wicked-core-wtguard-db-{name}-{}-{:?}",
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
            verify,
            ran: ran.clone(),
        }),
    );
    (core, ran)
}

fn bug_run(session_id: &str, repo_ref: &str) -> LaunchSpec {
    LaunchSpec {
        project_id: None,
        problem: "Fix the bug in src/app.ts".into(),
        // Two seats, so evaluator ≠ creator has a distinct seat to route the evaluator onto.
        clis: vec![cli("a"), cli("b")],
        entity_mode: wicked_core::EntityMode::Shared,
        session_id: session_id.into(),
        human_confirm: HumanConfirm::None,
        repo_ref: Some(repo_ref.to_string()),
        workflow: Some("bug".into()),
        extra_write_roots: Vec::new(),
        extra_read_roots: Vec::new(),
        project_graph: None,
    }
}

/// Generous: the Cargo-fixture runs compile a crate inside the worktree under `cargo test -j`
/// load, and a satisfied wait returns the moment the status matches.
const WAIT_BUDGET: Duration = Duration::from_secs(240);

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
    while let Ok(ev) = events.recv_timeout(Duration::from_millis(300)) {
        out.push(ev);
    }
    out
}

/// The `gateEvaluated` record for `ord`, destructured into what these tests assert on.
struct Gate {
    has_floor: bool,
    criterion: Option<String>,
    deterministic_pass: bool,
    agent_verdict: Option<String>,
    denial_reason: Option<String>,
    denial_source: Option<String>,
    combined: bool,
}

/// Codex review on #414: the repo checks floor NEVER runs repo-controlled scripts without an OS
/// write boundary — on a host with no `sandbox-exec`/`bwrap` (a bare Linux CI runner) the floor
/// fails CLOSED and the gate denies with that reason. The tests below that expect the checks to
/// have RUN branch on this: where the host cannot arm a boundary they assert the fail-closed
/// contract instead (denied, source `repo_checks`, nothing ran) — never a silent skip.
fn checks_refused_without_a_boundary(gate: &Gate) -> bool {
    gate.denial_source.as_deref() == Some("repo_checks")
        && gate
            .denial_reason
            .as_deref()
            .is_some_and(|r| r.contains("no OS write boundary could be armed"))
}

/// The fail-closed shape on the wire: `repoChecksEvaluated` fired for `ord`, `passed: false`, no
/// check ran, and the gate's reason says the checks were NOT run.
fn assert_fail_closed_without_boundary(evs: &[CoreEvent], gate: &Gate, ord: u32) {
    eprintln!(
        "evaluator_worktree_guard: no OS-sandbox tool on this host — asserting the fail-closed \
         repo-checks contract instead of the ran-checks path"
    );
    assert!(
        !gate.combined && !gate.deterministic_pass,
        "{:?}",
        gate.denial_reason
    );
    assert!(
        gate.denial_reason
            .as_deref()
            .is_some_and(|r| r.contains("NOT run") && r.contains("fail-closed")),
        "{:?}",
        gate.denial_reason
    );
    let (passed, checks) = evs
        .iter()
        .find_map(|ev| match ev {
            CoreEvent::RepoChecksEvaluated {
                ord: o,
                passed,
                checks,
                ..
            } if *o == ord => Some((*passed, checks.clone())),
            _ => None,
        })
        .expect("repoChecksEvaluated emitted even when the floor refused to run");
    assert!(
        !passed && checks.is_empty(),
        "nothing may run unsandboxed: {checks:?}"
    );
}

fn gate_for(events: &[CoreEvent], want_ord: u32) -> Gate {
    events
        .iter()
        .find_map(|ev| match ev {
            CoreEvent::GateEvaluated {
                ord,
                criterion,
                has_deterministic_floor,
                deterministic_pass,
                agent_verdict,
                denial_reason,
                denial,
                combined,
                ..
            } if *ord == want_ord => Some(Gate {
                has_floor: *has_deterministic_floor,
                criterion: criterion.clone(),
                deterministic_pass: *deterministic_pass,
                agent_verdict: agent_verdict.clone(),
                denial_reason: denial_reason.clone(),
                denial_source: denial.as_ref().map(|d| d.source.clone()),
                combined: *combined,
            }),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no gateEvaluated for ord {want_ord}"))
}

/// F-036, the defect itself: the evaluator rewrites the fix. The gate must DENY on the worktree
/// guard — not pass on "a diff exists" — name the rewritten path, emit the mutation event, and
/// escalate to a human (`bug/verify` is `human_confirm_if: verdict_not_pass`).
#[test]
fn an_evaluator_that_rewrites_the_fix_is_denied_with_the_path_named() {
    let repo = make_git_repo("rewrite", None);
    let (core, ran) = core_for("rewrite", VerifyBehaviour::RewritesTheFix);
    let entry = core
        .register_repo(RepoSpec {
            name: "rewrite".into(),
            root_path: repo.to_str().unwrap().into(),
            registered_at: 1,
        })
        .expect("register");
    let events = core.subscribe();
    core.launch_run(bug_run("r-rewrite", &entry.id))
        .expect("launch");
    assert!(
        wait_status(&core, "r-rewrite", SessionStatus::AwaitingHuman),
        "a rewritten tree must escalate the verify gate to a human, never complete"
    );
    let evs = drain(&events);
    assert!(
        ran.lock().unwrap().iter().any(|p| p == "verify"),
        "the verify unit ran: {:?}",
        ran.lock().unwrap()
    );

    // The verify gate (ord 4): denied by the WORKTREE GUARD, with the path.
    let verify = gate_for(&evs, 4);
    assert!(
        !verify.combined,
        "the rewriting evaluator must not pass its gate"
    );
    assert_eq!(
        verify.denial_source.as_deref(),
        Some("worktree_guard"),
        "the denying layer is the worktree guard, not a floor that happens to notice a diff"
    );
    let reason = verify
        .denial_reason
        .expect("a denied gate carries its reason");
    assert!(
        reason.contains("M src/app.ts") && reason.contains("D src/fix.ts"),
        "the denial names every path the evaluator changed, with its status: {reason}"
    );
    assert!(
        reason.contains("executes_code: false") && reason.contains("git read-tree --reset -u"),
        "the denial names the rule and the restore: {reason}"
    );
    // The pinned diff floor still PASSED (a diff exists) — which is exactly why it could not be
    // the instrument here; the record shows the guard overriding it.
    assert!(verify.has_floor && verify.deterministic_pass);

    // The mutation event names the same paths, for the ledger and the studio.
    let mutation = evs
        .iter()
        .find_map(|ev| match ev {
            CoreEvent::EvaluatorMutatedWorktree {
                ord,
                phase,
                changed,
                head_moved,
                ..
            } if *ord == 4 => Some((phase.clone(), changed.clone(), *head_moved)),
            _ => None,
        })
        .expect("evaluatorMutatedWorktree emitted for the verify unit");
    assert_eq!(mutation.0, "verify");
    let mut paths: Vec<String> = mutation
        .1
        .iter()
        .map(|c| format!("{} {}", c.status, c.path))
        .collect();
    paths.sort();
    assert_eq!(paths, vec!["D src/fix.ts", "M src/app.ts"]);
    assert!(!mutation.2, "HEAD did not move");

    // The repo checks did NOT run over the rewritten tree — certifying the wrong code is worse
    // than running nothing, and the guard's denial is the honest record.
    assert!(
        !evs.iter()
            .any(|ev| matches!(ev, CoreEvent::RepoChecksEvaluated { ord: 4, .. })),
        "repo checks must not run over a tree the evaluator rewrote"
    );

    // F-039: the `fix` gate (ord 3) now evaluates something — the pinned diff floor AND a
    // distinct-seat verdict — instead of `combined: true` over nothing.
    let fix = gate_for(&evs, 3);
    assert!(fix.combined, "the creator's real diff passes its gate");
    assert!(fix.has_floor, "the fix gate carries a deterministic floor");
    assert!(
        fix.criterion
            .as_deref()
            .is_some_and(|c| c.contains("the run left a change in its worktree")),
        "the fix gate's criterion is the evidence floor: {:?}",
        fix.criterion
    );
    assert_eq!(
        fix.agent_verdict.as_deref(),
        Some("pass"),
        "a distinct seat judged the fix (layer 2 ran)"
    );

    // The recon phases (ord 1, 2) are guarded too and left the tree alone — they pass.
    for ord in [1, 2] {
        let g = gate_for(&evs, ord);
        assert!(
            g.combined,
            "a guarded phase that writes nothing passes (ord {ord})"
        );
    }
    let _ = std::fs::remove_dir_all(&repo);
}

/// A well-behaved evaluator over a repo with a PASSING check: the run completes, and the verify
/// gate's deterministic floor is BOTH the pinned diff floor and the engine-run `cargo test`, whose
/// exit 0 is attached to the gate as `repoChecksEvaluated`.
#[test]
fn a_clean_evaluator_passes_and_the_repo_checks_are_the_gates_evidence() {
    let repo = make_git_repo("clean", Some(true));
    let (core, _ran) = core_for("clean", VerifyBehaviour::LeavesTreeAlone);
    let entry = core
        .register_repo(RepoSpec {
            name: "clean".into(),
            root_path: repo.to_str().unwrap().into(),
            registered_at: 1,
        })
        .expect("register");
    let events = core.subscribe();
    core.launch_run(bug_run("r-clean", &entry.id))
        .expect("launch");
    let completed = wait_status(&core, "r-clean", SessionStatus::Completed);
    if !completed {
        // The one legitimate way this run does NOT complete: the host cannot arm a write
        // boundary, so the floor refused to run the checks and the gate escalated.
        assert!(
            wait_status(&core, "r-clean", SessionStatus::AwaitingHuman),
            "a clean evaluator over passing checks completes the run"
        );
        let evs = drain(&events);
        let verify = gate_for(&evs, 4);
        assert!(
            checks_refused_without_a_boundary(&verify),
            "a clean evaluator over passing checks completes the run: {:?}",
            verify.denial_reason
        );
        assert_fail_closed_without_boundary(&evs, &verify, 4);
        let _ = std::fs::remove_dir_all(&repo);
        return;
    }
    let evs = drain(&events);

    let verify = gate_for(&evs, 4);
    assert!(verify.combined && verify.deterministic_pass);
    assert!(verify.has_floor);
    let criterion = verify
        .criterion
        .expect("the verify gate names its criterion");
    assert!(
        criterion.contains("the run left a change in its worktree")
            && criterion.contains("repository's own checks pass"),
        "the criterion names BOTH deterministic instruments: {criterion}"
    );
    let (passed, checks, skipped) = evs
        .iter()
        .find_map(|ev| match ev {
            CoreEvent::RepoChecksEvaluated {
                ord,
                passed,
                checks,
                skipped,
                ..
            } if *ord == 4 => Some((*passed, checks.clone(), skipped.clone())),
            _ => None,
        })
        .expect("repoChecksEvaluated emitted for the verify unit");
    assert!(passed && skipped.is_empty());
    assert_eq!(
        checks.len(),
        1,
        "one check detected: the Cargo manifest's `cargo test`"
    );
    assert_eq!(checks[0].name, "cargo-test");
    assert_eq!(checks[0].argv, vec!["cargo", "test"]);
    assert_eq!(checks[0].exit_code, Some(0), "{:?}", checks[0]);
    assert!(
        checks[0].stdout_tail.contains("test result: ok"),
        "the engine observed cargo's own output: {}",
        checks[0].stdout_tail
    );
    assert!(
        !evs.iter()
            .any(|ev| matches!(ev, CoreEvent::EvaluatorMutatedWorktree { .. })),
        "a clean evaluator produces no mutation event"
    );
    let _ = std::fs::remove_dir_all(&repo);
}

/// Codex review on #414: a "passing" check script that EDITS a tracked file. The evaluator leaves
/// the tree alone and the repo checks exit 0 — yet the gate must DENY on the worktree guard,
/// because the FINAL comparison is taken after the checks ran, not right after the seat returned.
/// Needs `npm` on PATH (every hosted CI runner; developer machines) — otherwise says so.
#[test]
fn a_passing_check_that_mutates_source_is_caught_by_the_final_comparison() {
    if Command::new("npm").arg("--version").output().is_err() {
        eprintln!("npm not on PATH — the mutating-check test cannot run here");
        return;
    }
    let repo = make_git_repo("mutating-check", None);
    // A `test` script that passes while appending to a tracked source file; node_modules present
    // so the floor installs nothing (no network).
    std::fs::write(
        repo.join("package.json"),
        r#"{"name":"mutating-check","version":"0.0.0","scripts":{"test":"node -e \"require('fs').appendFileSync('src/app.ts','// touched by the test script\\n')\""}}"#,
    )
    .unwrap();
    std::fs::create_dir_all(repo.join("node_modules")).unwrap();
    std::fs::write(repo.join(".gitignore"), "node_modules/\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "add a mutating test script"]);

    let (core, _ran) = core_for("mutating-check", VerifyBehaviour::LeavesTreeAlone);
    let entry = core
        .register_repo(RepoSpec {
            name: "mutating-check".into(),
            root_path: repo.to_str().unwrap().into(),
            registered_at: 1,
        })
        .expect("register");
    let events = core.subscribe();
    core.launch_run(bug_run("r-mutating", &entry.id))
        .expect("launch");
    assert!(
        wait_status(&core, "r-mutating", SessionStatus::AwaitingHuman),
        "a check that edits the tree must not certify it"
    );
    let evs = drain(&events);
    let verify = gate_for(&evs, 4);
    assert!(!verify.combined);
    if checks_refused_without_a_boundary(&verify) {
        // No boundary ⇒ the mutating script never ran, so there is no write for the final
        // comparison to catch; the floor's refusal is the denial, and the tree stayed clean.
        assert_fail_closed_without_boundary(&evs, &verify, 4);
        assert!(
            !evs.iter()
                .any(|ev| matches!(ev, CoreEvent::EvaluatorMutatedWorktree { .. })),
            "nothing ran, so nothing mutated"
        );
        let _ = std::fs::remove_dir_all(&repo);
        return;
    }
    assert_eq!(
        verify.denial_source.as_deref(),
        Some("worktree_guard"),
        "the FINAL comparison caught the check's write: {:?}",
        verify.denial_reason
    );
    assert!(
        verify
            .denial_reason
            .as_deref()
            .is_some_and(|r| r.contains("M src/app.ts")),
        "{:?}",
        verify.denial_reason
    );
    // The checks themselves PASSED and were recorded — the passing script is exactly the trap.
    let checks_passed = evs.iter().find_map(|ev| match ev {
        CoreEvent::RepoChecksEvaluated { ord: 4, passed, .. } => Some(*passed),
        _ => None,
    });
    assert_eq!(checks_passed, Some(true), "the mutating check exited 0");
    let _ = std::fs::remove_dir_all(&repo);
}

/// F-039, the floor biting: the evaluator leaves the tree alone but the repository's own check
/// FAILS. The seat may claim whatever it likes — the engine ran `cargo test`, saw exit 101 and
/// the assertion text, and the gate denies on THAT evidence.
#[test]
fn a_failing_repo_check_denies_the_verify_gate_with_the_exit_code_as_evidence() {
    let repo = make_git_repo("failing", Some(false));
    let (core, _ran) = core_for("failing", VerifyBehaviour::LeavesTreeAlone);
    let entry = core
        .register_repo(RepoSpec {
            name: "failing".into(),
            root_path: repo.to_str().unwrap().into(),
            registered_at: 1,
        })
        .expect("register");
    let events = core.subscribe();
    core.launch_run(bug_run("r-failing", &entry.id))
        .expect("launch");
    assert!(
        wait_status(&core, "r-failing", SessionStatus::AwaitingHuman),
        "a failing repo check must escalate the verify gate to a human"
    );
    let evs = drain(&events);

    let verify = gate_for(&evs, 4);
    assert!(!verify.combined);
    assert!(
        !verify.deterministic_pass,
        "the deterministic layer failed — the repo checks are part of it"
    );
    assert_eq!(verify.denial_source.as_deref(), Some("repo_checks"));
    if checks_refused_without_a_boundary(&verify) {
        assert_fail_closed_without_boundary(&evs, &verify, 4);
        let _ = std::fs::remove_dir_all(&repo);
        return;
    }
    let reason = verify.denial_reason.expect("reason");
    assert!(
        reason.contains("cargo-test: exit 101") && reason.contains("test result: FAILED"),
        "the denial carries the exit code and cargo's own verdict line from the captured tail: \
         {reason}"
    );
    let (passed, checks) = evs
        .iter()
        .find_map(|ev| match ev {
            CoreEvent::RepoChecksEvaluated {
                ord,
                passed,
                checks,
                ..
            } if *ord == 4 => Some((*passed, checks.clone())),
            _ => None,
        })
        .expect("repoChecksEvaluated emitted");
    assert!(!passed);
    assert_eq!(checks[0].exit_code, Some(101));
    assert!(
        format!("{}{}", checks[0].stdout_tail, checks[0].stderr_tail).contains("REPO CHECK BOOM")
    );
    let _ = std::fs::remove_dir_all(&repo);
}
