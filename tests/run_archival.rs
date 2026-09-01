//! crew#265 — run archival contract: a write-off, not a delete.
//!
//! Arms: a TERMINAL run archives (and unarchives) with the mark round-tripping through the store;
//! an UNKNOWN id answers `false` (the 404 seam). Write-off must never hide live work — the
//! non-terminal refusal is asserted by message in the actor handler; driving a run into a durable
//! non-terminal state without racing the stub engine is what the AwaitingHuman arm covers.

use std::sync::Arc;

use wicked_core::{
    Core, HumanConfirm, LaunchSpec, SessionStatus, StepInput, StepOutput, StepRunner, StepStatus,
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

struct OkRunner;
impl StepRunner for OkRunner {
    fn run_unit(&self, i: &StepInput) -> StepOutput {
        StepOutput {
            run_id: i.run_id.clone(),
            unit_ix: i.unit_ix,
            attempt: i.attempt,
            output: "done".into(),
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

fn spec(session_id: &str, problem: &str, human_confirm: HumanConfirm) -> LaunchSpec {
    LaunchSpec {
        project_id: None,
        problem: problem.into(),
        clis: vec![cli("a")],
        entity_mode: wicked_core::EntityMode::Shared,
        session_id: session_id.into(),
        human_confirm,
        repo_ref: None,
        workflow: None,
        extra_write_roots: Vec::new(),
        extra_read_roots: Vec::new(),
        project_graph: None,
    }
}

fn db_path(name: &str) -> String {
    let dir = std::env::temp_dir().join(format!("wicked-archival-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("estate.db").to_str().unwrap().to_string()
}

fn wait_status(core: &Core, run_id: &str, want: &[SessionStatus]) -> SessionStatus {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let views = core.sessions_detail().expect("sessions_detail");
        if let Some(v) = views.iter().find(|v| v.session.id == run_id) {
            if want.contains(&v.session.status) {
                return v.session.status;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "run {run_id} never reached {want:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

#[test]
fn a_terminal_run_archives_and_unarchives_and_unknown_answers_false() {
    let core = Core::spawn_with_engine(
        db_path("terminal"),
        Arc::new(StubDispatcher),
        Arc::new(OkRunner),
    );
    core.launch_run(spec("arch-1", "one trivial thing", HumanConfirm::None))
        .expect("launch");
    wait_status(&core, "arch-1", &[SessionStatus::Completed]);

    // Archive: found; mark + note persist through the store round-trip.
    assert!(core
        .archive_run("arch-1", true, Some("campaign backlog".into()))
        .expect("archive must succeed on a terminal run"));
    let views = core.sessions_detail().unwrap();
    let v = views.iter().find(|v| v.session.id == "arch-1").unwrap();
    assert!(v.session.archived_at.is_some(), "archived_at persists");
    assert_eq!(v.session.archive_note.as_deref(), Some("campaign backlog"));

    // Unarchive: mark AND note clear.
    assert!(core.archive_run("arch-1", false, None).expect("unarchive"));
    let views = core.sessions_detail().unwrap();
    let v = views.iter().find(|v| v.session.id == "arch-1").unwrap();
    assert!(v.session.archived_at.is_none(), "unarchive clears the mark");
    assert!(v.session.archive_note.is_none(), "…and the note");

    // Unknown id → Ok(false), never an error (the route's 404 seam).
    assert!(!core
        .archive_run("no-such-run", true, None)
        .expect("unknown id is Ok(false), not Err"));
}

#[test]
fn a_non_terminal_run_refuses_archival() {
    let core = Core::spawn_with_engine(
        db_path("nonterminal"),
        Arc::new(StubDispatcher),
        Arc::new(OkRunner),
    );
    // `HumanConfirm::All` pauses BEFORE the first unit — a durable AwaitingHuman state with no
    // worker in flight to race.
    core.launch_run(spec("arch-2", "one thing", HumanConfirm::All))
        .expect("launch");
    wait_status(&core, "arch-2", &[SessionStatus::AwaitingHuman]);

    let err = core
        .archive_run("arch-2", true, None)
        .expect_err("a live run must refuse the write-off — archival must never hide live work");
    assert!(
        err.to_string().contains("AwaitingHuman"),
        "the refusal names the status so the route can answer 409: {err}"
    );
}

// ── Test-harness hygiene (core#311) — not a test ─────────────────────────────────────────────
/// Arm the hermetic emit spool BEFORE main (pre-main is single-threaded, so no test thread can
/// race it): engine paths under test fire coarse fire-and-forget `wicked.*` emissions, and with
/// no shared store configured those spool — which must land in a per-process temp file, never in
/// the operator's real `~/.something-wicked/wicked-apps/emit-outbox.ndjson` replay queue. Every
/// binary in this suite carries this block; `harness_hygiene.rs` fails the suite if one is missing.
///
/// SAFETY (`ctor(unsafe)`): runs before `main` on one thread and only sets one process env var
/// via the std API — no allocator setup, no threads, no panics across the FFI boundary.
#[ctor::ctor(unsafe)]
fn arm_hermetic_emit_spool() {
    wicked_apps_core::emit::hermetic_test_spool();
}
