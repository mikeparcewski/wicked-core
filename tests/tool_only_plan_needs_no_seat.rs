//! F-E2E-011 — a plan whose EVERY unit is a Tool executor needs no CLI seat.
//!
//! On crew 0.7.31 + core-ts 0.7.22 every `onboarding` run failed about one second after launch:
//! crew hands the tool-only workflow `clis: []` by design (wicked-crew#533 — its two
//! `wicked-estate` phases convene no council), and the engine's routing core refused the plan for
//! an empty eligible seat set BEFORE it looked at executor types — `sessionStarted {cliCount: 0}`
//! → "no eligible seat … every configured seat is benched — (sign a seat in …)" → `sessionFailed`.
//! No registered repo ever got a graph.
//!
//! These tests go through the REAL engine (`Core::launch_run` → plan → distribute → dispatch),
//! not the routing function alone: the seeded `onboarding` def, a registered repo, an empty seat
//! pool, and a stub `wicked-estate` on `PATH` that records what it was handed.
//!
//! POSIX only (`#![cfg(unix)]` — the suite's idiom for script-driven tests: `p4a_wrapped`,
//! `terminal`, `domain_extraction_e2e`): the stub is a shell script, which Windows can neither
//! resolve on `PATH` without a `PATHEXT` extension nor exec by bare name (`CreateProcess` appends
//! only `.exe`). The routing-level proofs in `src/distribute.rs` run on every platform.

#![cfg(unix)]

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use wicked_core::{
    Core, CoreEvent, EntityMode, HumanConfirm, LaunchSpec, RepoSpec, StepInput, StepOutput,
    StepRunner, StepStatus,
};
use wicked_council::types::{Confidence, Dispatcher, Vote};
use wicked_council::{AgenticCli, CouncilTask};

/// Per-process fixture root: the stub `wicked-estate`, its argv log, and the repo-graph root the
/// engine binds into the onboarding phases (`WICKED_ESTATE_REPO_GRAPH_ROOT`, so nothing resolves
/// under the operator's real state home).
fn fixture_root() -> PathBuf {
    std::env::temp_dir().join(format!("wicked-core-fe2e011-{}", std::process::id()))
}

/// One line per stub invocation: the argv the engine actually handed the tool.
fn argv_log() -> PathBuf {
    fixture_root().join("wicked-estate-argv.log")
}

/// Pre-main (single-threaded, so no test thread can race the env writes): arm the hermetic emit
/// spool (core#311), put a stub `wicked-estate` FIRST on `PATH` so the seeded onboarding def passes
/// the core#120 tool preflight and its two phases run to completion against this fixture (the stub
/// logs its argv and exits 0), and pin the repo-graph root under the fixture.
///
/// SAFETY (`ctor(unsafe)`): runs before `main` on one thread and only touches the filesystem and
/// process env vars via the std API — no allocator setup, no threads, no panics across the FFI
/// boundary.
#[ctor::ctor(unsafe)]
fn arm_hermetic_emit_spool() {
    wicked_apps_core::emit::hermetic_test_spool();
    let root = fixture_root();
    let _ = std::fs::remove_dir_all(&root);
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).expect("fixture bin dir");
    std::fs::create_dir_all(root.join("repo-graphs")).expect("fixture repo-graph root");
    let stub = bin.join("wicked-estate");
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexit 0\n",
        argv_log().display()
    );
    std::fs::write(&stub, script).expect("write the wicked-estate stub");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755))
            .expect("chmod the stub");
    }
    let mut paths = vec![bin];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    std::env::set_var("PATH", std::env::join_paths(paths).expect("PATH joins"));
    std::env::set_var("WICKED_ESTATE_REPO_GRAPH_ROOT", root.join("repo-graphs"));
}

/// Counts every ballot — a tool-only plan must dispatch none.
struct CountingDispatcher(AtomicUsize);
impl Dispatcher for CountingDispatcher {
    fn dispatch(&self, c: &AgenticCli, _: &CouncilTask) -> Option<Vote> {
        self.0.fetch_add(1, Ordering::SeqCst);
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

/// Counts every worker turn — a tool unit bypasses the runner entirely.
struct CountingRunner(AtomicUsize);
impl StepRunner for CountingRunner {
    fn run_unit(&self, i: &StepInput) -> StepOutput {
        self.0.fetch_add(1, Ordering::SeqCst);
        StepOutput {
            run_id: i.run_id.clone(),
            unit_ix: i.unit_ix,
            attempt: i.attempt,
            output: "ok".into(),
            status: StepStatus::Ok,
            usage: None,
            files: vec![],
            tools: Vec::new(),
            governed: false,
        }
    }
}

fn db_path(name: &str) -> String {
    let dir = std::env::temp_dir().join(format!(
        "wicked-core-fe2e011-db-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("estate.db").to_str().unwrap().to_string()
}

/// A throwaway git repo with one commit — what `register_repo` validates and the run's worktree
/// is based on.
fn make_git_repo(name: &str) -> PathBuf {
    let repo = std::env::temp_dir().join(format!(
        "wicked-core-fe2e011-repo-{name}-{}",
        std::process::id()
    ));
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

fn spec(session_id: &str, workflow: &str, repo_ref: Option<String>) -> LaunchSpec {
    LaunchSpec {
        project_id: None,
        problem: format!("Onboard repository: {session_id}"),
        // Exactly what crew hands a tool-only workflow (wicked-crew#533): no seat at all.
        clis: Vec::new(),
        entity_mode: EntityMode::Shared,
        session_id: session_id.into(),
        human_confirm: HumanConfirm::None,
        repo_ref,
        workflow: Some(workflow.into()),
        extra_write_roots: Vec::new(),
        extra_read_roots: Vec::new(),
        project_graph: None,
    }
}

/// Drain events until a terminal event for `session` is observed or the deadline expires.
/// Generous: `cargo test --workspace` runs dozens of binaries at once and this run adds a real
/// `git worktree add`; a satisfied wait returns the moment the terminal event lands.
fn drain_until_terminal(
    events: &std::sync::mpsc::Receiver<CoreEvent>,
    session: &str,
) -> Vec<CoreEvent> {
    let mut collected = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or(Duration::ZERO);
        if remaining.is_zero() {
            break;
        }
        match events.recv_timeout(remaining.min(Duration::from_millis(500))) {
            Ok(ev) => {
                let terminal = matches!(&ev,
                    CoreEvent::SessionCompleted { session: s } if s == session)
                    || matches!(&ev,
                    CoreEvent::SessionFailed { session: s, .. } if s == session)
                    || matches!(&ev,
                    CoreEvent::RunCancelled { session: s } if s == session)
                    || matches!(&ev,
                    CoreEvent::AwaitingHuman { session: s, .. } if s == session);
                collected.push(ev);
                if terminal {
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
        }
    }
    collected
}

/// The `error` events the run put on the wire.
fn errors(events: &[CoreEvent], sid: &str) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            CoreEvent::Error { session, message }
                if session.as_deref().is_none_or(|s| s == sid) =>
            {
                Some(message.clone())
            }
            _ => None,
        })
        .collect()
}

/// (a) The SEEDED `onboarding` def, launched with `clis: []` against a registered repo, is not
/// refused: both units are distributed `tool` to `wicked-estate`, the first tool unit is reached
/// and spawned with the repo bound in, both phases actually run, and the run completes — with no
/// council convened and no worker turn.
#[test]
fn the_seeded_onboarding_workflow_launched_with_no_seats_runs_both_tool_units() {
    let repo = make_git_repo("onboarding");
    let repo_str = repo.to_string_lossy().into_owned();
    let dispatcher = Arc::new(CountingDispatcher(AtomicUsize::new(0)));
    let runner = Arc::new(CountingRunner(AtomicUsize::new(0)));
    let core = Core::spawn_with_engine(db_path("onboarding"), dispatcher.clone(), runner.clone());
    let ev = core.subscribe();
    let entry = core
        .register_repo(RepoSpec {
            name: "fe2e011 onboarding".into(),
            root_path: repo_str.clone(),
            registered_at: 0,
        })
        .expect("register the repo");
    let sid = "fe2e011-onboarding";
    core.launch_run(spec(sid, "onboarding", Some(entry.id.clone())))
        .expect("a tool-only plan launches with no seats");
    let events = drain_until_terminal(&ev, sid);

    // 1. Nothing was refused for the empty seat set.
    let errs = errors(&events, sid);
    assert!(
        !errs.iter().any(|m| m.contains("no eligible seat")),
        "the empty roster was refused: {errs:?}"
    );

    // 2. Both units were distributed `tool`, to the tool's own program.
    let mut distributed: Vec<(u32, String, String)> = events
        .iter()
        .filter_map(|e| match e {
            CoreEvent::UnitDistributed {
                session,
                ord,
                cli,
                routing_method,
                ..
            } if session == sid => Some((*ord, cli.clone(), routing_method.clone())),
            _ => None,
        })
        .collect();
    distributed.sort();
    assert_eq!(
        distributed,
        vec![
            (1, "wicked-estate".to_string(), "tool".to_string()),
            (2, "wicked-estate".to_string(), "tool".to_string()),
        ],
        "events: {events:?}"
    );

    // 3. The run reached its first tool unit: the index command spawned with the repo bound in.
    let dispatched: Vec<(u32, Vec<String>)> = events
        .iter()
        .filter_map(|e| match e {
            CoreEvent::ToolExecutorDispatched {
                session, ord, cmd, ..
            } if session == sid => Some((*ord, cmd.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        dispatched.len(),
        2,
        "both tool phases spawned: {dispatched:?}"
    );
    let (ord, cmd) = &dispatched[0];
    assert_eq!(*ord, 1);
    assert_eq!(
        &cmd[..2],
        &["wicked-estate".to_string(), "index".to_string()]
    );
    assert!(
        cmd.contains(&repo_str),
        "the repo root is bound into the index phase: {cmd:?}"
    );
    assert!(
        !cmd.iter().any(|a| a.starts_with('{') && a.ends_with('}')),
        "no placeholder reaches the tool: {cmd:?}"
    );

    // 4. …and both phases actually RAN: the stub logged the argv it was handed, in phase order.
    let log = std::fs::read_to_string(argv_log()).expect("the stub wicked-estate ran");
    let lines: Vec<&str> = log.lines().collect();
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("index ") && l.contains(&repo_str)),
        "index ran against the repo: {lines:?}"
    );
    assert!(
        lines.iter().any(|l| l.starts_with("clusters --annotate ")),
        "annotate ran: {lines:?}"
    );

    // 5. The run completed; no council convened, no worker turn — there was no seat to give one.
    assert!(
        events
            .iter()
            .any(|e| matches!(e, CoreEvent::SessionCompleted { session } if session == sid)),
        "the tool-only run completes; errors: {errs:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, CoreEvent::CouncilConvened { session, .. } if session == sid)),
        "no council convenes for a tool-only plan"
    );
    assert_eq!(
        dispatcher.0.load(Ordering::SeqCst),
        0,
        "no ballot dispatched"
    );
    assert_eq!(runner.0.load(Ordering::SeqCst), 0, "no worker turn");
}

/// (b) The refusal is unchanged the moment a planned unit NEEDS a seat: a tool phase beside an
/// agent phase, launched with `clis: []`, still fails at distribution with the existing message —
/// nothing is distributed, no ballot is dispatched.
#[test]
fn a_plan_with_an_agent_unit_launched_with_no_seats_is_still_refused() {
    let dispatcher = Arc::new(CountingDispatcher(AtomicUsize::new(0)));
    let core = Core::spawn_with_engine(
        db_path("mixed"),
        dispatcher.clone(),
        Arc::new(CountingRunner(AtomicUsize::new(0))),
    );
    core.register_workflow(
        serde_json::json!({
            "id": "fe2e011-mixed",
            "phases": [
                {"id": "index", "kind": "recon",
                 "executor": {"type": "tool", "cmd": ["echo", "indexed"]}},
                {"id": "implement", "kind": "build", "depends_on": ["index"]}
            ]
        })
        .to_string(),
    )
    .expect("register the mixed workflow");
    let ev = core.subscribe();
    let sid = "fe2e011-mixed";
    core.launch_run(spec(sid, "fe2e011-mixed", None))
        .expect("the launch is accepted; the plan is refused at distribution");
    let events = drain_until_terminal(&ev, sid);

    let errs = errors(&events, sid);
    assert!(
        errs.iter().any(|m| {
            m.contains("council distribution failed")
                && m.contains("no eligible seat")
                && m.contains("every configured seat is benched")
        }),
        "the existing refusal is on the wire: {errs:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, CoreEvent::SessionFailed { session, .. } if session == sid)),
        "the run fails: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, CoreEvent::UnitDistributed { session, .. } if session == sid)),
        "nothing is distributed when a unit needs a seat and none is configured"
    );
    assert_eq!(
        dispatcher.0.load(Ordering::SeqCst),
        0,
        "no ballot dispatched"
    );
}
