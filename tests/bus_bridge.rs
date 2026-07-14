//! Rust↔wicked-bus bridge proving tests (DES-EXEC-001 §2.5).
//!
//!  1. `run_requested_event_launches_a_run` — the launch trigger end to end IN RUST: a
//!     `wicked.run.requested` row on the bus db drives a real `LaunchRun` on the Core actor (stub
//!     engine), the run completes, and the bridge emits `wicked.run.launched` back onto the bus.
//!  2. `cross_language_roundtrip` (`#[ignore]`, run explicitly) — proves the SCHEMA matches the JS
//!     bus in BOTH directions by shelling out to the wicked-bus Node CLI. Ignored by default so the
//!     normal suite has no Node dependency; run with `--ignored` for the cross-language evidence.

use std::sync::Arc;
use std::time::Duration;

use wicked_council::types::{Category, Confidence, Dispatcher, InputMode, Vote};
use wicked_council::{AgenticCli, CouncilTask};

use wicked_core::{BusDb, BusEmit, Core, CoreEvent, StepInput, StepOutput, StepRunner, StepStatus};

/// Deterministic council stub — votes without a subprocess (mirrors the P1 tests).
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

/// A runner that completes every unit immediately (no subprocess).
struct FastRunner;
impl StepRunner for FastRunner {
    fn run_unit(&self, input: &StepInput) -> StepOutput {
        StepOutput {
            run_id: input.run_id.clone(),
            unit_ix: input.unit_ix,
            attempt: input.attempt,
            output: format!("stub-output for {}", input.unit.description),
            status: StepStatus::Ok,
            usage: None,
            files: Vec::new(),
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
    }
}

fn tmp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "wicked-core-busbridge-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A `wicked.run.requested` on the bus is turned into a real, completing run on the Core actor, and
/// the bridge publishes `wicked.run.launched` back onto the bus. This is the whole §2.5 loop, proven
/// with the same stub engine the P1 tests use (no real CLIs).
#[test]
fn run_requested_event_launches_a_run() {
    let dir = tmp_dir("launch");
    let estate_db = dir.join("estate.db").to_str().unwrap().to_string();
    let bus_db = dir.join("bus.db").to_str().unwrap().to_string();

    let core = Core::spawn_with_engine(estate_db, Arc::new(StubDispatcher), Arc::new(FastRunner));
    let events = core.subscribe();

    // Connect the bridge FIRST (its cursor floor starts at the current tail = 0 on an empty bus), so
    // the request we emit next (event_id 1 > 0) is picked up. Roster is injected for the test.
    let bridge = core.connect_bus(bus_db.clone(), vec![cli("fake-a"), cli("fake-b")]);

    // Publish a launch request onto the bus (as a JS producer / another tool would). Free-text
    // planner (`workflow: null`) → the two-sentence problem decomposes into 2 units.
    let db = BusDb::open(&bus_db).unwrap();
    db.emit(&BusEmit::new(
        wicked_core::RUN_REQUESTED,
        "wicked-cli",
        "cli.run",
        serde_json::json!({
            "problem": "Do step one. Do step two",
            "args": { "session_id": "busrun" }
        }),
    ))
    .unwrap();

    // The run drives to completion via the live event stream.
    let mut completed = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        match events.recv_timeout(Duration::from_secs(1)) {
            Ok(CoreEvent::SessionCompleted { session }) if session == "busrun" => {
                completed = true;
                break;
            }
            Ok(CoreEvent::Error { message, .. }) => panic!("bridge launch errored: {message}"),
            Ok(_) => continue,
            Err(_) => continue,
        }
    }
    assert!(completed, "the bus-launched run reached SessionCompleted");

    // The run is a real, persisted session on the actor's store.
    let sessions = core.sessions().unwrap();
    assert!(
        sessions.iter().any(|s| s == "busrun"),
        "run persisted: {sessions:?}"
    );

    // Proof of the PUBLISH path: the bridge emitted `wicked.run.launched` back onto the bus, keyed on
    // the run id, readable by the same poll API (and, by schema, by the JS bus).
    let launched = db.poll(wicked_core::RUN_LAUNCHED, 0, 10).unwrap();
    assert_eq!(launched.len(), 1, "exactly one run.launched event");
    assert_eq!(launched[0].payload["run_id"], "busrun");

    bridge.stop();
}

// ─────────────────────────────────────────────────────────────────────────────────────────────────
// Cross-language schema round-trip (opt-in). Run: `cargo test -p wicked-core --test bus_bridge -- \
// --ignored --nocapture`. Requires `node` on PATH and the sibling wicked-bus checkout.
// ─────────────────────────────────────────────────────────────────────────────────────────────────

fn bus_cli_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("wicked-bus")
        .join("commands")
        .join("cli.js")
}

#[test]
#[ignore = "requires node + sibling wicked-bus; run with --ignored for cross-language evidence"]
fn cross_language_roundtrip() {
    let cli_js = bus_cli_path();
    assert!(
        cli_js.exists(),
        "wicked-bus CLI not found at {}",
        cli_js.display()
    );
    let dir = tmp_dir("xlang");

    // ── Direction 1: Rust → JS. Rust writes a row; the JS bus drains it and confirms it is well-formed.
    let db_a = dir.join("a.db");
    let db_a_s = db_a.to_str().unwrap().to_string();
    {
        let db = BusDb::open(&db_a_s).unwrap();
        db.emit(&BusEmit::new(
            wicked_core::RUN_REQUESTED,
            "wicked-core",
            "core.run",
            serde_json::json!({ "workflow": "feature", "problem": "rust wrote this" }),
        ))
        .unwrap();
    }
    let out = std::process::Command::new("node")
        .arg(&cli_js)
        .args([
            "subscribe",
            "--plugin",
            "xlang-js",
            "--filter",
            "wicked.crew.run.*",
            "--cursor-init",
            "oldest",
            "--once",
            "--drain",
        ])
        .args(["--db-path", &db_a_s])
        .output()
        .expect("run node subscribe");
    let stdout = String::from_utf8_lossy(&out.stdout);
    eprintln!("[Rust→JS] node subscribe stdout:\n{stdout}");
    eprintln!(
        "[Rust→JS] node subscribe stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let js_row: serde_json::Value = stdout
        .lines()
        .find(|l| l.contains("wicked.crew.run.requested"))
        .map(|l| serde_json::from_str(l).expect("JS emitted valid NDJSON"))
        .expect("JS bus drained the Rust-written event");
    assert_eq!(js_row["event_type"], "wicked.crew.run.requested");
    let payload: serde_json::Value =
        serde_json::from_str(js_row["payload"].as_str().unwrap()).unwrap();
    assert_eq!(
        payload["problem"], "rust wrote this",
        "JS read Rust's payload intact"
    );

    // ── Direction 2: JS → Rust. The JS bus writes a row; the Rust poller reads + parses it.
    let db_b = dir.join("b.db");
    let db_b_s = db_b.to_str().unwrap().to_string();
    let emit = std::process::Command::new("node")
        .arg(&cli_js)
        .args([
            "emit",
            "--type",
            "wicked.crew.run.requested",
            "--domain",
            "wicked-cli",
            "--payload",
            "{\"workflow\":\"bug\",\"problem\":\"js wrote this\"}",
        ])
        .args(["--db-path", &db_b_s])
        .output()
        .expect("run node emit");
    eprintln!(
        "[JS→Rust] node emit stdout:\n{}",
        String::from_utf8_lossy(&emit.stdout)
    );
    eprintln!(
        "[JS→Rust] node emit stderr:\n{}",
        String::from_utf8_lossy(&emit.stderr)
    );
    assert!(emit.status.success(), "node emit failed");
    let db = BusDb::open(&db_b_s).unwrap();
    let rust_read = db.poll(wicked_core::RUN_REQUESTED, 0, 10).unwrap();
    assert_eq!(rust_read.len(), 1, "Rust polled the JS-written event");
    assert_eq!(
        rust_read[0].payload["problem"], "js wrote this",
        "Rust parsed JS's payload intact"
    );
    eprintln!(
        "[JS→Rust] Rust polled: event_id={} type={} payload={}",
        rust_read[0].event_id, rust_read[0].event_type, rust_read[0].payload
    );
}
