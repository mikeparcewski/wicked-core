//! WT-C2 (DES-walkthrough-proof §4.3–§4.4, A11) — the `walkthrough_review` Tool through the REAL
//! engine: a user-composed plan on a registered repo, launched with an evidence root, dispatches
//! the catalog's fixed record command (a stub `wicked-garden` on `PATH`) inside the loopback jail
//! with the declared variables; its pinned result validator then decides the unit at its fold.
//!
//! - A PASS result completes the run.
//! - A FAIL result is a fold denial that opens the engine's escalation gate (A11: a pinned Tool
//!   unit's validator is evaluated at its fold), never the failover ladder: the tool exited 0.
//! - A host with no OS jail records nothing and escalates with `unjailed_host`.
//!
//! POSIX only: the stub is a shell script (the suite's idiom, see `tool_only_plan_needs_no_seat`).

#![cfg(unix)]

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;
use wicked_core::{
    Core, CoreEvent, EntityMode, HumanConfirm, LaunchSpec, PlanSteps, RepoSpec, SandboxLevel,
    StepInput, StepOutput, StepRunner, TeamConfig, TEAM_OUTBOX_FILE,
};
use wicked_council::types::{Confidence, Dispatcher, Vote};
use wicked_council::{AgenticCli, CouncilTask};

fn fixture_root() -> PathBuf {
    std::env::temp_dir().join(format!("wicked-core-wtc2-{}", std::process::id()))
}

/// The stub reads its verdict from `<fixture>/mode-<run id>` (PASS when absent).
fn mode_file(run: &str) -> PathBuf {
    fixture_root().join(format!("mode-{run}"))
}

/// Pre-main (single-threaded): the hermetic emit spool, and a stub `wicked-garden` FIRST on `PATH`
/// that records the environment it was handed into its proof root and writes the verdict file.
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
    let script = format!(
        "#!/bin/sh\n\
         env > \"$WICKED_EVIDENCE_ROOT/env.txt\"\n\
         printf '%s\\n' \"$*\" > \"$WICKED_EVIDENCE_ROOT/argv.txt\"\n\
         mode=$(cat '{root}/mode-'\"$WICKED_RUN_ID\" 2>/dev/null || echo PASS)\n\
         printf '{{\"overall\":\"%s\",\"chapters\":[{{\"key\":\"c1\",\"verdict\":\"%s\"}}]}}' \"$mode\" \"$mode\" > \"$WICKED_EVIDENCE_ROOT/result.json\"\n\
         echo 'WALKTHROUGH-SEAL {{}}'\n\
         exit 0\n",
        root = root.display()
    );
    std::fs::write(&stub, script).expect("write the stub");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    // Hermetic skills ladder: no published snapshot and an EMPTY claude config dir, so the ladder
    // finds no garden (`Absent`) rather than whatever plugin cache the host carries — a ladder
    // that FAILS refuses the record (Copilot on #697), which is not what these tests exercise.
    let config = root.join("claude-config");
    std::fs::create_dir_all(&config).expect("fixture claude config dir");
    std::env::remove_var("WICKED_SKILLS_SNAPSHOT");
    std::env::set_var("CLAUDE_CONFIG_DIR", &config);
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

/// A tool-only plan runs no worker turn.
struct NoTurns;
impl StepRunner for NoTurns {
    fn run_unit(&self, i: &StepInput) -> StepOutput {
        panic!("a tool-only plan ran a worker turn: {}", i.unit.id);
    }
}

fn dir(name: &str) -> PathBuf {
    let d = fixture_root().join(name);
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

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
    std::fs::write(repo.join("README.md"), "hello").unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "init"]);
    repo
}

struct Run {
    events: Vec<CoreEvent>,
    proof_root: PathBuf,
}

/// Launch a one-step plan (`walkthrough_review`, the catalog's fixed command) on a registered repo
/// with an evidence root, and collect events until the run completes, fails or pauses.
fn run(name: &str, mode: Option<&str>) -> Run {
    let base = dir(name);
    let repo = git_repo(name);
    let evidence = base.join("evidence");
    std::fs::create_dir_all(&evidence).unwrap();
    let core = Core::spawn_with_engine_team(
        base.join("estate.db").to_string_lossy().into_owned(),
        Arc::new(NoBallots),
        Arc::new(NoTurns),
        TeamConfig::new(None, Some(base.join(TEAM_OUTBOX_FILE)))
            .with_final_pass_budget(Duration::from_millis(300)),
    );
    let rx = core.subscribe();
    let entry = core
        .register_repo(RepoSpec {
            name: format!("wtc2 {name}"),
            root_path: repo.to_string_lossy().into_owned(),
            registered_at: 0,
        })
        .expect("register the repo");
    let sid = format!("wtc2-{name}");
    if let Some(m) = mode {
        std::fs::write(mode_file(&sid), m).unwrap();
    }
    let plan: PlanSteps = serde_json::from_value(json!({
        "steps": [{"catalog": "walkthrough_review", "id": "wr"}]
    }))
    .unwrap();
    core.launch_run(LaunchSpec {
        problem: "show the checkout working".into(),
        clis: Vec::new(),
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
        evidence_root: Some(evidence.to_string_lossy().into_owned()),
        primary: None,
    })
    .expect("the walkthrough plan launches");
    let mut events = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(ev) => {
                let done = match &ev {
                    CoreEvent::SessionCompleted { session } => *session == sid,
                    CoreEvent::SessionFailed { session, .. } => *session == sid,
                    CoreEvent::AwaitingHuman { session, .. } => *session == sid,
                    _ => false,
                };
                events.push(ev);
                if done {
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(_) => break,
        }
    }
    Run {
        events,
        proof_root: evidence.join("wr"),
    }
}

fn jailed() -> bool {
    wicked_core::sandbox_availability().0 == SandboxLevel::Sandboxed
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

/// A PASS take completes the run; the recorder ran the catalog's fixed command with the declared
/// variables (and nothing of the daemon's own), the tree under review named by `WICKED_TREE`.
#[test]
fn a_passing_walkthrough_completes_and_the_recorder_got_exactly_the_declared_env() {
    if !jailed() {
        eprintln!("SKIP: no OS jail on this host (the unjailed test covers it)");
        return;
    }
    let r = run("pass", Some("PASS"));
    assert!(
        r.events
            .iter()
            .any(|e| matches!(e, CoreEvent::SessionCompleted { .. })),
        "the run completes on a PASS: {:?}",
        types(&r.events)
    );
    assert!(escalation(&r.events).is_none());
    let argv = std::fs::read_to_string(r.proof_root.join("argv.txt")).unwrap();
    assert_eq!(argv.trim(), "run scripts/demo/walkthrough.mjs record");
    let env = std::fs::read_to_string(r.proof_root.join("env.txt")).unwrap();
    let get = |k: &str| {
        env.lines()
            .find_map(|l| l.strip_prefix(&format!("{k}=")).map(str::to_string))
    };
    assert_eq!(get("WICKED_RUN_ID").as_deref(), Some("wtc2-pass"));
    assert_eq!(get("WICKED_RUN_UNIT").as_deref(), Some("1"));
    assert_eq!(
        get("WICKED_EVIDENCE_ROOT").map(PathBuf::from),
        Some(r.proof_root.clone())
    );
    let tree = get("WICKED_TREE").expect("the tree id");
    assert!(
        tree.len() >= 40 && tree.chars().all(|c| c.is_ascii_hexdigit()),
        "{tree}"
    );
    // No plan step precedes the review, so no author dir is named.
    assert_eq!(get("WICKED_WALKTHROUGH_AUTHOR"), None);
    // The daemon's environment does not leak in: the test process's own engine variables (the
    // hermetic emit spool) are set in THIS process and must be absent in the recorder.
    for l in env.lines() {
        let k = l.split_once('=').map_or(l, |(k, _)| k);
        assert!(
            !k.starts_with("WICKED_")
                || [
                    "WICKED_RUN_ID",
                    "WICKED_RUN_UNIT",
                    "WICKED_EVIDENCE_ROOT",
                    "WICKED_TREE",
                    "WICKED_WALKTHROUGH_AUTHOR",
                    "WICKED_GARDEN_ROOT",
                ]
                .contains(&k),
            "undeclared {k} reached the recorder"
        );
    }
}

/// A11: the tool exits 0 with a FAIL verdict; the pinned result validator denies at the fold and
/// the unit escalates (deterministic denial path), it is not retried as a failed tool.
#[test]
fn a_failing_walkthrough_is_a_fold_denial_that_escalates() {
    if !jailed() {
        eprintln!("SKIP: no OS jail on this host (the unjailed test covers it)");
        return;
    }
    let r = run("fail", Some("FAIL"));
    let (summary, _source) = escalation(&r.events).unwrap_or_else(|| {
        panic!(
            "a FAIL take must escalate at the fold: {:?}",
            types(&r.events)
        )
    });
    assert!(
        summary.contains("walkthrough"),
        "the denial names the walkthrough result criterion: {summary}"
    );
    assert!(
        !r.events
            .iter()
            .any(|e| matches!(e, CoreEvent::SessionCompleted { .. })),
        "a FAIL never completes the run"
    );
}

/// No jail ⇒ no walkthrough (O4): the stub never runs, the proof root holds the engine's
/// `unjailed_host` result, and the unit escalates on it. Exercised for real only where the host
/// has no jail; elsewhere the record runner's unit test covers the same branch.
#[test]
fn an_unjailed_host_escalates_with_unjailed_host() {
    if jailed() {
        eprintln!(
            "SKIP: this host has an OS jail (actor::walkthrough_record_tests covers the branch)"
        );
        return;
    }
    let r = run("unjailed", Some("PASS"));
    assert!(escalation(&r.events).is_some(), "{:?}", types(&r.events));
    let result = std::fs::read_to_string(r.proof_root.join("result.json")).unwrap();
    assert!(result.contains("unjailed_host"), "{result}");
    assert!(
        !r.proof_root.join("env.txt").exists(),
        "the recorder never ran"
    );
}
