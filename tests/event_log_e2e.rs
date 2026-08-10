//! FINDING-014, end to end: a real run through the real actor must leave a durable event trail that
//! survives the run, and `Core::run_events` must read it back.
//!
//! The unit tests in `src/event_log.rs` prove the MECHANISM (a sink records what it is handed). They
//! cannot prove the WIRING — that the actor's 47 emission sites actually reach that sink during a run.
//! That distinction is exactly the failure mode of FINDING-021 and FINDING-024, where a mechanism was
//! correct and simply never invoked, and where every unit test passed while the feature did nothing.
//! So this drives a genuine `launch_run` to completion and then reads the log back through the public
//! API, with NO subscriber attached for the part that matters.
//!
//! Isolation needs no environment variable: the log root is derived from the store path each `Core` is
//! opened with, so pointing a test at a scratch database is all it takes to keep its history out of
//! everyone else's.

use std::sync::Arc;
use std::time::{Duration, Instant};

use wicked_core::{
    Core, CoreEvent, EntityMode, HumanConfirm, LaunchSpec, StepInput, StepOutput, StepRunner,
    StepStatus,
};
use wicked_council::types::{Category, Confidence, Dispatcher, InputMode, Vote};
use wicked_council::{AgenticCli, CouncilTask};

/// Point the log root at a scratch dir ONCE for the whole binary, and hand back a per-test subdir for
/// the store.
///
/// A per-test scratch directory to hold that test's store (and therefore its `<store>.events` log).
/// Keyed by pid so concurrent `cargo test` invocations cannot collide, and by name so tests running in
/// parallel threads cannot — no process-global state, nothing to restore.
fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir()
        .join(format!("wicked-core-evlog-e2e-{}", std::process::id()))
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
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

struct FirstOptionDispatcher;
impl Dispatcher for FirstOptionDispatcher {
    fn dispatch(&self, c: &AgenticCli, task: &CouncilTask) -> Option<Vote> {
        Some(Vote {
            cli: c.key.clone(),
            recommendation: task
                .options
                .first()
                .cloned()
                .unwrap_or_else(|| c.key.clone()),
            top_risk: "none".into(),
            change_my_mind: "no".into(),
            disqualifier: None,
            confidence: Confidence::default(),
            provenance: "event-log e2e".into(),
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

fn spec(session: &str) -> LaunchSpec {
    LaunchSpec {
        problem: "Do step one. Do step two. Do step three".into(),
        session_id: session.into(),
        clis: vec![cli("a")],
        human_confirm: HumanConfirm::None,
        repo_ref: None,
        entity_mode: EntityMode::Shared,
        workflow: None,
    }
}

/// Block until the run reaches a terminal status, polling the store rather than the event stream —
/// the whole point is that this test does not depend on a live subscriber.
fn wait_terminal(core: &Core, session: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if let Ok(views) = core.sessions_detail() {
            if let Some(v) = views.iter().find(|v| v.session.id == session) {
                let s = format!("{:?}", v.session.status);
                if s.contains("Completed") || s.contains("Failed") || s.contains("Cancelled") {
                    return;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("run `{session}` did not reach a terminal status in 20s");
}

/// The finding, stated as a test: run with NOBODY listening, then read the history back afterwards.
///
/// Under the old fanout-only `emit` this returned an empty vec — events were cloned to zero
/// subscribers and dropped, which is why an evidence bundle assembled after the fact had to re-derive
/// pseudo-events from unit records and shipped 6 invented entries for a 49-event run.
#[test]
fn a_run_with_no_subscriber_still_has_a_readable_event_history_afterwards() {
    let home = scratch("unwatched");
    let db = home.join("estate.db").to_str().unwrap().to_string();
    let core = Core::spawn_with_engine(db, Arc::new(FirstOptionDispatcher), Arc::new(OkRunner));

    // Deliberately NO core.subscribe() before the run.
    core.launch_run(spec("evlog-unwatched")).expect("launch");
    wait_terminal(&core, "evlog-unwatched");

    let events = core.run_events("evlog-unwatched");
    assert!(
        events.len() >= 5,
        "an unwatched run must still leave a real trail, got {}: {events:#?}",
        events.len()
    );

    // Every record carries the envelope an evidence reader depends on.
    for e in &events {
        assert!(
            e.get("type").and_then(|v| v.as_str()).is_some(),
            "every record is tagged: {e}"
        );
        assert_eq!(
            e.get("session").and_then(|v| v.as_str()),
            Some("evlog-unwatched"),
            "every record is attributed to this run: {e}"
        );
        assert!(
            e.get("ts").and_then(|v| v.as_u64()).unwrap_or(0) > 1_600_000_000_000,
            "capture-time epoch millis — nothing in the domain model carries a time value to \
             recover this from later: {e}"
        );
        assert!(e.get("seq").and_then(|v| v.as_u64()).is_some(), "{e}");
    }

    // Ordered, and ordered by the envelope rather than by luck of file order.
    let seqs: Vec<u64> = events
        .iter()
        .map(|e| e["seq"].as_u64().unwrap())
        .collect::<Vec<_>>();
    let mut sorted = seqs.clone();
    sorted.sort_unstable();
    assert_eq!(seqs, sorted, "read back in emission order");

    // The lifecycle skeleton a bundle actually reports on must be present by NAME. These are the
    // names core itself emits (`CoreEvent::to_json`) — the same strings `/ws` carries — so a bundle
    // built on this can no longer invent its own vocabulary the way `routingDecided` was invented.
    let types: Vec<&str> = events.iter().map(|e| e["type"].as_str().unwrap()).collect();
    for required in ["sessionStarted", "unitPlanned", "unitExecuting", "unitDone"] {
        assert!(
            types.contains(&required),
            "`{required}` missing from the recorded history: {types:?}"
        );
    }
    assert!(
        types.iter().filter(|t| **t == "unitPlanned").count() >= 2,
        "one record per planned unit, not one summary row: {types:?}"
    );
}

/// Two runs on one core must not contaminate each other's history — the isolation property that the
/// corpus campaign's cross-org leakage test rests on, asserted at the evidence layer.
#[test]
fn concurrent_runs_do_not_contaminate_each_others_history() {
    let home = scratch("isolation");
    let db = home.join("estate.db").to_str().unwrap().to_string();
    let core = Core::spawn_with_engine(db, Arc::new(FirstOptionDispatcher), Arc::new(OkRunner));

    core.launch_run(spec("evlog-org-a")).expect("launch a");
    core.launch_run(spec("evlog-org-b")).expect("launch b");
    wait_terminal(&core, "evlog-org-a");
    wait_terminal(&core, "evlog-org-b");

    let a = core.run_events("evlog-org-a");
    let b = core.run_events("evlog-org-b");
    assert!(!a.is_empty() && !b.is_empty(), "both runs recorded");
    for (id, evs) in [("evlog-org-a", &a), ("evlog-org-b", &b)] {
        for e in evs.iter() {
            assert_eq!(
                e["session"].as_str(),
                Some(id),
                "run `{id}`'s log contains only its own events: {e}"
            );
        }
    }
    assert!(
        core.run_events("evlog-never-launched").is_empty(),
        "an unknown run reads as an empty history, not an error and not someone else's"
    );
}

/// The log and the live stream must agree. If they could drift, an operator watching a run and an
/// auditor reading it afterwards would be looking at two different accounts of the same events —
/// which is the divergence that let the daemon coin `routingDecided` for something core never called
/// that. One mapping ([`CoreEvent::to_json`]) now feeds both, and this pins that.
#[test]
fn the_recorded_history_matches_what_the_live_stream_carried() {
    let home = scratch("agreement");
    let db = home.join("estate.db").to_str().unwrap().to_string();
    let core = Core::spawn_with_engine(db, Arc::new(FirstOptionDispatcher), Arc::new(OkRunner));

    let rx = core.subscribe();
    core.launch_run(spec("evlog-agree")).expect("launch");
    wait_terminal(&core, "evlog-agree");

    // Collect what the socket saw, using the same exclusions the log applies.
    let mut live: Vec<String> = Vec::new();
    while let Ok(ev) = rx.recv_timeout(Duration::from_millis(200)) {
        let json = ev.to_json();
        let Some(ty) = json.get("type").and_then(|v| v.as_str()) else {
            continue;
        };
        if matches!(
            ty,
            "cliOutputDelta" | "chatDelta" | "terminalOutput" | "heartbeat"
        ) {
            continue;
        }
        if json.get("session").and_then(|v| v.as_str()) == Some("evlog-agree") {
            live.push(ty.to_string());
        }
    }

    let logged: Vec<String> = core
        .run_events("evlog-agree")
        .iter()
        .map(|e| e["type"].as_str().unwrap().to_string())
        .collect();

    assert!(!live.is_empty(), "the subscriber saw something");
    assert_eq!(
        logged, live,
        "the durable history and the live stream must be the same events in the same order — \
         same names, no re-derivation, no invented types"
    );
}

/// A run's history has to outlive the process that produced it. This is the difference between a log
/// and a buffer, and the reason an in-memory ring was rejected: an evidence packet is normally
/// assembled long after the daemon that ran the work has restarted.
#[test]
fn history_survives_the_core_that_wrote_it() {
    let home = scratch("restart");
    let db = home.join("estate.db").to_str().unwrap().to_string();

    let before = {
        let core = Core::spawn_with_engine(
            db.clone(),
            Arc::new(FirstOptionDispatcher),
            Arc::new(OkRunner),
        );
        core.launch_run(spec("evlog-restart")).expect("launch");
        wait_terminal(&core, "evlog-restart");
        core.run_events("evlog-restart")
        // `core` drops here — actor thread torn down, subscribers gone.
    };
    assert!(!before.is_empty());

    let core2 = Core::spawn_with_engine(db, Arc::new(FirstOptionDispatcher), Arc::new(OkRunner));
    let after = core2.run_events("evlog-restart");
    assert_eq!(
        after.len(),
        before.len(),
        "a fresh Core reads the prior Core's complete history"
    );
    assert_eq!(
        after.iter().map(|e| e["seq"].clone()).collect::<Vec<_>>(),
        before.iter().map(|e| e["seq"].clone()).collect::<Vec<_>>(),
        "and in the same order"
    );
}

/// Streaming chunks must not be in the durable history. Not a nicety: a long run emits deltas by the
/// thousand, and letting them in would bury the lifecycle events an evidence packet is made of (and
/// balloon the file) while adding nothing — the content is already persisted as captured work output.
#[test]
fn streamed_output_chunks_are_kept_out_of_the_durable_history() {
    let home = scratch("nodeltas");
    let db = home.join("estate.db").to_str().unwrap().to_string();
    let core = Core::spawn_with_engine(db, Arc::new(FirstOptionDispatcher), Arc::new(OkRunner));

    core.launch_run(spec("evlog-nodeltas")).expect("launch");
    wait_terminal(&core, "evlog-nodeltas");

    let types: Vec<String> = core
        .run_events("evlog-nodeltas")
        .iter()
        .map(|e| e["type"].as_str().unwrap().to_string())
        .collect();
    assert!(
        !types.iter().any(|t| t == "cliOutputDelta"),
        "chunk events excluded: {types:?}"
    );
    assert!(
        types.iter().any(|t| t == "unitDone"),
        "while real lifecycle events are kept: {types:?}"
    );
}

/// Guard against the regression that would make all of the above vacuous: a `CoreEvent` that reaches
/// a subscriber but not the log. Both go through one `EventSink::emit`, so the only way to reintroduce
/// the gap is a second emission path — which this catches by comparing the *set* of run-scoped types
/// seen live against those recorded, over a run that exercises planning, routing, execution and
/// completion.
#[test]
fn no_event_reaches_a_subscriber_without_also_reaching_the_log() {
    let home = scratch("nogap");
    let db = home.join("estate.db").to_str().unwrap().to_string();
    let core = Core::spawn_with_engine(db, Arc::new(FirstOptionDispatcher), Arc::new(OkRunner));

    let rx = core.subscribe();
    core.launch_run(spec("evlog-nogap")).expect("launch");
    wait_terminal(&core, "evlog-nogap");

    let mut live: std::collections::BTreeSet<String> = Default::default();
    while let Ok(ev) = rx.recv_timeout(Duration::from_millis(200)) {
        let j: serde_json::Value = ev.to_json();
        let Some(ty) = j.get("type").and_then(|v| v.as_str()) else {
            continue;
        };
        if j.get("session").and_then(|v| v.as_str()) != Some("evlog-nogap") {
            continue;
        }
        if matches!(
            ty,
            "cliOutputDelta" | "chatDelta" | "terminalOutput" | "heartbeat"
        ) {
            continue;
        }
        live.insert(ty.to_string());
    }
    let logged: std::collections::BTreeSet<String> = core
        .run_events("evlog-nogap")
        .iter()
        .map(|e| e["type"].as_str().unwrap().to_string())
        .collect();

    let missing: Vec<&String> = live.difference(&logged).collect();
    assert!(
        missing.is_empty(),
        "these event types were fanned out live but never recorded — a second emission path has \
         been introduced that bypasses the durable log: {missing:?}"
    );
}

/// Sanity: `CoreEvent::to_json` is reachable from outside core. It has to be public for the daemon to
/// name events the same way core does; if it were private the crew side would be pushed straight back
/// into inventing its own vocabulary.
#[test]
fn to_json_is_part_of_the_public_surface() {
    let j = CoreEvent::SessionCompleted {
        session: "s".into(),
    }
    .to_json();
    assert_eq!(j["type"], "sessionCompleted");
    assert_eq!(j["session"], "s");
}
