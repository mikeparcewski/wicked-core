//! P10 METHODOLOGY — the recon→build→review→test spine made real at the CLI level. Proves
//! evaluator ≠ creator: a REVIEW-stage unit is reassigned off the builder's CLI so the critic differs
//! from the code it checks.

use std::sync::Arc;
use std::time::{Duration, Instant};

use wicked_core::{
    Core, CoreEvent, EntityMode, HumanConfirm, LaunchSpec, SessionStatus, StepInput, StepOutput,
    StepRunner, StepStatus,
};
use wicked_council::types::{Category, Confidence, Dispatcher, InputMode, Vote};
use wicked_council::{AgenticCli, CouncilTask};

/// Always recommends seat "a" — so without the evaluator≠creator pass, BOTH the build and review
/// units would land on "a".
struct FixedDispatcher;
impl Dispatcher for FixedDispatcher {
    fn dispatch(&self, cli: &AgenticCli, _t: &CouncilTask) -> Option<Vote> {
        Some(Vote {
            cli: cli.key.clone(),
            recommendation: "a".into(),
            top_risk: "none".into(),
            change_my_mind: "no".into(),
            disqualifier: None,
            confidence: Confidence::default(),
            provenance: "fixed".into(),
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
        health: None,
    }
}

fn db_path(name: &str) -> String {
    let dir = std::env::temp_dir().join(format!("wicked-core-p10-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("estate.db").to_str().unwrap().to_string()
}

/// How long a completion wait may take before it is called a failure.
///
/// Far above the ~2s an unloaded run needs, because the only thing a longer budget can change is
/// whether a slow-but-correct run under concurrent test load is misreported as a broken one.
const WAIT_BUDGET: Duration = Duration::from_secs(20);

/// Poll until `run_id` completes.
///
/// On timeout it reports the status it actually observed: the caller asserts on a bare `bool`, and
/// "assertion failed" alone cannot distinguish a run that stalled (a real defect) from one that was
/// merely slow under `cargo test --all` load (a harness problem).
fn wait_done(core: &Core, run_id: &str) -> bool {
    let start = Instant::now();
    let mut last: Option<SessionStatus> = None;
    while start.elapsed() < WAIT_BUDGET {
        if let Ok(views) = core.sessions_detail() {
            if let Some(v) = views.iter().find(|s| s.session.id == run_id) {
                if v.session.status == SessionStatus::Completed {
                    return true;
                }
                last = Some(v.session.status);
            }
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    eprintln!(
        "wait_done({run_id}): timed out after {:?} waiting for Completed; last observed {last:?} \
         (None means the run never appeared in sessions_detail at all)",
        start.elapsed()
    );
    false
}

#[test]
fn review_unit_runs_a_distinct_cli_from_the_builder() {
    let core = Core::spawn_with_engine(
        db_path("eval"),
        Arc::new(FixedDispatcher),
        Arc::new(OkRunner),
    );
    // A "build" unit + a "review" unit, roster of two seats. D-11 (core#393): free text plans ONE
    // unit now, so the two stages come from a 2-phase def (`kind` is the stage — data, not a
    // keyword guess); the evaluator≠creator rule under test is unchanged.
    core.register_workflow(
        r#"{"id":"p10-build-review","phases":[
          {"id":"build","kind":"build","gate":"auto"},
          {"id":"review","kind":"review","gate":"auto","depends_on":["build"]}]}"#,
    )
    .expect("register the 2-phase def");
    core.launch_run(LaunchSpec {
        base_ref: None,
        project_id: None,
        problem: "Build the auth feature. Then review it for security".into(),
        clis: vec![cli("a"), cli("b")],
        entity_mode: EntityMode::Shared,
        session_id: "r".into(),
        human_confirm: HumanConfirm::None,
        auto_deliver: false,
        repo_ref: None,
        workflow: Some("p10-build-review".into()),
        extra_write_roots: Vec::new(),
        extra_read_roots: Vec::new(),
        project_graph: None,
    })
    .unwrap();
    assert!(wait_done(&core, "r"), "the run completes");

    let views = core.sessions_detail().unwrap();
    let v = views.iter().find(|v| v.session.id == "r").unwrap();
    assert_eq!(v.units.len(), 2);
    let build = &v.units[0];
    let review = &v.units[1];
    assert_eq!(
        build.assigned_cli.as_deref(),
        Some("a"),
        "the builder is the council's pick (a)"
    );
    assert_eq!(
        review.assigned_cli.as_deref(),
        Some("b"),
        "the REVIEW unit was reassigned off the builder's CLI — evaluator ≠ creator, got: {:?}",
        review.assigned_cli
    );
    assert!(
        matches!(
            &review.routing,
            Some(wicked_core::RoutingInfo::EvaluatorDistinct { .. })
        ),
        "the review unit records the evaluator-distinct routing, got: {:?}",
        review.routing
    );
}

/// Counts every ballot dispatched — a ballot is a council seat's subprocess turn. Every seat
/// votes capability profile 1, so a council (were one convened) agrees in one round.
struct CountingDispatcher {
    calls: Arc<std::sync::atomic::AtomicUsize>,
}
impl Dispatcher for CountingDispatcher {
    fn dispatch(&self, cli: &AgenticCli, _t: &CouncilTask) -> Option<Vote> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Some(Vote {
            cli: cli.key.clone(),
            recommendation: "1 — fit".into(),
            top_risk: "none".into(),
            change_my_mind: "no".into(),
            disqualifier: None,
            confidence: Confidence::default(),
            provenance: "counting".into(),
        })
    }
}

/// core#590 S5 — distribution convenes NO council: a build→review run on a two-seat roster
/// dispatches zero ballots and emits no council event; the builder is routed `teamed` to the
/// first seat and evaluator ≠ creator still moves the review off it. Asserted through the run's
/// own read model and event stream (the wire shape a consumer sees), with fixed values. On the
/// pre-S5 engine this run balloted 4 times (2 seats × 2 units) and routed `council`.
#[test]
fn a_run_convenes_no_council_and_still_separates_evaluator_from_creator() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let core = Core::spawn_with_engine(
        db_path("teamed"),
        Arc::new(CountingDispatcher {
            calls: calls.clone(),
        }),
        Arc::new(OkRunner),
    );
    core.register_workflow(
        r#"{"id":"p10-build-review","phases":[
          {"id":"build","kind":"build","gate":"auto"},
          {"id":"review","kind":"review","gate":"auto","depends_on":["build"]}]}"#,
    )
    .expect("register the 2-phase def");
    let events = core.subscribe();
    core.launch_run(LaunchSpec {
        base_ref: None,
        project_id: None,
        problem: "Build the auth feature. Then review it for security".into(),
        clis: vec![cli("a"), cli("b")],
        entity_mode: EntityMode::Shared,
        session_id: "teamed".into(),
        human_confirm: HumanConfirm::None,
        auto_deliver: false,
        repo_ref: None,
        workflow: Some("p10-build-review".into()),
        extra_write_roots: Vec::new(),
        extra_read_roots: Vec::new(),
        project_graph: None,
    })
    .unwrap();
    assert!(wait_done(&core, "teamed"), "the run completes");
    let seen: Vec<CoreEvent> = events.try_iter().collect();

    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "no council ballot is dispatched to route a unit"
    );
    assert!(
        !seen.iter().any(|e| matches!(
            e,
            CoreEvent::CouncilConvened { .. }
                | CoreEvent::CouncilVoted { .. }
                | CoreEvent::CouncilSeatFailed { .. }
        )),
        "no council event is emitted"
    );
    let v = core
        .sessions_detail()
        .unwrap()
        .into_iter()
        .find(|v| v.session.id == "teamed")
        .unwrap();
    assert_eq!(v.units.len(), 2);
    assert_eq!(v.units[0].assigned_cli.as_deref(), Some("a"));
    assert_eq!(
        serde_json::to_value(&v.units[0].routing).unwrap(),
        serde_json::json!({"method": "teamed", "winner": "a"})
    );
    assert_eq!(v.units[1].assigned_cli.as_deref(), Some("b"));
    assert_eq!(
        serde_json::to_value(&v.units[1].routing).unwrap(),
        serde_json::json!({"method": "evaluator_distinct", "winner": "b", "was": "a"})
    );
    let distributed: Vec<(u32, String, String)> = seen
        .iter()
        .filter_map(|e| match e {
            CoreEvent::UnitDistributed {
                ord,
                cli,
                routing_method,
                ..
            } => Some((*ord, cli.clone(), routing_method.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        distributed,
        vec![
            (1, "a".to_string(), "teamed".to_string()),
            (2, "b".to_string(), "evaluator_distinct".to_string()),
        ]
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
