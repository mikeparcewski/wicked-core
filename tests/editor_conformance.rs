//! (DES-EDITOR-CONFORMANCE-HOST, e2e F18) The editor-plugin conformance step through the REAL engine.
//!
//! F18: the editor-plugin review could never pass, because its `test` step told the review seat to
//! run the conformance harness and no seat has a headless browser. The engine now runs it on the
//! host, as the `editor_conformance` Tool, and the review cites the engine's record.
//!
//! A plan `build → conformance → test` on a registered repo dispatches the catalog's fixed command (a stub
//! `wicked-garden` FIRST on `PATH`) in the serve jail with the declared variables; the editor
//! conformance floor decides the unit from the report the stub writes to the engine's out file:
//!
//! - a PASS report completes the run, and the `test` evaluator's context carries
//!   `[editor conformance record — unit 1]` (PASSED, on the worktree's tree) and the rule never to
//!   run the harness in the seat. Without the fix there is no such step and no record: this fails;
//! - a failed case is a verdict (`editor_conformance_failed`), sent back to the creator;
//! - a host that could not run the cases (`no_browser`), or no report at all, is a refusal
//!   (`editor_conformance_refused`) the gate retries;
//! - a host with no jail confining the harness to one port runs nothing (`unjailed_host`).
//!
//! POSIX only: the stub is a shell script (the suite's idiom).

#![cfg(unix)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::json;
use wicked_core::{
    Core, CoreEvent, EntityMode, HumanConfirm, HumanDecision, LaunchSpec, PlanSteps, RepoSpec,
    SandboxLevel, StepInput, StepOutput, StepRunner, StepStatus, TeamConfig, TEAM_OUTBOX_FILE,
};
use wicked_council::types::{Category, Confidence, Dispatcher, InputMode, Vote};
use wicked_council::{AgenticCli, CouncilTask};

fn fixture_root() -> PathBuf {
    std::env::temp_dir().join(format!("wicked-core-ec-{}", std::process::id()))
}

/// The stub reads its mode from `<fixture>/mode-<run id>`: `PASS` (every protocol-2 case passes),
/// `FAIL` (case 14 fails), `REFUSE` (no headless browser), `NONE` (writes no report).
fn mode_file(run: &str) -> PathBuf {
    fixture_root().join(format!("mode-{run}"))
}

const PASS_CASES: &str = r#"{"0-manifest":{"ok":true},"1-handshake":{"ok":true},"2-sizes":{"ok":true},"3-no-network":{"ok":true},"4-permissions":{"ok":true},"5-typing":{"ok":true},"6-forwarding":{"ok":true},"8-strict-input":{"ok":true},"9-liveness":{"ok":true},"10-keyboard":{"ok":true},"11-theme":{"ok":true},"12-hash-pinned":{"ok":true},"13-surfaces":{"ok":true},"14-card":{"ok":true},"15-options":{"ok":true},"16-full":{"ok":true}}"#;

/// Pre-main (single-threaded): the hermetic emit spool, a stub `wicked-garden` first on `PATH`, and
/// the two daemon variables crew exports (an interpreter file and a studio dist with index.html).
///
/// SAFETY (`ctor(unsafe)`): runs before `main` on one thread and only touches the filesystem and
/// process env vars via the std API.
#[ctor::ctor(unsafe)]
fn arm() {
    wicked_apps_core::emit::hermetic_test_spool();
    let root = fixture_root();
    let _ = std::fs::remove_dir_all(&root);
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).expect("fixture bin dir");
    let stub = bin.join("wicked-garden");
    let fail_cases = PASS_CASES.replace(r#""14-card":{"ok":true}"#, r#""14-card":{"ok":false}"#);
    let script = format!(
        "#!/bin/sh\n\
         echo \"ARGV $*\"\n\
         for f in acme-x/wicked-pack.json built.txt ignored.html; do if [ -e \"$WICKED_EDITOR_PACK_ROOT/$f\" ]; then echo \"HAS $f\"; fi; done\n\
         env | sed 's/^/ENV /'\n\
         mode=$(cat '{root}/mode-'\"$WICKED_RUN_ID\" 2>/dev/null || echo PASS)\n\
         case \"$mode\" in\n\
           PASS) printf '%s' '{{\"ok\":true,\"editors\":[{{\"id\":\"acme-x\",\"version\":\"0.1.0\",\"sha256\":\"ab12\",\"protocol\":[1,2],\"cases\":{pass}}}]}}' > \"$WG_EDITOR_CONFORMANCE_OUT\";;\n\
           FAIL) printf '%s' '{{\"ok\":false,\"cause\":\"case_failed\",\"editors\":[{{\"id\":\"acme-x\",\"protocol\":[1,2],\"cases\":{fail}}}]}}' > \"$WG_EDITOR_CONFORMANCE_OUT\"; exit 1;;\n\
           REFUSE) printf '%s' '{{\"ok\":false,\"cause\":\"no_browser\",\"error\":\"no headless browser\"}}' > \"$WG_EDITOR_CONFORMANCE_OUT\"; exit 3;;\n\
         esac\n\
         exit 0\n",
        root = root.display(),
        pass = PASS_CASES,
        fail = fail_cases,
    );
    std::fs::write(&stub, script).expect("write the stub");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let py = root.join("venv-python");
    std::fs::write(&py, "#!/bin/sh\nexit 0\n").expect("fixture interpreter");
    let dist = root.join("studio-dist");
    std::fs::create_dir_all(&dist).expect("fixture dist");
    std::fs::write(dist.join("index.html"), "<!doctype html>").expect("fixture index");
    // Hermetic skills ladder: no snapshot and an empty claude config dir, so the ladder finds no garden
    // (`Absent`) and the fixed command resolves the stub from PATH.
    let config = root.join("claude-config");
    std::fs::create_dir_all(&config).expect("fixture claude config dir");
    std::env::remove_var("WICKED_SKILLS_SNAPSHOT");
    std::env::set_var("CLAUDE_CONFIG_DIR", &config);
    std::env::set_var(wicked_core::editor_conformance::PYTHON_ENV, &py);
    std::env::set_var(wicked_core::editor_conformance::STUDIO_DIST_ENV, &dist);
    let mut paths = vec![bin];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    std::env::set_var("PATH", std::env::join_paths(paths).expect("PATH joins"));
}

struct NoBallots;
impl Dispatcher for NoBallots {
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

/// Each run's `test` unit's context: `(label, text)` pairs, by run id.
type Seen = Mutex<HashMap<String, Vec<(String, String)>>>;

/// What each run's `test` unit was handed as context.
fn seen() -> &'static Seen {
    static SEEN: OnceLock<Seen> = OnceLock::new();
    SEEN.get_or_init(Default::default)
}

/// The `test` evaluator's seat: records its context and passes. No other unit runs a worker turn.
/// The seats: `build` (the creator) writes a file and an ignored one into the worktree; `test` records
/// its context and passes.
struct Reviewer;
impl StepRunner for Reviewer {
    fn run_unit(&self, i: &StepInput) -> StepOutput {
        // The agent judge (the creator's gate) answers in its own contract.
        if i.unit.session_id == "validator" {
            return StepOutput {
                run_id: i.run_id.clone(),
                unit_ix: i.unit_ix,
                attempt: i.attempt,
                output: "PASS\nthe work is there\nPASS".into(),
                status: StepStatus::Ok,
                usage: None,
                files: Vec::new(),
                tools: Vec::new(),
                governed: false,
            };
        }
        if i.unit.id.ends_with(":build") {
            let wd = i.workdir.as_ref().expect("a repo-bound run has a worktree");
            std::fs::write(wd.join("built.txt"), format!("attempt {}", i.attempt)).unwrap();
            std::fs::write(wd.join("ignored.html"), "never exported").unwrap();
        }
        if i.unit.id.ends_with(":test") {
            seen().lock().unwrap().insert(
                i.run_id.clone(),
                i.prior_outputs
                    .iter()
                    .map(|p| (p.label.clone(), p.output.clone()))
                    .collect(),
            );
        }
        StepOutput {
            run_id: i.run_id.clone(),
            unit_ix: i.unit_ix,
            attempt: i.attempt,
            output: if i.unit.id.ends_with(":build") {
                "Built the editor pack.".into()
            } else {
                "ITEM 1: PASS — the engine's conformance record\nVERDICT: PASS".into()
            },
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

fn dir(name: &str) -> PathBuf {
    let d = fixture_root().join(name);
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A repository holding one editor pack (the stub does not read it; the worktree is real).
fn git_repo(name: &str) -> PathBuf {
    let repo = dir(&format!("repo-{name}"));
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "t@t"]);
    git(&["config", "user.name", "t"]);
    git(&["config", "commit.gpgsign", "false"]);
    std::fs::write(repo.join(".gitignore"), "ignored.html\n").unwrap();
    std::fs::create_dir_all(repo.join("acme-x/editors/acme-x")).unwrap();
    std::fs::write(
        repo.join("acme-x/wicked-pack.json"),
        r#"{"spec":2,"name":"acme-x","editors":[{"id":"acme-x","entry":"editors/acme-x/index.html","protocol":[1,2]}]}"#,
    )
    .unwrap();
    std::fs::write(
        repo.join("acme-x/editors/acme-x/index.html"),
        "<!doctype html>",
    )
    .unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "init"]);
    repo
}

struct Run {
    sid: String,
    core: Core,
    rx: std::sync::mpsc::Receiver<CoreEvent>,
    events: Vec<CoreEvent>,
}

/// Launch `conformance → test` on a registered repo and collect events until the run completes,
/// fails or pauses.
fn run(name: &str, mode: &str) -> Run {
    let base = dir(name);
    let repo = git_repo(name);
    let core = Core::spawn_with_engine_team(
        base.join("estate.db").to_string_lossy().into_owned(),
        Arc::new(NoBallots),
        Arc::new(Reviewer),
        TeamConfig::new(None, Some(base.join(TEAM_OUTBOX_FILE)))
            .with_final_pass_budget(Duration::from_millis(300)),
    );
    let rx = core.subscribe();
    let entry = core
        .register_repo(RepoSpec {
            name: format!("ec {name}"),
            root_path: repo.to_string_lossy().into_owned(),
            registered_at: 0,
        })
        .expect("register the repo");
    let sid = format!("ec-{name}");
    std::fs::write(mode_file(&sid), mode).unwrap();
    // A declared touch set keeps the floor from adding steps around the two under test.
    let plan: PlanSteps = serde_json::from_value(json!({
        "steps": [
            {"catalog": "produce", "id": "build"},
            {"catalog": "editor_conformance", "id": "conformance", "depends_on": ["build"]},
            {"catalog": "critique", "id": "test", "depends_on": ["conformance"]}
        ],
        "touch": ["acme-x/editors/acme-x/index.html"]
    }))
    .unwrap();
    core.launch_run(LaunchSpec {
        problem: "build an editor plugin".into(),
        clis: vec![cli("a"), cli("b")],
        entity_mode: EntityMode::Shared,
        session_id: sid.clone(),
        human_confirm: HumanConfirm::None,
        auto_deliver: false,
        repo_ref: Some(entry.id),
        base_ref: None,
        workflow: None,
        project_id: None,
        extra_write_roots: Vec::new(),
        extra_read_roots: Vec::new(),
        project_graph: None,
        plan: Some(plan),
        deliver_step: None,
        exclude_seats: Vec::new(),
        evidence_root: None,
        primary: None,
        reduced_assurance: false,
        power: false,
        voice: None,
        deliverables: Vec::new(),
        qe_acceptance: Default::default(),
    })
    .expect("the conformance plan launches");
    let mut r = Run {
        sid,
        core,
        rx,
        events: Vec::new(),
    };
    r.until_stopped();
    // A repo-bound plan with a creator scores high on an unindexed repo: approve its plan gate.
    for _ in 0..3 {
        let plan_gate = r.events.iter().rev().find_map(|e| match e {
            CoreEvent::AwaitingHuman { gate_kind, .. } => Some(gate_kind == "plan_approval"),
            CoreEvent::SessionCompleted { .. } | CoreEvent::GateEscalated { .. } => Some(false),
            _ => None,
        });
        if plan_gate != Some(true) {
            break;
        }
        r.answer(HumanDecision::Approve {
            amend: None,
            amend_scope: Default::default(),
        });
    }
    r
}

impl Run {
    /// Collect events until the run completes, fails or pauses.
    fn until_stopped(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(120);
        while Instant::now() < deadline {
            match self.rx.recv_timeout(Duration::from_millis(500)) {
                Ok(ev) => {
                    let done = match &ev {
                        CoreEvent::SessionCompleted { session } => *session == self.sid,
                        CoreEvent::SessionFailed { session, .. } => *session == self.sid,
                        CoreEvent::AwaitingHuman { session, .. } => *session == self.sid,
                        _ => false,
                    };
                    self.events.push(ev);
                    if done {
                        return;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(_) => return,
            }
        }
    }

    /// Answer the gate, then collect until the run stops again.
    fn answer(&mut self, d: HumanDecision) {
        self.core
            .confirm_gate(&self.sid, d)
            .expect("the gate takes the answer");
        self.until_stopped();
    }

    /// How many times the plan's step `step` was dispatched (the floor may add steps before it).
    fn dispatches_of(&self, step: &str) -> usize {
        let ord = self
            .events
            .iter()
            .find_map(|e| match e {
                CoreEvent::UnitPlanned {
                    ord, description, ..
                } if description.starts_with(&format!("{step} — ")) => Some(*ord),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no step {step} planned"));
        self.dispatches(ord)
    }

    /// How many times unit `ord` was dispatched.
    fn dispatches(&self, ord: u32) -> usize {
        self.events
            .iter()
            .filter(|e| {
                matches!(e, CoreEvent::UnitDispatched { session, ord: o, .. } if *session == self.sid && *o == ord)
            })
            .count()
    }
}

/// The launcher that confines the harness to one port on this platform.
fn jail_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "sandbox-exec"
    } else {
        "bwrap"
    }
}

/// Whether this host refused the step for want of a jail (Windows, or Linux without a usable bwrap):
/// the one condition under which these tests stop early. The refusal itself is asserted.
fn refused_unjailed(r: &Run) -> bool {
    match escalation(&r.events) {
        Some((summary, source)) if summary.contains("unjailed_host") => {
            assert_eq!(source, "editor_conformance_refused", "{summary}");
            assert!(
                !(cfg!(target_os = "macos")
                    && wicked_core::sandbox_availability().0 == SandboxLevel::Sandboxed),
                "a macOS host with sandbox-exec must run the step: {summary}"
            );
            eprintln!("SKIP: no serve jail on this host; the step refused: {summary}");
            true
        }
        _ => false,
    }
}

fn types(events: &[CoreEvent]) -> Vec<String> {
    events
        .iter()
        .map(|e| e.to_json()["type"].as_str().unwrap_or("?").to_string())
        .collect()
}

fn escalation(events: &[CoreEvent]) -> Option<(String, String)> {
    events.iter().find_map(|e| match e {
        CoreEvent::GateEscalated {
            verdict_summary,
            denial_source,
            ..
        } => Some((verdict_summary.clone(), denial_source.clone())),
        _ => None,
    })
}

fn completed(events: &[CoreEvent]) -> bool {
    events
        .iter()
        .any(|e| matches!(e, CoreEvent::SessionCompleted { .. }))
}

/// THE F18 test: the engine runs the cases on the host, and the review is handed its record.
#[test]
fn a_passing_host_run_completes_and_the_review_cites_the_engines_record() {
    let r = run("pass", "PASS");
    if refused_unjailed(&r) {
        return;
    }
    assert!(completed(&r.events), "{:?}", types(&r.events));
    assert!(escalation(&r.events).is_none());
    // The fixed command, with exactly the declared variables (nothing of the daemon's own WICKED_*).
    let out = r
        .core
        .work_output(&format!("{}:conformance", r.sid))
        .expect("the Tool's output");
    assert!(
        out.contains("ARGV run scripts/editor/conformance.py --engine --json"),
        "{out}"
    );
    let get = |k: &str| {
        out.lines()
            .find_map(|l| l.strip_prefix(&format!("ENV {k}=")).map(str::to_string))
    };
    assert_eq!(
        get("WICKED_EDITOR_CONFORMANCE_JAIL").as_deref(),
        Some(jail_name())
    );
    // The cases ran on an export of the worktree's TREE: the pack and the build's file are there, the
    // ignored file is not, and it is not the live worktree.
    let root = get("WICKED_EDITOR_PACK_ROOT").unwrap();
    assert!(!root.contains("wicked-worktrees"), "{root}");
    assert!(
        out.contains("HAS acme-x/wicked-pack.json") && out.contains("HAS built.txt"),
        "{out}"
    );
    assert!(!out.contains("HAS ignored.html"), "{out}");
    let port: u16 = get("WG_EDITOR_CONFORMANCE_PORT").unwrap().parse().unwrap();
    assert!(port >= 1024);
    assert_eq!(
        get("WG_EDITOR_CONFORMANCE_PYTHON"),
        std::env::var(wicked_core::editor_conformance::PYTHON_ENV).ok()
    );
    let allowed = [
        "WICKED_RUN_ID",
        "WICKED_RUN_UNIT",
        "WICKED_EDITOR_CONFORMANCE_JAIL",
        "WICKED_EDITOR_PACK_ROOT",
        "WICKED_GARDEN_ROOT",
        "WICKED_GARDEN_CACHE_DIR",
    ];
    for l in out.lines().filter_map(|l| l.strip_prefix("ENV ")) {
        let k = l.split_once('=').map_or(l, |(k, _)| k);
        assert!(
            !k.starts_with("WICKED_") || allowed.contains(&k),
            "undeclared {k} reached the harness"
        );
        assert!(!k.starts_with("CLAUDE_"), "{k} reached the harness");
    }
    // The review is handed the engine's record, and told never to run the harness itself.
    let ctx = seen()
        .lock()
        .unwrap()
        .get(&r.sid)
        .cloned()
        .expect("the test unit ran");
    let (label, text) = ctx
        .iter()
        .find(|(l, _)| l.starts_with("[editor conformance record"))
        .unwrap_or_else(|| panic!("no conformance record in the review's context: {ctx:?}"));
    assert!(
        label.starts_with("[editor conformance record — unit "),
        "{label}"
    );
    assert!(text.contains("PASSED"), "{text}");
    assert!(
        text.contains("acme-x 0.1.0 (entry sha256 ab12; cases 0-6, 8-16)"),
        "{text}"
    );
    assert!(
        text.contains(", tree "),
        "the record names the tree it ran on: {text}"
    );
    let checklist = ctx
        .iter()
        .find(|(l, _)| l.starts_with("[review checklist"))
        .map(|(_, t)| t.clone())
        .unwrap_or_default();
    assert!(
        checklist.contains("Never run conformance.py in your seat"),
        "{checklist}"
    );
}

#[test]
fn a_failed_case_is_a_verdict_and_a_host_that_cannot_run_is_a_retry() {
    for (mode, source, says) in [
        (
            "FAIL",
            "editor_conformance_failed",
            "acme-x: 14-card failed",
        ),
        ("REFUSE", "editor_conformance_refused", "no_browser"),
        ("NONE", "editor_conformance_refused", "no_report"),
    ] {
        let mut r = run(&format!("m-{}", mode.to_lowercase()), mode);
        if refused_unjailed(&r) {
            return;
        }
        let (summary, got) = escalation(&r.events)
            .unwrap_or_else(|| panic!("{mode}: must escalate: {:?}", types(&r.events)));
        assert_eq!(got, source, "{mode}: {summary}");
        assert!(summary.contains(says), "{mode}: {summary}");
        assert!(!completed(&r.events), "{mode} never completes the run");
        assert!(
            seen().lock().unwrap().get(&r.sid).is_none(),
            "{mode}: the review never starts on a tree that did not pass"
        );
        assert_eq!(
            (r.dispatches_of("build"), r.dispatches_of("conformance")),
            (1, 1),
            "{mode}"
        );
        // The host (or the pack) is fixed; the gate's answer decides what runs again.
        std::fs::write(mode_file(&r.sid), "PASS").unwrap();
        if mode == "FAIL" {
            // A verdict on the pack: the send-back re-runs the creator, then the cases on the new tree.
            r.answer(HumanDecision::RequestChanges {
                note: Some("fix case 14".into()),
            });
            assert_eq!(
                (r.dispatches_of("build"), r.dispatches_of("conformance")),
                (2, 2),
                "{:?}",
                types(&r.events)
            );
        } else {
            // The host could not run them: retry re-runs the Tool only, never the creator.
            r.answer(HumanDecision::Approve {
                amend: None,
                amend_scope: Default::default(),
            });
            assert_eq!(
                (r.dispatches_of("build"), r.dispatches_of("conformance")),
                (1, 2),
                "{:?}",
                types(&r.events)
            );
        }
        assert!(completed(&r.events), "{mode}: {:?}", types(&r.events));
        let ctx = seen().lock().unwrap().get(&r.sid).cloned().unwrap();
        let (_, text) = ctx
            .iter()
            .find(|(l, _)| l.starts_with("[editor conformance record"))
            .unwrap();
        assert!(
            text.contains("PASSED") && !text.contains("UNVERIFIED"),
            "{mode}: {text}"
        );
    }
}
