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

fn spec(session_id: &str, clis: Vec<AgenticCli>) -> LaunchSpec {
    LaunchSpec {
        base_ref: None,
        project_id: None,
        problem: "Do step one.".into(),
        clis,
        entity_mode: EntityMode::Shared,
        session_id: session_id.into(),
        human_confirm: HumanConfirm::None,
        auto_deliver: false,
        repo_ref: None,
        workflow: None,
        extra_write_roots: Vec::new(),
        extra_read_roots: Vec::new(),
        project_graph: None,
        plan: None,
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

/// core#292 — the SAME failure, from a seat whose CLI has no input-governance adapter.
///
/// The gate-hook injection is claude-only — see `execute_wrapped`, where the
/// `(Some(_), false)` arm yields `GovernanceUnenforced` — so a campaign unit dispatched to
/// codex/agy/pi comes back with `StepOutput.governed == false` even though the run itself is
/// governance-armed.
///
/// This is NOT byte-identical to `FailFirstGoverned`; it differs in the failure message, in
/// structure, and in returning `governed: false` on BOTH outputs. That last one is deliberate and
/// worth knowing: setting `governed: true` on the SUCCESS output trips the phase-substance gate,
/// because the 2-char "ok" body is too thin for a governed unit. What isolates the variable is not
/// textual sameness but a control — on unmodified `main`, flipping only the failing output's
/// `governed` flag turns this test green, and flipping it back turns it red.
struct FailFirstUngovernedSeat {
    calls: AtomicU32,
}
impl StepRunner for FailFirstUngovernedSeat {
    fn run_unit(&self, i: &StepInput) -> StepOutput {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        StepOutput {
            run_id: i.run_id.clone(),
            unit_ix: i.unit_ix,
            attempt: i.attempt,
            output: if n == 0 {
                "(cli `agy` exited 1) timeout waiting for response".into()
            } else {
                "ok".to_string()
            },
            status: if n == 0 {
                StepStatus::Failed
            } else {
                StepStatus::Ok
            },
            usage: None,
            files: vec![],
            tools: Vec::new(),
            // The FINDING-063 disclosure shape: governed campaign unit, unenforceable seat.
            governed: false,
        }
    }
}

/// core#292. Field evidence: three governed runs died at one worker failure each (`agy exited 1`
/// ×2, `codex exited 1`) with healthy seats idle. The ladder's condition was `output.governed`,
/// which means "the runner armed the input-governance hook" — true only for claude — so failover
/// armed for exactly the seat that does not exhibit the exit-1/timeout failure mode, and never for
/// the ones that do. Phase idempotency (the ladder's actual justification) is a property of the
/// campaign phase, not of which CLI can host a PreToolUse hook.
#[test]
fn an_ungoverned_seat_worker_error_also_fails_over() {
    let core = Core::spawn_with_engine(
        ":memory:".to_string(),
        Arc::new(NumericDispatcher),
        Arc::new(FailFirstUngovernedSeat {
            calls: AtomicU32::new(0),
        }),
    );
    let ev = core.subscribe();
    core.launch_run(spec("failover-ungoverned", vec![cli("a"), cli("b")]))
        .expect("launch");

    let collected = drain_until_terminal(&ev, "failover-ungoverned");

    let failed_over = collected.iter().any(|e| {
        matches!(e,
        CoreEvent::StepFailed { session, detail, .. }
            if session == "failover-ungoverned" && detail.contains("failing over to"))
    });
    assert!(
        failed_over,
        "a worker-originated failure on a seat with no gate-hook adapter must ALSO fail over — \
         the ladder is keyed to phase idempotency, not to input governance, got: {collected:?}"
    );
    assert!(
        collected.iter().any(|e| matches!(
            e,
            CoreEvent::SessionCompleted { session, .. } if session == "failover-ungoverned"
        )),
        "the run must COMPLETE on the failover seat, not die on the first seat's error"
    );
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

// ── core#461: a worker that exits on a DEAD-SEAT refusal (quota / sign-in / missing binary) ──

/// The wrapped runner's own exit frame around copilot's refusal from run 390b273e — a seat that is
/// dead for this work, not a transient. The run died through the triage judge (`fail` → no gate).
const QUOTA_EXIT: &str =
    "(cli `a` exited 1) You have exceeded your monthly quota for premium requests.";

/// Seat `a` exits on the quota refusal every time; every other seat succeeds. Counts how often
/// the failure-triage judge was convened (the judge runs under a `triage-` run id) and rules FAIL
/// when it is — the way the real judge did ("no flag can fix a quota cap").
struct DeadSeatA {
    judge_convened: AtomicU32,
}
impl StepRunner for DeadSeatA {
    fn run_unit(&self, i: &StepInput) -> StepOutput {
        let (output, status) = if i.run_id.starts_with("triage-") {
            self.judge_convened.fetch_add(1, Ordering::SeqCst);
            (
                "DECISION: FAIL\nno flag can fix a quota cap".to_string(),
                StepStatus::Ok,
            )
        } else if i.unit.assigned_cli.as_deref() == Some("a") {
            (QUOTA_EXIT.to_string(), StepStatus::Failed)
        } else {
            ("ok".to_string(), StepStatus::Ok)
        };
        StepOutput {
            run_id: i.run_id.clone(),
            unit_ix: i.unit_ix,
            attempt: i.attempt,
            output,
            status,
            usage: None,
            files: vec![],
            tools: Vec::new(),
            governed: false,
        }
    }
}

/// An ATTENDED run — a human is present (any `HumanConfirm` but `None`) so failures may pause —
/// with no gate ord that matches, so nothing pauses before the work.
fn attended(session_id: &str, clis: Vec<AgenticCli>) -> LaunchSpec {
    LaunchSpec {
        base_ref: None,
        human_confirm: HumanConfirm::Before(99),
        ..spec(session_id, clis)
    }
}

/// Like [`drain_until_terminal`], but a human gate (`AwaitingHuman`) also ends the drain.
fn drain_until_gate_or_terminal(
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
                let stop = matches!(&ev,
                    CoreEvent::SessionCompleted { session: s, .. }
                    | CoreEvent::SessionFailed { session: s, .. }
                    | CoreEvent::AwaitingHuman { session: s, .. } if s == session);
                collected.push(ev);
                if stop {
                    break;
                }
            }
            Err(_) => continue,
        }
    }
    collected
}

/// (core#461 b) An ATTENDED run whose worker exits on a classified seat refusal does not convene
/// the triage judge — the seat is dead for this work and no judge can rule otherwise — it takes
/// the failover ladder: the seat is benched, the work moves to the next eligible seat, the run
/// COMPLETES. (Run 390b273e convened the judge on this exact exit, it ruled `fail`, and the run
/// died with claude and opencode idle.)
#[test]
fn a_dead_seat_worker_exit_fails_over_without_convening_the_triage_judge() {
    let runner = Arc::new(DeadSeatA {
        judge_convened: AtomicU32::new(0),
    });
    let core = Core::spawn_with_engine(
        ":memory:".to_string(),
        Arc::new(NumericDispatcher),
        runner.clone(),
    );
    let ev = core.subscribe();
    core.launch_run(attended("dead-seat-failover", vec![cli("a"), cli("b")]))
        .expect("launch");

    let collected = drain_until_gate_or_terminal(&ev, "dead-seat-failover");

    assert!(
        collected.iter().any(|e| matches!(e,
            CoreEvent::StepFailed { session, detail, .. }
                if session == "dead-seat-failover" && detail.contains("failing over to 'b'"))),
        "the quota exit fails over to the eligible seat, got: {collected:?}"
    );
    assert!(
        collected.iter().any(|e| matches!(
            e,
            CoreEvent::SessionCompleted { session, .. } if session == "dead-seat-failover"
        )),
        "the run COMPLETES on the failover seat, got: {collected:?}"
    );
    assert_eq!(
        runner.judge_convened.load(Ordering::SeqCst),
        0,
        "a classified seat refusal never convenes the triage judge"
    );
}

/// (core#461 b) The same exit on a roster with NO eligible seat left, attended: the run PAUSES at
/// core#464's escalation gate — `gateEscalated` with the `dead_seat` class, a prompt naming the
/// seat, the class and the reassign lever — instead of ending `sessionFailed` with no gate. The
/// unit carries the `dead_seat` denial the gate's reassign arm keys on.
#[test]
fn a_dead_seat_worker_exit_with_no_seat_left_pauses_at_the_escalation_gate() {
    let runner = Arc::new(DeadSeatA {
        judge_convened: AtomicU32::new(0),
    });
    let core = Core::spawn_with_engine(
        ":memory:".to_string(),
        Arc::new(NumericDispatcher),
        runner.clone(),
    );
    let ev = core.subscribe();
    core.launch_run(attended("dead-seat-gate", vec![cli("a")]))
        .expect("launch");

    let collected = drain_until_gate_or_terminal(&ev, "dead-seat-gate");

    let gate = collected.iter().find_map(|e| match e {
        CoreEvent::AwaitingHuman {
            session,
            gate_kind,
            prompt,
            ..
        } if session == "dead-seat-gate" => Some((gate_kind.clone(), prompt.clone())),
        _ => None,
    });
    let (gate_kind, prompt) = gate.unwrap_or_else(|| {
        panic!("a dead-seat exit with no seat left must PAUSE, got: {collected:?}")
    });
    assert_eq!(gate_kind, "escalation", "core#464's one denial route");
    assert!(
        collected.iter().any(|e| matches!(e,
            CoreEvent::GateEscalated { session, condition, denial_source, .. }
                if session == "dead-seat-gate" && condition == "dead_seat" && denial_source == "dead_seat")),
        "the denial rides gateEscalated with the dead_seat class: {collected:?}"
    );
    assert!(
        prompt.contains("Unit 1 (a) failed on a dead seat")
            && prompt.contains("exhausted its quota")
            && prompt.contains("quota_exhausted")
            && prompt.contains("Reassign the unit"),
        "the prompt names the seat, the class and the lever: {prompt}"
    );
    assert!(
        !collected.iter().any(
            |e| matches!(e, CoreEvent::SessionFailed { session, .. } if session == "dead-seat-gate")
        ),
        "no sessionFailed — the operator decides: {collected:?}"
    );
    assert_eq!(runner.judge_convened.load(Ordering::SeqCst), 0);
    let views = core.sessions_detail().expect("views");
    let v = views
        .iter()
        .find(|v| v.session.id == "dead-seat-gate")
        .expect("the run is on record");
    assert_eq!(v.session.status, SessionStatus::AwaitingHuman);
    assert_eq!(
        v.units[0].denial.as_ref().map(|d| d.source.as_str()),
        Some("dead_seat"),
        "the structured denial names the class the gate's reassign arm keys on: {:?}",
        v.units[0].denial
    );
}

/// Control (core#461 b, disclosed scope): an AUTONOMOUS run (`HumanConfirm::None`) on a dead
/// single seat keeps the standard fail contract — there is no operator to ask.
#[test]
fn an_autonomous_run_on_a_dead_single_seat_still_fails_closed() {
    let runner = Arc::new(DeadSeatA {
        judge_convened: AtomicU32::new(0),
    });
    let core = Core::spawn_with_engine(
        ":memory:".to_string(),
        Arc::new(NumericDispatcher),
        runner.clone(),
    );
    let ev = core.subscribe();
    core.launch_run(spec("dead-seat-solo", vec![cli("a")]))
        .expect("launch");

    let collected = drain_until_gate_or_terminal(&ev, "dead-seat-solo");
    assert!(
        collected.iter().any(
            |e| matches!(e, CoreEvent::SessionFailed { session, .. } if session == "dead-seat-solo")
        ),
        "no operator in the loop → the run fails as before, got: {collected:?}"
    );
    assert_eq!(runner.judge_convened.load(Ordering::SeqCst), 0);
}
