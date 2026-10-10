//! A seat benched mid-run is never handed another unit of that run, and the bench is on the wire.
//!
//! crew 0.7.45's release smoke (wicked-ci S04, run 36946090323): core#590 S5 removed the
//! per-phase ballots that used to find a dead seat BEFORE routing. Distribution now seats every
//! unit at plan time, so a seat that looks signed in but cannot work (copilot, out of quota) was
//! handed both review units. Its first refusal benched it for the run (`source: "worker"`), the
//! unit failed over to a live seat, and then the SECOND review unit was dispatched to the same
//! benched seat anyway: one more dead turn, one more failover. Nothing on the wire said the seat
//! had been benched, so the launcher handed it work again on the next launch too.
//!
//! These tests drive the real engine (`Core::launch_run` → plan → distribute → dispatch) with a
//! scripted runner:
//!  * the benched seat is dispatched exactly once (the turn that found it dead), the later unit it
//!    held is re-seated on a live seat before it runs (`unitReassigned`), and the run completes;
//!  * `seatBenched {cli, reason, source}` is emitted once, when the bench is added;
//!  * with no live seat left, the run still pauses at the dead-seat escalation gate with a plain
//!    prompt; it never hangs and never ends silently.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use wicked_core::{
    Core, CoreEvent, EntityMode, HumanConfirm, LaunchSpec, StepInput, StepOutput, StepRunner,
    StepStatus,
};
use wicked_council::types::{Category, Confidence, Dispatcher, InputMode, Vote};
use wicked_council::{AgenticCli, CouncilTask};

/// Pre-main: arm the hermetic emit spool (core#311) so nothing this suite emits reaches the
/// operator's real replay queue. SAFETY (`ctor(unsafe)`): runs before `main` on one thread and
/// only sets process env vars via the std API.
#[ctor::ctor(unsafe)]
fn arm_hermetic_emit_spool() {
    wicked_apps_core::emit::hermetic_test_spool();
}

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

/// No ballot routes a unit since core#590 S5; any dispatch here would be a regression.
struct NoBallots;
impl Dispatcher for NoBallots {
    fn dispatch(&self, _c: &AgenticCli, _: &CouncilTask) -> Option<Vote> {
        None
    }
}

/// The dead seat: signed in as far as any probe can tell, but every turn ends on the quota
/// refusal, in the wrapped runner's own exit frame (the smoke's copilot shim says exactly this).
const DEAD: &str = "copilot";
const QUOTA_EXIT: &str = "(cli `copilot` exited 1) You've exceeded your monthly quota for premium \
                          requests. Upgrade your plan or wait for the quota to reset.";

/// Records which seat every unit dispatch landed on. `copilot` refuses on quota; every other seat
/// answers, and an evaluator's answer ends `VERDICT: PASS` (core#498's convention) so the review
/// gates pass.
struct Scripted {
    seats: Mutex<Vec<(u32, String)>>,
    dead_calls: AtomicU32,
}
impl Scripted {
    fn new() -> Self {
        Scripted {
            seats: Mutex::new(Vec::new()),
            dead_calls: AtomicU32::new(0),
        }
    }
}
impl StepRunner for Scripted {
    fn run_unit(&self, i: &StepInput) -> StepOutput {
        let seat = i
            .unit
            .assigned_cli
            .clone()
            .unwrap_or_else(|| "claude".into());
        self.seats.lock().unwrap().push((i.unit.ord, seat.clone()));
        let (output, status) = if seat == DEAD {
            self.dead_calls.fetch_add(1, Ordering::SeqCst);
            (QUOTA_EXIT.to_string(), StepStatus::Failed)
        } else {
            (
                "Checked the work; it does what the unit asked.\nVERDICT: PASS".to_string(),
                StepStatus::Ok,
            )
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

/// build → two reviews of it. Distribution puts the build on the first seat and moves BOTH
/// reviews off that builder seat onto the next eligible one (`evaluator_distinct`): the dead seat
/// when it is second in the roster — the smoke's exact shape (units 2 and 4 on copilot).
const BUILD_TWO_REVIEWS: &str = r#"{"id":"benched-seat-two-reviews","phases":[
  {"id":"build","kind":"build","gate":"auto"},
  {"id":"review","kind":"review","gate":"auto","depends_on":["build"]},
  {"id":"verify","kind":"review","gate":"auto","depends_on":["build"]}]}"#;

/// An ATTENDED launch (a human is present, so a dead-seat exit with no seat left may pause) with
/// no gate ord that matches, so nothing pauses before the work.
fn launch(sid: &str, clis: Vec<AgenticCli>) -> LaunchSpec {
    LaunchSpec {
        base_ref: None,
        project_id: None,
        problem: "Fix the thing, then check it twice.".into(),
        clis,
        entity_mode: EntityMode::Shared,
        session_id: sid.into(),
        human_confirm: HumanConfirm::Before(99),
        auto_deliver: false,
        repo_ref: None,
        workflow: Some("benched-seat-two-reviews".into()),
        extra_write_roots: Vec::new(),
        extra_read_roots: Vec::new(),
        project_graph: None,
        plan: None,
        deliver_step: None,
        exclude_seats: Vec::new(),
        evidence_root: None,
        primary: None,
        reduced_assurance: false,
        deliverables: Vec::new(),
    }
}

fn engine(runner: Arc<Scripted>) -> Core {
    let core = Core::spawn_with_engine(":memory:".to_string(), Arc::new(NoBallots), runner);
    core.register_workflow(BUILD_TWO_REVIEWS)
        .expect("register the build → two reviews def");
    core
}

/// Collect frames for `sid` until it completes, fails or pauses for a human (or 15 s pass).
fn drain(events: &std::sync::mpsc::Receiver<CoreEvent>, sid: &str) -> Vec<CoreEvent> {
    let mut out = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or(Duration::ZERO);
        if remaining.is_zero() {
            break;
        }
        match events.recv_timeout(remaining.min(Duration::from_millis(200))) {
            Ok(ev) => {
                let stop = matches!(&ev,
                    CoreEvent::SessionCompleted { session: s, .. }
                    | CoreEvent::SessionFailed { session: s, .. }
                    | CoreEvent::AwaitingHuman { session: s, .. } if s == sid);
                out.push(ev);
                if stop {
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
        }
    }
    out
}

/// The seat each unit was planned on, from the wire (`unitDistributed`), by ord.
fn planned(evs: &[CoreEvent], sid: &str) -> Vec<(u32, String)> {
    evs.iter()
        .filter_map(|e| match e {
            CoreEvent::UnitDistributed {
                session, ord, cli, ..
            } if session == sid => Some((*ord, cli.clone())),
            _ => None,
        })
        .collect()
}

/// The smoke's shape. The dead seat holds BOTH review units at plan time; its first refusal
/// benches it; the second review never reaches it — it is re-seated on the live non-builder seat
/// before it runs, the move is on the wire, and the run completes.
#[test]
fn a_seat_benched_mid_run_is_never_dispatched_again_and_its_later_unit_is_re_seated() {
    let sid = "benched-seat-reseat";
    let runner = Arc::new(Scripted::new());
    let core = engine(runner.clone());
    let ev = core.subscribe();
    core.launch_run(launch(sid, vec![cli("claude"), cli(DEAD), cli("opencode")]))
        .expect("launch");
    let evs = drain(&ev, sid);

    // The precondition the bug needs: both reviews were planned on the dead seat.
    let plan = planned(&evs, sid);
    assert_eq!(
        plan.iter()
            .filter(|(_, c)| c == DEAD)
            .map(|(o, _)| *o)
            .collect::<Vec<_>>(),
        vec![2, 3],
        "both reviews planned on the dead seat (evaluator_distinct): {plan:?}"
    );

    assert_eq!(
        runner.dead_calls.load(Ordering::SeqCst),
        1,
        "the benched seat is dispatched exactly once — the turn that found it dead; dispatches: {:?}",
        runner.seats.lock().unwrap()
    );
    let bench_frames: Vec<(u32, String, String, String)> = evs
        .iter()
        .filter_map(|e| match e {
            CoreEvent::SeatBenched {
                session,
                ord,
                cli,
                reason,
                source,
            } if session == sid => Some((*ord, cli.clone(), reason.clone(), source.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        bench_frames,
        vec![(
            2,
            DEAD.to_string(),
            "quota_exhausted (no success in the run)".to_string(),
            "worker".to_string()
        )],
        "one seatBenched frame, when the bench is added, naming the seat, its reason and the \
         source: {evs:?}"
    );
    assert!(
        evs.iter().any(|e| matches!(e,
            CoreEvent::UnitReassigned { session, ord: 3, previous_cli, new_cli: Some(n), .. }
                if session == sid && previous_cli == DEAD && n == "opencode")),
        "unit 3 is re-seated off the benched seat onto the live non-builder seat, on the wire: \
         {evs:?}"
    );
    assert!(
        evs.iter()
            .any(|e| matches!(e, CoreEvent::SessionCompleted { session } if session == sid)),
        "the run completes on the live seats: {evs:?}"
    );
    let dispatches = runner.seats.lock().unwrap().clone();
    assert!(
        dispatches.iter().any(|(o, s)| *o == 3 && s == "opencode"),
        "unit 3 ran on opencode: {dispatches:?}"
    );
}

/// No live seat is left for the reviews (the only other seat built the work, so evaluator ≠
/// creator excludes it): the dead seat's refusal still ends at the plain dead-seat escalation
/// gate. The run pauses with a prompt that names the seat and the lever; it does not hang and does
/// not fail silently.
#[test]
fn with_no_live_seat_left_the_run_pauses_plainly_at_the_dead_seat_gate() {
    let sid = "benched-seat-all-dead";
    let runner = Arc::new(Scripted::new());
    let core = engine(runner.clone());
    let ev = core.subscribe();
    core.launch_run(launch(sid, vec![cli("claude"), cli(DEAD)]))
        .expect("launch");
    let evs = drain(&ev, sid);

    let prompt = evs
        .iter()
        .find_map(|e| match e {
            CoreEvent::AwaitingHuman {
                session,
                gate_kind,
                prompt,
                ..
            } if session == sid && gate_kind == "escalation" => Some(prompt.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("the run must PAUSE at the escalation gate: {evs:?}"));
    assert!(
        prompt.contains("failed on a dead seat")
            && prompt.contains("quota_exhausted")
            && prompt.contains("Reassign the unit"),
        "the prompt names the dead seat, its class and the lever: {prompt}"
    );
    assert!(
        evs.iter().any(|e| matches!(e,
            CoreEvent::SeatBenched { session, cli, .. } if session == sid && cli == DEAD)),
        "the bench is on the wire even when nothing is left to re-seat onto: {evs:?}"
    );
    assert!(
        !evs.iter()
            .any(|e| matches!(e, CoreEvent::SessionFailed { session, .. } if session == sid)),
        "no silent failure; the operator decides: {evs:?}"
    );
    assert_eq!(
        runner.dead_calls.load(Ordering::SeqCst),
        1,
        "the dead seat ran once, the turn that found it dead"
    );
}

/// build → review → verify, where `verify` checks BOTH earlier units. Distribution puts the build
/// on the first seat and both reviews on the dead seat. The dead seat's refusal on `review` fails
/// that unit over to the third seat — which makes the third seat a creator of what `verify`
/// checks. When the cursor reaches `verify`, every eligible seat built part of its input.
const VERIFY_CHECKS_BOTH: &str = r#"{"id":"benched-seat-verify-both","phases":[
  {"id":"build","kind":"build","gate":"auto"},
  {"id":"review","kind":"review","gate":"auto","depends_on":["build"]},
  {"id":"verify","kind":"review","gate":"auto","depends_on":["build","review"]}]}"#;

/// A later unit planned on a benched seat with NO eligible seat left to take it is not dispatched
/// to the benched seat: the run pauses on it at the dead-seat gate, with a prompt that says it was
/// never seated and names the levers (sign in, reassign, reject). The dead seat ran exactly once.
#[test]
fn a_later_unit_with_no_eligible_seat_left_pauses_instead_of_reaching_the_benched_seat() {
    let sid = "benched-seat-no-reseat";
    let runner = Arc::new(Scripted::new());
    let core = Core::spawn_with_engine(":memory:".to_string(), Arc::new(NoBallots), runner.clone());
    core.register_workflow(VERIFY_CHECKS_BOTH)
        .expect("register the build → review → verify def");
    let ev = core.subscribe();
    let mut spec = launch(sid, vec![cli("claude"), cli(DEAD), cli("opencode")]);
    spec.workflow = Some("benched-seat-verify-both".into());
    core.launch_run(spec).expect("launch");
    let evs = drain(&ev, sid);

    let plan = planned(&evs, sid);
    assert_eq!(
        plan.iter()
            .filter(|(_, c)| c == DEAD)
            .map(|(o, _)| *o)
            .collect::<Vec<_>>(),
        vec![2, 3],
        "both reviews planned on the dead seat: {plan:?}"
    );
    let (gate_kind, prompt) = evs
        .iter()
        .find_map(|e| match e {
            CoreEvent::AwaitingHuman {
                session,
                ord: 3,
                gate_kind,
                prompt,
                ..
            } if session == sid => Some((gate_kind.clone(), prompt.clone())),
            _ => None,
        })
        .unwrap_or_else(|| panic!("the run must PAUSE on unit 3: {evs:?}"));
    assert_eq!(gate_kind, "escalation");
    assert!(
        prompt.contains("Unit 3 was never seated")
            && prompt.contains(&format!("planned on '{DEAD}'"))
            && prompt.contains("reassign"),
        "the prompt says the unit was never seated, why, and the levers: {prompt}"
    );
    assert!(
        evs.iter().any(|e| matches!(e,
            CoreEvent::GateEscalated { session, ord: 3, condition, .. }
                if session == sid && condition == "dead_seat")),
        "the pause is the dead-seat escalation: {evs:?}"
    );
    assert_eq!(
        runner.dead_calls.load(Ordering::SeqCst),
        1,
        "the benched seat is never handed unit 3; dispatches: {:?}",
        runner.seats.lock().unwrap()
    );
    assert!(
        !evs.iter()
            .any(|e| matches!(e, CoreEvent::SessionFailed { session, .. } if session == sid)),
        "no silent failure: {evs:?}"
    );
}
