//! P10 METHODOLOGY — the recon→build→review→test spine made real at the CLI level. Proves
//! evaluator ≠ creator: a REVIEW-stage unit is reassigned off the builder's CLI so the critic differs
//! from the code it checks.

use std::sync::Arc;
use std::time::{Duration, Instant};

use wicked_core::{
    Core, EntityMode, HumanConfirm, LaunchSpec, SessionStatus, StepInput, StepOutput, StepRunner,
    StepStatus,
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
    // "build" unit + "review" unit (the keyword classifies stage), roster of two seats.
    core.launch_run(LaunchSpec {
        project_id: None,
        problem: "Build the auth feature. Then review it for security".into(),
        clis: vec![cli("a"), cli("b")],
        entity_mode: EntityMode::Shared,
        session_id: "r".into(),
        human_confirm: HumanConfirm::None,
        repo_ref: None,
        workflow: None,
        extra_write_roots: Vec::new(),
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
