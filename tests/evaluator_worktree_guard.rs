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
//! 4. a check the creator BREAKS (green on the run base, red on the head) DENIES the creator's own
//!    gate with the exit code, output tail and the base comparison as evidence — before verify
//!    ever runs (core#467); a check the base fails IDENTICALLY is recorded and denies nothing
//!    (F-RC2-009: the floor's sandbox, not the change, is the cause).
//! 5. (core#464) a denial on ANY unit — the read-only `reproduce` rung writing a note into the
//!    tree, a governance deny on its output — PAUSES the run at the escalation gate with a route
//!    back (retry against the restored tree / cancel) instead of ending it `sessionFailed`, and a
//!    read-only unit has a NOTES ROOT outside the tree where a note never trips the guard.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use wicked_apps_core::{ConformanceClaim, Decision};
use wicked_council::types::{Category, Confidence, Dispatcher, InputMode, Vote};
use wicked_council::{AgenticCli, CouncilTask};

use wicked_core::{
    decisions_path_for, gov_run_dir, Core, CoreEvent, HumanConfirm, HumanDecision, LaunchSpec,
    RepoSpec, SessionStatus, StepInput, StepOutput, StepRunner, StepStatus,
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

/// What the fake seat does when it runs the `reproduce` phase — the read-only recon rung whose
/// guard denial used to END the run (core#464).
#[derive(Clone, Copy)]
enum ReproduceBehaviour {
    /// Reads and reports; writes nothing.
    LeavesTreeAlone,
    /// The core#464 shape (runs e20a3ffb / dd5b8f54): writes its analysis note INTO the worktree —
    /// on its first attempt only, so the retry a human approves behaves.
    WritesANoteIntoTheTree,
    /// Writes the same note under the unit's NOTES ROOT, the sanctioned place outside the tree.
    WritesANoteUnderTheNotesRoot,
    /// Produces a real result, and a governance Deny lands in the run's decisions log for the
    /// unit's phase — the retroactive `boundary-deny` of core#463 item 3 (F-RC1-046).
    TripsTheBoundary,
}

/// A seat that plays every role of the `bug` workflow deterministically:
/// * `reproduce` (a read-only Neutral rung) behaves per [`ReproduceBehaviour`];
/// * `fix` (the Creator) writes the fix into the worktree — a real diff, so the evidence floors pass;
/// * `verify` (the Evaluator) either rewrites that fix or leaves the tree alone;
/// * the engine's own JUDGE sessions (`session_id == "validator"`) get a well-formed PASS, so the
///   distinct-seat layer-2 verdict is exercised without an LLM;
/// * everything else reports prose.
struct ScriptedSeat {
    verify: VerifyBehaviour,
    reproduce: ReproduceBehaviour,
    ran: Arc<Mutex<Vec<String>>>,
    /// Every governed-unit input the seat was handed, for the tests that inspect what the engine
    /// told it (the notes root, the write boundary).
    inputs: Arc<Mutex<Vec<StepInput>>>,
}
impl StepRunner for ScriptedSeat {
    fn run_unit(&self, input: &StepInput) -> StepOutput {
        let phase = input.unit.phase_id().unwrap_or("").to_string();
        self.ran.lock().unwrap().push(phase.clone());
        if input.unit.session_id != "validator" {
            self.inputs.lock().unwrap().push(input.clone());
        }
        let mut output = format!("did: {phase}");
        if input.unit.session_id == "validator" {
            // The agent judge's contract: the SAME verdict word on the first and the last line.
            output = "PASS\nthe work meets the criterion\nPASS".to_string();
        } else if let Some(wd) = &input.workdir {
            match phase.as_str() {
                "reproduce" => match self.reproduce {
                    ReproduceBehaviour::LeavesTreeAlone => {}
                    ReproduceBehaviour::WritesANoteIntoTheTree => {
                        if input.attempt == 0 {
                            std::fs::create_dir_all(wd.join("evidence")).unwrap();
                            std::fs::write(
                                wd.join("evidence/repro.md"),
                                "# reproduce\nthe bug reproduces on main\n",
                            )
                            .unwrap();
                        }
                    }
                    ReproduceBehaviour::WritesANoteUnderTheNotesRoot => {
                        if let Some(root) = input.unit.notes_root.as_deref() {
                            std::fs::write(Path::new(root).join("repro.md"), "# reproduce\n")
                                .unwrap();
                        }
                    }
                    ReproduceBehaviour::TripsTheBoundary => {
                        // Exactly what `wicked-core gate-hook` appends when a tool call trips a
                        // deny policy — at the unit's REAL phase, after the work was done.
                        let claim = ConformanceClaim {
                            claim_id: format!("hookdeny-{}", input.unit.ord),
                            scope: format!("wicked-agent/{}/unit/x", input.run_id),
                            phase: format!("unit-{}", input.unit.ord),
                            policy_ids: vec!["pol-deny-estate-index".into()],
                            decision: Decision::Deny,
                            obligations: vec![],
                            evaluated_context_ref: "sha256:test".into(),
                            criteria: "no `wicked-estate index` inside a governed unit".into(),
                            evaluator_identity: "wicked-governance".into(),
                            evaluated_at: 1_750_000_000,
                        };
                        let path = decisions_path_for(&input.run_id, input.attempt);
                        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                        std::fs::write(
                            &path,
                            format!("{}\n", serde_json::to_string(&claim).unwrap()),
                        )
                        .unwrap();
                        output = "reproduced: src/app.ts still reads `buggy` on main; the fix \
                                  must change that line"
                            .to_string();
                    }
                },
                "fix" => {
                    std::fs::write(wd.join("src/app.ts"), "fixed\n").unwrap();
                    std::fs::write(wd.join("src/fix.ts"), "the fix\n").unwrap();
                    // core#467: when the fixture asks for it, the creator BREAKS the repo's own
                    // test — a regression the creator's floor must catch before verify.
                    if wd.join(".break-in-fix").is_file() {
                        std::fs::write(
                            wd.join("src/lib.rs"),
                            "#[cfg(test)]\nmod t {\n    #[test]\n    fn boom() {\n        \
                             assert!(false, \"REPO CHECK BOOM\");\n    }\n}\n",
                        )
                        .unwrap();
                    }
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
        // DES-L1 PR-1A (D-9): `bug.verify` is an Evaluator agent unit — the seat answers the
        // verdict contract the fold now parses (the guard, floors and judge still gate it).
        if input.unit.session_id != "validator"
            && input.unit.role == wicked_core::PhaseRole::Evaluator
            && input.unit.tool_cmd.is_none()
        {
            output.push_str("\nVERDICT: PASS");
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
    // The Windows runner checks out with `core.autocrlf=true`; the engine's restore goes through
    // git's checkout, so a byte-exact LF assertion on a restored file needs the conversion off.
    git(&repo, &["config", "core.autocrlf", "false"]);
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

type Inputs = Arc<Mutex<Vec<StepInput>>>;

fn core_with(
    name: &str,
    verify: VerifyBehaviour,
    reproduce: ReproduceBehaviour,
) -> (Core, Arc<Mutex<Vec<String>>>, Inputs) {
    let dir = std::env::temp_dir().join(format!(
        "wicked-core-wtguard-db-{name}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("estate.db").to_str().unwrap().to_string();
    let ran = Arc::new(Mutex::new(Vec::new()));
    let inputs: Inputs = Arc::new(Mutex::new(Vec::new()));
    let core = Core::spawn_with_engine(
        db,
        Arc::new(StubDispatcher),
        Arc::new(ScriptedSeat {
            verify,
            reproduce,
            ran: ran.clone(),
            inputs: inputs.clone(),
        }),
    );
    (core, ran, inputs)
}

fn core_for(name: &str, verify: VerifyBehaviour) -> (Core, Arc<Mutex<Vec<String>>>) {
    let (core, ran, _inputs) = core_with(name, verify, ReproduceBehaviour::LeavesTreeAlone);
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
        auto_deliver: false,
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

/// Collect events until one satisfies `done` (returned WITH the match) or the wait budget runs
/// out — an `Err` naming the timeout as a timeout, never as an outcome.
fn wait_for_event(
    events: &std::sync::mpsc::Receiver<CoreEvent>,
    done: impl Fn(&CoreEvent) -> bool,
) -> Result<Vec<CoreEvent>, String> {
    let start = Instant::now();
    let mut out = Vec::new();
    while start.elapsed() < WAIT_BUDGET {
        match events.recv_timeout(Duration::from_millis(250)) {
            Ok(ev) => {
                let hit = done(&ev);
                out.push(ev);
                if hit {
                    return Ok(out);
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    Err(format!(
        "the awaited event did not arrive within {WAIT_BUDGET:?} (a timeout, not an outcome); \
         saw {} events",
        out.len()
    ))
}

fn session_failed(evs: &[CoreEvent]) -> bool {
    evs.iter()
        .any(|e| matches!(e, CoreEvent::SessionFailed { .. }))
}

/// The `awaitingHuman` for `ord`: `(reviewing_ord, gate_kind, prompt)`.
fn gate_pause(evs: &[CoreEvent], want_ord: u32) -> (Option<u32>, String, String) {
    evs.iter()
        .find_map(|ev| match ev {
            CoreEvent::AwaitingHuman {
                ord,
                reviewing_ord,
                gate_kind,
                prompt,
                ..
            } if *ord == want_ord => Some((*reviewing_ord, gate_kind.clone(), prompt.clone())),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no awaitingHuman for ord {want_ord}"))
}

/// The `gateEscalated` for `ord`, destructured into what the core#464 tests assert on.
struct Escalation {
    condition: String,
    denial_source: String,
    def_gate: bool,
    output_captured: bool,
    restored: bool,
    discarded: Vec<String>,
    suggestion_ref: Option<String>,
    attempt: u32,
}

fn escalation_for(evs: &[CoreEvent], want_ord: u32) -> Escalation {
    evs.iter()
        .find_map(|ev| match ev {
            CoreEvent::GateEscalated {
                ord,
                condition,
                denial_source,
                def_gate,
                output_captured,
                restored,
                discarded,
                suggestion_ref,
                attempt,
                ..
            } if *ord == want_ord => Some(Escalation {
                condition: condition.clone(),
                denial_source: denial_source.clone(),
                def_gate: *def_gate,
                output_captured: *output_captured,
                restored: *restored,
                discarded: discarded
                    .iter()
                    .map(|c| format!("{} {}", c.status, c.path))
                    .collect(),
                suggestion_ref: suggestion_ref.clone(),
                attempt: *attempt,
            }),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no gateEscalated for ord {want_ord}"))
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
    // core#431 (F-3R2-010): the engine RAN the restore — the denial says the edit was discarded
    // instead of printing a `git read-tree` command for the operator to run by hand.
    assert!(
        reason.contains("executes_code: false")
            && reason.contains("DISCARDED")
            && reason.contains("restored the creator's tree"),
        "the denial names the rule and states the restore: {reason}"
    );
    assert!(
        !reason.contains("git read-tree --reset -u"),
        "no manual remedy once the engine restored the tree: {reason}"
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
                restored,
                restore_error,
                ..
            } if *ord == 4 => Some((
                phase.clone(),
                changed.clone(),
                *head_moved,
                *restored,
                restore_error.clone(),
            )),
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
    // core#431 (F-3R2-010): the engine restored the creator's tree itself — the event says so,
    // `worktreeRestored` names what was discarded, the worktree holds the creator's fix again,
    // and the human prompt says Approve retries against the RESTORED tree (not the evaluator's).
    assert!(mutation.3, "restored: {:?}", mutation.4);
    let discarded = evs
        .iter()
        .find_map(|ev| match ev {
            CoreEvent::WorktreeRestored {
                ord,
                phase,
                discarded,
                head,
                ..
            } if *ord == 4 => Some((phase.clone(), discarded.clone(), head.clone())),
            _ => None,
        })
        .expect("worktreeRestored emitted for the verify unit");
    assert_eq!(discarded.0, "verify");
    let mut discarded_paths: Vec<String> = discarded
        .1
        .iter()
        .map(|c| format!("{} {}", c.status, c.path))
        .collect();
    discarded_paths.sort();
    assert_eq!(
        discarded_paths, paths,
        "the restore discards exactly the mutation"
    );
    assert!(
        discarded.2.is_none(),
        "HEAD had not moved, so none was reset"
    );
    let wt = repo.join("wicked-worktrees").join("r-rewrite");
    assert_eq!(
        std::fs::read_to_string(wt.join("src/app.ts")).unwrap(),
        "fixed\n",
        "the creator's fix is back in the worktree"
    );
    assert_eq!(
        std::fs::read_to_string(wt.join("src/fix.ts")).unwrap(),
        "the fix\n",
        "the file the evaluator deleted is back"
    );
    let prompt = evs
        .iter()
        .find_map(|ev| match ev {
            CoreEvent::AwaitingHuman { ord, prompt, .. } if *ord == 4 => Some(prompt.clone()),
            _ => None,
        })
        .expect("the verify gate escalated to a human");
    assert!(
        prompt.contains("edit was discarded") && prompt.contains("restored tree"),
        "the gate prompt says what Approve now means: {prompt}"
    );

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

/// The `repoChecksEvaluated` record for `ord`: `(floor, outcome, passed, checks)`.
fn checks_for(
    events: &[CoreEvent],
    want_ord: u32,
) -> (String, String, bool, Vec<wicked_core::CheckRun>) {
    events
        .iter()
        .find_map(|ev| match ev {
            CoreEvent::RepoChecksEvaluated {
                ord,
                floor,
                outcome,
                passed,
                checks,
                ..
            } if *ord == want_ord => {
                Some((floor.clone(), outcome.clone(), *passed, checks.clone()))
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("no repoChecksEvaluated for ord {want_ord}"))
}

/// core#467, the creator floor biting: the fixture's base PASSES `cargo test`; the fix phase
/// BREAKS it. The creator's own floor runs at the end of the fix phase, the baseline diff finds
/// the base green ⇒ REGRESSION, and the fix unit is denied on that evidence — the seat may claim
/// whatever it likes; verify never runs on a red tree. The run PAUSES at the escalation gate ON
/// the fix unit (core#464, class `floor_failed` — the token S4b's rework route keys on) one phase
/// earlier than before, with the check tails, the base comparison and the bound on the record.
#[test]
fn a_regression_the_creator_introduces_is_denied_at_the_creator_gate_before_verify() {
    let repo = make_git_repo("regression", Some(true));
    std::fs::write(repo.join(".break-in-fix"), "").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "ask the fix to break the test"]);
    let (core, ran) = core_for("regression", VerifyBehaviour::LeavesTreeAlone);
    let entry = core
        .register_repo(RepoSpec {
            name: "regression".into(),
            root_path: repo.to_str().unwrap().into(),
            registered_at: 1,
        })
        .expect("register");
    let events = core.subscribe();
    core.launch_run(bug_run("r-regression", &entry.id))
        .expect("launch");
    assert!(
        wait_status(&core, "r-regression", SessionStatus::AwaitingHuman),
        "a regression is denied at the creator gate (sandboxed) or fails closed at verify — \
         either way the run PAUSES at a gate (core#464), never sessionFailed"
    );
    let evs = drain(&events);
    assert!(
        !session_failed(&evs),
        "no sessionFailed without a decided gate"
    );
    if gate_for(&evs, 3).denial_source.as_deref() != Some("repo_checks") {
        // No OS boundary on this host: the creator's DEFAULT floor is disclosed, not denied
        // (F-7R2-005), so the red tree reaches verify, whose DECLARED floor fails closed.
        let verify = gate_for(&evs, 4);
        assert!(
            checks_refused_without_a_boundary(&verify),
            "{:?}",
            verify.denial_reason
        );
        assert_fail_closed_without_boundary(&evs, &verify, 4);
        let _ = std::fs::remove_dir_all(&repo);
        return;
    }
    // The creator's floor denied: the run paused at the ESCALATION gate on the fix unit
    // (core#464), classed as a floor failure — the token S4b's rework route keys on.
    let (reviewing, kind, _prompt) = gate_pause(&evs, 3);
    assert_eq!((reviewing, kind.as_str()), (Some(3), "escalation"));
    let g = escalation_for(&evs, 3);
    assert_eq!(g.condition, "floor_failed");
    assert_eq!(g.denial_source, "repo_checks");
    assert!(
        !ran.lock().unwrap().iter().any(|p| p == "verify"),
        "verify never runs on a red creator tree: {:?}",
        ran.lock().unwrap()
    );
    let fix = gate_for(&evs, 3);
    assert!(!fix.combined && !fix.deterministic_pass);
    assert_eq!(
        fix.denial_source.as_deref(),
        Some("repo_checks"),
        "{:?}",
        fix.denial_reason
    );
    assert!(fix.has_floor);
    let criterion = fix.criterion.expect("the fix gate names its criterion");
    assert!(
        criterion.contains("the run left a change in its worktree")
            && criterion.contains("repository's own checks pass"),
        "the fix gate's criterion is the evidence floor AND the checks: {criterion}"
    );
    let reason = fix.denial_reason.expect("reason");
    assert!(
        reason.contains("cargo-test: exit 101")
            && reason.contains("[regression]")
            && reason.contains("REGRESSION: the run base")
            && reason.contains("test result: FAILED"),
        "the denial carries the exit code, the classification, the base comparison and cargo's \
         own verdict line from the captured tail: {reason}"
    );
    let (floor, outcome, passed, checks) = checks_for(&evs, 3);
    assert_eq!(floor, "creator");
    assert_eq!(outcome, "failed");
    assert!(!passed);
    let c = &checks[0];
    assert_eq!(c.name, "cargo-test");
    assert_eq!(c.exit_code, Some(101));
    assert_eq!(c.outcome(), "failed");
    assert_eq!(c.classification.as_deref(), Some("regression"));
    assert_eq!(c.regressions, vec!["test t::boom".to_string()]);
    assert!(c.pre_existing.is_empty());
    assert!(
        format!("{}{}", c.stdout_tail, c.stderr_tail).contains("REPO CHECK BOOM"),
        "the FULL tail on the record carries the assertion text the bounded denial excerpt cuts"
    );
    let base = c.base.as_deref().expect("the base run rides the check");
    assert!(
        base.run.as_ref().is_some_and(|b| b.passed()),
        "the run base is green: {base:?}"
    );
    assert!(c.bound_s > 0, "the effective bound is on the record");
    assert!(
        !evs.iter()
            .any(|ev| matches!(ev, CoreEvent::GateEvaluated { ord: 4, .. })),
        "no verify gate — the run stopped at the creator"
    );
    let _ = std::fs::remove_dir_all(&repo);
}

/// core#464, the run-loss shape itself (runs e20a3ffb / dd5b8f54): the read-only `reproduce` rung
/// writes its analysis note INTO the worktree. The guard denies and restores exactly as before —
/// and the run now PAUSES at the escalation gate (`evaluator_mutated_worktree`, the reverted path
/// named, `unitDenied` still emitted first) instead of ending `sessionFailed`. Approving the gate
/// re-dispatches the SAME unit (attempt 1) against the restored tree, and the run goes on to `fix`.
#[test]
fn a_recon_phase_that_writes_a_note_into_the_tree_pauses_at_a_gate_and_the_approved_retry_continues(
) {
    let repo = make_git_repo("recon-note", None);
    let (core, ran, _inputs) = core_with(
        "recon-note",
        VerifyBehaviour::LeavesTreeAlone,
        ReproduceBehaviour::WritesANoteIntoTheTree,
    );
    let entry = core
        .register_repo(RepoSpec {
            name: "recon-note".into(),
            root_path: repo.to_str().unwrap().into(),
            registered_at: 1,
        })
        .expect("register");
    let events = core.subscribe();
    core.launch_run(bug_run("r-recon-note", &entry.id))
        .expect("launch");
    assert!(
        wait_status(&core, "r-recon-note", SessionStatus::AwaitingHuman),
        "a guard denial on a read-only recon rung must PAUSE the run at a gate, never fail it"
    );
    let evs = drain(&events);
    assert!(
        !session_failed(&evs),
        "no sessionFailed without a decided gate"
    );
    assert!(
        ran.lock().unwrap().iter().all(|p| p != "fix"),
        "the creator has not run: the run stopped at the denied rung: {:?}",
        ran.lock().unwrap()
    );

    // The gate: kind `escalation`, ON the reproduce unit (ord 2), reviewing itself, with the
    // restored-tree prompt that used to be unreachable for every phase but verify — now naming the
    // reverted path and the engine-authored precedence over `human_confirm: none`.
    let (reviewing, kind, prompt) = gate_pause(&evs, 2);
    assert_eq!((reviewing, kind.as_str()), (Some(2), "escalation"));
    assert!(
        prompt.contains("edit was discarded")
            && prompt.contains("restored tree")
            && prompt.contains("A evidence/repro.md")
            && prompt.contains("`reproduce`")
            && prompt.contains("engine gate"),
        "the prompt says what Approve retries against, which path was reverted, and why the \
         run-level policy did not silence it: {prompt}"
    );
    // The class and the restore outcome, on the wire — what a decision arm keys on.
    let g = escalation_for(&evs, 2);
    assert_eq!(g.condition, "evaluator_mutated_worktree");
    assert_eq!(g.denial_source, "worktree_guard");
    assert!(
        !g.def_gate,
        "the reproduce phase's def gate is `auto` — the ENGINE authored this pause"
    );
    assert!(g.output_captured, "the rung's output exists to be accepted");
    assert!(g.restored, "the creator's tree was put back");
    assert_eq!(g.discarded, vec!["A evidence/repro.md"]);
    assert!(
        g.suggestion_ref
            .as_deref()
            .is_some_and(|r| r.starts_with("refs/wicked/suggestions/")),
        "the discarded note is pinned, not lost: {:?}",
        g.suggestion_ref
    );
    assert_eq!(g.attempt, 0);
    // Observability kept: the denial is still booked, THEN the gate — never the run end.
    let denied_at = evs
        .iter()
        .position(|e| matches!(e, CoreEvent::UnitDenied { ord: 2, .. }))
        .expect("unitDenied for the reproduce unit");
    let gate_at = evs
        .iter()
        .position(|e| matches!(e, CoreEvent::AwaitingHuman { ord: 2, .. }))
        .unwrap();
    assert!(denied_at < gate_at, "unitDenied precedes the gate");
    assert_eq!(
        gate_for(&evs, 2).denial_source.as_deref(),
        Some("worktree_guard")
    );
    assert!(
        evs.iter().any(|e| matches!(
            e,
            CoreEvent::EvaluatorMutatedWorktree { ord: 2, phase, restored: true, .. }
                if phase == "reproduce"
        )),
        "the mutation event names the recon phase and the restore"
    );
    let wt = repo.join("wicked-worktrees").join("r-recon-note");
    assert!(
        !wt.join("evidence/repro.md").exists(),
        "the restore discarded the note from the tree"
    );

    // Approve → the SAME unit re-dispatches (attempt 1, `resumed`), behaves against the restored
    // tree, passes its gate, and the run reaches the creator.
    assert_eq!(
        core.confirm_gate("r-recon-note", HumanDecision::Approve { amend: None })
            .expect("approve the denial gate"),
        SessionStatus::Executing
    );
    let after = wait_for_event(&events, |e| {
        matches!(e, CoreEvent::GateEvaluated { ord: 3, .. })
    })
    .expect("the fix unit ran and was gated after the approved retry");
    assert!(after
        .iter()
        .any(|e| matches!(e, CoreEvent::Resumed { ord: 2, .. })));
    assert!(
        after.iter().any(|e| matches!(
            e,
            CoreEvent::UnitDispatched {
                ord: 2,
                attempt: 1,
                ..
            }
        )),
        "the retry is the same unit at attempt 1"
    );
    assert!(
        after.iter().any(|e| matches!(
            e,
            CoreEvent::GateEvaluated {
                ord: 2,
                combined: true,
                ..
            }
        )),
        "the retried reproduce passed its gate"
    );
    assert!(!session_failed(&after));
    let ran = ran.lock().unwrap().clone();
    assert_eq!(
        ran.iter().filter(|p| *p == "reproduce").count(),
        2,
        "reproduce ran twice: the denied attempt and the approved retry: {ran:?}"
    );
    assert!(
        ran.iter().any(|p| p == "fix"),
        "the run continued to the creator after the retry: {ran:?}"
    );
    let _ = std::fs::remove_dir_all(&repo);
}

/// core#464 / core#463 item 3 (F-RC1-046): a governance Deny recorded for a recon rung AFTER its
/// output was captured — `unitOutputCaptured ok`, then the fold reads the log — pauses the run at
/// the escalation gate as `boundary_deny`, never books a retroactive `sessionFailed`.
#[test]
fn a_boundary_deny_on_a_recon_phase_whose_output_was_captured_pauses_at_a_gate() {
    let repo = make_git_repo("recon-boundary", None);
    let (core, ran, _inputs) = core_with(
        "recon-boundary",
        VerifyBehaviour::LeavesTreeAlone,
        ReproduceBehaviour::TripsTheBoundary,
    );
    let entry = core
        .register_repo(RepoSpec {
            name: "recon-boundary".into(),
            root_path: repo.to_str().unwrap().into(),
            registered_at: 1,
        })
        .expect("register");
    let events = core.subscribe();
    core.launch_run(bug_run("r-recon-boundary", &entry.id))
        .expect("launch");
    assert!(
        wait_status(&core, "r-recon-boundary", SessionStatus::AwaitingHuman),
        "a boundary deny on a recon rung must pause the run at a gate, never fail it"
    );
    let evs = drain(&events);
    assert!(!session_failed(&evs));
    assert!(ran.lock().unwrap().iter().all(|p| p != "fix"));

    let (reviewing, kind, prompt) = gate_pause(&evs, 2);
    assert_eq!((reviewing, kind.as_str()), (Some(2), "escalation"));
    assert!(
        prompt.contains("DENIED by input governance")
            && prompt.contains("output was captured")
            && prompt.contains("hookdeny-2"),
        "the prompt names the denial and the captured output: {prompt}"
    );
    let g = escalation_for(&evs, 2);
    assert_eq!(g.condition, "boundary_deny");
    assert_eq!(g.denial_source, "input_governance");
    assert!(!g.def_gate);
    assert!(g.output_captured);
    assert!(!g.restored && g.discarded.is_empty() && g.suggestion_ref.is_none());
    // The core#463 sequence: the output was captured `ok` BEFORE the denial gated the run.
    let captured_at = evs
        .iter()
        .position(|e| {
            matches!(
                e,
                CoreEvent::UnitOutputCaptured { ord: 2, step_status, .. } if step_status == "ok"
            )
        })
        .expect("the recon output was captured ok");
    let gate_at = evs
        .iter()
        .position(|e| matches!(e, CoreEvent::AwaitingHuman { ord: 2, .. }))
        .unwrap();
    assert!(captured_at < gate_at);
    assert!(evs
        .iter()
        .any(|e| matches!(e, CoreEvent::UnitDenied { ord: 2, .. })));
    assert_eq!(
        gate_for(&evs, 2).denial_source.as_deref(),
        Some("input_governance")
    );
    let _ = std::fs::remove_dir_all(&repo);
    let _ = std::fs::remove_dir_all(gov_run_dir("r-recon-boundary"));
}

/// core#464 item 2 (cancel keeps today's behaviour): rejecting the denial gate cancels the run —
/// here at the verify escalation over a DIRTY tree (the creator's uncommitted fix, restored after
/// the evaluator's rewrite) — and the worktree is KEPT and named (`worktreeRetained`, core#456).
#[test]
fn rejecting_the_denial_gate_cancels_the_run_and_keeps_the_dirty_worktree() {
    let repo = make_git_repo("reject-dirty", None);
    let (core, _ran) = core_for("reject-dirty", VerifyBehaviour::RewritesTheFix);
    let entry = core
        .register_repo(RepoSpec {
            name: "reject-dirty".into(),
            root_path: repo.to_str().unwrap().into(),
            registered_at: 1,
        })
        .expect("register");
    let events = core.subscribe();
    core.launch_run(bug_run("r-reject-dirty", &entry.id))
        .expect("launch");
    assert!(wait_status(
        &core,
        "r-reject-dirty",
        SessionStatus::AwaitingHuman
    ));
    let before = drain(&events);
    assert_eq!(gate_pause(&before, 4).1, "escalation");
    assert_eq!(
        core.confirm_gate("r-reject-dirty", HumanDecision::Reject)
            .expect("reject the denial gate"),
        SessionStatus::Cancelled
    );
    let evs = drain(&events);
    assert!(evs
        .iter()
        .any(|e| matches!(e, CoreEvent::RunCancelled { session } if session == "r-reject-dirty")));
    assert!(
        evs.iter().any(|e| matches!(
            e,
            CoreEvent::WorktreeRetained { session, .. } if session == "r-reject-dirty"
        )),
        "the creator's uncommitted fix keeps the worktree alive: {evs:?}"
    );
    assert!(!session_failed(&before) && !session_failed(&evs));
    let wt = repo.join("wicked-worktrees").join("r-reject-dirty");
    assert_eq!(
        std::fs::read_to_string(wt.join("src/fix.ts")).unwrap(),
        "the fix\n",
        "the retained tree is the creator's restored fix"
    );
    let _ = std::fs::remove_dir_all(&repo);
}

/// The reject arm on a CLEAN tree (the reproduce gate: nothing but the base commit under it):
/// cancelled, the clean tree reaped by the ordinary rule, and — still — no `sessionFailed`.
#[test]
fn rejecting_the_denial_gate_on_a_clean_recon_tree_cancels_without_failing() {
    let repo = make_git_repo("reject-clean", None);
    let (core, _ran, _inputs) = core_with(
        "reject-clean",
        VerifyBehaviour::LeavesTreeAlone,
        ReproduceBehaviour::WritesANoteIntoTheTree,
    );
    let entry = core
        .register_repo(RepoSpec {
            name: "reject-clean".into(),
            root_path: repo.to_str().unwrap().into(),
            registered_at: 1,
        })
        .expect("register");
    let events = core.subscribe();
    core.launch_run(bug_run("r-reject-clean", &entry.id))
        .expect("launch");
    assert!(wait_status(
        &core,
        "r-reject-clean",
        SessionStatus::AwaitingHuman
    ));
    let before = drain(&events);
    assert_eq!(gate_pause(&before, 2).1, "escalation");
    assert_eq!(
        core.confirm_gate("r-reject-clean", HumanDecision::Reject)
            .unwrap(),
        SessionStatus::Cancelled
    );
    let evs = drain(&events);
    assert!(evs
        .iter()
        .any(|e| matches!(e, CoreEvent::RunCancelled { session } if session == "r-reject-clean")));
    assert!(!session_failed(&before) && !session_failed(&evs));
    let _ = std::fs::remove_dir_all(&repo);
}

/// core#464 item 2: a bound read-only unit carries a NOTES ROOT — outside the worktree and the
/// clone, joined into its own write boundary, absent from the creator's — and a note written
/// there never trips the guard: the rung passes, the run reaches the creator, the tree holds no
/// note.
#[test]
fn a_read_only_phase_writing_under_its_notes_root_never_trips_the_guard() {
    let repo = make_git_repo("notes-root", None);
    let (core, _ran, inputs) = core_with(
        "notes-root",
        VerifyBehaviour::LeavesTreeAlone,
        ReproduceBehaviour::WritesANoteUnderTheNotesRoot,
    );
    let entry = core
        .register_repo(RepoSpec {
            name: "notes-root".into(),
            root_path: repo.to_str().unwrap().into(),
            registered_at: 1,
        })
        .expect("register");
    let events = core.subscribe();
    core.launch_run(bug_run("r-notes-root", &entry.id))
        .expect("launch");
    let evs = wait_for_event(&events, |e| {
        matches!(e, CoreEvent::GateEvaluated { ord: 3, .. })
    })
    .expect("the run reached the creator's gate — the recon rung was not denied");
    assert!(
        !evs.iter()
            .any(|e| matches!(e, CoreEvent::EvaluatorMutatedWorktree { ord: 2, .. })),
        "a note under the notes root is not a mutation"
    );
    assert!(
        gate_for(&evs, 2).combined,
        "the reproduce rung passed its gate"
    );
    assert!(!evs
        .iter()
        .any(|e| matches!(e, CoreEvent::AwaitingHuman { ord: 2, .. })));

    // What the engine told the seat.
    let inputs = inputs.lock().unwrap().clone();
    let repro = inputs
        .iter()
        .find(|i| i.unit.phase_id() == Some("reproduce"))
        .expect("the reproduce input");
    let root = repro
        .unit
        .notes_root
        .as_deref()
        .expect("a bound read-only unit carries a notes root");
    let wt = repo.join("wicked-worktrees").join("r-notes-root");
    assert!(
        !Path::new(root).starts_with(&wt) && !Path::new(root).starts_with(&repo),
        "the notes root is outside the worktree and the clone: {root}"
    );
    assert!(
        Path::new(root).join("repro.md").is_file(),
        "the note landed where the seat was told"
    );
    assert!(
        repro
            .governance
            .as_ref()
            .is_some_and(|g| g.extra_write_roots.iter().any(|r| r == root)),
        "the unit's write boundary admits its notes root: {:?}",
        repro.governance.as_ref().map(|g| &g.extra_write_roots)
    );
    let triage = inputs
        .iter()
        .find(|i| i.unit.phase_id() == Some("triage"))
        .expect("the triage input");
    assert!(
        triage.unit.notes_root.is_some() && triage.unit.notes_root != repro.unit.notes_root,
        "every bound read-only rung gets its own root"
    );
    let fix = inputs
        .iter()
        .find(|i| i.unit.phase_id() == Some("fix"))
        .expect("the fix input");
    assert!(
        fix.unit.notes_root.is_none(),
        "a creator keeps its declared write roots — no notes root"
    );
    assert!(
        fix.governance
            .as_ref()
            .is_some_and(|g| !g.extra_write_roots.iter().any(|r| r == root)),
        "the recon rung's notes root does not leak into the creator's boundary"
    );
    assert!(
        !wt.join("repro.md").exists() && !wt.join("evidence").exists(),
        "the worktree holds no note"
    );
    let _ = std::fs::remove_dir_all(&repo);
    if let Some(run_dir) = Path::new(root).parent() {
        let _ = std::fs::remove_dir_all(run_dir);
    }
}

/// F-RC2-009 — BASELINE-DIFF: the fixture's base ALREADY fails `cargo test` and the fix changes
/// nothing about that. The floor's sandbox reports the failure on the head, runs the same check
/// on the base, finds the failure sets IDENTICAL ⇒ `floor_env_mismatch`: recorded on both the
/// creator's and the evaluator's floor, denying neither — the run COMPLETES, the shared failure
/// is listed, and the base run is paid for once (verify reads the creator's cached run).
#[test]
fn a_check_the_base_fails_identically_is_recorded_not_denied() {
    let repo = make_git_repo("shared-failure", Some(false));
    let (core, _ran) = core_for("shared-failure", VerifyBehaviour::LeavesTreeAlone);
    let entry = core
        .register_repo(RepoSpec {
            name: "shared-failure".into(),
            root_path: repo.to_str().unwrap().into(),
            registered_at: 1,
        })
        .expect("register");
    let events = core.subscribe();
    core.launch_run(bug_run("r-shared", &entry.id))
        .expect("launch");
    if !wait_status(&core, "r-shared", SessionStatus::Completed) {
        // No OS boundary: nothing ran, the verify floor fails closed and escalates.
        assert!(
            wait_status(&core, "r-shared", SessionStatus::AwaitingHuman),
            "a failure the base shares completes the run (sandboxed) or fails closed at verify"
        );
        let evs = drain(&events);
        let verify = gate_for(&evs, 4);
        assert!(
            checks_refused_without_a_boundary(&verify),
            "{:?}",
            verify.denial_reason
        );
        assert_fail_closed_without_boundary(&evs, &verify, 4);
        let _ = std::fs::remove_dir_all(&repo);
        return;
    }
    let evs = drain(&events);
    for (ord, want_floor) in [(3u32, "creator"), (4, "verify")] {
        let g = gate_for(&evs, ord);
        assert!(
            g.combined && g.deterministic_pass,
            "ord {ord}: a failure the base shares denies nothing: {:?}",
            g.denial_reason
        );
        let (floor, outcome, passed, checks) = checks_for(&evs, ord);
        assert_eq!(floor, want_floor);
        assert_eq!(outcome, "passed");
        assert!(passed);
        let c = &checks[0];
        assert_eq!(
            c.exit_code,
            Some(101),
            "ord {ord}: the head DID fail the check"
        );
        assert_eq!(c.outcome(), "failed");
        assert_eq!(
            c.classification.as_deref(),
            Some("floor_env_mismatch"),
            "ord {ord}: {c:?}"
        );
        assert_eq!(c.pre_existing, vec!["test t::boom".to_string()]);
        assert!(c.regressions.is_empty());
        let base = c.base.as_deref().expect("the base run rides the check");
        assert!(base.run.as_ref().is_some_and(|b| !b.passed()));
        if ord == 4 {
            assert!(
                base.cached,
                "verify reads the creator's base run from the run's cache"
            );
        }
    }
    let _ = std::fs::remove_dir_all(&repo);
}
