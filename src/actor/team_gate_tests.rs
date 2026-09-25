//! DES-TEAMING-002 P1 through the REAL engine: `Core` → plan → the required-transition gates →
//! dispatch, with a real bus db, a temp team outbox and a scaled retry bound (the bound is
//! injectable: `TeamConfig::with_schedule`), so no test sleeps the production 31 s.
//!
//! Each §4.8 row the actor owns has a named fixture here asserting its four columns: (a) what the
//! core store holds, (b) what the bus holds, (c) what references point at, (d) what a later
//! replay/drain does.

use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::{Duration, Instant};

use wicked_council::types::{Category, Confidence, Dispatcher, InputMode, Vote};
use wicked_council::{AgenticCli, CouncilTask};

use super::CONTINUE_WITHOUT_TEAM;
use crate::team::events as tev;
use crate::team::publish::tests::{rig, Rig};
use crate::team::publish::{TeamConfig, TEAM_OUTBOX_FILE};
use crate::workflow::{HumanDecision, StepInput, StepOutput, StepRunner, StepStatus};
use crate::{Core, CoreEvent, HumanConfirm, LaunchSpec, SessionStatus};
use wicked_apps_core::ToNode;

struct StubDispatcher;
impl Dispatcher for StubDispatcher {
    fn dispatch(&self, cli: &AgenticCli, _t: &CouncilTask) -> Option<Vote> {
        Some(Vote {
            cli: cli.key.clone(),
            recommendation: "1".into(),
            top_risk: "none".into(),
            change_my_mind: "no".into(),
            disqualifier: None,
            confidence: Confidence::default(),
            provenance: "stub".into(),
        })
    }
}

/// Counts every worker turn: "no dispatch" is asserted as zero turns.
#[derive(Default)]
struct CountingRunner(AtomicUsize);
impl StepRunner for CountingRunner {
    fn run_unit(&self, i: &StepInput) -> StepOutput {
        self.0.fetch_add(1, AtomicOrdering::SeqCst);
        StepOutput {
            run_id: i.run_id.clone(),
            unit_ix: i.unit_ix,
            attempt: i.attempt,
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

/// A fast bound: 3 retries 40 ms apart, 30 ms per bus write.
fn fast(rig: &Rig) -> TeamConfig {
    TeamConfig::new(Some(rig.bus.clone()), Some(rig.outbox.clone()))
        .with_schedule(vec![Duration::from_millis(40); 3])
        .with_attempt_wait(Duration::from_millis(30))
}

struct Engine {
    core: Core,
    runner: Arc<CountingRunner>,
    events: Receiver<CoreEvent>,
    db: String,
}

fn engine(rig: &Rig, cfg: TeamConfig) -> Engine {
    let db = rig.dir.join("core.db").to_string_lossy().into_owned();
    engine_on(&db, cfg)
}

fn engine_on(db: &str, cfg: TeamConfig) -> Engine {
    let runner = Arc::new(CountingRunner::default());
    let core = Core::spawn_with_engine_team(
        db.to_string(),
        Arc::new(StubDispatcher),
        runner.clone(),
        cfg,
    );
    let events = core.subscribe();
    core.ping();
    Engine {
        core,
        runner,
        events,
        db: db.to_string(),
    }
}

/// Launch a two-unit TEAM run: its def is the engine's composed per-run def (D1's marker).
fn launch_team(e: &Engine, run: &str) {
    let def: crate::workflow::WorkflowDef = serde_json::from_value(serde_json::json!({
        "id": format!("{run}:plan-1"),
        "phases": [
            {"id": "understand", "kind": "build", "gate": "auto"},
            {"id": "build", "kind": "build", "gate": "auto"}
        ]
    }))
    .unwrap();
    e.core
        .register_composed(def)
        .expect("composed def registers");
    e.core
        .launch_run(LaunchSpec {
            base_ref: None,
            project_id: None,
            problem: "p1 team run".into(),
            clis: vec![cli("a"), cli("b")],
            entity_mode: crate::EntityMode::Shared,
            session_id: run.into(),
            human_confirm: HumanConfirm::None,
            auto_deliver: false,
            repo_ref: None,
            workflow: Some(format!("{run}:plan-1")),
            extra_write_roots: Vec::new(),
            extra_read_roots: Vec::new(),
            project_graph: None,
        })
        .expect("launch");
}

/// Poll until `cond` holds, generously (slow Windows runners), returning early.
fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if cond() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting for {what}");
}

fn status(e: &Engine, run: &str) -> Option<SessionStatus> {
    e.core
        .sessions_detail()
        .ok()?
        .into_iter()
        .find(|v| v.session.id == run)
        .map(|v| v.session.status)
}

fn wait_status(e: &Engine, run: &str, want: SessionStatus) {
    wait_for(&format!("{run} to reach {want:?}"), || {
        status(e, run) == Some(want)
    });
}

fn drain_events(e: &Engine) -> Vec<CoreEvent> {
    e.events.try_iter().collect()
}

fn awaiting_kinds(evs: &[CoreEvent], run: &str) -> Vec<String> {
    evs.iter()
        .filter_map(|ev| match ev {
            CoreEvent::AwaitingHuman {
                session, gate_kind, ..
            } if session == run => Some(gate_kind.clone()),
            _ => None,
        })
        .collect()
}

fn resumed(evs: &[CoreEvent], run: &str) -> usize {
    evs.iter()
        .filter(|ev| matches!(ev, CoreEvent::Resumed { session, .. } if session == run))
        .count()
}

fn approve(amend: Option<&str>) -> HumanDecision {
    HumanDecision::Approve {
        amend: amend.map(str::to_string),
        amend_scope: crate::workflow::AmendScope::Cursor,
    }
}

fn outbox_has_run_tombstone(rig: &Rig, run: &str) -> bool {
    rig.outbox_lines()
        .iter()
        .any(|l| l["superseded_run"] == run)
}

fn unit_transports(e: &Engine, run: &str) -> Vec<Option<String>> {
    e.core
        .run_team(run)
        .unwrap()
        .expect("a team run")
        .units
        .into_iter()
        .map(|u| u.transport)
        .collect()
}

// ── §4.8 row 1 / P1 (b), (e) ─────────────────────────────────────────────────────────────────────

/// §4.8 row 1 — `path.started` fails past the bound: (a) `transport: none` + reason persisted on
/// the run and in EVERY unit's snapshot, stamped before the unit's turn (the stamp reads the run's
/// state at dispatch); (b) nothing for the run on the bus, ever; (c) no team row to reference;
/// (d) the `superseded_run` tombstone precedes the store write, and a replay after the bus returns
/// publishes nothing. The run is never armed for the supervisor (`live_team_runs`).
#[test]
fn row1_path_started_fails_past_the_bound_the_run_proceeds_unteamed() {
    let rig = rig("row1");
    rig.refuse(&[]);
    let e = engine(&rig, fast(&rig));
    launch_team(&e, "r1");
    wait_status(&e, "r1", SessionStatus::Completed);
    assert_eq!(e.runner.0.load(AtomicOrdering::SeqCst), 2);
    let view = e.core.run_team("r1").unwrap().expect("a team run");
    assert_eq!(view.transport, "none");
    assert!(
        view.reason
            .as_deref()
            .unwrap_or("")
            .contains(tev::PATH_STARTED),
        "{view:?}"
    );
    assert_eq!(unit_transports(&e, "r1"), vec![Some("none".into()); 2]);
    assert!(outbox_has_run_tombstone(&rig, "r1"));
    assert!(rig.types("r1").is_empty());
    rig.allow();
    let r = e.core.replay_team_outbox().unwrap();
    assert!(r.published.is_empty(), "{r:?}");
    assert!(
        rig.types("r1").is_empty(),
        "replay publishes nothing for the run"
    );
    assert!(e.core.live_team_runs().unwrap().is_empty());
}

/// P1 (b) — no CoreEvent discloses the fallback: the persisted state and the read route do.
#[test]
fn p1_b_no_core_event_is_named_team_transport_disabled() {
    let needle = ["team", "Transport", "Disabled"].concat();
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut hits = Vec::new();
    let mut stack = vec![root.join("src"), root.join("crates")];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let p = entry.unwrap().path();
            if p.is_dir() {
                if p.file_name()
                    .is_some_and(|n| n == "target" || n == "node_modules")
                {
                    continue;
                }
                stack.push(p);
            } else if p
                .extension()
                .is_some_and(|x| x == "rs" || x == "ts" || x == "js")
            {
                let src = std::fs::read_to_string(&p).unwrap_or_default();
                if src.to_lowercase().contains(&needle.to_lowercase()) {
                    hits.push(p.display().to_string());
                }
            }
        }
    }
    assert!(hits.is_empty(), "no `{needle}` event may exist: {hits:?}");
}

// ── §4.8 row 2 / P1 (b), (e) ─────────────────────────────────────────────────────────────────────

/// Launch with `plan.accepted` refused and wait for the `team_transport` pause.
fn paused_on_plan(name: &str) -> (Rig, Engine) {
    let rig = rig(name);
    rig.refuse(&[tev::PLAN_ACCEPTED]);
    let e = engine(&rig, fast(&rig));
    launch_team(&e, name);
    wait_status(&e, name, SessionStatus::AwaitingHuman);
    (rig, e)
}

/// §4.8 row 2 — `plan.accepted` fails past the bound: (a) the pause is durable (session +
/// open gate row, `gate_kind: team_transport`, the reason in the prompt), no unit dispatched;
/// (b) the run's earlier facts (`path.started`) but neither `plan.accepted` nor anything E queued
/// after it; (c) nothing on the bus references the missing plan; (d) approve publishes the queued
/// lines in order and the run proceeds teamed.
#[test]
fn row2_plan_accepted_fails_past_the_bound_the_run_pauses_team_transport() {
    let (rig, e) = paused_on_plan("row2");
    let evs = drain_events(&e);
    assert_eq!(awaiting_kinds(&evs, "row2"), vec!["team_transport"]);
    let prompt = evs
        .iter()
        .find_map(|ev| match ev {
            CoreEvent::AwaitingHuman { prompt, .. } => Some(prompt.clone()),
            _ => None,
        })
        .unwrap();
    assert!(prompt.contains(tev::PLAN_ACCEPTED), "{prompt}");
    assert_eq!(
        e.runner.0.load(AtomicOrdering::SeqCst),
        0,
        "no unit dispatched"
    );
    assert_eq!(rig.types("row2"), vec![tev::PATH_STARTED]);
    let view = e.core.run_team("row2").unwrap().unwrap();
    assert_eq!(view.transport, "bus");
    assert_eq!(view.pending.as_deref(), Some(tev::PLAN_ACCEPTED));

    rig.allow();
    let s = e.core.confirm_gate("row2", approve(None)).unwrap();
    assert_eq!(
        s,
        SessionStatus::Executing,
        "the reply waits for the acknowledgement"
    );
    wait_status(&e, "row2", SessionStatus::Completed);
    // `path.ended` is published after the run completes (not required): wait for it.
    wait_for("path.ended", || {
        rig.types("row2").last().map(String::as_str) == Some(tev::PATH_ENDED)
    });
    assert_eq!(
        rig.types("row2"),
        vec![
            tev::PATH_STARTED,
            tev::PLAN_ACCEPTED,
            tev::GATE_OPENED,
            tev::GATE_DECIDED,
            tev::PATH_ENDED
        ]
    );
    assert_eq!(e.runner.0.load(AtomicOrdering::SeqCst), 2);
    assert_eq!(e.core.run_team("row2").unwrap().unwrap().transport, "bus");
}

/// §4.8 row 2, answered "continue without team" / P1 (e): the tombstone is written before
/// `transport: none` is persisted; the run proceeds un-teamed (every unit `transport: none`);
/// once the bus returns, the live drain and `replay_team_outbox` publish ZERO rows for the run.
#[test]
fn row2_continue_without_team_publishes_nothing_more_for_the_run() {
    let (rig, e) = paused_on_plan("row2c");
    let s = e
        .core
        .confirm_gate("row2c", approve(Some(super::CONTINUE_WITHOUT_TEAM)))
        .unwrap();
    assert_eq!(s, SessionStatus::Executing);
    wait_status(&e, "row2c", SessionStatus::Completed);
    assert!(outbox_has_run_tombstone(&rig, "row2c"));
    let view = e.core.run_team("row2c").unwrap().unwrap();
    assert_eq!(view.transport, "none");
    assert_eq!(unit_transports(&e, "row2c"), vec![Some("none".into()); 2]);
    let before = rig.types("row2c");
    assert_eq!(before, vec![tev::PATH_STARTED]);
    rig.allow();
    let r = e.core.replay_team_outbox().unwrap();
    assert!(r.published.is_empty(), "{r:?}");
    assert_eq!(rig.types("row2c"), before, "zero rows after the fallback");
}

/// §4.8 row 2, answered reject / P1 (e): tombstone, then cancel; only `path.ended` is published
/// for the rejected run once the bus returns.
#[test]
fn row2_reject_publishes_only_path_ended() {
    let (rig, e) = paused_on_plan("row2r");
    let s = e.core.confirm_gate("row2r", HumanDecision::Reject).unwrap();
    assert_eq!(s, SessionStatus::Cancelled);
    assert!(outbox_has_run_tombstone(&rig, "row2r"));
    rig.allow();
    e.core.replay_team_outbox().unwrap();
    wait_for("path.ended on the bus", || {
        rig.types("row2r") == vec![tev::PATH_STARTED, tev::PATH_ENDED]
    });
    e.core.replay_team_outbox().unwrap();
    assert_eq!(rig.types("row2r"), vec![tev::PATH_STARTED, tev::PATH_ENDED]);
    assert_eq!(e.runner.0.load(AtomicOrdering::SeqCst), 0);
}

// ── §4.8 rows 3 and 4 / P1 (b), (g) ──────────────────────────────────────────────────────────────

/// §4.8 row 3 / P1 (g) — `gate.opened` fails past the bound: (a) the pause is durable as every
/// pause; (b) not `gate.opened`, nor E's later facts; (c) `gate.decided` cannot land first, so it
/// never dangles — checked while the answer retries, and after replay; (d) it lands in order when
/// the bus returns.
#[test]
fn row3_gate_opened_fails_gate_decided_never_lands_before_it() {
    let (rig, e) = paused_on_plan("row3");
    // The plan fact may now land, but the gate's own gate.opened is refused.
    rig.refuse(&[tev::GATE_OPENED]);
    let s = e.core.confirm_gate("row3", approve(None)).unwrap();
    assert_eq!(
        s,
        SessionStatus::AwaitingHuman,
        "still paused: the decision cannot land"
    );
    let t = rig.types("row3");
    assert!(!t.contains(&tev::GATE_DECIDED.to_string()), "{t:?}");
    assert!(!t.contains(&tev::GATE_OPENED.to_string()), "{t:?}");
    e.core.replay_team_outbox().unwrap();
    let t = rig.types("row3");
    assert!(
        !t.contains(&tev::GATE_DECIDED.to_string()),
        "after replay: {t:?}"
    );
    rig.allow();
    e.core.replay_team_outbox().unwrap();
    let t = rig.types("row3");
    let opened = t
        .iter()
        .position(|x| x == tev::GATE_OPENED)
        .expect("opened");
    let decided = t
        .iter()
        .position(|x| x == tev::GATE_DECIDED)
        .expect("decided");
    assert!(opened < decided, "{t:?}");
    assert_eq!(e.runner.0.load(AtomicOrdering::SeqCst), 0);
}

/// §4.8 row 4 / P1 (b) — `gate.decided` fails past the bound: (a) the decision is in the resolved
/// interaction row and the run STAYS paused `team_transport` (a fresh gate); no `Resumed`, no
/// dispatch; (b) `gate.opened` but not its decision; (c) nothing references the missing decision;
/// (d) as row 2 — a later approve lands it and the run proceeds.
#[test]
fn row4_gate_decided_fails_past_the_bound_the_run_stays_paused() {
    let (rig, e) = paused_on_plan("row4");
    rig.refuse(&[tev::GATE_DECIDED]);
    drain_events(&e);
    let s = e.core.confirm_gate("row4", approve(None)).unwrap();
    assert_eq!(s, SessionStatus::AwaitingHuman);
    assert_eq!(status(&e, "row4"), Some(SessionStatus::AwaitingHuman));
    let evs = drain_events(&e);
    assert_eq!(resumed(&evs, "row4"), 0, "no Resumed");
    assert_eq!(awaiting_kinds(&evs, "row4"), vec!["team_transport"]);
    assert_eq!(e.runner.0.load(AtomicOrdering::SeqCst), 0, "no dispatch");
    let t = rig.types("row4");
    assert!(t.contains(&tev::GATE_OPENED.to_string()), "{t:?}");
    assert!(!t.contains(&tev::GATE_DECIDED.to_string()), "{t:?}");
    rig.allow();
    assert_eq!(
        e.core.confirm_gate("row4", approve(None)).unwrap(),
        SessionStatus::Executing
    );
    wait_status(&e, "row4", SessionStatus::Completed);
    assert!(resumed(&drain_events(&e), "row4") >= 1);
}

// ── §4.8 row 6 ───────────────────────────────────────────────────────────────────────────────────

/// §4.8 row 6 — bus absent at boot: (a) `transport: none` on the run, snapshots built locally
/// with `ledger_source: no_bus`; (b) nothing; (c) no `gate.opened` is published; (d) nothing —
/// no line is generated or spooled without a bus (the outbox is never created).
#[test]
fn row6_bus_absent_at_boot_the_run_is_unteamed_and_nothing_spools() {
    let rig = rig("row6");
    let cfg = TeamConfig::new(None, Some(rig.outbox.clone()));
    let e = engine(&rig, cfg);
    launch_team(&e, "row6");
    wait_status(&e, "row6", SessionStatus::Completed);
    let view = e.core.run_team("row6").unwrap().unwrap();
    assert_eq!(view.transport, "none");
    assert!(view.reason.unwrap().contains("no bus"));
    assert!(view
        .units
        .iter()
        .all(|u| u.transport.as_deref() == Some("none")
            && u.ledger_source.as_deref() == Some("no_bus")));
    assert!(!rig.outbox.exists(), "nothing spooled without a bus");
    assert!(rig.types("row6").is_empty());
    assert!(
        e.core.replay_team_outbox().is_err(),
        "no bus to replay onto"
    );
}

// ── §4.8 row 12 / P1 (f) ─────────────────────────────────────────────────────────────────────────

/// Leave a store whose team run waits on `path.started` (spooled, bus refusing, a long bound),
/// then drop the engine — the daemon died before the fallback was persisted.
fn crashed_mid_path_started(rig: &Rig, run: &str) -> String {
    rig.refuse(&[]);
    let slow = TeamConfig::new(Some(rig.bus.clone()), Some(rig.outbox.clone()))
        .with_schedule(vec![Duration::from_secs(600)])
        .with_attempt_wait(Duration::from_millis(30));
    let e = engine(rig, slow);
    launch_team(&e, run);
    wait_for("path.started spooled", || {
        rig.outbox_lines()
            .iter()
            .any(|l| l["type"] == tev::PATH_STARTED)
    });
    let db = e.db.clone();
    let view = e.core.run_team(run).unwrap().unwrap();
    assert_eq!(view.transport, "pending");
    drop(e);
    db
}

/// §4.8 row 12 / P1 (f) — crash between the tombstone and the store write: (a) on boot the run is
/// `transport: none`; (b) nothing for the superseded facts; (c) none; (d) the tombstone is already
/// written ⇒ the boot drain and `replay_team_outbox` publish nothing for the run.
#[test]
fn row12_crash_between_tombstone_and_store_write_boots_unteamed() {
    let rig = rig("row12");
    let db = crashed_mid_path_started(&rig, "row12");
    // The tombstone reached the outbox; the store write did not.
    crate::team::publish::supersede_run_at(&rig.outbox, "row12", tev::PATH_STARTED, "bound")
        .unwrap();
    let e = engine_on(&db, fast(&rig));
    rig.allow();
    let view = e.core.run_team("row12").unwrap().unwrap();
    assert_eq!(view.transport, "none");
    e.core.replay_team_outbox().unwrap();
    assert!(rig.types("row12").is_empty(), "{:?}", rig.types("row12"));
}

/// P1 (f), the earlier crash: no tombstone yet either. Boot writes it BEFORE persisting
/// `transport: none`, so the boot drain and a replay publish nothing for the run.
#[test]
fn p1_f_crash_before_the_tombstone_boot_writes_it_first() {
    let rig = rig("f");
    let db = crashed_mid_path_started(&rig, "rf");
    assert!(!outbox_has_run_tombstone(&rig, "rf"));
    let e = engine_on(&db, fast(&rig));
    assert!(outbox_has_run_tombstone(&rig, "rf"));
    rig.allow();
    assert_eq!(e.core.run_team("rf").unwrap().unwrap().transport, "none");
    e.core.replay_team_outbox().unwrap();
    assert!(rig.types("rf").is_empty());
}

// ── P1 (c) ───────────────────────────────────────────────────────────────────────────────────────

/// P1 (c) — the actor thread never blocks on a publish: another connection holds the bus
/// EXCLUSIVE for longer than the whole retry bound (the acceptance's 60 s, scaled with the
/// bound) while a team run waits on its `path.started`; every `ping` and `subscribe` round-trip
/// through the actor is answered promptly the whole time.
#[test]
fn p1_c_the_actor_answers_while_the_bus_is_locked() {
    let rig = rig("c");
    let cfg = TeamConfig::new(Some(rig.bus.clone()), Some(rig.outbox.clone()))
        .with_schedule(vec![Duration::from_millis(300); 4])
        .with_attempt_wait(Duration::from_millis(250));
    // Held for at least twice the bound AND until the run has fallen back (so the lock outlives
    // every retry however slow the runner), capped generously.
    let min_hold = cfg.bound() * 2;
    let e = engine(&rig, cfg);
    let holder = rusqlite::Connection::open(&rig.bus).unwrap();
    holder.execute_batch("BEGIN EXCLUSIVE;").unwrap();
    launch_team(&e, "rc");
    let start = Instant::now();
    let mut worst = Duration::ZERO;
    let mut rounds = 0u32;
    loop {
        let t = Instant::now();
        e.core.ping();
        let _sub = e.core.subscribe();
        e.core.sessions().unwrap();
        let fell_back = e
            .core
            .run_team("rc")
            .ok()
            .flatten()
            .is_some_and(|v| v.transport == "none");
        worst = worst.max(t.elapsed());
        rounds += 1;
        if (start.elapsed() >= min_hold && fell_back) || start.elapsed() > Duration::from_secs(60) {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    holder.execute_batch("COMMIT;").unwrap();
    assert!(
        rounds > 10,
        "the actor was probed throughout ({rounds} rounds)"
    );
    assert!(
        worst < Duration::from_secs(2),
        "an actor round-trip took {worst:?} while the bus was locked"
    );
    wait_status(&e, "rc", SessionStatus::Completed);
    assert_eq!(e.core.run_team("rc").unwrap().unwrap().transport, "none");
}

/// The outbox a test engine writes is the one it was handed — never a real home's.
#[test]
fn test_engines_write_only_their_temp_outbox() {
    let (rig, e) = paused_on_plan("hyg");
    drop(e);
    assert_eq!(rig.outbox.file_name().unwrap(), TEAM_OUTBOX_FILE);
    assert!(rig.outbox.starts_with(std::env::temp_dir()));
    assert!(!rig.outbox_lines().is_empty());
}

/// A run cancelled while its `path.started` is in flight: the cancel tombstones the run, so the
/// fact never lands (no path that will not end), and a late acknowledgement dispatches nothing.
#[test]
fn a_run_cancelled_while_its_fact_is_in_flight_dispatches_nothing() {
    let rig = rig("cancelled");
    rig.refuse(&[]);
    let cfg = TeamConfig::new(Some(rig.bus.clone()), Some(rig.outbox.clone()))
        .with_schedule(vec![Duration::from_millis(200); 3])
        .with_attempt_wait(Duration::from_millis(30));
    let e = engine(&rig, cfg);
    launch_team(&e, "rx");
    wait_for("the run to wait on path.started", || {
        e.core
            .run_team("rx")
            .ok()
            .flatten()
            .and_then(|v| v.pending)
            .as_deref()
            == Some(tev::PATH_STARTED)
    });
    assert_eq!(e.core.cancel_run("rx").unwrap(), SessionStatus::Cancelled);
    wait_for("the cancel's run tombstone", || {
        outbox_has_run_tombstone(&rig, "rx")
    });
    rig.allow();
    // Past every retry of the in-flight fact (3 × 200 ms), and a replay on top.
    std::thread::sleep(Duration::from_millis(900));
    e.core.replay_team_outbox().unwrap();
    e.core.ping();
    assert!(rig.types("rx").is_empty(), "{:?}", rig.types("rx"));
    assert_eq!(status(&e, "rx"), Some(SessionStatus::Cancelled));
    assert_eq!(e.runner.0.load(AtomicOrdering::SeqCst), 0);
}

// ── Boot reconcile: an answer recorded before a crash is FINISHED, never asked again ─────────────

/// Leave the store as a crash leaves it right after `answer_transport_gate` persisted
/// `Superseding` (the gate row answered) and before the publisher's `TeamSuperseded` came back:
/// no tombstone in the outbox yet. `reject` picks the recorded answer.
fn crashed_mid_answer(name: &str, reject: bool) -> (Rig, String) {
    let (rig, e) = paused_on_plan(name);
    let db = e.db.clone();
    drop(e);
    let mut store = wicked_apps_core::open_store_any(Some(&db)).expect("store opens");
    let mut session = crate::domain::get_session(&store, name).unwrap().unwrap();
    let team = session.team.as_mut().expect("team state");
    let pending = team.pending.as_mut().expect("paused on plan.accepted");
    assert_eq!(pending.stage, crate::domain::PendingStage::Paused);
    pending.stage = crate::domain::PendingStage::Superseding;
    if reject {
        pending.then = crate::domain::TeamBlocked::Cancel;
    }
    crate::domain::put_node(&mut store, session.to_node()).unwrap();
    let answer = if reject {
        r#"{"approve":false,"action":"reject","amend":null}"#.to_string()
    } else {
        format!(r#"{{"approve":true,"action":"approve","amend":"{CONTINUE_WITHOUT_TEAM}"}}"#)
    };
    crate::interaction::resolve_open_for_session(
        &mut store,
        name,
        crate::interaction::InteractionStatus::Answered,
        Some(answer),
        crate::interaction::now_millis(),
    )
    .unwrap();
    drop(store);
    assert!(!outbox_has_run_tombstone(&rig, name));
    (rig, db)
}

fn open_team_gates(db: &str, run: &str) -> usize {
    let store = wicked_apps_core::open_store_any(Some(db)).expect("store opens");
    crate::interaction::list_interactions(
        &store,
        Some(run),
        Some(crate::interaction::InteractionStatus::Open),
    )
    .unwrap()
    .len()
}

fn amended_units(e: &Engine, run: &str) -> usize {
    e.core
        .sessions_detail()
        .unwrap()
        .into_iter()
        .filter(|v| v.session.id == run)
        .flat_map(|v| v.units)
        .filter(|u| u.description.contains("operator amendment"))
        .count()
}

/// Boot after a crash mid "continue without team": the recorded answer is finished — tombstone
/// re-issued FIRST, `transport: none` persisted, no gate opened (so the generic confirm path can
/// never take the transport answer as a unit amendment); the run resumes un-teamed like any run
/// the restart orphaned, and the row-12 rule holds: a replay publishes nothing more for it.
#[test]
fn boot_finishes_a_continue_without_team_answer_recorded_before_the_crash() {
    let (rig, db) = crashed_mid_answer("bootc", false);
    let e = engine_on(&db, fast(&rig));
    assert!(outbox_has_run_tombstone(&rig, "bootc"));
    let view = e.core.run_team("bootc").unwrap().unwrap();
    assert_eq!(view.transport, "none", "{view:?}");
    assert_eq!(view.pending, None);
    assert_eq!(open_team_gates(&db, "bootc"), 0, "no gate is asked again");
    assert_ne!(status(&e, "bootc"), Some(SessionStatus::AwaitingHuman));
    assert!(
        e.core
            .confirm_gate("bootc", approve(Some(CONTINUE_WITHOUT_TEAM)))
            .is_err(),
        "no gate to answer"
    );
    // The daemon restarted, so the run resumes the way every orphaned run does.
    e.core.resume_run("bootc").unwrap();
    wait_status(&e, "bootc", SessionStatus::Completed);
    assert_eq!(
        amended_units(&e, "bootc"),
        0,
        "no generic amendment on any unit"
    );
    assert_eq!(unit_transports(&e, "bootc"), vec![Some("none".into()); 2]);
    rig.allow();
    e.core.replay_team_outbox().unwrap();
    assert_eq!(rig.types("bootc"), vec![tev::PATH_STARTED]);
}

/// Boot after a crash mid reject: the recorded answer is finished — tombstone first, then the
/// run is cancelled at boot with no new gate; only `path.ended` joins `path.started` on the bus.
#[test]
fn boot_finishes_a_reject_answer_recorded_before_the_crash() {
    let (rig, db) = crashed_mid_answer("bootr", true);
    let e = engine_on(&db, fast(&rig));
    assert!(outbox_has_run_tombstone(&rig, "bootr"));
    assert_eq!(status(&e, "bootr"), Some(SessionStatus::Cancelled));
    assert_eq!(open_team_gates(&db, "bootr"), 0);
    assert_eq!(e.runner.0.load(AtomicOrdering::SeqCst), 0);
    rig.allow();
    e.core.replay_team_outbox().unwrap();
    wait_for("path.ended", || {
        rig.types("bootr") == vec![tev::PATH_STARTED, tev::PATH_ENDED]
    });
}

/// Boot after a crash while a REQUIRED fact was still publishing (no answer recorded): the pause
/// re-opens WITH its pending fact kept paused, so `answer_transport_gate` — never the generic
/// confirm path — takes the reply.
#[test]
fn boot_reopens_a_publishing_fact_as_a_transport_gate_the_team_handler_answers() {
    let rig = rig("bootp");
    rig.refuse(&[tev::PLAN_ACCEPTED]);
    let slow = TeamConfig::new(Some(rig.bus.clone()), Some(rig.outbox.clone()))
        .with_schedule(vec![Duration::from_secs(600)])
        .with_attempt_wait(Duration::from_millis(30));
    let e = engine(&rig, slow);
    launch_team(&e, "bootp");
    wait_for("plan.accepted in flight", || {
        e.core
            .run_team("bootp")
            .ok()
            .flatten()
            .and_then(|v| v.pending)
            .as_deref()
            == Some(tev::PLAN_ACCEPTED)
    });
    let db = e.db.clone();
    drop(e);
    let e = engine_on(&db, fast(&rig));
    assert_eq!(status(&e, "bootp"), Some(SessionStatus::AwaitingHuman));
    let view = e.core.run_team("bootp").unwrap().unwrap();
    assert_eq!(view.pending.as_deref(), Some(tev::PLAN_ACCEPTED));
    assert!(
        e.core
            .confirm_gate("bootp", HumanDecision::RequestChanges { note: None })
            .is_err(),
        "the team handler refuses request-changes on a transport gate"
    );
    let s = e
        .core
        .confirm_gate("bootp", approve(Some(CONTINUE_WITHOUT_TEAM)))
        .unwrap();
    assert_eq!(s, SessionStatus::Executing);
    wait_status(&e, "bootp", SessionStatus::Completed);
    assert_eq!(amended_units(&e, "bootp"), 0);
    assert_eq!(e.core.run_team("bootp").unwrap().unwrap().transport, "none");
}

// ── Boot reconcile covers TERMINAL team runs too (#623 review round 3) ───────────────────────────

/// Set `run`'s persisted status, as a terminal transition's `put_node` leaves it.
fn set_status(db: &str, run: &str, to: SessionStatus) {
    let mut store = wicked_apps_core::open_store_any(Some(db)).expect("store opens");
    let mut s = crate::domain::get_session(&store, run).unwrap().unwrap();
    s.status = to;
    crate::domain::put_node(&mut store, s.to_node()).unwrap();
}

fn path_ended_statuses(rig: &Rig, run: &str) -> Vec<String> {
    let c = rig.conn();
    let mut st = c
        .prepare("SELECT payload FROM events WHERE event_type = ?1 ORDER BY event_id")
        .unwrap();
    st.query_map([tev::PATH_ENDED], |r| r.get::<_, String>(0))
        .unwrap()
        .filter_map(|r| r.ok())
        .map(|p| serde_json::from_str::<serde_json::Value>(&p).unwrap())
        .filter(|p| p["run_id"] == run)
        .map(|p| p["status"].as_str().unwrap_or("").to_string())
        .collect()
}

/// Crash between a terminal `put_node(Cancelled)` and the publisher writing the run tombstone,
/// while the run's `path.started` is still spooled: boot tombstones the terminal run BEFORE the
/// boot drain, so neither the drain nor a replay opens a path that never ends.
#[test]
fn boot_tombstones_a_run_cancelled_before_its_tombstone_was_written() {
    let rig = rig("bootx");
    let db = crashed_mid_path_started(&rig, "bootx");
    set_status(&db, "bootx", SessionStatus::Cancelled);
    assert!(!outbox_has_run_tombstone(&rig, "bootx"));
    rig.allow();
    let e = engine_on(&db, fast(&rig));
    assert!(outbox_has_run_tombstone(&rig, "bootx"));
    e.core.ping();
    e.core.replay_team_outbox().unwrap();
    std::thread::sleep(Duration::from_millis(200));
    assert!(rig.types("bootx").is_empty(), "{:?}", rig.types("bootx"));
    assert!(e.core.run_team("bootx").unwrap().unwrap().ended);
}

/// A teamed run reached `status` and the crash came before its `path.started` was followed by
/// `path.ended` (nothing spooled): boot publishes exactly ONE `path.ended` with that status, and
/// marks the run ended so a later boot publishes nothing more.
fn boot_ends_a_teamed_terminal_run(name: &str, status: SessionStatus, want: &str) {
    let rig = rig(name);
    rig.refuse(&[tev::PATH_ENDED]);
    // A retry schedule this engine never reaches: its publisher cannot land path.ended later.
    let slow = TeamConfig::new(Some(rig.bus.clone()), Some(rig.outbox.clone()))
        .with_schedule(vec![Duration::from_secs(600)])
        .with_attempt_wait(Duration::from_millis(30));
    let e = engine(&rig, slow);
    launch_team(&e, name);
    wait_status(&e, name, SessionStatus::Completed);
    wait_for("path.ended spooled", || {
        rig.outbox_lines()
            .iter()
            .any(|l| l["type"] == tev::PATH_ENDED)
    });
    let db = e.db.clone();
    drop(e);
    if status != SessionStatus::Completed {
        set_status(&db, name, status);
    }
    // The crash came before path.ended was even spooled.
    let _ = std::fs::remove_file(&rig.outbox);
    rig.allow();
    assert!(path_ended_statuses(&rig, name).is_empty());
    let e = engine_on(&db, fast(&rig));
    wait_for("path.ended", || !path_ended_statuses(&rig, name).is_empty());
    wait_for("the run marked ended", || {
        e.core.run_team(name).unwrap().unwrap().ended
    });
    drop(e);
    let e = engine_on(&db, fast(&rig));
    e.core.ping();
    e.core.replay_team_outbox().unwrap();
    assert_eq!(path_ended_statuses(&rig, name), vec![want.to_string()]);
    assert!(
        rig.outbox_lines().is_empty(),
        "a later boot spools nothing more"
    );
}

#[test]
fn boot_publishes_one_path_ended_for_a_completed_teamed_run() {
    boot_ends_a_teamed_terminal_run("bootok", SessionStatus::Completed, "completed");
}

#[test]
fn boot_publishes_one_path_ended_for_a_failed_teamed_run() {
    boot_ends_a_teamed_terminal_run("bootfail", SessionStatus::Failed, "failed");
}

// ── A held confirm_gate reply never outlives its run (#623 review round 4) ───────────────────────

/// Answer approve on `run`'s `team_transport` pause from another thread while the test holds
/// the publisher off the outbox (its lock): the reply is HELD waiting on the ack.
fn hold_reply(e: &Engine, run: &str) -> std::sync::mpsc::Receiver<anyhow::Result<SessionStatus>> {
    let (tx, rx) = std::sync::mpsc::channel();
    let core = e.core.clone();
    let run = run.to_string();
    std::thread::spawn(move || {
        let _ = tx.send(core.confirm_gate(&run, approve(None)));
    });
    wait_for("the reply to be held", || e.core.held_team_replies() == 1);
    rx
}

/// Cancelled while a team_transport approve waits on the publisher: the held reply is settled
/// from the run's durable status at once (Ok(Cancelled)), never left to hang, and the actor holds
/// no reply afterwards.
#[test]
fn a_held_reply_is_settled_when_the_run_is_cancelled_before_the_ack() {
    let (rig, e) = paused_on_plan("held-c");
    let m = crate::team::publish::outbox_mutex(&rig.outbox);
    let lock = m.lock().unwrap();
    let rx = hold_reply(&e, "held-c");
    assert_eq!(
        e.core.cancel_run("held-c").unwrap(),
        SessionStatus::Cancelled
    );
    let got = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("the held reply is answered promptly");
    assert_eq!(got.unwrap(), SessionStatus::Cancelled);
    assert_eq!(e.core.held_team_replies(), 0);
    drop(lock);
}

/// The same when the run reaches Failed by any other path: the durable status settles the reply
/// on the actor's next command.
#[test]
fn a_held_reply_is_settled_when_the_run_fails_before_the_ack() {
    let (rig, e) = paused_on_plan("held-f");
    let m = crate::team::publish::outbox_mutex(&rig.outbox);
    let lock = m.lock().unwrap();
    let rx = hold_reply(&e, "held-f");
    set_status(&e.db, "held-f", SessionStatus::Failed);
    e.core.ping();
    let got = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("the held reply is answered promptly");
    assert_eq!(got.unwrap(), SessionStatus::Failed);
    assert_eq!(e.core.held_team_replies(), 0);
    drop(lock);
}

// ── Absence never skips a tombstone (#623 review round 5): the outbox belongs to the state home ──

/// A config with NO bus (WICKED_BUS_DB unset) but the same state-home outbox.
fn no_bus(rig: &Rig) -> TeamConfig {
    TeamConfig::new(None, Some(rig.outbox.clone()))
}

/// Replace the outbox file with a directory (every write to it fails), keeping the file aside.
fn break_outbox(rig: &Rig) -> std::path::PathBuf {
    let aside = rig.outbox.with_extension("aside");
    let _ = std::fs::rename(&rig.outbox, &aside);
    std::fs::create_dir_all(&rig.outbox).unwrap();
    aside
}

fn mend_outbox(rig: &Rig, aside: &std::path::Path) {
    std::fs::remove_dir_all(&rig.outbox).unwrap();
    let _ = std::fs::rename(aside, &rig.outbox);
}

/// Recorded continue-without-team, restart with NO bus: the tombstone is still written (the
/// outbox is the state home's), so when the bus returns a drain publishes nothing more.
#[test]
fn nobus_boot_writes_the_tombstone_for_a_recorded_continue() {
    let (rig, db) = crashed_mid_answer("nbc", false);
    let e = engine_on(&db, no_bus(&rig));
    assert!(outbox_has_run_tombstone(&rig, "nbc"));
    assert_eq!(e.core.run_team("nbc").unwrap().unwrap().transport, "none");
    rig.allow();
    rig.team_bus().drain_all();
    assert_eq!(rig.types("nbc"), vec![tev::PATH_STARTED]);
}

/// Recorded reject, restart with NO bus: tombstone, cancel, and the run's `path.ended` spooled,
/// so when the bus returns only `path.ended` joins `path.started`.
#[test]
fn nobus_boot_writes_the_tombstone_for_a_recorded_reject() {
    let (rig, db) = crashed_mid_answer("nbr", true);
    let e = engine_on(&db, no_bus(&rig));
    assert!(outbox_has_run_tombstone(&rig, "nbr"));
    assert_eq!(status(&e, "nbr"), Some(SessionStatus::Cancelled));
    rig.allow();
    rig.team_bus().drain_all();
    assert_eq!(rig.types("nbr"), vec![tev::PATH_STARTED, tev::PATH_ENDED]);
}

/// The tombstone cannot be written at boot (the outbox path is unwritable): the recorded answer
/// is NOT finished; the pause stays open and the team handler answers it.
#[test]
fn boot_keeps_the_pause_when_the_tombstone_cannot_be_written() {
    let (rig, db) = crashed_mid_answer("nbx", false);
    let aside = break_outbox(&rig);
    let e = engine_on(&db, no_bus(&rig));
    assert_eq!(status(&e, "nbx"), Some(SessionStatus::AwaitingHuman));
    let view = e.core.run_team("nbx").unwrap().unwrap();
    assert_eq!(
        view.pending.as_deref(),
        Some(tev::PLAN_ACCEPTED),
        "{view:?}"
    );
    assert_ne!(view.transport, "none");
    assert!(
        e.core
            .confirm_gate("nbx", HumanDecision::RequestChanges { note: None })
            .is_err(),
        "the team handler owns the reopened pause"
    );
    drop(e);
    mend_outbox(&rig, &aside);
}

/// Paused, restart with NO bus, answered continue-without-team: the actor writes the tombstone
/// itself (there is no publisher), then runs un-teamed; the bus's return publishes nothing more.
#[test]
fn nobus_answer_writes_the_tombstone_before_continuing() {
    let (rig, e) = paused_on_plan("nba");
    let db = e.db.clone();
    drop(e);
    let e = engine_on(&db, no_bus(&rig));
    let s = e
        .core
        .confirm_gate("nba", approve(Some(CONTINUE_WITHOUT_TEAM)))
        .unwrap();
    assert_eq!(s, SessionStatus::Executing);
    assert!(outbox_has_run_tombstone(&rig, "nba"));
    wait_status(&e, "nba", SessionStatus::Completed);
    rig.allow();
    rig.team_bus().drain_all();
    assert_eq!(rig.types("nba"), vec![tev::PATH_STARTED]);
}

/// The same answer when the tombstone cannot be written: the run stays paused `team_transport`
/// (the team handler's pause), never continues without its tombstone.
#[test]
fn nobus_answer_keeps_the_pause_when_the_tombstone_cannot_be_written() {
    let (rig, e) = paused_on_plan("nbw");
    let db = e.db.clone();
    drop(e);
    let aside = break_outbox(&rig);
    let e = engine_on(&db, no_bus(&rig));
    let s = e
        .core
        .confirm_gate("nbw", approve(Some(CONTINUE_WITHOUT_TEAM)))
        .unwrap();
    assert_eq!(s, SessionStatus::AwaitingHuman);
    assert_eq!(status(&e, "nbw"), Some(SessionStatus::AwaitingHuman));
    let view = e.core.run_team("nbw").unwrap().unwrap();
    assert_ne!(view.transport, "none", "{view:?}");
    assert!(view.pending.is_some());
    assert_eq!(e.runner.0.load(AtomicOrdering::SeqCst), 0);
    drop(e);
    mend_outbox(&rig, &aside);
}

/// The boot drain publishes only lines of runs the boot reconcile READ: a line of a run the store
/// does not know (or could not read) stays for an explicit replay, never goes out unreconciled.
#[test]
fn the_boot_drain_skips_lines_of_runs_it_did_not_reconcile() {
    let rig = rig("ghost");
    rig.refuse(&[]);
    rig.team_bus()
        .publish(&crate::team::publish::tests::fixture(
            tev::PATH_STARTED,
            0,
            "ghost-run",
        ))
        .unwrap();
    rig.allow();
    let e = engine(&rig, fast(&rig));
    e.core.ping();
    std::thread::sleep(Duration::from_millis(500));
    e.core.ping();
    assert!(
        rig.types("ghost-run").is_empty(),
        "{:?}",
        rig.types("ghost-run")
    );
    // The operator's explicit replay still publishes it.
    e.core.replay_team_outbox().unwrap();
    assert_eq!(rig.types("ghost-run"), vec![tev::PATH_STARTED]);
}
