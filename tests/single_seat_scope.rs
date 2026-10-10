//! core#747 — a run restricted to ONE seat ran its `pa-scope` unit and then died with the bare
//! message "the PA-scoped plan could not be decided": nothing told the operator what to change.
//!
//! Through the REAL engine (`Core` actor → launch → the PA's scope answer → the scoped plan's
//! decision → distribution): a `feature` launch on a one-seat roster still fails at the scope
//! step's boundary (distribution never leaves a team run's review/test step on its creator's
//! seat — DES-TEAMING-002 §8.1 D1, a decision this test does not revisit), but the failure now
//! NAMES the cause and the remedy, and the scope unit is the only unit that ran.
use std::sync::Arc;
use std::time::{Duration, Instant};

use wicked_core::{
    BusDb, Core, CoreEvent, EntityMode, HumanConfirm, LaunchSpec, StepInput, StepOutput,
    StepRunner, StepStatus, TeamConfig, TEAM_OUTBOX_FILE,
};
use wicked_council::types::{Category, Confidence, Dispatcher, InputMode, Vote};
use wicked_council::{AgenticCli, CouncilTask};

/// Generous: a loaded CI host (and Windows) must never flake on a slow actor.
const DEADLINE: Duration = Duration::from_secs(90);

/// Pre-main: arm the hermetic emit spool (core#311), so nothing this binary trips — the run's
/// gate transitions, its failure — spools to the operator's real replay queue. Required of every
/// test binary by `tests/harness_hygiene.rs`.
///
/// SAFETY (`ctor(unsafe)`): runs before `main` on one thread and only sets one process env var
/// via the std API — no allocator setup, no threads, no panics across the FFI boundary.
#[ctor::ctor(unsafe)]
fn arm_hermetic_emit_spool() {
    wicked_apps_core::emit::hermetic_test_spool();
}

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

/// The one seat's worker: the PA's read-only `pa-scope` step rates the risk (the run has no
/// repo, so the `RISK` grammar applies); any other unit is recorded by id so the test can prove
/// nothing else ran.
struct ScopeOnly {
    ran: Arc<std::sync::Mutex<Vec<String>>>,
}
impl StepRunner for ScopeOnly {
    fn run_unit(&self, input: &StepInput) -> StepOutput {
        self.ran.lock().unwrap().push(input.unit.id.clone());
        StepOutput {
            run_id: input.run_id.clone(),
            unit_ix: input.unit_ix,
            attempt: input.attempt,
            output: "RISK {\"score\":30,\"reasons\":[\"a demo feature\"]}".into(),
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

fn tmp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "wicked-core-single-seat-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn a_feature_launch_on_one_seat_fails_at_the_scope_boundary_naming_the_cause() {
    let dir = tmp_dir("feature");
    let db = dir.join("estate.db").to_str().unwrap().to_string();
    let bus = dir.join("bus.db").to_string_lossy().into_owned();
    BusDb::shared(&bus).expect("bus db");
    let ran: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
    let core = Core::spawn_with_engine_team(
        db,
        Arc::new(StubDispatcher),
        Arc::new(ScopeOnly { ran: ran.clone() }),
        TeamConfig::new(None, Some(dir.join(TEAM_OUTBOX_FILE)))
            .with_final_pass_budget(Duration::from_millis(300)),
    );
    let rx = core.subscribe();
    let run = "one-seat".to_string();
    core.launch_run(LaunchSpec {
        problem: "stage a stalled run for a demo chapter".into(),
        clis: vec![cli("solo")],
        entity_mode: EntityMode::Shared,
        session_id: run.clone(),
        human_confirm: HumanConfirm::None,
        auto_deliver: false,
        repo_ref: None,
        base_ref: None,
        workflow: Some("feature".into()),
        project_id: None,
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
        qe_acceptance: Default::default(),
    })
    .expect("a one-seat launch is accepted: its scope step can run on the seat");

    let end = Instant::now() + DEADLINE;
    let mut seen: Vec<CoreEvent> = Vec::new();
    let mut failed = false;
    while !failed {
        if Instant::now() > end {
            panic!(
                "timed out waiting for the run to fail; saw {:#?}",
                seen.iter()
                    .map(|e| e.to_json()["type"].clone())
                    .collect::<Vec<_>>()
            );
        }
        if let Ok(ev) = rx.recv_timeout(Duration::from_millis(50)) {
            failed = matches!(&ev, CoreEvent::SessionFailed { session, .. } if session == &run);
            seen.push(ev);
        }
    }
    let errors: Vec<&str> = seen
        .iter()
        .filter_map(|e| match e {
            CoreEvent::Error { session, message } if session.as_deref() == Some(&run) => {
                Some(message.as_str())
            }
            _ => None,
        })
        .collect();
    let scoped = errors
        .iter()
        .find(|m| m.starts_with("the PA-scoped plan could not be decided"))
        .unwrap_or_else(|| panic!("no scope-boundary error; errors: {errors:?}"));
    // The cause rides the message (core#747: it used to be the bare prefix), and it names the
    // remedy — a second seat — not just that something "could not be decided".
    assert!(
        scoped.len() > "the PA-scoped plan could not be decided".len() + 2,
        "the message carries its cause: {scoped}"
    );
    assert!(
        scoped.contains("evaluator\u{2260}creator") && scoped.contains("seat"),
        "the cause names the separation and the seat it needs: {scoped}"
    );
    assert!(
        scoped.contains("add a signed-in"),
        "the cause names the remedy: {scoped}"
    );
    // Only the scope step ran on the seat: no creator unit was dispatched.
    let ran = ran.lock().unwrap().clone();
    assert_eq!(ran.len(), 1, "one unit ran: {ran:?}");
    assert!(ran[0].ends_with(":pa-scope"), "{ran:?}");
}
