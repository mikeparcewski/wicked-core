//! crew#277 — autonomous seat failover and failed-run resume.
//!
//! Three governed dogfood runs each died at ONE unit on a seat-level worker error (agy exit-1
//! timeout ×2, copilot hang) while healthy seats sat idle, and the only recovery was a full
//! relaunch that re-burned every verified phase. These tests pin the two recovery seams:
//!  * a governed unit whose worker fails TRANSIENTLY fails over to the next eligible seat;
//!  * a FAILED run resumes from its cursor unit instead of no-opping.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use wicked_core::{
    Core, CoreEvent, EntityMode, HumanConfirm, LaunchSpec, SessionStatus, StepInput, StepOutput,
    StepRunner, StepStatus,
};
use wicked_council::types::{Category, Confidence, Dispatcher, InputMode, Vote};
use wicked_council::{AgenticCli, CouncilTask};

fn cli(key: &str) -> AgenticCli {
    AgenticCli {
        key: key.into(),
        display_name: key.into(),
        binary: key.into(),
        alt_binaries: Vec::new(),
        headless_invocation: format!("{key} -p {{PROMPT}}"),
        category: Category::AgenticCoder,
        input_mode: InputMode::PromptArg,
        version_probe: Vec::new(),
        trust_flags: Vec::new(),
        confidence: Confidence::default(),
        enabled_for_council: true,
        acp: None,
        capabilities: None,
        login_invocation: None,
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

fn spec(session_id: &str, clis: Vec<AgenticCli>) -> LaunchSpec {
    LaunchSpec {
        project_id: None,
        problem: "Do step one.".into(),
        clis,
        entity_mode: EntityMode::Shared,
        session_id: session_id.into(),
        human_confirm: HumanConfirm::None,
        repo_ref: None,
        workflow: None,
        extra_write_roots: Vec::new(),
        project_graph: None,
    }
}

fn drain_until_terminal(
    events: &std::sync::mpsc::Receiver<CoreEvent>,
    session: &str,
) -> Vec<CoreEvent> {
    let mut collected = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
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
                    CoreEvent::SessionCompleted { session: s, .. }
                    | CoreEvent::SessionFailed { session: s, .. } if s == session);
                collected.push(ev);
                if terminal {
                    break;
                }
            }
            Err(_) => continue,
        }
    }
    collected
}

/// Fails the FIRST dispatch with a transient, governed worker error; succeeds after. The
/// failure message matches `is_transient_cli_failure` (the wrapped runner's nonzero-exit shape).
struct FailFirstGoverned {
    calls: AtomicU32,
}
impl StepRunner for FailFirstGoverned {
    fn run_unit(&self, i: &StepInput) -> StepOutput {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            StepOutput {
                run_id: i.run_id.clone(),
                unit_ix: i.unit_ix,
                attempt: i.attempt,
                output: "(cli `x` exited 1) timeout waiting for response".into(),
                status: StepStatus::Failed,
                usage: None,
                files: vec![],
                tools: Vec::new(),
                governed: true,
            }
        } else {
            // Ungoverned success: the governance fold (armed-marker + decisions log) is not
            // under test here — only the failover/resume mechanics, which key off the FAILED
            // output's governed flag.
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
}

/// THE crew#277 shape: a two-seat roster, the first worker dies on a seat-level error, the run
/// must fail over to the other seat and COMPLETE — not die with verified work behind it.
#[test]
fn a_transient_governed_worker_error_fails_over_to_the_next_seat() {
    let core = Core::spawn_with_engine(
        ":memory:".to_string(),
        Arc::new(NumericDispatcher),
        Arc::new(FailFirstGoverned {
            calls: AtomicU32::new(0),
        }),
    );
    let ev = core.subscribe();
    core.launch_run(spec("failover", vec![cli("a"), cli("b")]))
        .expect("launch");

    let collected = drain_until_terminal(&ev, "failover");

    let failed_over = collected.iter().any(|e| {
        matches!(e,
        CoreEvent::StepFailed { session, detail, .. }
            if session == "failover" && detail.contains("failing over to"))
    });
    assert!(
        failed_over,
        "a transient governed worker error with an eligible second seat must emit the \
         failover StepFailed, got: {collected:?}"
    );
    assert!(
        collected.iter().any(
            |e| matches!(e, CoreEvent::SessionCompleted { session, .. } if session == "failover")
        ),
        "the run must COMPLETE on the failover seat, not die on the first seat's error"
    );
}

/// Control: with a single-seat roster there is no eligible seat — the standard fail contract
/// holds exactly as before (no retry loop, no silent behavior change).
#[test]
fn a_single_seat_roster_still_fails_closed() {
    let core = Core::spawn_with_engine(
        ":memory:".to_string(),
        Arc::new(NumericDispatcher),
        Arc::new(FailFirstGoverned {
            calls: AtomicU32::new(0),
        }),
    );
    let ev = core.subscribe();
    core.launch_run(spec("solo-fail", vec![cli("a")]))
        .expect("launch");

    let collected = drain_until_terminal(&ev, "solo-fail");
    assert!(
        collected.iter().any(
            |e| matches!(e, CoreEvent::SessionFailed { session, .. } if session == "solo-fail")
        ),
        "no eligible failover seat → the run fails as before"
    );
}

/// crew#277's second ask: a FAILED run is not a tombstone. `resume_run` re-dispatches the
/// cursor unit (attempt bumped, unit reset), and with the transient gone the run completes.
#[test]
fn resume_re_dispatches_the_cursor_unit_of_a_failed_run() {
    let core = Core::spawn_with_engine(
        ":memory:".to_string(),
        Arc::new(NumericDispatcher),
        Arc::new(FailFirstGoverned {
            calls: AtomicU32::new(0),
        }),
    );
    let ev = core.subscribe();
    core.launch_run(spec("resume-fail", vec![cli("a")]))
        .expect("launch");
    let first = drain_until_terminal(&ev, "resume-fail");
    assert!(
        first.iter().any(
            |e| matches!(e, CoreEvent::SessionFailed { session, .. } if session == "resume-fail")
        ),
        "precondition: the single-seat run fails on the transient"
    );

    let status = core
        .resume_run("resume-fail")
        .expect("resume accepts a failed run");
    assert_eq!(
        status,
        SessionStatus::Executing,
        "resume must put a failed run back on the executing path, not no-op"
    );
    let second = drain_until_terminal(&ev, "resume-fail");
    assert!(
        second.iter().any(
            |e| matches!(e, CoreEvent::SessionCompleted { session, .. } if session == "resume-fail")
        ),
        "with the transient gone, the resumed run completes from its cursor unit"
    );
}
