//! core#556 — a launch REFUSAL keeps its human decision point on EVERY attempt.
//!
//! Run `db708484`: the unit's first refusal opened a gate, the operator took the gate's own
//! reassign-and-retry, the retried attempt refused identically, and the engine went straight to
//! `sessionFailed` — the `attempt == 0` guard removed the gate on the second refusal. These tests
//! drive the REAL engine (`Core::launch_run` → dispatch → the step-failure reducer) with a runner
//! that refuses every attempt, and approve each gate: every refusal must pause for a decision,
//! never fail the run.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use wicked_core::{
    Core, CoreEvent, EntityMode, HumanConfirm, HumanDecision, LaunchSpec, StepInput, StepOutput,
    StepRunner, StepStatus,
};
use wicked_council::types::{Category, Confidence, Dispatcher, InputMode, Vote};
use wicked_council::{AgenticCli, CouncilTask};

#[ctor::ctor(unsafe)]
fn arm_hermetic_emit_spool() {
    wicked_apps_core::emit::hermetic_test_spool();
}

fn db_path(name: &str) -> String {
    let dir = std::env::temp_dir().join(format!(
        "wicked-core-launch-refusal-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("estate.db").to_str().unwrap().to_string()
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

struct NoBallots;
impl Dispatcher for NoBallots {
    fn dispatch(&self, _c: &AgenticCli, _: &CouncilTask) -> Option<Vote> {
        None
    }
}

/// Refuses every dispatch before any work starts: attempt 0 with the TTY refusal the engine
/// classifies as an ENVIRONMENT refusal; later attempts with `later` (the same TTY refusal, or an
/// unclassified launch refusal).
struct RefusingRunner {
    later: &'static str,
    dispatches: AtomicU32,
}

const TTY_REFUSAL: &str = "bubbletea: error opening TTY: could not open TTY";
const FENCE_REFUSAL: &str =
    "wicked-core: refused to launch the worker: the skills snapshot fence rejected the state home";

impl StepRunner for RefusingRunner {
    fn run_unit(&self, i: &StepInput) -> StepOutput {
        self.dispatches.fetch_add(1, Ordering::SeqCst);
        StepOutput {
            run_id: i.run_id.clone(),
            unit_ix: i.unit_ix,
            attempt: i.attempt,
            output: if i.attempt == 0 {
                TTY_REFUSAL.into()
            } else {
                self.later.into()
            },
            status: StepStatus::Failed,
            usage: None,
            files: vec![],
            tools: Vec::new(),
            governed: false,
        }
    }
}

const ONE_BUILD: &str = r#"{"id":"refusal-one-build","phases":[
  {"id":"build","kind":"build","gate":"auto"}]}"#;

fn spec(sid: &str) -> LaunchSpec {
    LaunchSpec {
        base_ref: None,
        project_id: None,
        problem: "Build the thing.".into(),
        clis: vec![cli("claude"), cli("opencode")],
        entity_mode: EntityMode::Shared,
        session_id: sid.into(),
        // An operator is in the loop (anything but `none`); `before:99` names no unit of this
        // one-phase run, so the only pauses are the ones the refusals open.
        human_confirm: HumanConfirm::Before(99),
        auto_deliver: false,
        repo_ref: None,
        workflow: Some("refusal-one-build".into()),
        extra_write_roots: Vec::new(),
        extra_read_roots: Vec::new(),
        project_graph: None,
        plan: None,
        deliver_step: None,
        exclude_seats: Vec::new(),
        evidence_root: None,
    }
}

fn collect_until(
    events: &std::sync::mpsc::Receiver<CoreEvent>,
    within: Duration,
    until: impl Fn(&CoreEvent) -> bool,
) -> Vec<CoreEvent> {
    let mut out = Vec::new();
    let deadline = Instant::now() + within;
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or(Duration::ZERO);
        if remaining.is_zero() {
            break;
        }
        match events.recv_timeout(remaining.min(Duration::from_millis(200))) {
            Ok(ev) => {
                let done = until(&ev);
                out.push(ev);
                if done {
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
        }
    }
    out
}

fn paused_or_failed(sid: &str) -> impl Fn(&CoreEvent) -> bool + '_ {
    move |e| {
        matches!(e, CoreEvent::AwaitingHuman { session, .. } if session == sid)
            || matches!(e, CoreEvent::SessionFailed { session, .. } if session == sid)
    }
}

/// The prompt of the run's (single) `awaitingHuman` in `evs`; panics — naming the events — when
/// the refusal went to `sessionFailed` instead.
fn gate_prompt(evs: &[CoreEvent], sid: &str) -> String {
    assert!(
        !evs.iter()
            .any(|e| matches!(e, CoreEvent::SessionFailed { session, .. } if session == sid)),
        "a launch refusal must gate, never sessionFailed: {evs:?}"
    );
    evs.iter()
        .find_map(|e| match e {
            CoreEvent::AwaitingHuman {
                session, prompt, ..
            } if session == sid => Some(prompt.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("an awaitingHuman gate on the wire: {evs:?}"))
}

fn approve() -> HumanDecision {
    HumanDecision::Approve {
        amend: None,
        amend_scope: Default::default(),
    }
}

/// Two consecutive environment refusals of the same unit → two gates; the second names its
/// attempt. (Before core#556 the second went to `sessionFailed`.)
#[test]
fn two_consecutive_environment_refusals_open_two_gates() {
    let sid = "refusal-env-twice";
    let runner = Arc::new(RefusingRunner {
        later: TTY_REFUSAL,
        dispatches: AtomicU32::new(0),
    });
    let core = Core::spawn_with_engine(db_path("env"), Arc::new(NoBallots), runner.clone());
    core.register_workflow(ONE_BUILD).expect("register def");
    let ev = core.subscribe();
    core.launch_run(spec(sid)).expect("launch");

    let first = collect_until(&ev, Duration::from_secs(20), paused_or_failed(sid));
    let p1 = gate_prompt(&first, sid);
    assert!(p1.contains("refused its environment on attempt 1"), "{p1}");

    core.confirm_gate(sid, approve())
        .expect("approve the retry");
    let second = collect_until(&ev, Duration::from_secs(20), paused_or_failed(sid));
    let p2 = gate_prompt(&second, sid);
    assert!(
        p2.contains("refused its environment on attempt 2") && p2.contains("CLI requires a TTY"),
        "the second gate names the cause and the attempt: {p2}"
    );
    // A refusal is the environment's, not the seat's: the prompt never offers the reassign lever
    // the gate card hides for it (studio#315), and says why.
    for p in [&p1, &p2] {
        assert!(
            !p.contains("different CLI"),
            "no reassign lever on a refusal: {p}"
        );
        assert!(p.contains("Another CLI meets the same environment"), "{p}");
    }
    assert_eq!(runner.dispatches.load(Ordering::SeqCst), 2);
}

/// An environment refusal, then — after the operator's retry — an UNCLASSIFIED launch refusal:
/// the triage judge already ruled on attempt 0, so the retried attempt pauses for the operator
/// with the cause and the attempt number, never `sessionFailed`.
#[test]
fn an_unclassified_refusal_on_a_retried_attempt_gates() {
    let sid = "refusal-fence-retry";
    let runner = Arc::new(RefusingRunner {
        later: FENCE_REFUSAL,
        dispatches: AtomicU32::new(0),
    });
    let core = Core::spawn_with_engine(db_path("fence"), Arc::new(NoBallots), runner.clone());
    core.register_workflow(ONE_BUILD).expect("register def");
    let ev = core.subscribe();
    core.launch_run(spec(sid)).expect("launch");

    let first = collect_until(&ev, Duration::from_secs(20), paused_or_failed(sid));
    gate_prompt(&first, sid);
    core.confirm_gate(sid, approve())
        .expect("approve the retry");

    let second = collect_until(&ev, Duration::from_secs(20), paused_or_failed(sid));
    let p2 = gate_prompt(&second, sid);
    assert!(
        p2.contains("failed again on attempt 2") && p2.contains("skills snapshot fence"),
        "the gate names the cause and the attempt: {p2}"
    );
    assert!(
        !p2.contains("different CLI"),
        "no reassign lever on a launch refusal: {p2}"
    );
    assert!(
        p2.contains("Another CLI meets the same environment"),
        "{p2}"
    );
    // The retry is still a decision: approving it dispatches a third attempt.
    core.confirm_gate(sid, approve()).expect("approve again");
    let third = collect_until(&ev, Duration::from_secs(20), paused_or_failed(sid));
    gate_prompt(&third, sid);
    assert_eq!(runner.dispatches.load(Ordering::SeqCst), 3);
}

/// core#718 — a retried attempt whose launch failed for a SEAT-SPECIFIC reason must not claim
/// "another CLI meets the same environment". Here claude's ACP carrier was down and claude's
/// wrapped carrier cannot be governed on this host (org settings allow only managed hooks): that
/// refusal is claude's alone — opencode runs on its own carrier. The gate offers the reassign by
/// name instead of withholding it, and its prompt is not the launch-refusal sentence the studio
/// hides the reassign lever for (studio#315).
#[test]
fn a_seat_specific_launch_failure_offers_reassignment_to_the_other_seats() {
    const SEAT_REFUSAL: &str = "[wicked-core] ACP unavailable for 'claude' (remote managed \
        settings could not be loaded); using single-shot fallback\n(input governance refused the \
        launch: claude can't be governed on the wrapped path on this machine: org settings allow \
        only managed hooks (`allowManagedHooksOnly: true` in /etc/claude-code/managed-settings.json); \
        ACP is unavailable — refusing the governed unit rather than running it ungoverned)";
    let sid = "refusal-seat-specific";
    let runner = Arc::new(RefusingRunner {
        later: SEAT_REFUSAL,
        dispatches: AtomicU32::new(0),
    });
    let core = Core::spawn_with_engine(db_path("seat"), Arc::new(NoBallots), runner.clone());
    core.register_workflow(ONE_BUILD).expect("register def");
    let ev = core.subscribe();
    core.launch_run(spec(sid)).expect("launch");

    let first = collect_until(&ev, Duration::from_secs(20), paused_or_failed(sid));
    gate_prompt(&first, sid);
    core.confirm_gate(sid, approve())
        .expect("approve the retry");

    let second = collect_until(&ev, Duration::from_secs(20), paused_or_failed(sid));
    let p2 = gate_prompt(&second, sid);
    assert!(
        !p2.contains("Another CLI meets the same environment"),
        "a seat-specific failure must not claim every seat fails the same way: {p2}"
    );
    assert!(
        p2.contains("specific to seat 'claude'") && p2.contains("reassign the unit to 'opencode'"),
        "the gate names the seat and offers the other eligible seat: {p2}"
    );
    assert!(
        !p2.contains("before its work was judged"),
        "not the launch-refusal sentence the studio hides the reassign lever for: {p2}"
    );
    assert!(p2.contains("on attempt 2"), "{p2}");
}
