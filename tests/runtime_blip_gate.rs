//! (PRODUCT-WORKS J4) A container runtime blip is an infrastructure fault, never a seat failure.
//!
//! On the rig a Rancher Desktop restart took docker away mid-run: claude, then copilot, each
//! "failed" the unit with `container_runtime_down`, the failover ladder counted both, and the run
//! parked at a dead-seat card. The wrapped runner now waits for the runtime and, when it stays
//! down, returns its own "container runtime unavailable" line (`runtime_wait`). These tests pin
//! what the actor does with that line: no failover, no bench, a `failure` gate naming the
//! runtime, and Approve retries on the SAME seat.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use wicked_core::{
    Core, CoreEvent, EntityMode, HumanConfirm, HumanDecision, LaunchSpec, StepInput, StepOutput,
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
        deliver_step: None,
        exclude_seats: Vec::new(),
        evidence_root: None,
        primary: None,
        reduced_assurance: false,
        power: false,
        voice: None,
        deliverables: Vec::new(),
        qe_acceptance: Default::default(),
    }
}

/// The wrapped runner's own line once the runtime stayed down for its whole wait
/// (`runtime_wait::unavailable_output`), around the floor's refusal.
const RUNTIME_UNAVAILABLE: &str = "(container runtime unavailable: the container runtime did not \
answer for 180s, so the unit did not run on seat 'a' — an infrastructure fault on this host, not \
the seat's (the seat is not benched); start the container runtime (Rancher Desktop, Docker \
Desktop, `podman machine start`, `colima start`), then retry)\n(seat 'a' could not run this \
governed unit: its seat container did not arm — container_runtime_down: a container runtime CLI \
is installed but its engine did not answer)";

/// The floor's own refusal (`worker_sandbox::floor_refusal_text`), as the runner's first line.
const FLOOR_REFUSAL: &str = "(seat 'a' could not run this governed unit: its seat container did \
not arm — container_runtime_down: a container runtime CLI is installed but its engine did not \
answer (`<runtime> version` failed) — start it and retry; the unit is refused rather than run \
outside the floor (fail closed) — fix it, or let the run fail over to an eligible seat)";

/// Seat `a`'s first `fail_first` dispatches end on `text` (the runtime-unavailable line unless
/// set), then succeed; every dispatch's seat is recorded.
struct RuntimeDown {
    fail_first: u32,
    calls: AtomicU32,
    seats: Mutex<Vec<String>>,
    text: &'static str,
}
impl StepRunner for RuntimeDown {
    fn run_unit(&self, i: &StepInput) -> StepOutput {
        let seat = i.unit.assigned_cli.clone().unwrap_or_default();
        self.seats.lock().unwrap().push(seat);
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let (output, status) = if n < self.fail_first {
            (self.text.to_string(), StepStatus::Failed)
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

fn drain(
    events: &std::sync::mpsc::Receiver<CoreEvent>,
    session: &str,
    stop_at_gate: bool,
) -> Vec<CoreEvent> {
    let mut collected = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(15);
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        match events.recv_timeout(remaining.min(Duration::from_millis(500))) {
            Ok(ev) => {
                let stop = matches!(&ev,
                    CoreEvent::SessionCompleted { session: s, .. }
                    | CoreEvent::SessionFailed { session: s, .. } if s == session)
                    || (stop_at_gate
                        && matches!(&ev, CoreEvent::AwaitingHuman { session: s, .. } if s == session));
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

fn attended(session_id: &str, clis: Vec<AgenticCli>) -> LaunchSpec {
    LaunchSpec {
        human_confirm: HumanConfirm::Before(99),
        ..spec(session_id, clis)
    }
}

#[test]
fn a_runtime_that_stayed_down_pauses_as_infrastructure_and_approve_retries_the_same_seat() {
    let sid = "runtime-down-gate";
    let runner = Arc::new(RuntimeDown {
        fail_first: 1,
        calls: AtomicU32::new(0),
        seats: Mutex::new(Vec::new()),
        text: RUNTIME_UNAVAILABLE,
    });
    let core = Core::spawn_with_engine(
        ":memory:".to_string(),
        Arc::new(NumericDispatcher),
        runner.clone(),
    );
    let ev = core.subscribe();
    core.launch_run(attended(sid, vec![cli("a"), cli("b")]))
        .expect("launch");

    let collected = drain(&ev, sid, true);
    let (gate_kind, prompt) = collected
        .iter()
        .find_map(|e| match e {
            CoreEvent::AwaitingHuman {
                session,
                gate_kind,
                prompt,
                ..
            } if session == sid => Some((gate_kind.clone(), prompt.clone())),
            _ => None,
        })
        .unwrap_or_else(|| panic!("the run must pause, got: {collected:?}"));
    assert_eq!(gate_kind, "failure");
    assert!(
        prompt.contains("container runtime unavailable")
            && prompt.contains("not seat 'a''s")
            && prompt.contains("Start the container runtime")
            && prompt.contains("reassigning does not fix it"),
        "the gate names the runtime, not the seat: {prompt}"
    );
    assert!(
        collected.iter().any(|e| matches!(e,
            CoreEvent::StepFailed { session, failure_kind, .. }
                if session == sid && matches!(failure_kind, wicked_core::StepFailureKind::EnvironmentRefused))),
        "the step failure is the environment's (a skin offers no reassign): {collected:?}"
    );
    assert!(
        !collected.iter().any(|e| matches!(e,
            CoreEvent::StepFailed { detail, .. } if detail.contains("failing over"))),
        "no failover to another seat: {collected:?}"
    );
    assert!(
        !collected.iter().any(|e| matches!(
            e,
            CoreEvent::SeatBenched { .. } | CoreEvent::GateEscalated { .. }
        )),
        "no bench, no dead-seat escalation: {collected:?}"
    );
    assert_eq!(*runner.seats.lock().unwrap(), vec!["a".to_string()]);
    let views = core.sessions_detail().expect("views");
    let v = views
        .iter()
        .find(|v| v.session.id == sid)
        .expect("on record");
    assert!(
        v.session.benched_seats.is_empty(),
        "{:?}",
        v.session.benched_seats
    );
    assert!(
        v.units[0].worker_failed_clis.is_empty(),
        "the seat is not counted as having failed the unit: {:?}",
        v.units[0].worker_failed_clis
    );

    core.confirm_gate(
        sid,
        HumanDecision::Approve {
            amend: None,
            amend_scope: Default::default(),
        },
    )
    .expect("approve");
    let post = drain(&ev, sid, false);
    assert!(
        post.iter()
            .any(|e| matches!(e, CoreEvent::SessionCompleted { session } if session == sid)),
        "the retried unit completes once the runtime is back: {post:?}"
    );
    assert_eq!(
        runner.seats.lock().unwrap().first().map(String::as_str),
        Some("a")
    );
    assert_eq!(
        runner.seats.lock().unwrap().get(1).map(String::as_str),
        Some("a"),
        "Approve retries on the SAME seat"
    );
}

#[test]
fn an_autonomous_run_fails_on_a_runtime_that_stayed_down_without_touching_the_seats() {
    let sid = "runtime-down-solo";
    let runner = Arc::new(RuntimeDown {
        fail_first: u32::MAX,
        calls: AtomicU32::new(0),
        seats: Mutex::new(Vec::new()),
        text: RUNTIME_UNAVAILABLE,
    });
    let core = Core::spawn_with_engine(
        ":memory:".to_string(),
        Arc::new(NumericDispatcher),
        runner.clone(),
    );
    let ev = core.subscribe();
    core.launch_run(spec(sid, vec![cli("a"), cli("b")]))
        .expect("launch");
    let collected = drain(&ev, sid, true);
    assert!(
        collected
            .iter()
            .any(|e| matches!(e, CoreEvent::SessionFailed { session, .. } if session == sid)),
        "{collected:?}"
    );
    assert!(
        !collected
            .iter()
            .any(|e| matches!(e, CoreEvent::SeatBenched { .. })),
        "{collected:?}"
    );
    assert_eq!(
        *runner.seats.lock().unwrap(),
        vec!["a".to_string()],
        "never failed over to b"
    );
}

/// A raw floor refusal that reaches the actor is the runtime's too: no failover to `b`.
#[test]
fn the_floors_raw_runtime_refusal_never_fails_over_to_another_seat() {
    let sid = "runtime-raw-refusal";
    let runner = Arc::new(RuntimeDown {
        fail_first: 1,
        calls: AtomicU32::new(0),
        seats: Mutex::new(Vec::new()),
        text: FLOOR_REFUSAL,
    });
    let core = Core::spawn_with_engine(
        ":memory:".to_string(),
        Arc::new(NumericDispatcher),
        runner.clone(),
    );
    let ev = core.subscribe();
    core.launch_run(attended(sid, vec![cli("a"), cli("b")]))
        .expect("launch");
    let collected = drain(&ev, sid, true);
    assert!(
        collected.iter().any(|e| matches!(e,
            CoreEvent::AwaitingHuman { session, gate_kind, .. } if session == sid && gate_kind == "failure")),
        "{collected:?}"
    );
    assert_eq!(*runner.seats.lock().unwrap(), vec!["a".to_string()]);
}

// ── Test-harness hygiene (core#311) — not a test ─────────────────────────────────────────────
/// SAFETY (`ctor(unsafe)`): runs before `main` on one thread and only sets one process env var
/// via the std API — no allocator setup, no threads, no panics across the FFI boundary.
#[ctor::ctor(unsafe)]
fn arm_hermetic_emit_spool() {
    wicked_apps_core::emit::hermetic_test_spool();
}
