//! EP-K3 (DES-artifact-editor-plugins §7.6): a launch-time `exclude_seats` list rides from the
//! `LaunchSpec` onto every dispatched unit, where the judge selection unions it into its exclusion
//! set (the selection itself is pinned in `cli_runner`'s unit tests). An omitted list is empty.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use wicked_core::{
    Core, CoreEvent, EntityMode, HumanConfirm, LaunchSpec, StepInput, StepOutput, StepRunner,
    StepStatus,
};
use wicked_council::types::{Category, Confidence, Dispatcher, InputMode, Vote};
use wicked_council::{AgenticCli, CouncilTask};

struct StubDispatcher;
impl Dispatcher for StubDispatcher {
    fn dispatch(&self, cli: &AgenticCli, _task: &CouncilTask) -> Option<Vote> {
        Some(Vote {
            cli: cli.key.clone(),
            recommendation: "fake-a".into(),
            top_risk: "none".into(),
            change_my_mind: "no".into(),
            disqualifier: None,
            confidence: Confidence::default(),
            provenance: "stub".into(),
        })
    }
}

/// Records the launch exclusion each dispatched unit carries.
struct Recording {
    seen: Arc<Mutex<Vec<Vec<String>>>>,
}
impl StepRunner for Recording {
    fn run_unit(&self, input: &StepInput) -> StepOutput {
        self.seen
            .lock()
            .unwrap()
            .push(input.unit.exclude_seats.clone());
        StepOutput {
            run_id: input.run_id.clone(),
            unit_ix: input.unit_ix,
            attempt: input.attempt,
            output: "done".into(),
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

const DEF: &str = r#"{"id":"exclude-2","phases":[
  {"id":"one","kind":"build","gate":"auto"},
  {"id":"two","kind":"build","gate":"auto","depends_on":["one"]}]}"#;

fn run_with(name: &str, exclude_seats: Vec<String>) -> Vec<Vec<String>> {
    let dir =
        std::env::temp_dir().join(format!("wicked-core-exclude-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let core = Core::spawn_with_engine(
        dir.join("estate.db").to_str().unwrap().to_string(),
        Arc::new(StubDispatcher),
        Arc::new(Recording { seen: seen.clone() }),
    );
    let events = core.subscribe();
    core.register_workflow(DEF).unwrap();
    let run = core
        .launch_run(LaunchSpec {
            base_ref: None,
            project_id: None,
            problem: "Do step one. Do step two".into(),
            clis: vec![cli("fake-a"), cli("fake-b")],
            entity_mode: EntityMode::Shared,
            session_id: format!("x-{name}"),
            human_confirm: HumanConfirm::None,
            auto_deliver: false,
            repo_ref: None,
            workflow: Some("exclude-2".into()),
            extra_write_roots: Vec::new(),
            extra_read_roots: Vec::new(),
            project_graph: None,
            plan: None,
            deliver_step: None,
            exclude_seats,
            evidence_root: None,
            primary: None,
        })
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(Instant::now() < deadline, "run {run} did not complete");
        match events.recv_timeout(Duration::from_secs(1)) {
            Ok(CoreEvent::SessionCompleted { session }) if session == run => break,
            Ok(CoreEvent::Error { message, .. }) => panic!("run {run} errored: {message}"),
            _ => continue,
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    let got = seen.lock().unwrap().clone();
    got
}

#[test]
fn every_dispatched_unit_carries_the_launch_exclusion() {
    let seen = run_with("set", vec!["codex".into(), " pi#2 ".into(), "".into()]);
    assert!(seen.len() >= 2, "{seen:?}");
    for unit in &seen {
        // Trimmed, empties dropped, order kept.
        assert_eq!(unit, &["codex".to_string(), "pi#2".to_string()]);
    }
}

#[test]
fn an_omitted_exclusion_is_empty_on_every_unit() {
    let seen = run_with("none", Vec::new());
    assert!(seen.len() >= 2, "{seen:?}");
    assert!(seen.iter().all(Vec::is_empty), "{seen:?}");
}

/// The straight-through `Core::launch` path carries no judge exclusion, so it REFUSES a launch
/// that names one rather than silently dropping it.
#[test]
fn the_straight_through_path_refuses_an_exclusion_instead_of_dropping_it() {
    let dir =
        std::env::temp_dir().join(format!("wicked-core-exclude-legacy-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let core = Core::spawn_with_engine(
        dir.join("estate.db").to_str().unwrap().to_string(),
        Arc::new(StubDispatcher),
        Arc::new(Recording {
            seen: Arc::new(Mutex::new(Vec::new())),
        }),
    );
    let events = core.subscribe();
    let id = core.launch(LaunchSpec {
        base_ref: None,
        project_id: None,
        problem: "Do one thing".into(),
        clis: vec![cli("fake-a"), cli("fake-b")],
        entity_mode: EntityMode::Shared,
        session_id: "x-legacy".into(),
        human_confirm: HumanConfirm::None,
        auto_deliver: false,
        repo_ref: None,
        workflow: None,
        extra_write_roots: Vec::new(),
        extra_read_roots: Vec::new(),
        project_graph: None,
        plan: None,
        deliver_step: None,
        exclude_seats: vec!["codex".into()],
        evidence_root: None,
        primary: None,
    });
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(Instant::now() < deadline, "no refusal for {id}");
        match events.recv_timeout(Duration::from_secs(1)) {
            Ok(CoreEvent::Error { message, .. }) => {
                assert!(message.contains("exclude_seats"), "{message}");
                break;
            }
            Ok(CoreEvent::SessionCompleted { session }) if session == id => {
                panic!("the straight-through path ran a launch whose exclusion it cannot honour")
            }
            _ => continue,
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Pre-main arming of the hermetic emit spool (core#311): engine paths under test fire
/// fire-and-forget `wicked.*` emissions, which must land in a per-process temp file, never in the
/// operator's real replay queue. `harness_hygiene.rs` fails the suite if a binary lacks this.
///
/// SAFETY (`ctor(unsafe)`): runs before `main` on one thread and only sets one process env var
/// via the std API — no allocator setup, no threads, no panics across the FFI boundary.
#[ctor::ctor(unsafe)]
fn arm_hermetic_emit_spool() {
    wicked_apps_core::emit::hermetic_test_spool();
}
