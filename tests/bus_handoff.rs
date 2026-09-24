//! DES-TEAMING-002 T0 (amended) — the engine is handed a bus on every boot, so the bus must never
//! cost the actor anything and must never be opened-and-closed in the daemon.
//!
//!  1. `default_boot_serves_commands_while_the_bus_file_is_locked` — with `WICKED_BUS_DB` set and
//!     exec mediation off (the default crew boot after T0), a bus file another connection holds
//!     EXCLUSIVE does not stall the actor: the first command is answered at once. Before this seam
//!     the launch poller's start-point snapshot opened the bus ON the actor thread and waited out
//!     the 5 s busy timeout first.
//!  2. `no_production_path_opens_a_private_bus_connection` — a source guard: outside test code,
//!     nothing calls `BusDb::open` (a private connection that is later dropped = an open-and-close
//!     of the bus file, the F-E2E-021 lock-loss class when another SQLite library shares the file
//!     in the same process). Every production path takes the process-wide `BusDb::shared` handle.
//!
//!  3. `default_boot_opens_the_bus_once_on_the_bridge_thread` — on a default boot the engine's ONE
//!     bus connection is opened by the launch-bridge thread, a bus event launches a run through it,
//!     and a second `Core` in the same process reuses it (no close, no second open).
//!  4. `exec_boot_mediates_over_the_same_single_connection` — with `WICKED_BUS_EXEC` on, exec
//!     mediation still publishes `task.dispatched` / `task.completed` over the bus (unchanged), on
//!     the same single connection, opened by a bus thread.
//!
//! These tests mutate process env (`WICKED_BUS_DB` / `WICKED_BUS_EXEC`), which the actor reads at
//! spawn, so they are serialized on one lock and live in their own test binary.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use wicked_core::{
    shared_bus_stats, BusDb, BusEmit, Core, CoreEvent, StepInput, StepOutput, StepRunner,
    StepStatus, BUS_EXEC_INIT_THREAD, BUS_POLLER_THREAD, RUN_REQUESTED, TASK_COMPLETED,
    TASK_DISPATCHED,
};
use wicked_council::types::{Confidence, Dispatcher, Vote};
use wicked_council::{AgenticCli, CouncilTask};

static ENV_LOCK: Mutex<()> = Mutex::new(());

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
        "wicked-core-bus-handoff-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn default_boot_serves_commands_while_the_bus_file_is_locked() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let dir = tmp_dir("locked");
    let bus_db = dir.join("bus.db").to_string_lossy().to_string();
    let estate_db = dir.join("estate.db").to_string_lossy().to_string();

    // Another writer holds the bus file EXCLUSIVE (rollback journal, so even a read waits): what a
    // busy or wedged bus looks like to an opener with a 5 s busy timeout.
    let holder = rusqlite::Connection::open(&bus_db).unwrap();
    holder
        .execute_batch("PRAGMA journal_mode=DELETE; CREATE TABLE hold(x); BEGIN EXCLUSIVE; INSERT INTO hold VALUES (1);")
        .unwrap();

    std::env::set_var("WICKED_BUS_DB", &bus_db);
    std::env::remove_var("WICKED_BUS_EXEC");
    let started = Instant::now();
    let core = Core::spawn_with_engine(estate_db, Arc::new(StubDispatcher), Arc::new(FastRunner));
    let sessions = core.sessions();
    let first_answer = started.elapsed();
    std::env::remove_var("WICKED_BUS_DB");

    holder.execute_batch("COMMIT;").unwrap();
    drop(core);
    let _ = std::fs::remove_dir_all(&dir);

    assert!(sessions.is_ok(), "the actor answered: {sessions:?}");
    assert!(
        first_answer < Duration::from_secs(2),
        "the actor must not wait on the bus at startup; first command answered after {first_answer:?}"
    );
}

/// Strip every `#[cfg(test)]` item (a `mod`/`fn`/`impl` block, or a one-line item) from Rust source,
/// counting braces outside string and char literals. Good enough for this crate's own sources.
fn strip_test_items(src: &str) -> String {
    let mut out = String::new();
    let mut lines = src.lines().peekable();
    while let Some(line) = lines.next() {
        if line.trim() != "#[cfg(test)]" {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        // Skip further attributes, then the item: until its braces balance (or a `;` item ends).
        let mut depth: i64 = 0;
        let mut opened = false;
        for item_line in lines.by_ref() {
            let t = item_line.trim();
            if !opened && t.starts_with("#[") {
                continue;
            }
            let mut in_str = false;
            let mut escaped = false;
            for c in item_line.chars() {
                if in_str {
                    if escaped {
                        escaped = false;
                    } else if c == '\\' {
                        escaped = true;
                    } else if c == '"' {
                        in_str = false;
                    }
                    continue;
                }
                match c {
                    '"' => in_str = true,
                    '{' => {
                        depth += 1;
                        opened = true;
                    }
                    '}' => depth -= 1,
                    _ => {}
                }
            }
            if (opened && depth <= 0) || (!opened && t.ends_with(';')) {
                break;
            }
        }
    }
    out
}

#[test]
fn no_production_path_opens_a_private_bus_connection() {
    let src_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut offenders = Vec::new();
    let mut stack = vec![src_dir];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let src = std::fs::read_to_string(&path).unwrap();
            for (i, line) in strip_test_items(&src).lines().enumerate() {
                let code = line.split("//").next().unwrap_or("");
                if code.contains("BusDb::open(") {
                    offenders.push(format!("{}: {}", path.display(), line.trim()));
                    let _ = i;
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "production code must take the process-wide BusDb::shared handle, never a private \
         BusDb::open connection (an open-and-close of the bus file):\n{}",
        offenders.join("\n")
    );
}

/// Wait (bounded) until the process-wide registry has opened `bus_db`.
fn wait_for_open(bus_db: &str) -> (usize, String) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(stats) = shared_bus_stats(bus_db) {
            return stats;
        }
        assert!(
            Instant::now() < deadline,
            "the engine never opened {bus_db}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Emit a `run.requested` for `session` through the process-wide handle and wait for the run to
/// complete on `events`.
fn launch_over_the_bus(bus_db: &str, events: &std::sync::mpsc::Receiver<CoreEvent>, session: &str) {
    BusDb::shared(bus_db)
        .unwrap()
        .emit(&BusEmit::new(
            RUN_REQUESTED,
            "wicked-cli",
            "cli.run",
            serde_json::json!({ "problem": "Do step one", "args": { "session_id": session } }),
        ))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        match events.recv_timeout(Duration::from_millis(500)) {
            Ok(CoreEvent::SessionCompleted { session: s }) if s == session => return,
            Ok(CoreEvent::Error { message, .. }) => panic!("bus launch errored: {message}"),
            _ => continue,
        }
    }
    panic!("the bus-launched run {session} never completed");
}

fn count_rows(bus_db: &str, event_type: &str) -> usize {
    BusDb::shared(bus_db)
        .unwrap()
        .poll(event_type, 0, 10_000)
        .unwrap()
        .len()
}

#[test]
fn default_boot_opens_the_bus_once_on_the_bridge_thread() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let dir = tmp_dir("once");
    let bus_db = dir.join("bus.db").to_string_lossy().to_string();

    std::env::set_var("WICKED_BUS_DB", &bus_db);
    std::env::remove_var("WICKED_BUS_EXEC");
    let core = Core::spawn_with_engine(
        dir.join("a.db").to_string_lossy().to_string(),
        Arc::new(StubDispatcher),
        Arc::new(FastRunner),
    );
    let events = core.subscribe();
    let (opens, opener) = wait_for_open(&bus_db);
    assert_eq!(opens, 1);
    assert_eq!(
        opener, BUS_POLLER_THREAD,
        "opened by the bridge thread, not the actor"
    );
    launch_over_the_bus(&bus_db, &events, "handoff-a");
    assert_eq!(
        count_rows(&bus_db, TASK_DISPATCHED),
        0,
        "exec mediation stays off"
    );
    drop(core);

    // A second engine in the same process (a restart, a test harness) reuses the connection.
    let core = Core::spawn_with_engine(
        dir.join("b.db").to_string_lossy().to_string(),
        Arc::new(StubDispatcher),
        Arc::new(FastRunner),
    );
    let events = core.subscribe();
    launch_over_the_bus(&bus_db, &events, "handoff-b");
    std::env::remove_var("WICKED_BUS_DB");
    drop(core);
    assert_eq!(
        shared_bus_stats(&bus_db),
        Some((1, BUS_POLLER_THREAD.to_string())),
        "one connection for the life of the process: none closed, none reopened"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn exec_boot_mediates_over_the_same_single_connection() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let dir = tmp_dir("exec");
    let bus_db = dir.join("bus.db").to_string_lossy().to_string();

    std::env::set_var("WICKED_BUS_DB", &bus_db);
    std::env::set_var("WICKED_BUS_EXEC", "1");
    let core = Core::spawn_with_engine(
        dir.join("e.db").to_string_lossy().to_string(),
        Arc::new(StubDispatcher),
        Arc::new(FastRunner),
    );
    let events = core.subscribe();
    // Exec mode arms before the actor serves its first command: once it answers, the bus is open,
    // and it was opened by a bus thread (not the actor, and not this test).
    core.sessions().unwrap();
    let (opens, opener) = shared_bus_stats(&bus_db).expect("exec mediation opened the bus");
    assert_eq!(opens, 1);
    assert!(
        opener == BUS_EXEC_INIT_THREAD || opener == BUS_POLLER_THREAD,
        "opened by a bus thread, got {opener:?}"
    );
    launch_over_the_bus(&bus_db, &events, "handoff-exec");
    std::env::remove_var("WICKED_BUS_EXEC");
    std::env::remove_var("WICKED_BUS_DB");
    drop(core);

    assert!(
        count_rows(&bus_db, TASK_DISPATCHED) >= 1,
        "exec mediation published task.dispatched"
    );
    assert!(
        count_rows(&bus_db, TASK_COMPLETED) >= 1,
        "and consumed its task.completed"
    );
    assert_eq!(
        shared_bus_stats(&bus_db).map(|s| s.0),
        Some(1),
        "exec mediation, the bridge, the publisher and the judge share one connection"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ── Test-harness hygiene (core#311) — not a test ─────────────────────────────────────────────
/// Arm the hermetic emit spool BEFORE main (pre-main is single-threaded, so no test thread can
/// race it): the engine paths these tests drive fire `wicked.*` governance emissions, which with no
/// shared store spool — into a per-process temp file, never the operator's real replay queue.
/// `harness_hygiene.rs` fails the suite if a test binary lacks this block.
///
/// SAFETY (`ctor(unsafe)`): runs before `main` on one thread and only sets one process env var
/// via the std API — no allocator setup, no threads, no panics across the FFI boundary.
#[ctor::ctor(unsafe)]
fn arm_hermetic_emit_spool() {
    wicked_apps_core::emit::hermetic_test_spool();
}
