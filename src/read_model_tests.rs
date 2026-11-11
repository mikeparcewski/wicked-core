//! (core#1390) The read model through the engine: run reads answer while the actor is held by a
//! long command, they read what the actor committed, the actor's timing names the long command,
//! and no decision path can reach the read model.

use crate::actor::sessions_detail_fold_tests::session;
use crate::domain::{put_nodes, WorkUnit};
use std::sync::mpsc::channel;
use std::sync::Arc;
use std::time::{Duration, Instant};
use wicked_apps_core::ToNode;

struct NoCouncil;
impl wicked_council::types::Dispatcher for NoCouncil {
    fn dispatch(
        &self,
        cli: &crate::AgenticCli,
        _t: &wicked_council::CouncilTask,
    ) -> Option<wicked_council::types::Vote> {
        Some(wicked_council::types::Vote {
            cli: cli.key.clone(),
            recommendation: "x".into(),
            top_risk: "none".into(),
            change_my_mind: "no".into(),
            disqualifier: None,
            confidence: Default::default(),
            provenance: "stub".into(),
        })
    }
}

fn temp_db(tag: &str) -> (std::path::PathBuf, String) {
    let dir = std::env::temp_dir().join(format!(
        "wicked-core-rm-{tag}-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("estate.db").to_string_lossy().into_owned();
    (dir, db)
}

/// Two runs with units written before the engine opens the store.
fn seed(db: &str) {
    let mut store = wicked_apps_core::open_store(Some(db)).unwrap();
    let mut nodes = vec![session("run-a").to_node(), session("run-b").to_node()];
    for ord in [2u32, 1] {
        nodes.push(WorkUnit::pending(format!("run-a:u{ord}"), "run-a", ord, "a").to_node());
    }
    nodes.push(WorkUnit::pending("run-b:u1", "run-b", 1, "b").to_node());
    put_nodes(&mut store, &nodes).unwrap();
}

#[test]
fn run_reads_answer_while_the_actor_is_held_and_its_timing_names_the_hold() {
    let (dir, db) = temp_db("held");
    seed(&db);
    let core = crate::Core::spawn_with_engine(
        db.clone(),
        Arc::new(NoCouncil),
        Arc::new(crate::workflow::StubStepRunner),
    );
    // The actor has opened the store and answers.
    core.ping();

    let (entered_tx, entered) = channel();
    let (release, release_rx) = channel();
    core.hold_actor(entered_tx, release_rx);
    entered
        .recv_timeout(Duration::from_secs(10))
        .expect("the actor parks on the hold");
    // A command queued behind the hold: the backlog the stats must see.
    let pinger = {
        let core = core.clone();
        std::thread::spawn(move || core.ping())
    };
    std::thread::sleep(Duration::from_millis(100));

    let t = Instant::now();
    let mut ids = core.sessions().expect("sessions");
    ids.sort();
    assert_eq!(ids, vec!["run-a", "run-b"]);
    let mut views = core.sessions_detail().expect("sessions_detail");
    views.sort_by(|a, b| a.session.id.cmp(&b.session.id));
    let units: Vec<(String, Vec<u32>)> = views
        .iter()
        .map(|v| {
            (
                v.session.id.clone(),
                v.units.iter().map(|u| u.ord).collect(),
            )
        })
        .collect();
    assert_eq!(
        units,
        vec![("run-a".into(), vec![1, 2]), ("run-b".into(), vec![1])],
        "each run with its own units, in ord order"
    );
    assert_eq!(core.work_output("run-a:u1"), None, "never ran: no output");
    assert!(core.unit_transcript_at("run-a:u1", None).is_none());
    assert_eq!(
        core.run_team("run-a").expect("a known run"),
        None,
        "not a team run"
    );
    let stats = core.actor_stats();
    let took = t.elapsed();
    assert!(
        took < Duration::from_secs(2),
        "every read answered without the actor (took {took:?})"
    );
    assert_eq!(
        stats.running.as_ref().map(|r| r.command.as_str()),
        Some("HoldActor"),
        "the stats name the command every read would have waited behind: {stats:?}"
    );

    std::thread::sleep(Duration::from_millis(200));
    release.send(()).unwrap();
    pinger.join().unwrap();
    core.ping();
    let stats = core.actor_stats();
    let hold = stats
        .commands
        .iter()
        .find(|c| c.command == "HoldActor")
        .expect("the hold was timed");
    assert!(hold.max_ms >= 300, "the hold lasted ≥ 300 ms: {hold:?}");
    assert!(
        stats.max_backlog >= 1,
        "the ping queued behind the hold was waiting when the actor became free: {stats:?}"
    );
    let idle = Instant::now();
    while core.actor_stats().running.is_some() && idle.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        core.actor_stats().running,
        None,
        "nothing runs once the actor is idle"
    );
    assert!(stats.commands.iter().any(|c| c.command == "Ping"));

    drop(core);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_unknown_run_is_still_the_actors_answer_and_an_in_memory_core_reads_through_the_actor() {
    let (dir, db) = temp_db("unknown");
    seed(&db);
    let core = crate::Core::spawn_with_engine(
        db,
        Arc::new(NoCouncil),
        Arc::new(crate::workflow::StubStepRunner),
    );
    let err = core.run_team("no-such-run").expect_err("unknown run");
    assert!(err.to_string().contains("run not found"), "{err}");
    drop(core);
    let _ = std::fs::remove_dir_all(&dir);

    let mem = crate::Core::spawn_with_engine(
        ":memory:",
        Arc::new(NoCouncil),
        Arc::new(crate::workflow::StubStepRunner),
    );
    assert!(mem.sessions().unwrap().is_empty());
    assert!(mem.sessions_detail().unwrap().is_empty());
}

#[test]
fn a_write_committed_through_the_actor_is_what_the_next_read_sees() {
    let (dir, db) = temp_db("ryw");
    let core = crate::Core::spawn_with_engine(
        db,
        Arc::new(NoCouncil),
        Arc::new(crate::workflow::StubStepRunner),
    );
    core.ping();
    assert!(core.sessions_detail().unwrap().is_empty());
    let p = core.project_create("rm-ryw", None).expect("create");
    // The reply came after the write committed: a project read through the read-only path sees it.
    assert!(crate::project::list_projects(
        &wicked_apps_core::open_store_ro(Some(dir.join("estate.db").to_str().unwrap())).unwrap()
    )
    .unwrap()
    .iter()
    .any(|q| q.id == p.id));
    drop(core);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The Core methods allowed to answer from the read model. Every one is a read a caller shows or
/// polls; none decides a gate, a reassign, a delivery or a deny.
const READ_ONLY_METHODS: [&str; 5] = [
    "sessions",
    "sessions_detail",
    "work_output",
    "unit_transcript_at",
    "run_team",
];

#[test]
fn read_model_serves_reads_only() {
    // 1. Inside `Core`, only the listed read methods touch `self.reads`.
    let lib = include_str!("lib.rs");
    let mut current: Option<String> = None;
    let mut users = std::collections::BTreeSet::new();
    for line in lib.lines() {
        let t = line.trim_start();
        if let Some(rest) = t
            .strip_prefix("pub fn ")
            .or_else(|| t.strip_prefix("pub(crate) fn "))
            .or_else(|| t.strip_prefix("fn "))
        {
            current = rest.split(['(', '<']).next().map(str::to_string);
        }
        if line.contains("self.reads") || t.starts_with(".reads") {
            users.insert(current.clone().unwrap_or_default());
        }
    }
    let allowed: std::collections::BTreeSet<String> =
        READ_ONLY_METHODS.iter().map(|s| s.to_string()).collect();
    assert!(
        users.is_subset(&allowed),
        "only read methods may answer from the read model; also used by: {:?}",
        users.difference(&allowed).collect::<Vec<_>>()
    );
    assert!(
        users.contains("sessions_detail"),
        "the guard sees the real uses: {users:?}"
    );

    // 2. Nothing outside `lib.rs` and the read model itself names it: the actor, the gate hook,
    // the relay, the runners and every decision module can never reach it.
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut offenders = Vec::new();
    let mut stack = vec![src.clone()];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            let rel = p
                .strip_prefix(&src)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if !rel.ends_with(".rs")
                || matches!(
                    rel.as_str(),
                    "lib.rs" | "read_model.rs" | "read_model_tests.rs"
                )
            {
                continue;
            }
            let text = std::fs::read_to_string(&p).unwrap();
            if text.contains("read_model") || text.contains("ReadModel") {
                offenders.push(rel);
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "the read model is for Core's read methods only; named in: {offenders:?}"
    );
}
