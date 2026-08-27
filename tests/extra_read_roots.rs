//! core#294 — `extraReadRoots`, the launch-declared READ-ONLY mirror of `extraWriteRoots`.
//!
//! The constraint space this closes (crew#311 → core#293): a run that must be GROUNDED in repo
//! content without binding its worktree had exactly three options, and all three were wrong —
//! `repoRef` killed the ACP permission stream, unbound absolute reads were denied by the boundary,
//! and `extraWriteRoots` on the repo root worked only by handing a doc-draft worker WRITE access
//! to the tree it was supposed to read.
//!
//! What must hold at the LAUNCH seam (the boundary semantics themselves are proven in
//! `execute_wrapped`'s unit tests, against the same two helpers the launcher calls):
//! 1. An unusable read root REJECTS the launch — synchronously, with NO session persisted. The
//!    same contract the write half carries: an invalid boundary declaration must never become a
//!    live run whose grounding silently did not happen.
//! 2. A valid one is PERSISTED on the session, so a resume/redrive re-arms the same grounding
//!    rather than depending on the daemon's memory of the launch.
//! 3. Declaring read roots widens NOTHING on the write side.
//! 4. And the one that makes the other three worth anything: the declared roots are DELIVERED —
//!    every governed unit the run dispatches receives them on its `StepInput.governance`, which is
//!    the only channel by which either carrier can arm a boundary at all.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use wicked_council::types::{Category, Confidence, Dispatcher, InputMode, Vote};
use wicked_council::{AgenticCli, CouncilTask};

use wicked_core::{
    Core, HumanConfirm, LaunchSpec, SessionStatus, StepInput, StepOutput, StepRunner, StepStatus,
};

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
    fn run_unit(&self, input: &StepInput) -> StepOutput {
        StepOutput {
            run_id: input.run_id.clone(),
            unit_ix: input.unit_ix,
            attempt: input.attempt,
            output: "ok".into(),
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

fn spec(session_id: &str, read_roots: Vec<String>) -> LaunchSpec {
    LaunchSpec {
        problem: "Draft the overview. Review the draft".into(),
        clis: vec![cli("a")],
        entity_mode: wicked_core::EntityMode::Shared,
        session_id: session_id.into(),
        human_confirm: HumanConfirm::All, // pause immediately; this test is about the LAUNCH seam
        repo_ref: None,
        workflow: None,
        project_id: None,
        extra_write_roots: Vec::new(),
        extra_read_roots: read_roots,
        project_graph: None,
    }
}

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("wicked-core-xrr-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::canonicalize(&dir).unwrap()
}

fn spawn(db: &std::path::Path) -> Core {
    Core::spawn_with_engine(
        db.to_str().unwrap().to_string(),
        Arc::new(StubDispatcher),
        Arc::new(OkRunner),
    )
}

/// `$HOME` is the prerequisite the boundary validation fails CLOSED without — and it is unset on
/// Windows, where the shell spells it `USERPROFILE`. Give the process one (only when it has none)
/// so the accept-path assertions below judge the ROOT, not the environment.
fn ensure_home() {
    if std::env::var_os("HOME").is_none() {
        std::env::set_var("HOME", scratch("home"));
    }
}

/// (1) + (3). A relative read root names no tree anyone declared — it would bind to whatever cwd
/// the launcher happened to have — so the launch is refused before a session exists.
#[test]
fn an_unusable_read_root_refuses_the_launch_with_no_session() {
    let db = scratch("reject").join("estate.db");
    let core = spawn(&db);

    let err = core
        .launch_run(spec("run-bad-read", vec!["relative/reference".into()]))
        .expect_err("a relative read root must refuse the launch");
    assert!(
        err.to_string().contains("read root"),
        "the failure must name WHICH declaration was refused, got: {err}"
    );
    assert!(
        !core
            .sessions()
            .unwrap()
            .contains(&"run-bad-read".to_string()),
        "a refused launch must persist NO session — a live run whose grounding silently did not \
         happen is worse than no run"
    );
}

/// (2) + (3). A valid read root rides the session, and touches the write half not at all.
#[test]
fn a_valid_read_root_is_persisted_on_the_session_and_widens_no_writes() {
    ensure_home();
    let db = scratch("accept").join("estate.db");
    let reference = scratch("reference-repo");
    let core = spawn(&db);

    let declared = reference.to_string_lossy().into_owned();
    core.launch_run(spec("run-grounded", vec![declared.clone()]))
        .expect("an absolute reference root outside the pin tree must be admitted");

    let views = core.sessions_detail().expect("sessions_detail");
    let session = &views
        .iter()
        .find(|v| v.session.id == "run-grounded")
        .expect("the launched run is persisted")
        .session;

    assert_eq!(
        session.extra_read_roots,
        vec![declared],
        "the declared grounding must survive on the session, so a resume re-arms the SAME \
         boundary rather than trusting the daemon's memory of the launch"
    );
    assert!(
        session.extra_write_roots.is_empty(),
        "declaring READ roots must widen nothing on the write side — that inversion is the whole \
         defect core#294 closes"
    );
}

// ── (4) DELIVERY ────────────────────────────────────────────────────────────────────────────────

/// A runner that runs nothing and only REMEMBERS what the actor armed it with. `governed: false`
/// on the way out: this stub writes no gate-hook marker, and claiming otherwise would make the
/// deny-dominant fold fail the run for a missing marker instead of letting it complete.
struct GovernanceSpy {
    /// One entry per dispatched unit: the read roots on that unit's `StepInput.governance`
    /// (`None` ⇒ the unit was dispatched UNGOVERNED, which is itself a delivery failure here).
    seen: Arc<Mutex<Vec<Option<Vec<String>>>>>,
}
impl StepRunner for GovernanceSpy {
    fn run_unit(&self, input: &StepInput) -> StepOutput {
        self.seen.lock().unwrap().push(
            input
                .governance
                .as_ref()
                .map(|g| g.extra_read_roots.clone()),
        );
        StepOutput {
            run_id: input.run_id.clone(),
            unit_ix: input.unit_ix,
            attempt: input.attempt,
            output: "ok".into(),
            status: StepStatus::Ok,
            usage: None,
            files: Vec::new(),
            tools: Vec::new(),
            governed: false,
        }
    }
}

/// Upper bound on the run. The wait returns the instant the run goes terminal, so a generous bound
/// costs nothing on an idle host — same reasoning as `governance_in_run::RUN_DEADLINE`.
const RUN_DEADLINE: Duration = Duration::from_secs(60);

fn wait_terminal(core: &Core, run_id: &str) -> Result<SessionStatus, String> {
    let deadline = Instant::now() + RUN_DEADLINE;
    let mut last = None;
    while Instant::now() < deadline {
        if let Ok(v) = core.sessions_detail() {
            if let Some(s) = v.iter().find(|s| s.session.id == run_id) {
                if matches!(
                    s.session.status,
                    SessionStatus::Completed | SessionStatus::Failed | SessionStatus::Cancelled
                ) {
                    return Ok(s.session.status);
                }
                last = Some(s.session.status);
            }
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    Err(format!(
        "run {run_id} never reached a terminal status within {}s — it was still {} when the wait \
         gave up, so this is a timeout, not a delivery outcome",
        RUN_DEADLINE.as_secs(),
        last.map_or_else(
            || "absent from sessions_detail".to_string(),
            |s| format!("{s:?}")
        )
    ))
}

/// (4) THE DELIVERY PATH. Persisting a read root on the session and validating it at launch are
/// both worthless if `dispatch_unit` never hands it to the worker: the boundary is armed from
/// `StepInput.governance`, on BOTH carriers, and nothing else. Before this test, the single line
/// that performs that hand-off (`extra_read_roots: session.extra_read_roots.clone()`) could be
/// replaced with `Vec::new()` and the whole suite stayed green — the feature's primary delivery
/// path had no coverage at all, only its two endpoints.
///
/// The run really executes (`HumanConfirm::None`, a file-backed store so `in_process_governance`
/// arms at all), so what is asserted is what a REAL governed unit received, not a fixture.
///
/// Mutations that must turn this red:
///   - `dispatch_unit`: `extra_read_roots: session.extra_read_roots.clone()` → `Vec::new()`
///     (or dropping the field, which falls back to the empty `..g`) — the equality assert fails,
///     naming the root the worker never got.
///   - `plan_and_distribute` / `AgentSession`: dropping the persisted roots — same assert, since
///     the dispatch reads them from the SESSION.
#[test]
fn every_dispatched_unit_receives_the_declared_read_roots() {
    ensure_home();
    let db = scratch("deliver").join("estate.db");
    let reference = scratch("deliver-reference");
    let declared = reference.to_string_lossy().into_owned();

    let seen = Arc::new(Mutex::new(Vec::new()));
    let core = Core::spawn_with_engine(
        db.to_str().unwrap().to_string(),
        Arc::new(StubDispatcher),
        Arc::new(GovernanceSpy {
            seen: Arc::clone(&seen),
        }),
    );

    let mut s = spec("run-deliver-read", vec![declared.clone()]);
    s.human_confirm = HumanConfirm::None; // no gate — this test is about what the WORKER receives
    core.launch_run(s).expect("the launch is admitted");

    let status =
        wait_terminal(&core, "run-deliver-read").expect("the run reaches a terminal state");
    assert_eq!(
        status,
        SessionStatus::Completed,
        "the spy runner succeeds every unit; a non-Completed run means this test measured \
         something other than delivery"
    );

    let seen = seen.lock().unwrap().clone();
    assert!(
        !seen.is_empty(),
        "the run dispatched no unit at all — there was no delivery to observe, so a green \
         assertion below would prove nothing"
    );
    for (i, roots) in seen.iter().enumerate() {
        let roots = roots.as_ref().unwrap_or_else(|| {
            panic!(
                "unit {i} was dispatched with NO governance context — a governed run's declared \
                 grounding cannot reach a worker that was never handed one"
            )
        });
        assert_eq!(
            roots,
            &vec![declared.clone()],
            "unit {i} was dispatched WITHOUT the read root the launch declared and the session \
             persisted — the boundary is armed from this field on both carriers, so a worker that \
             does not receive it is a worker whose grounding silently did not happen (core#294)"
        );
    }
}
