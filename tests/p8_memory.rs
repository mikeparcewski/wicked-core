//! P8 MEMORY — the orchestrator's episodic memory (wired to `wicked_memory`). Proves: (1) explicit
//! capture → recall round-trips through the engine; (2) a run that reaches a terminal state is
//! AUTO-captured, so recall surfaces past run outcomes.

use std::sync::Arc;
use std::time::{Duration, Instant};

use wicked_core::{
    Core, HumanConfirm, LaunchSpec, SessionStatus, StepInput, StepOutput, StepRunner, StepStatus,
};
use wicked_council::types::{Category, Confidence, Dispatcher, InputMode, Vote};
use wicked_council::{AgenticCli, CouncilTask};

struct StubDispatcher;
impl Dispatcher for StubDispatcher {
    fn dispatch(&self, cli: &AgenticCli, _t: &CouncilTask) -> Option<Vote> {
        Some(Vote {
            cli: cli.key.clone(),
            recommendation: cli.key.clone(),
            top_risk: "none".into(),
            change_my_mind: "no".into(),
            disqualifier: None,
            confidence: Confidence::default(),
            provenance: "stub".into(),
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
            output: "did the work".into(),
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
    }
}

fn spec(session_id: &str, problem: &str) -> LaunchSpec {
    LaunchSpec {
        project_id: None,
        problem: problem.into(),
        clis: vec![cli("a")],
        entity_mode: wicked_core::EntityMode::Shared,
        session_id: session_id.into(),
        human_confirm: HumanConfirm::None,
        repo_ref: None,
        workflow: None,
        extra_write_roots: Vec::new(),
        project_graph: None,
    }
}

fn db_path(name: &str) -> String {
    let dir = std::env::temp_dir().join(format!("wicked-core-p8-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("estate.db").to_str().unwrap().to_string()
}

/// How long a status wait may take before it is called a failure.
///
/// Far above the ~2s an unloaded run needs, because the only thing a longer budget can change is
/// whether a slow-but-correct run under concurrent test load is misreported as a broken one.
const WAIT_BUDGET: Duration = Duration::from_secs(20);

/// Poll until `run_id` reaches `want`.
///
/// Two things about this helper are deliberate.
///
/// The budget is generous. A satisfied wait returns the moment the status matches, so raising it
/// costs nothing on the happy path — it only changes the outcome in the case that would otherwise
/// be a false failure. `cargo test --all` runs dozens of test binaries concurrently, and the old
/// 5s budget sat close enough to the ~2s an unloaded run takes that these files failed
/// intermittently under that load.
///
/// On timeout it reports the status it actually observed. Every caller wraps this in
/// `assert!(wait_status(..))`, and a bare `false` makes libtest print only "assertion failed" —
/// which cannot distinguish a run that stalled (a real defect) from one that was merely slow (a
/// harness problem). Naming the last-seen status is what makes the next occurrence diagnosable.
fn wait_status(core: &Core, run_id: &str, want: SessionStatus) -> bool {
    let start = Instant::now();
    let mut last: Option<SessionStatus> = None;
    while start.elapsed() < WAIT_BUDGET {
        if let Ok(views) = core.sessions_detail() {
            if let Some(v) = views.iter().find(|v| v.session.id == run_id) {
                if v.session.status == want {
                    return true;
                }
                last = Some(v.session.status);
            }
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    eprintln!(
        "wait_status({run_id}): timed out after {:?} waiting for {want:?}; last observed {last:?} \
         (None means the run never appeared in sessions_detail at all)",
        start.elapsed()
    );
    false
}

#[test]
fn explicit_capture_then_recall_round_trips() {
    let core = Core::spawn_with_engine(
        db_path("explicit"),
        Arc::new(StubDispatcher),
        Arc::new(OkRunner),
    );
    // A ROOT (global) memory round-trips through recall (recall queries at root).
    core.capture_memory("agy handled the cache refactor cleanly", "")
        .expect("capture");
    let hits = core.recall_memories("refactor", 5).expect("recall");
    assert!(
        hits.iter().any(|m| m.content.contains("refactor")),
        "recall surfaces the captured memory, got: {hits:?}"
    );

    // PER-APP SCOPING: an app-scoped memory lists under ITS app, but NOT under a different app, and
    // global listing (root) still sees everything.
    core.capture_memory("Wicked uses a single-writer Core actor", "app:wicked")
        .expect("capture app");
    let mine = core.list_memories("app:wicked", 20).expect("list wicked");
    assert!(
        mine.iter().any(|m| m.content.contains("single-writer")),
        "the app's scoped listing surfaces its own memory, got: {mine:?}"
    );
    let other = core.list_memories("app:other", 20).expect("list other");
    assert!(
        !other.iter().any(|m| m.content.contains("single-writer")),
        "a DIFFERENT app's scope does NOT see it (isolation), got: {other:?}"
    );
    let all = core.list_memories("", 20).expect("list all");
    assert!(
        all.iter().any(|m| m.content.contains("single-writer"))
            && all.iter().any(|m| m.content.contains("refactor")),
        "global (root) listing sees both the app + root memories, got: {all:?}"
    );
}

#[test]
fn a_completed_run_is_auto_captured_and_recallable() {
    let core = Core::spawn_with_engine(
        db_path("auto"),
        Arc::new(StubDispatcher),
        Arc::new(OkRunner),
    );
    core.launch_run(spec("r", "Add JWT authentication to the API"))
        .unwrap();
    assert!(
        wait_status(&core, "r", SessionStatus::Completed),
        "the run completes"
    );
    // The terminal outcome was auto-captured; recall finds it by the brief.
    let hits = core
        .recall_memories("JWT authentication", 5)
        .expect("recall");
    assert!(
        hits.iter()
            .any(|m| m.content.contains("JWT authentication") && m.content.contains("completed")),
        "a completed run is remembered + recallable, got: {hits:?}"
    );
}

#[test]
fn mcp_tools_list_and_recall_round_trip() {
    let core =
        Core::spawn_with_engine(db_path("mcp"), Arc::new(StubDispatcher), Arc::new(OkRunner));
    // tools/list advertises the memory tools.
    let list = core
        .mcp_call(serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}))
        .expect("mcp ok")
        .expect("response");
    let tools = list["result"]["tools"].as_array().expect("tools array");
    assert!(
        tools.iter().any(|t| t["name"] == "memory.recall"),
        "memory.recall is advertised"
    );

    // capture via MCP, then recall via MCP — the tool surface round-trips.
    core.mcp_call(serde_json::json!({
        "jsonrpc":"2.0","id":2,"method":"tools/call",
        "params":{"name":"memory.capture","arguments":{"content":"the deploy gate blocks rm -rf"}}
    }))
    .expect("capture ok");
    let recall = core
        .mcp_call(serde_json::json!({
            "jsonrpc":"2.0","id":3,"method":"tools/call",
            "params":{"name":"memory.recall","arguments":{"query":"deploy gate","k":5}}
        }))
        .expect("mcp ok")
        .expect("response");
    let text = recall["result"]["content"][0]["text"]
        .as_str()
        .expect("text content");
    assert!(
        text.contains("deploy gate"),
        "memory.recall via MCP returns the captured memory, got: {text}"
    );
}

#[test]
fn recall_is_empty_not_error_when_nothing_matches() {
    let core = Core::spawn_with_engine(
        db_path("empty"),
        Arc::new(StubDispatcher),
        Arc::new(OkRunner),
    );
    let hits = core
        .recall_memories("nothing was ever stored here", 5)
        .expect("recall ok");
    assert!(hits.is_empty(), "no false memories, got: {hits:?}");
}
