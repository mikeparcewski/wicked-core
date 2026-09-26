//! wicked-core#631 — `Core::bus_emit` / `Core::bus_read`: the engine is the one writer and the one
//! SQLite library on its bus, so crew emits and reads its rows through the engine instead of
//! holding a second SQLite library on the same file in the same process.
//!
//!  1. `bus_emit_and_bus_read_go_through_the_engines_one_connection` — a wire emit lands on the
//!     core's bus as a wicked-bus row, a duplicate key resolves to the existing row, and
//!     `bus_read` pages it back by type prefix — all on the process-wide shared connection, never a
//!     second one.
//!  2. `bus_emit_never_waits_on_the_actor_and_the_actor_never_waits_on_it` — with the bus file held
//!     EXCLUSIVE by another connection, a `bus_emit` on another thread waits out SQLite's busy
//!     timeout while the actor keeps answering commands at once: the call runs on its caller's
//!     thread and never reaches the actor.
//!  3. `a_core_without_a_bus_refuses` — no bus configured: both calls fail with a clear error.
//!
//! The bus is handed through `TeamConfig` (what `WICKED_BUS_DB` sets in production), so these tests
//! touch no process env.

use std::sync::Arc;
use std::time::{Duration, Instant};

use wicked_core::{
    shared_bus_stats, Core, StepInput, StepOutput, StepRunner, StepStatus, TeamConfig,
};
use wicked_council::types::{Confidence, Dispatcher, Vote};
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

struct FastRunner;
impl StepRunner for FastRunner {
    fn run_unit(&self, input: &StepInput) -> StepOutput {
        StepOutput {
            run_id: input.run_id.clone(),
            unit_ix: input.unit_ix,
            attempt: input.attempt,
            output: "ok".into(),
            status: StepStatus::Ok,
            usage: None,
            files: Vec::new(),
            tools: Vec::new(),
            governed: false,
        }
    }
}

fn tmp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "wicked-core-bus-emit-read-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn core_on(dir: &std::path::Path, bus: Option<&str>) -> Core {
    Core::spawn_with_engine_team(
        dir.join("estate.db").to_string_lossy().to_string(),
        Arc::new(StubDispatcher),
        Arc::new(FastRunner),
        TeamConfig::new(
            bus.map(str::to_string),
            Some(dir.join("team-outbox.ndjson")),
        ),
    )
}

#[test]
fn bus_emit_and_bus_read_go_through_the_engines_one_connection() {
    let dir = tmp_dir("roundtrip");
    let bus = dir.join("bus.db").to_string_lossy().to_string();
    let core = core_on(&dir, Some(&bus));

    let row = r#"{"event_type":"wicked.crew.project.created","domain":"wicked-crew",
                  "subdomain":"project","payload":{"project_id":"p1"},
                  "idempotency_key":"k1","producer_id":"wicked-crew"}"#;
    let id = core.bus_emit(row).unwrap();
    assert!(id > 0);
    assert_eq!(
        core.bus_emit(row).unwrap(),
        id,
        "a duplicate key is the existing row"
    );
    core.bus_emit(
        r#"{"event_type":"wicked.interactive.doc.created","domain":"wicked-interactive","payload":{}}"#,
    )
    .unwrap();

    let page = core.bus_read(0, 100, Some("wicked.crew."), false).unwrap();
    assert_eq!(page.rows.len(), 1);
    assert_eq!(page.rows[0]["event_id"].as_i64(), Some(id));
    assert_eq!(page.rows[0]["payload"]["project_id"], "p1");
    assert_eq!(page.rows[0]["producer_id"], "wicked-crew");
    assert_eq!(core.bus_read(0, 100, None, false).unwrap().rows.len(), 2);
    assert_eq!(
        core.bus_read(page.next, 100, Some("wicked.crew."), false)
            .unwrap()
            .rows
            .len(),
        0
    );

    let (opens, _) = shared_bus_stats(&bus).expect("the engine holds the bus");
    assert_eq!(opens, 1, "one connection for the life of the process");
}

#[test]
fn bus_emit_never_waits_on_the_actor_and_the_actor_never_waits_on_it() {
    let dir = tmp_dir("locked");
    let bus = dir.join("bus.db").to_string_lossy().to_string();
    let core = core_on(&dir, Some(&bus));
    // Open the engine's connection first (schema in place), then lock the file from outside.
    core.bus_read(0, 0, None, false).unwrap();
    let holder = rusqlite::Connection::open(&bus).unwrap();
    holder
        .execute_batch(
            "BEGIN EXCLUSIVE; INSERT INTO core_exec_meta(key, value) VALUES ('hold', '1');",
        )
        .unwrap();

    let emitter = core.clone();
    let started = Instant::now();
    let emit = std::thread::spawn(move || {
        let r = emitter.bus_emit(r#"{"event_type":"wicked.a.b","domain":"d","payload":{}}"#);
        (r.is_ok(), started.elapsed())
    });
    std::thread::sleep(Duration::from_millis(200));
    // The actor answers while the emit is stuck behind the lock.
    let asked = Instant::now();
    core.sessions().unwrap();
    assert!(
        asked.elapsed() < Duration::from_secs(1),
        "the actor answered in {:?} while a bus emit waited",
        asked.elapsed()
    );
    holder.execute_batch("COMMIT;").unwrap();
    let (ok, took) = emit.join().unwrap();
    assert!(ok, "the emit lands once the lock is released");
    assert!(
        took >= Duration::from_millis(200),
        "the emit did wait on the lock ({took:?})"
    );
}

#[test]
fn a_core_without_a_bus_refuses() {
    let dir = tmp_dir("nobus");
    let core = core_on(&dir, None);
    let e = core
        .bus_emit(r#"{"event_type":"wicked.a.b","domain":"d","payload":{}}"#)
        .unwrap_err()
        .to_string();
    assert!(e.contains("no bus"), "{e}");
    assert!(core
        .bus_read(0, 10, None, false)
        .unwrap_err()
        .to_string()
        .contains("no bus"));
}

/// Arm the hermetic emit spool before `main` (core#311): nothing this binary spawns may spool to
/// the operator's real replay queue.
///
/// SAFETY (`ctor(unsafe)`): runs before `main` on one thread and only sets one process env var
/// via the std API — no allocator setup, no threads, no panics across the FFI boundary.
#[ctor::ctor(unsafe)]
fn arm_hermetic_emit_spool() {
    wicked_apps_core::emit::hermetic_test_spool();
}
