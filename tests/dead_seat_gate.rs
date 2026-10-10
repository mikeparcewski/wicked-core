//! D-10 / core#473-M1 (F-RC2-007) — every seat benched at distribution parks the run at the
//! cursor's `dead_seat` escalation gate instead of `sessionFailed`.
//!
//! "Every seat on my roster was out of quota or signed out and the run died in 2 s with
//! `sessionFailed` — no gate, nothing to approve after I signed a seat in." These tests go through
//! the REAL engine (`Core::launch_run` → plan → distribute → the `PlanFailed` arm). Since core#590
//! S5 distribution convenes no council, so no ballot benches a seat; the typed `NoEligibleSeat`
//! reaches the actor from the bench that remains: a build→review plan on a roster whose only seat
//! distinct from the builder was benched by the launcher's health probe, which makes
//! evaluator ≠ creator unsatisfiable (core#560 — refused, never a creator-seat review).

use std::sync::Arc;
use std::time::{Duration, Instant};

use wicked_core::{
    get_session, session_units, Core, CoreEvent, EntityMode, HumanConfirm, HumanDecision,
    LaunchSpec, SessionStatus, StepInput, StepOutput, StepRunner, StepStatus, UnitStatus,
};
use wicked_council::types::{Category, Confidence, Dispatcher, InputMode, SeatHealth, Vote};
use wicked_council::{AgenticCli, CouncilTask};

/// Pre-main: arm the hermetic emit spool (core#311) so nothing this suite emits reaches the
/// operator's real replay queue. SAFETY (`ctor(unsafe)`): runs before `main` on one thread and
/// only sets process env vars via the std API.
#[ctor::ctor(unsafe)]
fn arm_hermetic_emit_spool() {
    wicked_apps_core::emit::hermetic_test_spool();
}

fn db_path(name: &str) -> String {
    let dir = std::env::temp_dir().join(format!(
        "wicked-core-deadseat-{name}-{}",
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

/// No ballot is dispatched to route a unit (core#590 S5); counts any that is, so a test can prove
/// the dead-seat path convened nothing.
struct NoBallots;
static BALLOTS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
impl Dispatcher for NoBallots {
    fn dispatch(&self, _c: &AgenticCli, _: &CouncilTask) -> Option<Vote> {
        BALLOTS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        None
    }
}

/// Completes every agent unit immediately with Ok status.
struct OkRunner;
impl StepRunner for OkRunner {
    fn run_unit(&self, i: &StepInput) -> StepOutput {
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

/// The 2-phase build→review def: the review must run on a seat that did not build.
const BUILD_REVIEW: &str = r#"{"id":"deadseat-build-review","phases":[
  {"id":"build","kind":"build","gate":"auto"},
  {"id":"review","kind":"review","gate":"auto","depends_on":["build"]}]}"#;

/// `codex` is usable; `claude` — the only seat distinct from the builder — was found signed out
/// by the launcher's health probe. The intake admits the run (one seat is usable); distribution
/// routes the build to `codex` and then cannot seat the review anywhere but its creator.
fn spec(sid: &str) -> LaunchSpec {
    let mut claude = cli("claude");
    claude.health = Some(SeatHealth::unusable("signed out"));
    LaunchSpec {
        base_ref: None,
        project_id: None,
        problem: "Build the thing.".into(),
        clis: vec![cli("codex"), claude],
        entity_mode: EntityMode::Shared,
        session_id: sid.into(),
        human_confirm: HumanConfirm::None,
        auto_deliver: false,
        repo_ref: None,
        workflow: Some("deadseat-build-review".into()),
        extra_write_roots: Vec::new(),
        extra_read_roots: Vec::new(),
        project_graph: None,
        plan: None,
        deliver_step: None,
        exclude_seats: Vec::new(),
        evidence_root: None,
        primary: None,
        reduced_assurance: false,
    }
}

fn engine(db: String, runner: Arc<dyn StepRunner>) -> Core {
    let core = Core::spawn_with_engine(db, Arc::new(NoBallots), runner);
    core.register_workflow(BUILD_REVIEW)
        .expect("register the build→review def");
    core
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

fn no_session_failed(evs: &[CoreEvent], sid: &str) {
    assert!(
        !evs.iter()
            .any(|e| matches!(e, CoreEvent::SessionFailed { session, .. } if session == sid)),
        "no denial reaches sessionFailed without a decided gate: {evs:?}"
    );
}

/// Launch → the bench leaves the review no distinct seat → the typed refusal →
/// `gateEscalated{condition: dead_seat, attempt: 0, defGate: false, outputCaptured: false}` then
/// `awaitingHuman{gateKind: escalation}` on the cursor; the store holds `AwaitingHuman`, the bench
/// (the launcher's), every unit `Pending` and provisionally seated on the roster's first seat; 0
/// ballots; 0 `sessionFailed`. Reject → `runCancelled`.
#[test]
fn an_all_benched_distribution_parks_at_the_dead_seat_gate_and_reject_cancels() {
    let sid = "deadseat-reject";
    let db = db_path("reject");
    let core = engine(db.clone(), Arc::new(OkRunner));
    let ev = core.subscribe();
    core.launch_run(spec(sid))
        .expect("launch (one usable seat: the intake admits it)");

    let evs = collect_until(
        &ev,
        Duration::from_secs(15),
        |e| matches!(e, CoreEvent::AwaitingHuman { session, .. } if session == sid),
    );
    no_session_failed(&evs, sid);
    let escalated = evs
        .iter()
        .position(|e| matches!(e, CoreEvent::GateEscalated { session, .. } if session == sid))
        .unwrap_or_else(|| panic!("gateEscalated on the wire: {evs:?}"));
    let paused = evs
        .iter()
        .position(|e| matches!(e, CoreEvent::AwaitingHuman { session, .. } if session == sid))
        .unwrap_or_else(|| panic!("awaitingHuman on the wire: {evs:?}"));
    assert!(
        escalated < paused,
        "gateEscalated precedes awaitingHuman: {evs:?}"
    );
    match &evs[escalated] {
        CoreEvent::GateEscalated {
            ord,
            condition,
            verdict_summary,
            attempt,
            denial_source,
            def_gate,
            output_captured,
            ..
        } => {
            assert_eq!(*ord, 1, "the cursor unit");
            assert_eq!(condition, "dead_seat");
            assert_eq!(denial_source, "dead_seat");
            assert_eq!(*attempt, 0, "never ran");
            assert!(!def_gate && !output_captured);
            assert!(
                verdict_summary.contains(&format!("no eligible seat for {sid}"))
                    && verdict_summary
                        .contains("evaluator\u{2260}creator unsatisfiable for unit(s) [2]")
                    && verdict_summary.contains("1 of 2 seats benched")
                    && verdict_summary.contains("claude (signed out — launcher)")
                    && verdict_summary.contains("provisionally seated on 'codex'"),
                "{verdict_summary}"
            );
        }
        other => unreachable!("{other:?}"),
    }
    match &evs[paused] {
        CoreEvent::AwaitingHuman {
            ord,
            reviewing_ord,
            gate_kind,
            prompt,
            ..
        } => {
            assert_eq!((*ord, *reviewing_ord), (1, Some(1)));
            assert_eq!(gate_kind, "escalation");
            // DES §7 (11): under run-level `human_confirm: none` the prompt discloses why the run
            // paused anyway (the core#464 note).
            assert!(
                prompt.contains("engine gate: a denied unit pauses for a decision"),
                "the human_confirm:none note rides the prompt: {prompt}"
            );
        }
        other => unreachable!("{other:?}"),
    }
    // DES §7 (13): the dead-seat gate's summary carries no home prefix (core#466).
    if let CoreEvent::GateEscalated {
        verdict_summary, ..
    } = &evs[escalated]
    {
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .unwrap_or_default();
        assert!(
            home.is_empty() || !verdict_summary.contains(&home),
            "no home prefix in the summary: {verdict_summary}"
        );
    }
    assert!(
        !evs.iter()
            .any(|e| matches!(e, CoreEvent::UnitDistributed { session, .. } if session == sid)),
        "nothing was seated: {evs:?}"
    );
    assert_eq!(
        BALLOTS.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "no ballot is dispatched on the way to the gate"
    );

    // The store: parked, bench persisted, units Pending on the provisional seat.
    {
        let store = wicked_apps_core::open_store_ro(Some(&db)).expect("read-only store");
        let session = get_session(&store, sid).unwrap().expect("session");
        assert_eq!(session.status, SessionStatus::AwaitingHuman);
        let benched: Vec<(&str, &str)> = session
            .benched_seats
            .iter()
            .map(|b| (b.cli.as_str(), b.source.as_str()))
            .collect();
        assert_eq!(
            benched,
            vec![("claude", "launcher")],
            "the launcher's bench, persisted"
        );
        let units = session_units(&store, sid).unwrap();
        assert!(!units.is_empty());
        for u in &units {
            assert_eq!(u.status, UnitStatus::Pending, "never ran: {u:?}");
            assert_eq!(u.assigned_cli.as_deref(), Some("codex"), "provisional seat");
        }
        assert_eq!(
            units[0].denial.as_ref().map(|d| d.source.as_str()),
            Some("dead_seat")
        );
    }

    // Reject → cancelled, still no sessionFailed.
    let status = core
        .confirm_gate(sid, HumanDecision::Reject)
        .expect("reject");
    assert_eq!(status, SessionStatus::Cancelled);
    let post = collect_until(
        &ev,
        Duration::from_secs(5),
        |e| matches!(e, CoreEvent::RunCancelled { session, .. } if session == sid),
    );
    assert!(
        post.iter()
            .any(|e| matches!(e, CoreEvent::RunCancelled { session, .. } if session == sid)),
        "{post:?}"
    );
    no_session_failed(&post, sid);
}

/// Approve at the dead-seat gate → `resumed{ord:1}` → `unitDispatched{attempt: 0}` on the
/// provisional seat (the unit never ran, so no attempt bump); with the stub runner answering, the
/// run completes — the gate was a decision, not a verdict.
#[test]
fn approve_at_the_dead_seat_gate_dispatches_the_cursor_on_the_provisional_seat() {
    let sid = "deadseat-approve";
    let db = db_path("approve");
    let core = engine(db.clone(), Arc::new(OkRunner));
    let ev = core.subscribe();
    // (core#850) The approve releases the PROVISIONAL seating — the review on the builder's seat —
    // which only a `reduced` run may grade on; a full run's fold refuses it
    // (`tests/assurance_contract.rs`). This test is about the dispatch, so it opts in.
    let mut launch = spec(sid);
    launch.reduced_assurance = true;
    core.launch_run(launch).expect("launch");
    let evs = collect_until(
        &ev,
        Duration::from_secs(15),
        |e| matches!(e, CoreEvent::AwaitingHuman { session, .. } if session == sid),
    );
    no_session_failed(&evs, sid);

    let status = core
        .confirm_gate(
            sid,
            HumanDecision::Approve {
                amend: None,
                amend_scope: Default::default(),
            },
        )
        .expect("approve");
    assert_ne!(status, SessionStatus::Failed);
    let post = collect_until(&ev, Duration::from_secs(15), |e| {
        matches!(e, CoreEvent::SessionCompleted { session } if session == sid)
            || matches!(e, CoreEvent::SessionFailed { session, .. } if session == sid)
            || matches!(e, CoreEvent::RunCancelled { session, .. } if session == sid)
    });
    no_session_failed(&post, sid);
    let resumed = post
        .iter()
        .position(|e| matches!(e, CoreEvent::Resumed { session, ord: 1 } if session == sid))
        .unwrap_or_else(|| panic!("resumed{{ord:1}}: {post:?}"));
    let dispatched = post
        .iter()
        .position(|e| {
            matches!(e, CoreEvent::UnitDispatched { session, ord: 1, attempt: 0, .. } if session == sid)
        })
        .unwrap_or_else(|| panic!("unitDispatched{{ord:1, attempt:0}}: {post:?}"));
    assert!(resumed < dispatched, "{post:?}");
    assert!(
        post.iter()
            .any(|e| matches!(e, CoreEvent::SessionCompleted { session } if session == sid)),
        "the stub runner answers on the provisional seat and the run completes: {post:?}"
    );
    // core#412: the stub seat reports no usage, so its burn is disclosed UNREPORTED, not $0.
    assert!(
        post.iter().any(|e| matches!(e,
            CoreEvent::CliUsageUnreported { session, ord: 1, attempt: 0, cli }
                if session == sid && cli == "codex")),
        "{post:?}"
    );
    assert!(
        !post
            .iter()
            .any(|e| matches!(e, CoreEvent::CliUsage { session, .. } if session == sid)),
        "no usage frame for a seat that reported none"
    );
    let store = wicked_apps_core::open_store_ro(Some(&db)).expect("read-only store");
    let units = session_units(&store, sid).unwrap();
    assert_eq!(units[0].assigned_cli.as_deref(), Some("codex"));
}

/// Records the seat of every unit it is handed, and completes it.
struct SeatLog(std::sync::Mutex<Vec<(u32, u32, Option<String>)>>);
impl StepRunner for SeatLog {
    fn run_unit(&self, i: &StepInput) -> StepOutput {
        self.0
            .lock()
            .unwrap()
            .push((i.unit.ord, i.attempt, i.unit.assigned_cli.clone()));
        OkRunner.run_unit(i)
    }
}

/// core#773: reassign on a run PARKED at a gate re-seats the cursor unit in place — no dispatch,
/// no attempt bump, `unitReassigned{previousAttemptReaped: true}` — and the gate's approve then
/// dispatches it ONCE, on the new seat (approve-then-reassign used to dispatch a phantom attempt
/// on the old seat first). A re-route (`None`) is refused while parked.
#[test]
fn a_reassign_at_a_gate_reseats_in_place_and_the_approve_dispatches_once_there() {
    let sid = "deadseat-reassign";
    let db = db_path("reassign");
    let log = Arc::new(SeatLog(std::sync::Mutex::new(Vec::new())));
    let core = engine(db.clone(), log.clone());
    let ev = core.subscribe();
    core.launch_run(spec(sid)).expect("launch");
    let _ = collect_until(
        &ev,
        Duration::from_secs(15),
        |e| matches!(e, CoreEvent::AwaitingHuman { session, .. } if session == sid),
    );
    assert!(
        core.reassign_unit(sid, 1, None).is_err(),
        "a re-route needs a running unit"
    );
    assert!(
        core.reassign_unit(sid, 2, Some("claude".into())).is_err(),
        "only the cursor unit"
    );
    core.reassign_unit(sid, 1, Some("claude".into()))
        .expect("re-seat the parked cursor unit");
    let parked = collect_until(
        &ev,
        Duration::from_secs(5),
        |e| matches!(e, CoreEvent::UnitReassigned { session, .. } if session == sid),
    );
    assert!(
        parked.iter().any(|e| matches!(e,
            CoreEvent::UnitReassigned {
                session, ord: 1, attempt: 0, previous_cli, new_cli, previous_attempt_reaped: true, ..
            } if session == sid && previous_cli == "codex" && new_cli.as_deref() == Some("claude"))),
        "{parked:?}"
    );
    assert!(
        !parked
            .iter()
            .any(|e| matches!(e, CoreEvent::UnitDispatched { session, .. } if session == sid)),
        "nothing dispatched while parked: {parked:?}"
    );
    {
        let store = wicked_apps_core::open_store_ro(Some(&db)).expect("read-only store");
        assert_eq!(
            get_session(&store, sid).unwrap().unwrap().status,
            SessionStatus::AwaitingHuman
        );
    }
    core.confirm_gate(
        sid,
        HumanDecision::Approve {
            amend: None,
            amend_scope: Default::default(),
        },
    )
    .expect("approve");
    let post = collect_until(
        &ev,
        Duration::from_secs(15),
        |e| matches!(e, CoreEvent::UnitDispatched { session, ord: 1, .. } if session == sid),
    );
    assert!(
        post.iter().any(|e| matches!(e,
            CoreEvent::UnitDispatched { session, ord: 1, attempt: 0, .. } if session == sid)),
        "{post:?}"
    );
    let _ = collect_until(&ev, Duration::from_secs(10), |e| {
        matches!(e, CoreEvent::UnitDispatched { session, ord: 2, .. } if session == sid)
            || matches!(e, CoreEvent::AwaitingHuman { session, .. } if session == sid)
            || matches!(e, CoreEvent::SessionCompleted { session } if session == sid)
    });
    let ran: Vec<_> = log
        .0
        .lock()
        .unwrap()
        .iter()
        .filter(|(o, _, _)| *o == 1)
        .cloned()
        .collect();
    assert_eq!(
        ran,
        vec![(1, 0, Some("claude".to_string()))],
        "unit 1 ran once, on the new seat"
    );
}
