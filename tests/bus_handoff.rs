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
//! These tests mutate process env (`WICKED_BUS_DB` / `WICKED_BUS_EXEC`), which the actor reads at
//! spawn, so they are serialized on one lock and live in their own test binary.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use wicked_council::types::{Confidence, Dispatcher, Vote};
use wicked_council::{AgenticCli, CouncilTask};
use wicked_core::{Core, StepInput, StepOutput, StepRunner, StepStatus};

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
