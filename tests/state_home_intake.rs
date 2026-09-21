//! core#411 / wicked-crew#497 (acceptance findings F-RC1-011, F-RC2-020, F-032/F-033): an entry
//! under the daemon's state home that the state-home registry does not classify is a
//! CONFIGURATION error, refused at INTAKE — not a worker failure discovered at the run's first
//! unit and handed to the triage judge.
//!
//! Driven through a REAL `Core` and a REAL actor: with `WICKED_SKILLS_SNAPSHOT` pointing at a
//! fixture generation whose state home holds an unregistered directory (the live daemon's
//! `skills.fixture-debris-…` shape), `launch_run` must return a SYNCHRONOUS, TYPED
//! `StateHomeConfigError` — naming the entry and the remedy in operator terms, citing no test
//! fixture — with NO session persisted, NO event emitted for the run, and the agent runner never
//! called: the old first-worker refusal path is unreachable for this class. With the same state
//! home holding only registered entries — the three an operator variable can place there
//! (`workflows`, `steering-inbox`, `interactive`) included — the same launch is admitted and runs.
//!
//! One test in its own binary: it sets a process-global variable, and integration tests run one
//! process per file.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use wicked_core::{
    Core, CoreEvent, EntityMode, HumanConfirm, LaunchSpec, StateHomeConfigError, StepInput,
    StepOutput, StepRunner, StepStatus,
};
use wicked_council::types::{Category, Confidence, Dispatcher, InputMode, Vote};
use wicked_council::{AgenticCli, CouncilTask};

/// The crew-shaped snapshot fixture shared with the other skills integration tests.
#[path = "support/skills_snapshot_fixture.rs"]
mod skills_fixture;

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

struct NumericDispatcher;
impl Dispatcher for NumericDispatcher {
    fn dispatch(&self, c: &AgenticCli, _: &CouncilTask) -> Option<Vote> {
        Some(Vote {
            cli: c.key.clone(),
            recommendation: "1".into(),
            top_risk: "none".into(),
            change_my_mind: "no".into(),
            disqualifier: None,
            confidence: Confidence::default(),
            provenance: "numeric".into(),
        })
    }
}

/// Completes every agent unit with Ok — and records that it was asked to. For the refused launch
/// this must NEVER fire: the refusal is synchronous, before any unit exists.
struct RecordingOkRunner(Arc<AtomicBool>);
impl StepRunner for RecordingOkRunner {
    fn run_unit(&self, i: &StepInput) -> StepOutput {
        self.0.store(true, Ordering::SeqCst);
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

/// Drain events until a terminal event for `session` is observed or the deadline expires.
fn drain_until_terminal(
    events: &std::sync::mpsc::Receiver<CoreEvent>,
    session: &str,
) -> Vec<CoreEvent> {
    let mut collected = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(20);
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
                    CoreEvent::RunCancelled { session: s, .. } if s == session)
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

/// Every event currently queued (no waiting beyond a short settle) — for asserting SILENCE.
fn drain_settled(events: &std::sync::mpsc::Receiver<CoreEvent>) -> Vec<CoreEvent> {
    let mut collected = Vec::new();
    while let Ok(ev) = events.recv_timeout(Duration::from_millis(300)) {
        collected.push(ev);
    }
    collected
}

fn spec(session_id: &str, workflow: &str) -> LaunchSpec {
    LaunchSpec {
        base_ref: None,
        project_id: None,
        problem: "Extract the rules.".into(),
        clis: vec![cli("stub")],
        entity_mode: EntityMode::Shared,
        session_id: session_id.into(),
        human_confirm: HumanConfirm::None,
        auto_deliver: false,
        repo_ref: None,
        workflow: Some(workflow.into()),
        extra_write_roots: Vec::new(),
        extra_read_roots: Vec::new(),
        project_graph: None,
    }
}

#[test]
fn an_unregistered_state_home_entry_refuses_the_launch_at_intake_with_a_typed_config_error() {
    let base = std::fs::canonicalize(std::env::temp_dir())
        .unwrap()
        .join(format!("wicked-core-intake-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    // `<base>/crew-state/skills/snapshots/000001` — the state home is `<base>/crew-state`.
    let snapshot =
        skills_fixture::publish_fixture_snapshot(&base, "000001", &["wicked-garden-domain"]);
    let state_home = base.join("crew-state");
    std::env::set_var("WICKED_SKILLS_SNAPSHOT", &snapshot);
    std::env::remove_var("WICKED_WORKER_INHERIT_OPERATOR_CONFIG");

    let db = base.join("core.db").to_str().unwrap().to_string();
    let agent_ran = Arc::new(AtomicBool::new(false));
    let core = Core::spawn_with_engine(
        db,
        Arc::new(NumericDispatcher),
        Arc::new(RecordingOkRunner(agent_ran.clone())),
    );
    let ev = core.subscribe();
    core.register_workflow(
        serde_json::json!({
            "id": "one-agent-unit",
            "phases": [
                {"id": "extract", "kind": "build", "skill_ref": "wicked-garden-domain"}
            ]
        })
        .to_string(),
    )
    .expect("register workflow");

    // ── Refused at INTAKE: the live daemon's debris shape, a stray file, and a near-miss of a
    // registered NAME (`chats-x` beside the registered `chats` — a name claim is exact, never a
    // prefix). ──
    let debris = state_home.join("skills.fixture-debris-20260909");
    std::fs::create_dir_all(&debris).unwrap();
    std::fs::write(state_home.join("stray.txt"), "").unwrap();
    let near_miss = state_home.join("chats-x");
    std::fs::create_dir_all(&near_miss).unwrap();
    let err = core
        .launch_run(spec("intake-refused", "one-agent-unit"))
        .expect_err("an unregistered state-home entry refuses the launch synchronously");
    let typed = err
        .downcast_ref::<StateHomeConfigError>()
        .unwrap_or_else(|| panic!("the refusal is the TYPED configuration error, got: {err}"));
    assert_eq!(
        typed
            .unregistered
            .iter()
            .map(|u| (u.name.as_str(), u.level))
            .collect::<Vec<_>>(),
        vec![
            ("chats-x", "state-home"),
            ("skills.fixture-debris-20260909", "state-home"),
            ("stray.txt", "state-home")
        ],
        "every unregistered entry is named at once: {typed:?}"
    );
    assert_eq!(typed.var, "WICKED_SKILLS_SNAPSHOT");
    assert!(
        skills_fixture::names_generation(&typed.snapshot, &snapshot),
        "the error names the handed generation: `{}` vs `{}`",
        typed.snapshot,
        snapshot.display()
    );
    assert!(
        skills_fixture::names_generation(&typed.state_home, &state_home),
        "the error names the derived state home: `{}` vs `{}`",
        typed.state_home,
        state_home.display()
    );
    let text = err.to_string();
    for needle in [
        "configuration error",
        "`chats-x`",
        "`skills.fixture-debris-20260909`",
        "`stray.txt`",
        "the run was not started",
        "move each entry out of the state home",
    ] {
        assert!(text.contains(needle), "missing `{needle}` in: {text}");
    }
    // Operator terms only: no repository fixture path, and nothing that reads as a judge error.
    for banned in [
        "tests/fixtures",
        "state-home-subtrees.json",
        "triage judge",
        "skills snapshot refused the launch",
    ] {
        assert!(
            !text.contains(banned),
            "`{banned}` must not appear in: {text}"
        );
    }
    // NO session persisted, NO event for the run, NO worker: the first-worker refusal path is
    // unreachable for this class.
    assert!(
        !core
            .sessions()
            .expect("sessions")
            .iter()
            .any(|s| s == "intake-refused"),
        "a refused launch persists no session"
    );
    let silence = drain_settled(&ev);
    assert!(
        !silence
            .iter()
            .any(|e| format!("{e:?}").contains("intake-refused")),
        "a refused launch emits nothing for the run: {silence:?}"
    );
    assert!(
        !agent_ran.load(Ordering::SeqCst),
        "no worker ran — the launch never reached a unit"
    );

    // ── Admitted: the same state home with the entries gone and the registered names present —
    // the two env-placed ones, the crew-placed `interactive` root, and the `chats` transcripts
    // dir (registered, so fenced — never a refusal). ──
    std::fs::remove_dir_all(&debris).unwrap();
    std::fs::remove_dir_all(&near_miss).unwrap();
    std::fs::remove_file(state_home.join("stray.txt")).unwrap();
    for registered in ["workflows", "steering-inbox", "interactive", "chats"] {
        std::fs::create_dir_all(state_home.join(registered)).unwrap();
    }
    core.launch_run(spec("intake-admitted", "one-agent-unit"))
        .expect("a fully classified state home admits the launch");
    let events = drain_until_terminal(&ev, "intake-admitted");
    assert!(
        events.iter().any(|e| matches!(
            e,
            CoreEvent::SessionCompleted { session } if session == "intake-admitted"
        )),
        "the admitted run completes: {events:?}"
    );
    assert!(
        agent_ran.load(Ordering::SeqCst),
        "the admitted run reached its agent unit"
    );

    std::env::remove_var("WICKED_SKILLS_SNAPSHOT");
    drop(core);
    let _ = std::fs::remove_dir_all(&base);
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
