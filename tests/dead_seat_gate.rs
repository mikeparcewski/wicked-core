//! D-10 / core#473-M1 (F-RC2-007) — every seat benched at distribution parks the run at the
//! cursor's `dead_seat` escalation gate instead of `sessionFailed`.
//!
//! "Every seat on my roster was out of quota or signed out and the run died in 2 s with
//! `sessionFailed` — no gate, nothing to approve after I signed a seat in." These tests go through
//! the REAL engine (`Core::launch_run` → plan → distribute → the `PlanFailed` arm) with a stub
//! dispatcher whose every ballot fails `Not logged in`, so both seats are benched by their own
//! ballots and the typed `NoEligibleSeat` reaches the actor.

use std::sync::Arc;
use std::time::{Duration, Instant};

use wicked_core::{
    get_session, session_units, Core, CoreEvent, EntityMode, HumanConfirm, HumanDecision,
    LaunchSpec, SessionStatus, StepInput, StepOutput, StepRunner, StepStatus, UnitStatus,
};
use wicked_council::types::{
    BallotContext, Category, Confidence, DispatchOutcome, Dispatcher, InputMode, SeatFailure,
    SeatFailureKind, Vote,
};
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
        acp: None,
        capabilities: None,
        login_invocation: None,
        health: None,
    }
}

/// Every seat's ballot exits `Not logged in` — the whole roster is dead for the run. Counts the
/// ballots it was asked so a test can prove a council was (re-)convened.
struct AllSignedOut;
static BALLOTS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
impl Dispatcher for AllSignedOut {
    fn dispatch(&self, c: &AgenticCli, _: &CouncilTask) -> Option<Vote> {
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
    fn dispatch_ballot_detailed(
        &self,
        _cli: &AgenticCli,
        _task: &CouncilTask,
        _ctx: &BallotContext,
    ) -> DispatchOutcome {
        BALLOTS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        DispatchOutcome::Failed(
            SeatFailure::new(SeatFailureKind::NonZeroExit, "exit 1")
                .with_output("Not logged in · Please run /login", ""),
        )
    }
}

/// A runner that reports the unit it started and then BLOCKS until the test releases it — so a
/// unit can be reassigned while it is in flight (`ReassignUnit` requires an Executing cursor).
struct GatedRunner {
    started: std::sync::Mutex<std::sync::mpsc::Sender<u32>>,
    release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
}
impl StepRunner for GatedRunner {
    fn run_unit(&self, i: &StepInput) -> StepOutput {
        let _ = self.started.lock().unwrap().send(i.unit.ord);
        let _ = self.release.lock().unwrap().recv();
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

fn spec(sid: &str) -> LaunchSpec {
    LaunchSpec {
        base_ref: None,
        project_id: None,
        problem: "Build the thing.".into(),
        clis: vec![cli("codex"), cli("claude")],
        entity_mode: EntityMode::Shared,
        session_id: sid.into(),
        human_confirm: HumanConfirm::None,
        auto_deliver: false,
        repo_ref: None,
        workflow: None,
        extra_write_roots: Vec::new(),
        extra_read_roots: Vec::new(),
        project_graph: None,
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

fn no_session_failed(evs: &[CoreEvent], sid: &str) {
    assert!(
        !evs.iter()
            .any(|e| matches!(e, CoreEvent::SessionFailed { session, .. } if session == sid)),
        "no denial reaches sessionFailed without a decided gate: {evs:?}"
    );
}

/// Launch → every ballot fails → the typed refusal → `gateEscalated{condition: dead_seat,
/// attempt: 0, defGate: false, outputCaptured: false}` then `awaitingHuman{gateKind: escalation}`
/// on the cursor; the store holds `AwaitingHuman`, the bench (both seats), every unit `Pending`
/// and provisionally seated on the roster's first seat; 0 `sessionFailed`. Reject → `runCancelled`.
#[test]
fn an_all_benched_distribution_parks_at_the_dead_seat_gate_and_reject_cancels() {
    let sid = "deadseat-reject";
    let db = db_path("reject");
    let core = Core::spawn_with_engine(db.clone(), Arc::new(AllSignedOut), Arc::new(OkRunner));
    let ev = core.subscribe();
    core.launch_run(spec(sid))
        .expect("launch (no launcher bench — the ballots decide)");

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
                    && verdict_summary.contains("2 of 2 seats benched")
                    && verdict_summary.contains("codex (not_logged_in — ballot)")
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
        "no council seated anything: {evs:?}"
    );

    // The store: parked, bench persisted, units Pending on the provisional seat.
    {
        let store = wicked_apps_core::open_store_ro(Some(&db)).expect("read-only store");
        let session = get_session(&store, sid).unwrap().expect("session");
        assert_eq!(session.status, SessionStatus::AwaitingHuman);
        let mut benched: Vec<&str> = session
            .benched_seats
            .iter()
            .map(|b| b.cli.as_str())
            .collect();
        benched.sort();
        assert_eq!(
            benched,
            vec!["claude", "codex"],
            "one bench ledger, both seats"
        );
        assert!(session.benched_seats.iter().all(|b| b.source == "ballot"));
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
        |e| matches!(e, CoreEvent::RunCancelled { session } if session == sid),
    );
    assert!(
        post.iter()
            .any(|e| matches!(e, CoreEvent::RunCancelled { session } if session == sid)),
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
    let core = Core::spawn_with_engine(db.clone(), Arc::new(AllSignedOut), Arc::new(OkRunner));
    let ev = core.subscribe();
    core.launch_run(spec(sid)).expect("launch");
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
            || matches!(e, CoreEvent::RunCancelled { session } if session == sid)
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
    let store = wicked_apps_core::open_store_ro(Some(&db)).expect("read-only store");
    let units = session_units(&store, sid).unwrap();
    assert_eq!(units[0].assigned_cli.as_deref(), Some("codex"));
}

/// DES §7 (12) / BC-16: `POST /runs/:id/reassign {cli: null}` on a parked-then-approved run
/// re-convenes the council over a CLEARED bench — the ballots run again (the count rises; with the
/// run's bench still in force `:426` would have refused before any ballot) — and, every seat still
/// dead, the run parks again at the `dead_seat` gate with the bumped attempt; never `sessionFailed`.
#[test]
fn a_cli_null_reassign_re_councils_over_a_cleared_bench_and_parks_again() {
    let sid = "deadseat-reassign";
    let db = db_path("reassign");
    let (started_tx, started_rx) = std::sync::mpsc::channel::<u32>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let core = Core::spawn_with_engine(
        db,
        Arc::new(AllSignedOut),
        Arc::new(GatedRunner {
            started: std::sync::Mutex::new(started_tx),
            release: std::sync::Mutex::new(release_rx),
        }),
    );
    let ev = core.subscribe();
    core.launch_run(spec(sid)).expect("launch");
    let evs = collect_until(
        &ev,
        Duration::from_secs(15),
        |e| matches!(e, CoreEvent::AwaitingHuman { session, .. } if session == sid),
    );
    no_session_failed(&evs, sid);
    let ballots_before = BALLOTS.load(std::sync::atomic::Ordering::SeqCst);

    // Approve → the cursor dispatches on the provisional seat and blocks in the gated runner.
    core.confirm_gate(
        sid,
        HumanDecision::Approve {
            amend: None,
            amend_scope: Default::default(),
        },
    )
    .expect("approve");
    assert_eq!(
        started_rx.recv_timeout(Duration::from_secs(10)).ok(),
        Some(1),
        "the cursor unit dispatched (on the provisional seat) and is in flight"
    );

    // The operator's explicit "try again": re-council over a CLEARED bench.
    core.reassign_unit(sid, 1, None)
        .expect("reassign {cli:null}");
    let post = collect_until(
        &ev,
        Duration::from_secs(15),
        |e| matches!(e, CoreEvent::AwaitingHuman { session, .. } if session == sid),
    );
    no_session_failed(&post, sid);
    assert!(
        BALLOTS.load(std::sync::atomic::Ordering::SeqCst) > ballots_before,
        "the council was re-convened: the ballots ran again (the bench was cleared, not reused)"
    );
    let reassigned = post
        .iter()
        .position(|e| matches!(e, CoreEvent::UnitReassigned { session, ord: 1, new_cli: None, .. } if session == sid))
        .unwrap_or_else(|| panic!("unitReassigned{{newCli: null}}: {post:?}"));
    let gate = post
        .iter()
        .position(|e| matches!(e, CoreEvent::GateEscalated { session, condition, .. } if session == sid && condition == "dead_seat"))
        .unwrap_or_else(|| panic!("a second dead_seat gate: {post:?}"));
    assert!(reassigned < gate, "{post:?}");
    if let CoreEvent::GateEscalated { attempt, .. } = &post[gate] {
        assert!(*attempt >= 1, "the reassign bumped the attempt: {attempt}");
    }
    // Clean up: release the superseded worker and reject the parked run.
    drop(release_tx);
    let _ = core.confirm_gate(sid, HumanDecision::Reject);
}
