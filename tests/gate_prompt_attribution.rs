//! FINDING-032: a human-confirm pause must say what the operator is being asked to judge.
//!
//! A DEF-declared `HumanConfirm` gate fires AFTER its phase's work — `should_pause` reads the
//! PRECEDING unit's `GateSpec` — so the artifact under review is that phase's output. The engine
//! nonetheless described every mid-run pause as "Approve unit N before it runs: <N's description>",
//! pointing the operator at a phase that had produced nothing and, in the common case, had declared
//! no gate at all. Observed live on the shipped `feature` workflow, whose gate sits on `clarify`:
//!
//! ```text
//! {"type":"awaitingHuman","ord":2,"prompt":"Approve unit 2 before it runs: design"}
//! ```
//!
//! `packages/studio/src/components/GateNotifications.tsx` renders `prompt` and nothing else, so that
//! string was the operator's entire basis for a governance decision. The event carried no
//! attribution either, which left the evidence packet unable to answer "why did this run pause
//! here?" without re-reading the workflow def and re-deriving `should_pause` by hand.

use std::sync::Arc;
use std::time::{Duration, Instant};

use wicked_core::{
    Core, CoreEvent, EntityMode, HumanConfirm, LaunchSpec, StepInput, StepOutput, StepRunner,
    StepStatus,
};
use wicked_council::types::{Category, Confidence, Dispatcher, InputMode, Vote};
use wicked_council::{AgenticCli, CouncilTask};

/// Upper bound on reaching the gate. Same reasoning as FINDING-029/030: the wait returns the instant
/// the pause lands, so a generous bound costs nothing on an idle host and is only ever spent on a
/// run that is genuinely stuck — and a timeout is reported as a timeout, never as an outcome.
const GATE_DEADLINE: Duration = Duration::from_secs(60);

fn db_path(name: &str) -> String {
    let dir = std::env::temp_dir().join(format!(
        "wicked-core-gateattr-{name}-{}",
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

/// The `AwaitingHuman` for `session`, or an `Err` naming the timeout as a timeout.
fn wait_for_gate(
    events: &std::sync::mpsc::Receiver<CoreEvent>,
    session: &str,
) -> Result<(u32, Option<u32>, String), String> {
    let deadline = Instant::now() + GATE_DEADLINE;
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or(Duration::ZERO);
        if remaining.is_zero() {
            return Err(format!(
                "run {session} never paused for a human within {}s — this is a timeout, not a \
                 governance outcome",
                GATE_DEADLINE.as_secs()
            ));
        }
        match events.recv_timeout(remaining.min(Duration::from_millis(250))) {
            Ok(CoreEvent::AwaitingHuman {
                session: s,
                ord,
                reviewing_ord,
                prompt,
            }) if s == session => return Ok((ord, reviewing_ord, prompt)),
            Ok(CoreEvent::SessionCompleted { session: s })
            | Ok(CoreEvent::SessionFailed { session: s, .. })
            | Ok(CoreEvent::RunCancelled { session: s })
                if s == session =>
            {
                return Err(format!("run {session} went terminal without ever pausing"))
            }
            Ok(_) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err("event stream disconnected".into())
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
        }
    }
}

/// A gate declared on phase 1 pauses before phase 2 — and the pause names PHASE 1's output.
#[test]
fn a_def_gate_names_the_phase_whose_output_is_under_review() {
    // Mirrors the shipped `feature` workflow: the gate sits on the FIRST phase, the second declares
    // `auto`. This is the ordinary case, not a corner one.
    let def_json = serde_json::json!({
        "id": "attr-test",
        "phases": [
            { "id": "clarify", "kind": "recon",
              "gate": { "human_confirm": { "unconditional": false } } },
            { "id": "design", "kind": "recon", "gate": "auto", "depends_on": ["clarify"] }
        ]
    })
    .to_string();

    let core = Core::spawn_with_engine(
        db_path("defgate"),
        Arc::new(NumericDispatcher),
        Arc::new(OkRunner),
    );
    let ev = core.subscribe();
    core.register_workflow(&def_json).unwrap();
    core.launch_run(LaunchSpec {
        project_id: None,
        problem: "Do the thing.".into(),
        clis: vec![cli("stub")],
        entity_mode: EntityMode::Shared,
        session_id: "attr-def".into(),
        // No run-level policy: the DEF's own gate is the sole reason this run pauses.
        human_confirm: HumanConfirm::None,
        repo_ref: None,
        workflow: Some("attr-test".into()),
        extra_write_roots: Vec::new(),
        extra_read_roots: Vec::new(),
        project_graph: None,
    })
    .expect("launch");

    let (ord, reviewing_ord, prompt) = wait_for_gate(&ev, "attr-def").expect("the run pauses");

    assert_eq!(
        ord, 2,
        "the pause blocks unit 2 — that part was always right"
    );
    assert_eq!(
        reviewing_ord,
        Some(1),
        "the gate was declared by unit 1 and it is unit 1's OUTPUT the operator judges; without \
         this the log cannot say why the run stopped here"
    );
    assert!(
        prompt.contains("unit 1"),
        "the operator must be told which phase's work is under review — prompt was: {prompt}"
    );
    assert!(
        prompt.contains("clarify"),
        "and by its phase description, not just its number — prompt was: {prompt}"
    );
    // The regression itself: the old prompt was exactly "Approve unit 2 before it runs: design",
    // which asks the operator to bless a phase that has produced nothing.
    assert!(
        !prompt.starts_with("Approve unit 2 before it runs"),
        "the prompt must not lead with the phase that has not run — prompt was: {prompt}"
    );
}

/// A run-level `--confirm` pause reviews NOTHING: it is a policy applied before the unit runs, so
/// there is no produced artifact to attribute it to. `None` here is a real statement, not a gap.
#[test]
fn a_run_level_confirm_attributes_the_pause_to_no_unit() {
    let def_json = serde_json::json!({
        "id": "attr-runlevel",
        "phases": [
            { "id": "build", "kind": "build", "gate": "auto" },
            { "id": "review", "kind": "review", "gate": "auto" }
        ]
    })
    .to_string();

    let core = Core::spawn_with_engine(
        db_path("runlevel"),
        Arc::new(NumericDispatcher),
        Arc::new(OkRunner),
    );
    let ev = core.subscribe();
    core.register_workflow(&def_json).unwrap();
    core.launch_run(LaunchSpec {
        project_id: None,
        problem: "Do the thing.".into(),
        clis: vec![cli("stub")],
        entity_mode: EntityMode::Shared,
        session_id: "attr-run".into(),
        // Every gate in the def is `auto`, so the run-level policy is the only source of a pause.
        human_confirm: HumanConfirm::All,
        repo_ref: None,
        workflow: Some("attr-runlevel".into()),
        extra_write_roots: Vec::new(),
        extra_read_roots: Vec::new(),
        project_graph: None,
    })
    .expect("launch");

    let (ord, reviewing_ord, prompt) = wait_for_gate(&ev, "attr-run").expect("the run pauses");

    assert_eq!(ord, 1, "run-level All pauses before the very first unit");
    assert_eq!(
        reviewing_ord, None,
        "nothing has been produced yet — attributing this pause to a unit would invent an artifact"
    );
    assert!(
        prompt.contains("before it runs"),
        "the pre-dispatch wording is correct HERE, where the unit really has not run — prompt was: \
         {prompt}"
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
