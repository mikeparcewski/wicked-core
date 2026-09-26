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

/// A fast bound: 3 retries 40 ms apart, 30 ms per bus write, a 300 ms final-pass budget.
fn fast(rig: &Rig) -> TeamConfig {
    TeamConfig::new(Some(rig.bus.clone()), Some(rig.outbox.clone()))
        .with_schedule(vec![Duration::from_millis(40); 3])
        .with_attempt_wait(Duration::from_millis(30))
        // T5: a teamed unit's worker waits for S's `ledger.folded`; with no supervisor on the
        // bus in these fixtures it synthesizes after this budget (an empty ledger: no pause).
        .with_final_pass_budget(Duration::from_millis(300))
        .with_gate_poll(Duration::from_millis(20))
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
    launch_team_with(e, run, HumanConfirm::None);
}

fn launch_team_with(e: &Engine, run: &str, human_confirm: HumanConfirm) {
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
            human_confirm,
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
    // The engine's facts in FIFO order (the team_transport gate, then T5's unit-review gate of
    // each unit); each unit's own R facts (T5) ride the attempt's lane beside them.
    let types = rig.types("row2");
    let engine_facts: Vec<&str> = types
        .iter()
        .map(String::as_str)
        .filter(|t| *t != tev::STEP_CLAIMED && *t != tev::STEP_COMPLETED)
        .collect();
    assert_eq!(
        engine_facts,
        vec![
            tev::PATH_STARTED,
            tev::PLAN_ACCEPTED,
            tev::GATE_OPENED,
            tev::GATE_DECIDED,
            tev::GATE_OPENED,
            tev::GATE_DECIDED,
            tev::GATE_OPENED,
            tev::GATE_DECIDED,
            tev::PATH_ENDED
        ]
    );
    assert_eq!(
        types.iter().filter(|t| *t == tev::STEP_CLAIMED).count(),
        2,
        "one step.claimed per unit: {types:?}"
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
        .with_final_pass_budget(Duration::from_millis(300))
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
        .with_final_pass_budget(Duration::from_millis(300))
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
        .with_final_pass_budget(Duration::from_millis(300))
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
        .with_final_pass_budget(Duration::from_millis(300))
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
        .with_final_pass_budget(Duration::from_millis(300))
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

// ── Bus present at launch, absent now (#623 review round 6) ──────────────────────────────────────

/// A TEAMED run left live with no pending fact: path.started and plan.accepted acknowledged,
/// then paused at an ordinary run-level gate before unit 0. Returns the rig and its db.
fn live_teamed(name: &str) -> (Rig, String) {
    let rig = rig(name);
    let e = engine(&rig, fast(&rig));
    launch_team_with(&e, name, HumanConfirm::All);
    wait_status(&e, name, SessionStatus::AwaitingHuman);
    let view = e.core.run_team(name).unwrap().unwrap();
    assert_eq!(view.transport, "bus");
    assert_eq!(view.pending, None);
    assert_eq!(e.core.live_team_runs().unwrap().len(), 1);
    assert_eq!(rig.types(name), vec![tev::PATH_STARTED, tev::PLAN_ACCEPTED]);
    let db = e.db.clone();
    drop(e);
    (rig, db)
}

/// Restart with NO bus while a teamed run is live: this process cannot publish its required
/// facts, so boot pauses it `team_transport` (the team handler's gate) — never left listed as
/// live-teamed; continue-without-team then writes the tombstone before `transport: none`, and
/// the bus's return publishes nothing more for the run.
#[test]
fn nobus_restart_pauses_a_live_teamed_run_and_continue_unteams_it() {
    let (rig, db) = live_teamed("lt-c");
    let e = engine_on(&db, no_bus(&rig));
    assert_eq!(status(&e, "lt-c"), Some(SessionStatus::AwaitingHuman));
    assert!(
        e.core.live_team_runs().unwrap().is_empty(),
        "never armed without a publisher"
    );
    let view = e.core.run_team("lt-c").unwrap().unwrap();
    assert_ne!(view.transport, "bus", "not reported live-teamed: {view:?}");
    assert!(view.pending.is_some(), "{view:?}");
    assert!(
        e.core
            .confirm_gate("lt-c", HumanDecision::RequestChanges { note: None })
            .is_err(),
        "the team handler owns the pause"
    );
    e.core
        .confirm_gate("lt-c", approve(Some(CONTINUE_WITHOUT_TEAM)))
        .unwrap();
    assert!(outbox_has_run_tombstone(&rig, "lt-c"));
    assert_eq!(e.core.run_team("lt-c").unwrap().unwrap().transport, "none");
    rig.team_bus().drain_all();
    assert_eq!(
        rig.types("lt-c"),
        vec![tev::PATH_STARTED, tev::PLAN_ACCEPTED]
    );
}

/// The same pause answered reject: cancelled, and only `path.ended` joins once the bus returns.
#[test]
fn nobus_restart_pauses_a_live_teamed_run_and_reject_cancels_it() {
    let (rig, db) = live_teamed("lt-r");
    let e = engine_on(&db, no_bus(&rig));
    assert_eq!(status(&e, "lt-r"), Some(SessionStatus::AwaitingHuman));
    assert_eq!(
        e.core.confirm_gate("lt-r", HumanDecision::Reject).unwrap(),
        SessionStatus::Cancelled
    );
    assert!(outbox_has_run_tombstone(&rig, "lt-r"));
    rig.team_bus().drain_all();
    assert_eq!(
        rig.types("lt-r"),
        vec![tev::PATH_STARTED, tev::PLAN_ACCEPTED, tev::PATH_ENDED]
    );
}

/// No regression: restart WITH a bus leaves the teamed run live, listed and at its own gate.
#[test]
fn bus_restart_leaves_a_live_teamed_run_live() {
    let (rig, db) = live_teamed("lt-b");
    let e = engine_on(&db, fast(&rig));
    assert_eq!(status(&e, "lt-b"), Some(SessionStatus::AwaitingHuman));
    let view = e.core.run_team("lt-b").unwrap().unwrap();
    assert_eq!(view.transport, "bus");
    assert_eq!(view.pending, None);
    assert_eq!(e.core.live_team_runs().unwrap().len(), 1);
    assert!(!outbox_has_run_tombstone(&rig, "lt-b"));
}

// ── DES-TEAMING-002 T5 through the real engine ───────────────────────────────────────────────────

/// A worker seat that, on the run's FIRST unit, plays the supervisor raising one HIGH on the
/// attempt during its turn (a `finding.raised` row, S) — and nothing answers it.
struct RaisingRunner {
    bus: crate::team::publish::TeamBus,
    /// The unit whose turn raises the finding.
    on_ix: usize,
}
impl StepRunner for RaisingRunner {
    fn run_unit(&self, i: &StepInput) -> StepOutput {
        if i.unit_ix == self.on_ix {
            let f =
                crate::team::publish::tests::fixture_with(tev::FINDING_RAISED, 0, &i.run_id, |p| {
                    p["ord"] = serde_json::json!(i.unit.ord);
                    p["attempt"] = serde_json::json!(i.attempt);
                    p["raise_seq"] = serde_json::json!(1);
                });
            self.bus.publish(&f).expect("the finding is on the bus");
        }
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

fn engine_running(rig: &Rig, cfg: TeamConfig, worker: Arc<dyn StepRunner>) -> Engine {
    let db = rig.dir.join("core.db").to_string_lossy().into_owned();
    let core = Core::spawn_with_engine_team(db.clone(), Arc::new(StubDispatcher), worker, cfg);
    let events = core.subscribe();
    core.ping();
    Engine {
        core,
        runner: Arc::new(CountingRunner::default()),
        events,
        db,
    }
}

/// The payloads of every `event_type` row of `run`.
fn payloads(rig: &Rig, run: &str, event_type: &str) -> Vec<serde_json::Value> {
    let db = crate::bus::BusDb::shared(&rig.bus).unwrap();
    db.poll(event_type, 0, 10_000)
        .unwrap()
        .into_iter()
        .filter(|e| e.payload["run_id"] == run)
        .map(|e| e.payload)
        .collect()
}

/// T5 (c), engine half: no `ledger.folded` arrives within the (shortened) budget. The worker's
/// synthesized `timed_out` snapshot rides `ApplyStepResult`; the engine publishes
/// `gate.opened{kind:"unit_review", ledger_ref:null, ledger_source:"synthesized"}`, and — the
/// gate having approved the work with an unresolved HIGH in the ledger — the run pauses
/// `team_dispute` (no unattended continue). Nothing publishes `ledger.folded` but S: a LATE
/// fold from S lands on the bus and changes nothing already decided. Approve then continues.
#[test]
fn t5_c_final_pass_timeout_pauses_team_dispute_and_a_late_fold_changes_nothing() {
    let rig = rig("t5c-engine");
    let e = engine_running(
        &rig,
        fast(&rig),
        Arc::new(RaisingRunner {
            bus: rig.team_bus(),
            on_ix: 0,
        }),
    );
    launch_team(&e, "t5c");
    wait_status(&e, "t5c", SessionStatus::AwaitingHuman);
    let evs = drain_events(&e);
    assert_eq!(
        awaiting_kinds(&evs, "t5c"),
        vec!["team_dispute".to_string()]
    );
    assert!(
        payloads(&rig, "t5c", tev::LEDGER_FOLDED).is_empty(),
        "the worker never publishes ledger.folded"
    );
    // The engine's gate facts are fire-and-forget through the publisher thread: the durable
    // pause can be on record before its rows are on the bus, so wait (bounded) for them.
    wait_for(
        "the unit-review decision and the team_dispute gate on the bus",
        || {
            payloads(&rig, "t5c", tev::GATE_OPENED)
                .iter()
                .any(|p| p["kind"] == "team_dispute")
                && payloads(&rig, "t5c", tev::GATE_DECIDED)
                    .iter()
                    .any(|p| p["kind"] == "unit_review")
        },
    );
    let opened = payloads(&rig, "t5c", tev::GATE_OPENED);
    let review: Vec<_> = opened
        .iter()
        .filter(|p| p["kind"] == "unit_review")
        .collect();
    assert_eq!(review.len(), 1, "{opened:?}");
    assert_eq!(review[0]["ledger_ref"], serde_json::Value::Null);
    assert_eq!(review[0]["ledger_source"], "synthesized");
    let dispute: Vec<_> = opened
        .iter()
        .filter(|p| p["kind"] == "team_dispute")
        .collect();
    assert_eq!(dispute.len(), 1, "{opened:?}");
    assert_eq!(dispute[0]["finding_ids"].as_array().unwrap().len(), 1);
    let decided = payloads(&rig, "t5c", tev::GATE_DECIDED);
    assert!(
        decided
            .iter()
            .any(|p| p["kind"] == "unit_review" && p["decision"] == "paused"),
        "{decided:?}"
    );
    let unit0 = || e.core.run_team("t5c").unwrap().unwrap().units[0].clone();
    let before = unit0();
    assert_eq!(before.transport.as_deref(), Some("bus"));
    assert_eq!(before.ledger_source.as_deref(), Some("synthesized"));
    assert_eq!(before.final_pass.as_deref(), Some("timed_out"));
    assert_eq!(before.team_pause, Some(true));
    assert_eq!(before.findings, Some(1));

    // S's late fold for the same attempt: published by S alone, read by no one.
    let first = payloads(&rig, "t5c", tev::STEP_COMPLETED)[0].clone();
    let late = crate::team::publish::tests::fixture_with(tev::LEDGER_FOLDED, 0, "t5c", |p| {
        p["ord"] = first["ord"].clone();
        p["attempt"] = first["attempt"].clone();
    });
    rig.team_bus().publish(&late).unwrap();
    e.core.ping();
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(status(&e, "t5c"), Some(SessionStatus::AwaitingHuman));
    assert_eq!(
        unit0(),
        before,
        "a late fold changes nothing already decided"
    );
    assert_eq!(
        payloads(&rig, "t5c", tev::GATE_OPENED)
            .iter()
            .filter(|p| p["kind"] == "unit_review")
            .count(),
        1
    );

    // The human decides: approve continues at the cursor.
    e.core.confirm_gate("t5c", approve(None)).unwrap();
    wait_status(&e, "t5c", SessionStatus::Completed);
}

/// T5 (c) on the run's LAST unit: the `team_dispute` pause holds the run before it can finalize
/// (`Completed` is never reached unattended); reject cancels, and the unit's work stays on record.
#[test]
fn t5_c_a_dispute_on_the_last_unit_holds_the_run_before_it_completes() {
    let rig = rig("t5c-last");
    let e = engine_running(
        &rig,
        fast(&rig),
        Arc::new(RaisingRunner {
            bus: rig.team_bus(),
            on_ix: 1,
        }),
    );
    launch_team(&e, "t5cl");
    wait_status(&e, "t5cl", SessionStatus::AwaitingHuman);
    assert_eq!(
        awaiting_kinds(&drain_events(&e), "t5cl"),
        vec!["team_dispute".to_string()]
    );
    let view = e.core.run_team("t5cl").unwrap().unwrap();
    assert_eq!(view.units[0].team_pause, Some(false), "{view:?}");
    assert_eq!(view.units[1].team_pause, Some(true), "{view:?}");
    assert_eq!(
        e.core.confirm_gate("t5cl", HumanDecision::Reject).unwrap(),
        SessionStatus::Cancelled
    );
}

/// T5 (e): under `spawn_with_engine` with NO bus, the team unit produces no team rows and its
/// `UnitEvidence.team` — persisted on the unit — is the local snapshot: `transport: none`,
/// `no_bus`, an empty ledger.
#[test]
fn t5_e_no_bus_every_unit_holds_the_local_snapshot_and_nothing_is_published() {
    let rig = rig("t5e-engine");
    let cfg = TeamConfig::new(None, Some(rig.outbox.clone()))
        .with_final_pass_budget(Duration::from_millis(300));
    let e = engine_running(&rig, cfg, Arc::new(CountingRunner::default()));
    launch_team(&e, "t5e");
    wait_status(&e, "t5e", SessionStatus::Completed);
    let view = e.core.run_team("t5e").unwrap().unwrap();
    assert_eq!(view.transport, "none");
    assert_eq!(view.units.len(), 2);
    for u in &view.units {
        assert_eq!(u.transport.as_deref(), Some("none"), "{u:?}");
        assert_eq!(u.ledger_source.as_deref(), Some("no_bus"), "{u:?}");
        assert_eq!(u.findings, Some(0), "an empty ledger: {u:?}");
        assert_eq!(u.team_pause, Some(false), "{u:?}");
    }
    assert!(rig.types("t5e").is_empty(), "no team rows");
    assert!(
        rig.outbox_lines().is_empty(),
        "nothing spooled without a bus"
    );
}

/// §4.8 row 5 through the engine: every `step.claimed` fails past the bound → each attempt is
/// tombstoned, runs un-teamed with the reason in its snapshot, and publishes nothing — no
/// `step.claimed`, no `step.completed`, no unit-review gate — ever, even after a replay.
#[test]
fn t5_row5_a_failed_step_claimed_unteams_the_attempt_through_the_engine() {
    let rig = rig("t5r5-engine");
    rig.refuse(&[tev::STEP_CLAIMED]);
    let e = engine(&rig, fast(&rig));
    launch_team(&e, "t5r5");
    wait_status(&e, "t5r5", SessionStatus::Completed);
    let view = e.core.run_team("t5r5").unwrap().unwrap();
    assert_eq!(
        view.transport, "bus",
        "the RUN stays teamed; the attempts are not"
    );
    for u in &view.units {
        assert_eq!(u.transport.as_deref(), Some("none"), "{u:?}");
        assert!(
            u.reason
                .as_deref()
                .unwrap_or("")
                .starts_with("un-teamed attempt"),
            "{u:?}"
        );
    }
    rig.allow();
    e.core.replay_team_outbox().unwrap();
    let types = rig.types("t5r5");
    for t in [tev::STEP_CLAIMED, tev::STEP_COMPLETED, tev::GATE_OPENED] {
        assert!(!types.iter().any(|x| x == t), "{t} published: {types:?}");
    }
}

// ── DES-TEAMING-002 T6 through the real engine ───────────────────────────────────────────────────

use crate::team::supervisor::tests::{FakeCouncil, FakeHost};
use crate::team::supervisor::{Council, CouncilOutcome};

/// A T6 bound: the supervisor folds long before it; the worker returns as soon as the fold lands
/// (generous for slow CI hosts: only a stuck fold waits it out).
fn t6_cfg(rig: &Rig) -> TeamConfig {
    fast(rig).with_final_pass_budget(Duration::from_secs(90))
}

/// An engine with the team supervisor on the bus (injected host and council).
fn supervised(
    rig: &Rig,
    db: &str,
    worker: Arc<dyn StepRunner>,
    host: Arc<FakeHost>,
    council: Arc<dyn Council>,
    exec: bool,
) -> Engine {
    let core = Core::spawn_with_engine_team_supervised(
        db.to_string(),
        Arc::new(StubDispatcher),
        worker,
        t6_cfg(rig),
        exec.then(|| rig.bus.clone()),
        host,
        council,
        |c| c.poll = Duration::from_millis(20),
    );
    let events = core.subscribe();
    core.ping();
    Engine {
        core,
        runner: Arc::new(CountingRunner::default()),
        events,
        db: db.to_string(),
    }
}

/// Every CoreEvent the engine emitted, gathered as the test goes.
fn collect(e: &Engine, into: &mut Vec<CoreEvent>) {
    into.extend(e.events.try_iter());
}

fn ord_events(evs: &[CoreEvent], run: &str, ord: u32) -> Vec<String> {
    evs.iter()
        .filter_map(|ev| {
            let j = ev.to_json();
            (j["session"] == run && j["ord"] == ord).then(|| {
                let t = j["type"].as_str().unwrap_or("?").to_string();
                match ev {
                    CoreEvent::GateDecided { allow, .. } => format!("{t}:{allow}"),
                    CoreEvent::AwaitingHuman { gate_kind, .. } => format!("{t}:{gate_kind}"),
                    CoreEvent::UnitDispatched { attempt, .. } => format!("{t}:{attempt}"),
                    _ => t,
                }
            })
        })
        .collect()
}

/// DES-001 #13 + #16 (g) on the T6 engine: on a `team_dispute` pause the unit's events are exactly
/// `gateEvaluated` → `awaitingHuman{team_dispute}` — no `gateDecided`, no `unitDone` — and the
/// cursor stays on the unit. Approve publishes the human's `gate.decided{kind:"team_dispute"}` and
/// only then emits `resumed` → `gateDecided{allow:true}` → `unitDone`; the unit is never
/// re-dispatched and its attempt never bumped.
#[test]
fn t6_13_a_dispute_withholds_the_decision_until_approved_and_never_redispatches() {
    let rig = rig("t6-13");
    let e = engine_running(
        &rig,
        fast(&rig),
        Arc::new(RaisingRunner {
            bus: rig.team_bus(),
            on_ix: 0,
        }),
    );
    let mut evs = Vec::new();
    launch_team(&e, "t613");
    wait_status(&e, "t613", SessionStatus::AwaitingHuman);
    collect(&e, &mut evs);
    let before = ord_events(&evs, "t613", 1);
    let tail: Vec<&str> = before
        .iter()
        .map(String::as_str)
        .filter(|t| {
            t.starts_with("gateEvaluated")
                || t.starts_with("gateDecided")
                || t.starts_with("unitDone")
                || t.starts_with("awaitingHuman")
        })
        .collect();
    assert_eq!(
        tail,
        ["gateEvaluated", "awaitingHuman:team_dispute"],
        "{before:?}"
    );
    let session = e
        .core
        .sessions_detail()
        .unwrap()
        .into_iter()
        .find(|v| v.session.id == "t613")
        .unwrap()
        .session;
    assert_eq!(session.unit_ix, 0, "the cursor stays on the disputed unit");
    let dispute = session
        .team
        .clone()
        .unwrap()
        .dispute
        .expect("the gate is recorded");
    assert_eq!(dispute.kind, crate::domain::DisputeKind::Finding);

    e.core.confirm_gate("t613", approve(None)).unwrap();
    wait_status(&e, "t613", SessionStatus::Completed);
    collect(&e, &mut evs);
    let after = ord_events(&evs, "t613", 1);
    let from = after
        .iter()
        .position(|t| t == "resumed")
        .expect("resumed after the approve");
    assert_eq!(
        &after[from..from + 3],
        ["resumed", "gateDecided:true", "unitDone"],
        "{after:?}"
    );
    assert_eq!(
        after
            .iter()
            .filter(|t| t.starts_with("unitDispatched"))
            .count(),
        1,
        "never re-dispatched: {after:?}"
    );
    let decided = payloads(&rig, "t613", tev::GATE_DECIDED);
    let human: Vec<_> = decided
        .iter()
        .filter(|p| p["kind"] == "team_dispute")
        .collect();
    assert_eq!(human.len(), 1, "{decided:?}");
    assert_eq!(human[0]["decision"], "human_approved");
    assert_eq!(human[0]["by"], "human");
    assert_eq!(human[0]["gate_id"], dispute.gate_id);
}

/// DES-001 #16 (k): approving a `team_dispute` with an amendment reruns the creator with it —
/// `unitReworkAmended`, then `unitDispatched` at the next attempt — after the human's
/// `gate.decided{human_amended}` lands.
#[test]
fn t6_16k_a_dispute_approved_with_an_amendment_reruns_the_creator() {
    let rig = rig("t6-16k");
    let e = engine_running(
        &rig,
        fast(&rig),
        Arc::new(RaisingRunner {
            bus: rig.team_bus(),
            on_ix: 1,
        }),
    );
    let mut evs = Vec::new();
    let def: crate::workflow::WorkflowDef = serde_json::from_value(serde_json::json!({
        "id": "t616k:plan-1",
        "phases": [
            {"id": "understand", "kind": "build", "gate": "auto"},
            {"id": "build", "kind": "build", "gate": "auto", "role": "creator"}
        ]
    }))
    .unwrap();
    e.core.register_composed(def).unwrap();
    e.core
        .launch_run(LaunchSpec {
            base_ref: None,
            project_id: None,
            problem: "t6 amend".into(),
            clis: vec![cli("a"), cli("b")],
            entity_mode: crate::EntityMode::Shared,
            session_id: "t616k".into(),
            human_confirm: HumanConfirm::None,
            auto_deliver: false,
            repo_ref: None,
            workflow: Some("t616k:plan-1".into()),
            extra_write_roots: Vec::new(),
            extra_read_roots: Vec::new(),
            project_graph: None,
        })
        .unwrap();
    wait_status(&e, "t616k", SessionStatus::AwaitingHuman);
    collect(&e, &mut evs);
    e.core
        .confirm_gate("t616k", approve(Some("cancel the stale fetch")))
        .unwrap();
    wait_for("the creator's rework dispatch", || {
        collect(&e, &mut evs);
        ord_events(&evs, "t616k", 2)
            .iter()
            .any(|t| t == "unitDispatched:1")
    });
    let seq = ord_events(&evs, "t616k", 2);
    let amended = seq.iter().position(|t| t == "unitReworkAmended").unwrap();
    let redispatch = seq.iter().position(|t| t == "unitDispatched:1").unwrap();
    assert!(amended < redispatch, "{seq:?}");
    assert!(payloads(&rig, "t616k", tev::GATE_DECIDED)
        .iter()
        .any(|p| p["kind"] == "team_dispute" && p["decision"] == "human_amended"));
}

/// DES-002 §4.1: the human's `team_dispute` answer is a REQUIRED fact. With the bus refusing
/// `gate.decided`, approve leaves the run paused — now `team_transport` — with no `resumed` and no
/// dispatch; once the bus takes it again, the transport gate's approve lands both decisions in
/// order and the run resumes.
#[test]
fn t6_the_dispute_answer_is_a_required_fact() {
    let rig = rig("t6-req");
    let e = engine_running(
        &rig,
        fast(&rig),
        Arc::new(RaisingRunner {
            bus: rig.team_bus(),
            on_ix: 0,
        }),
    );
    launch_team(&e, "t6req");
    wait_status(&e, "t6req", SessionStatus::AwaitingHuman);
    rig.refuse(&[tev::GATE_DECIDED]);
    let _ = drain_events(&e);
    let _ = e.core.confirm_gate("t6req", approve(None));
    wait_for("the transport pause", || {
        awaiting_kinds(&drain_events(&e), "t6req")
            .iter()
            .any(|k| k == super::TEAM_TRANSPORT_GATE)
            || e.core
                .run_team("t6req")
                .ok()
                .flatten()
                .and_then(|v| v.pending)
                .is_some_and(|p| p == tev::GATE_DECIDED)
    });
    assert_eq!(status(&e, "t6req"), Some(SessionStatus::AwaitingHuman));
    assert_eq!(
        resumed(&drain_events(&e), "t6req"),
        0,
        "nothing resumes unacknowledged"
    );
    assert!(payloads(&rig, "t6req", tev::GATE_DECIDED)
        .iter()
        .all(|p| p["kind"] != "team_dispute"));
    rig.allow();
    e.core.confirm_gate("t6req", approve(None)).unwrap();
    wait_status(&e, "t6req", SessionStatus::Completed);
    let decided = payloads(&rig, "t6req", tev::GATE_DECIDED);
    let dispute_at = decided.iter().position(|p| p["kind"] == "team_dispute");
    let transport_at = decided.iter().position(|p| p["kind"] == "team_transport");
    assert!(
        dispute_at.is_some() && dispute_at < transport_at,
        "{decided:#?}"
    );
}

// ── Member steps (§8.8): (f) (g) (h) through the engine and the supervisor ──────────────────────

/// A worker that does a member's step as the member and reviews it as the PA, from a script.
struct MemberRunner {
    /// `(review number) → the PA's review output`.
    review: Box<dyn Fn(usize) -> String + Send + Sync>,
    seen: std::sync::Mutex<Vec<StepInput>>,
    reviews: AtomicUsize,
}

impl MemberRunner {
    fn new(review: impl Fn(usize) -> String + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            review: Box::new(review),
            seen: Default::default(),
            reviews: AtomicUsize::new(0),
        })
    }
    fn inputs(&self, ord: u32) -> Vec<StepInput> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|i| i.unit.ord == ord)
            .cloned()
            .collect()
    }
}

impl StepRunner for MemberRunner {
    fn run_unit(&self, i: &StepInput) -> StepOutput {
        self.seen.lock().unwrap().push(i.clone());
        let output = if i
            .unit
            .member_step
            .as_ref()
            .is_some_and(|m| m.reviewing.is_some())
        {
            let n = self.reviews.fetch_add(1, AtomicOrdering::SeqCst);
            (self.review)(n)
        } else {
            "done".to_string()
        };
        StepOutput {
            run_id: i.run_id.clone(),
            unit_ix: i.unit_ix,
            attempt: i.attempt,
            output,
            status: StepStatus::Ok,
            usage: None,
            files: Vec::new(),
            tools: Vec::new(),
            governed: false,
        }
    }
}

/// The ord of the `write` step (ords are 1-based: `understand` is 1).
const WRITE: u32 = 2;

/// Launch a three-step team run whose middle step (`write`) is the team's: PA `a`, member `b`.
fn launch_member_run(e: &Engine, run: &str) {
    let def: crate::workflow::WorkflowDef = serde_json::from_value(serde_json::json!({
        "id": format!("{run}:plan-1"),
        "phases": [
            {"id": "understand", "kind": "build", "gate": "auto"},
            {"id": "write", "kind": "build", "gate": "auto", "owner": "team"},
            {"id": "finish", "kind": "build", "gate": "auto"}
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
            problem: "t6 member step".into(),
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

/// [`launch_member_run`] BOUND to a registered repository (`repo_ref`), so every unit runs in the
/// run's worktree and the engine snapshots it at dispatch.
fn launch_member_run_bound(e: &Engine, run: &str, repo_ref: &str) {
    let def: crate::workflow::WorkflowDef = serde_json::from_value(serde_json::json!({
        "id": format!("{run}:plan-1"),
        "phases": [
            {"id": "understand", "kind": "build", "gate": "auto"},
            {"id": "write", "kind": "build", "gate": "auto", "owner": "team"},
            {"id": "finish", "kind": "build", "gate": "auto"}
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
            problem: "t6 member step, bound".into(),
            clis: vec![cli("a"), cli("b")],
            entity_mode: crate::EntityMode::Shared,
            session_id: run.into(),
            human_confirm: HumanConfirm::None,
            auto_deliver: false,
            repo_ref: Some(repo_ref.into()),
            workflow: Some(format!("{run}:plan-1")),
            extra_write_roots: Vec::new(),
            extra_read_roots: Vec::new(),
            project_graph: None,
        })
        .expect("launch");
}

fn member_engine(
    name: &str,
    review: impl Fn(usize) -> String + Send + Sync + 'static,
    member: impl Fn(&str, &str) -> Result<String, String> + Send + Sync + 'static,
    council: FakeCouncil,
) -> (Rig, Engine, Arc<MemberRunner>, Arc<FakeCouncil>) {
    let rig = rig(name);
    let worker = MemberRunner::new(review);
    let host = Arc::new(FakeHost::new(member));
    let council = Arc::new(council);
    let db = rig.dir.join("core.db").to_string_lossy().into_owned();
    let e = supervised(&rig, &db, worker.clone(), host, council.clone(), false);
    (rig, e, worker, council)
}

fn unit_view(e: &Engine, run: &str, ord: u32) -> crate::domain::WorkUnit {
    e.core
        .sessions_detail()
        .unwrap()
        .into_iter()
        .find(|v| v.session.id == run)
        .unwrap()
        .units
        .into_iter()
        .find(|u| u.ord == ord)
        .unwrap()
}

/// T6 (f): an `owner:"team"` step runs on a MEMBER seat (`assigned_cli` = the member, its
/// `step.claimed{by}`), does not count until the PA's `step.reviewed{verdict:"accepted"}`, and
/// then advances: its `gateDecided{allow:true}` + `unitDone` follow the review, and the unit's
/// evidence is the member attempt's snapshot.
#[test]
fn t6_f_a_member_step_runs_on_the_member_and_counts_once_the_pa_accepts() {
    let (rig, e, worker, _) = member_engine(
        "t6-f",
        |_| "reviewed\nSTEP write: ACCEPT — it covers the migration".into(),
        |_, _| Ok("DONE".into()),
        FakeCouncil::yes(),
    );
    let mut evs = Vec::new();
    launch_member_run(&e, "t6f");
    wait_status(&e, "t6f", SessionStatus::Completed);
    collect(&e, &mut evs);
    let ord = unit_view(&e, "t6f", WRITE).ord;
    let runs = worker.inputs(ord);
    assert_eq!(runs.len(), 2, "the member's work, then the PA's review");
    assert_eq!(
        runs[0].unit.assigned_cli.as_deref(),
        Some("b"),
        "the member's seat"
    );
    assert_eq!(
        runs[1].unit.assigned_cli.as_deref(),
        Some("a"),
        "the PA reviews"
    );
    assert!(runs[1]
        .prior_outputs
        .iter()
        .any(|p| p.label == "[team step — write by b]"));
    let claimed = payloads(&rig, "t6f", tev::STEP_CLAIMED);
    assert!(claimed
        .iter()
        .any(|p| p["step_id"] == "write" && p["by"] == "b" && p["attempt"] == 0));
    let reviewed = payloads(&rig, "t6f", tev::STEP_REVIEWED);
    assert_eq!(reviewed.len(), 1);
    assert_eq!(reviewed[0]["verdict"], "accepted");
    assert_eq!(reviewed[0]["by"], "a");
    let seq = ord_events(&evs, "t6f", ord);
    let review_dispatch = seq.iter().position(|t| t == "unitDispatched:1").unwrap();
    let counted = seq.iter().position(|t| t == "gateDecided:true").unwrap();
    assert!(
        review_dispatch < counted,
        "counted only after the review: {seq:?}"
    );
    assert_eq!(seq.iter().filter(|t| *t == "unitDone").count(), 1);
    let u = unit_view(&e, "t6f", ord);
    assert_eq!(u.status, crate::domain::UnitStatus::Done);
    assert_eq!(u.assigned_cli.as_deref(), Some("b"));
    assert_eq!(
        u.team.as_ref().and_then(|t| t.claimed_event_id),
        claimed
            .iter()
            .zip(0..)
            .find(|(p, _)| p["step_id"] == "write" && p["attempt"] == 0)
            .map(|_| {
                let db = crate::bus::BusDb::shared(&rig.bus).unwrap();
                db.poll(tev::STEP_CLAIMED, 0, 100)
                    .unwrap()
                    .into_iter()
                    .find(|r| r.payload["step_id"] == "write" && r.payload["attempt"] == 0)
                    .unwrap()
                    .event_id
            }),
        "the unit's evidence is the member attempt's snapshot"
    );
}

/// T6 (g): `REJECT to:member` reworks the step on the member with the reason as its amendment
/// (the member took the rejection: `ACCEPT write`); the second review accepts it.
#[test]
fn t6_g_reject_to_member_reworks_it_on_the_member_with_the_reason() {
    let (_rig, e, worker, _) = member_engine(
        "t6-g1",
        |n| {
            if n == 0 {
                "STEP write: REJECT to:member — the rollback path is missing".into()
            } else {
                "STEP write: ACCEPT — fixed".into()
            }
        },
        |_, p| {
            if p.contains("your step was rejected") {
                Ok("ACCEPT write\nDONE".into())
            } else {
                Ok("DONE".into())
            }
        },
        FakeCouncil::yes(),
    );
    launch_member_run(&e, "t6g1");
    wait_status(&e, "t6g1", SessionStatus::Completed);
    let runs = worker.inputs(WRITE);
    let seats: Vec<&str> = runs
        .iter()
        .map(|i| i.unit.assigned_cli.as_deref().unwrap())
        .collect();
    assert_eq!(seats, ["b", "a", "b", "a"], "work, review, rework, review");
    let rework = &runs[2];
    assert!(rework
        .unit
        .rework_amendment
        .as_deref()
        .unwrap()
        .contains("the rollback path is missing"));
    let u = unit_view(&e, "t6g1", WRITE);
    let ms = u.member_step.unwrap();
    assert_eq!(ms.rejections, 1);
    assert_eq!(ms.reviews.len(), 2);
    assert_eq!(
        ms.reviews[0].held,
        Some(false),
        "the member took the rejection"
    );
}

/// T6 (g): `REJECT to:pa` re-plans the step onto the PA seat: it runs there as the PA's own step,
/// gated as any other, with no review.
#[test]
fn t6_g_reject_to_pa_replans_the_step_onto_the_pa() {
    let (_rig, e, worker, _) = member_engine(
        "t6-g2",
        |_| "STEP write: REJECT to:pa — I will take this one".into(),
        |_, p| {
            if p.contains("your step was rejected") {
                Ok("ACCEPT write\nDONE".into())
            } else {
                Ok("DONE".into())
            }
        },
        FakeCouncil::yes(),
    );
    launch_member_run(&e, "t6g2");
    wait_status(&e, "t6g2", SessionStatus::Completed);

    let seats: Vec<String> = worker
        .inputs(WRITE)
        .iter()
        .map(|i| i.unit.assigned_cli.clone().unwrap())
        .collect();
    assert_eq!(seats, ["b", "a", "a"], "work, review, the PA's own step");
    let u = unit_view(&e, "t6g2", WRITE);
    assert_eq!(u.owner, crate::workflow::StepOwner::Pa);
    assert!(u.member_step.unwrap().replanned);
}

/// T6 (g): the THIRD rejection goes to the PA (`MAX_STEP_REWORK` = 2 back to the member).
#[test]
fn t6_g_the_third_rejection_goes_to_the_pa() {
    let (_rig, e, worker, _) = member_engine(
        "t6-g3",
        |_| "STEP write: REJECT to:member — still wrong".into(),
        |_, p| {
            if p.contains("your step was rejected") {
                Ok("ACCEPT write\nDONE".into())
            } else {
                Ok("DONE".into())
            }
        },
        FakeCouncil::yes(),
    );
    launch_member_run(&e, "t6g3");
    wait_status(&e, "t6g3", SessionStatus::Completed);
    let seats: Vec<String> = worker
        .inputs(WRITE)
        .iter()
        .map(|i| i.unit.assigned_cli.clone().unwrap())
        .collect();
    assert_eq!(
        seats,
        ["b", "a", "b", "a", "b", "a", "a"],
        "three member attempts, three reviews, then the PA's own"
    );
}

/// T6 (h): a member `HOLD` on a rejection convenes ONE council (`trigger:"member_step"`) that
/// excludes the member and the PA: YES counts the step (no rework).
#[test]
fn t6_h_a_member_hold_convenes_one_council_and_yes_counts_the_step() {
    let (rig, e, worker, council) = member_engine(
        "t6-h1",
        |_| "STEP write: REJECT to:member — wrong API".into(),
        |_, p| {
            if p.contains("your step was rejected") {
                Ok("HOLD write — the API is the documented one\nDONE".into())
            } else {
                Ok("DONE".into())
            }
        },
        FakeCouncil::yes(),
    );
    launch_member_run(&e, "t6h1");
    wait_status(&e, "t6h1", SessionStatus::Completed);
    let called = payloads(&rig, "t6h1", tev::COUNCIL_CALLED);
    assert_eq!(called.len(), 1, "{called:#?}");
    assert_eq!(called[0]["trigger"], "member_step");
    assert_eq!(called[0]["subject"], "step:write:0");
    let calls = council.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].1.contains(&"b".to_string()) && calls[0].1.contains(&"a".to_string()));
    assert_eq!(
        worker.inputs(WRITE).len(),
        2,
        "counted: work + review, no rework"
    );
}

/// T6 (h): council NO keeps the rejection (the member reworks it).
#[test]
fn t6_h_council_no_keeps_the_rejection() {
    let (_rig, e, worker, _) = member_engine(
        "t6-h2",
        |n| {
            if n == 0 {
                "STEP write: REJECT to:member — wrong API".into()
            } else {
                "STEP write: ACCEPT — fine now".into()
            }
        },
        |_, p| {
            if p.contains("your step was rejected") {
                Ok("HOLD write — it is right\nDONE".into())
            } else {
                Ok("DONE".into())
            }
        },
        FakeCouncil::new(|_| FakeCouncil::ruling(Some(1))),
    );
    launch_member_run(&e, "t6h2");
    wait_status(&e, "t6h2", SessionStatus::Completed);
    assert_eq!(
        worker.inputs(WRITE).len(),
        4,
        "work, review, rework, review"
    );
}

/// T6 (h): a council with no verdict pauses `team_dispute` (a human decides); approve counts the
/// member's output.
#[test]
fn t6_h_no_council_verdict_pauses_team_dispute() {
    let (rig, e, worker, _) = member_engine(
        "t6-h3",
        |_| "STEP write: REJECT to:member — wrong API".into(),
        |_, p| {
            if p.contains("your step was rejected") {
                Ok("HOLD write — it is right\nDONE".into())
            } else {
                Ok("DONE".into())
            }
        },
        FakeCouncil::new(|_| CouncilOutcome::Failed("boom".into())),
    );
    launch_member_run(&e, "t6h3");
    wait_status(&e, "t6h3", SessionStatus::AwaitingHuman);
    assert!(payloads(&rig, "t6h3", tev::GATE_OPENED)
        .iter()
        .any(|p| p["kind"] == "team_dispute"));
    e.core.confirm_gate("t6h3", approve(None)).unwrap();
    wait_status(&e, "t6h3", SessionStatus::Completed);
    assert_eq!(
        worker.inputs(WRITE).len(),
        2,
        "no rework: the human counted it"
    );
}

/// §8.8, fail closed: a review with no `STEP` line never counts the step — it pauses.
#[test]
fn t6_a_review_without_a_step_line_pauses_and_never_counts() {
    let (_rig, e, _worker, _) = member_engine(
        "t6-nostep",
        |_| "I looked at it.".into(),
        |_, _| Ok("DONE".into()),
        FakeCouncil::yes(),
    );
    launch_member_run(&e, "t6ns");
    wait_status(&e, "t6ns", SessionStatus::AwaitingHuman);
    let u = unit_view(&e, "t6ns", WRITE);
    assert_ne!(u.status, crate::domain::UnitStatus::Done);
}

// ── (k) restart mid-step ─────────────────────────────────────────────────────────────────────────

/// Attempt 0 of unit 0 plays the supervisor raising one HIGH on it, then hangs (the daemon dies
/// mid-turn); every later attempt records its input and returns.
struct DyingRunner {
    bus: crate::team::publish::TeamBus,
    release: std::sync::Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    seen: std::sync::Mutex<Vec<StepInput>>,
}

impl StepRunner for DyingRunner {
    fn run_unit(&self, i: &StepInput) -> StepOutput {
        self.seen.lock().unwrap().push(i.clone());
        if i.unit_ix == 0 && i.attempt == 0 {
            let f =
                crate::team::publish::tests::fixture_with(tev::FINDING_RAISED, 0, &i.run_id, |p| {
                    p["ord"] = serde_json::json!(i.unit.ord);
                    p["attempt"] = serde_json::json!(0);
                    p["raise_seq"] = serde_json::json!(1);
                    p["by"] = serde_json::json!("claude#9");
                });
            self.bus.publish(&f).expect("the finding is on the bus");
            if let Some(rx) = self.release.lock().unwrap().take() {
                let _ = rx.recv();
            }
        }
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

/// The restart fixture: engine 1 runs attempt 0 of unit 0 until it has claimed and raised one
/// HIGH, then dies. Returns the rig, the db, the team state persisted before the kill, the
/// release for the hung worker, and engine 2's worker.
fn killed_mid_step(
    name: &str,
    exec: bool,
) -> (
    Rig,
    String,
    crate::domain::RunTeamState,
    std::sync::mpsc::Sender<()>,
) {
    let rig = rig(name);
    let db = rig.dir.join("core.db").to_string_lossy().into_owned();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let w1 = Arc::new(DyingRunner {
        bus: rig.team_bus(),
        release: std::sync::Mutex::new(Some(release_rx)),
        seen: Default::default(),
    });
    let e1 = supervised(
        &rig,
        &db,
        w1,
        Arc::new(FakeHost::new(|_, _| Ok("DONE".into()))),
        Arc::new(FakeCouncil::yes()),
        exec,
    );
    launch_team(&e1, name);
    wait_for("attempt 0's claim and its HIGH", || {
        payloads(&rig, name, tev::FINDING_RAISED).len() == 1
            && payloads(&rig, name, tev::STEP_CLAIMED).len() == 1
    });
    assert!(payloads(&rig, name, tev::STEP_COMPLETED).is_empty());
    let team = e1
        .core
        .sessions_detail()
        .unwrap()
        .into_iter()
        .find(|v| v.session.id == name)
        .unwrap()
        .session
        .team
        .unwrap();
    drop(e1);
    // Let the dead actor release the store before the new daemon opens it.
    std::thread::sleep(Duration::from_millis(300));
    (rig, db, team, release_tx)
}

fn restart(rig: &Rig, db: &str, exec: bool) -> (Engine, Arc<DyingRunner>) {
    let w2 = Arc::new(DyingRunner {
        bus: rig.team_bus(),
        release: std::sync::Mutex::new(None),
        seen: Default::default(),
    });
    let e2 = supervised(
        rig,
        db,
        w2.clone(),
        Arc::new(FakeHost::new(|_, _| Ok("DONE".into()))),
        Arc::new(FakeCouncil::yes()),
        exec,
    );
    (e2, w2)
}

fn assert_recovered(
    rig: &Rig,
    e2: &Engine,
    w2: &DyingRunner,
    run: &str,
    before: &crate::domain::RunTeamState,
) {
    wait_status(e2, run, SessionStatus::Completed);
    // The redriven attempt's claim attached LIVE: its gate read S's fold, never a timeout.
    let view = e2.core.run_team(run).unwrap().unwrap();
    assert_eq!(
        view.units[0].ledger_source.as_deref(),
        Some("folded"),
        "{view:?}"
    );
    assert_eq!(
        view.units[0].final_pass.as_deref(),
        Some("completed"),
        "{view:?}"
    );
    // The carried HIGH reached the redriven attempt's boundary, labelled.
    let redriven = w2
        .seen
        .lock()
        .unwrap()
        .iter()
        .rfind(|i| i.unit_ix == 0 && i.attempt >= 1)
        .cloned()
        .expect("unit 0 was redriven");
    assert!(redriven.attempt >= 1);
    let advice = redriven
        .prior_outputs
        .iter()
        .find(|p| p.label == crate::team::runner::ADVICE_LABEL)
        .expect("the advice block");
    assert!(
        advice.output.contains("carried_from_attempt:0"),
        "{}",
        advice.output
    );
    // The fold of the redriven attempt holds the carried finding.
    let folded = payloads(rig, run, tev::LEDGER_FOLDED);
    let ord = redriven.unit.ord;
    let f = folded
        .iter()
        .find(|p| p["ord"] == ord && p["attempt"] == redriven.attempt)
        .expect("the redriven attempt folded");
    assert!(f["ledger"]["findings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|x| x["carriedFromAttempt"] == 0));
    assert!(
        !folded.iter().any(|p| p["ord"] == ord && p["attempt"] == 0),
        "the dead attempt is never folded"
    );
    // The team state persisted before the kill is the one the run resumed on.
    let after = e2
        .core
        .sessions_detail()
        .unwrap()
        .into_iter()
        .find(|v| v.session.id == run)
        .unwrap()
        .session
        .team
        .unwrap();
    assert_eq!(after.plan_rev, before.plan_rev);
    assert_eq!(after.stream_floor, before.stream_floor);
    assert!(after.gate_seq >= before.gate_seq);
}

/// T6 (k), orphan + `POST /runs/:id/resume`: the daemon dies after attempt 0's `step.claimed` and
/// before its `step.completed`, with one unanswered HIGH on the stream. The new daemon's
/// supervisor replays the run from its `stream_floor`; the resumed attempt 1's claim attaches
/// live (no gate timeout), its boundary carries the HIGH `carried_from_attempt:0`, the plan and
/// gate sequence are those persisted before the kill, and the run completes.
#[test]
fn t6_k_restart_mid_step_resume_carries_the_high_and_attaches_live() {
    let (rig, db, before, release) = killed_mid_step("t6k-res", false);
    let (e2, w2) = restart(&rig, &db, false);
    assert_eq!(
        status(&e2, "t6k-res"),
        Some(SessionStatus::Executing),
        "an orphan"
    );
    e2.core.resume_run("t6k-res").unwrap();
    assert_recovered(&rig, &e2, &w2, "t6k-res", &before);
    drop(release);
}

/// T6 (k), armed exec redrive: the same kill, on an exec-mediated daemon; the new daemon
/// redrives the cursor unit itself (attempt bumped) and the same holds.
#[test]
fn t6_k_restart_mid_step_exec_redrive_carries_the_high_and_attaches_live() {
    let (rig, db, before, release) = killed_mid_step("t6k-exec", true);
    let (e2, w2) = restart(&rig, &db, true);
    assert_recovered(&rig, &e2, &w2, "t6k-exec", &before);
    drop(release);
}

/// T6 (k), the stream gap: the same kill, then the bus loses the run's `path.started` and attempt
/// 0's rows. The redriven attempt's fold is `stream_gap`, and the gate pauses for a human instead
/// of passing.
#[test]
fn t6_k_with_the_stream_gone_the_gate_records_stream_gap_and_pauses() {
    let (rig, db, _before, release) = killed_mid_step("t6k-gap", false);
    let conn = rig.conn();
    conn.execute(
        "DELETE FROM events WHERE subdomain = 'core.team' AND (event_type = ?1 OR \
         json_extract(payload, '$.attempt') = 0)",
        [tev::PATH_STARTED],
    )
    .unwrap();
    drop(conn);
    let (e2, _w2) = restart(&rig, &db, false);
    e2.core.resume_run("t6k-gap").unwrap();
    wait_status(&e2, "t6k-gap", SessionStatus::AwaitingHuman);
    let view = e2.core.run_team("t6k-gap").unwrap().unwrap();
    assert_eq!(
        view.units[0].final_pass.as_deref(),
        Some("stream_gap"),
        "{view:?}"
    );
    assert_eq!(view.units[0].team_pause, Some(true));
    assert_eq!(
        awaiting_kinds(&drain_events(&e2), "t6k-gap"),
        vec!["team_dispute".to_string()]
    );
    drop(release);
}

/// T6: an amend on a dispute over a unit with no creator to rerun is refused BEFORE the gate row
/// resolves or its `gate.decided` is published — the run stays paused on the same dispute.
#[test]
fn t6_an_amend_with_no_creator_to_rerun_is_refused_and_the_dispute_stays_open() {
    let rig = rig("t6-amend-no");
    let e = engine_running(
        &rig,
        fast(&rig),
        Arc::new(RaisingRunner {
            bus: rig.team_bus(),
            on_ix: 1,
        }),
    );
    launch_team(&e, "t6anc");
    wait_status(&e, "t6anc", SessionStatus::AwaitingHuman);
    let err = e
        .core
        .confirm_gate("t6anc", approve(Some("rework it")))
        .unwrap_err();
    assert!(format!("{err:#}").contains("no creator phase"), "{err:#}");
    assert!(payloads(&rig, "t6anc", tev::GATE_DECIDED)
        .iter()
        .all(|p| p["kind"] != "team_dispute"));
    assert_eq!(status(&e, "t6anc"), Some(SessionStatus::AwaitingHuman));
    e.core.confirm_gate("t6anc", approve(None)).unwrap();
    wait_status(&e, "t6anc", SessionStatus::Completed);
}

/// Edit a stopped engine's store: `f` over the run's session and its units.
fn edit_store(
    db: &str,
    run: &str,
    f: impl FnOnce(&mut crate::domain::AgentSession, &mut Vec<crate::domain::WorkUnit>),
) {
    let mut store = wicked_apps_core::open_store_any(Some(db)).expect("store opens");
    let mut s = crate::domain::get_session(&store, run).unwrap().unwrap();
    let mut units = crate::domain::session_units(&store, run).unwrap();
    f(&mut s, &mut units);
    crate::domain::put_node(&mut store, s.to_node()).unwrap();
    for u in &units {
        crate::domain::put_node(&mut store, u.to_node()).unwrap();
    }
}

/// Restart between the fold and the `team_dispute` pause (the unit is done, its ledger pauses, and
/// no gate was opened): the resumed run opens the dispute instead of passing the unit.
#[test]
fn t6_restart_between_the_fold_and_the_dispute_pause_reopens_the_dispute() {
    let rig = rig("t6-owed-d");
    let db = rig.dir.join("core.db").to_string_lossy().into_owned();
    {
        let core = Core::spawn_with_engine_team(
            db.clone(),
            Arc::new(StubDispatcher),
            Arc::new(RaisingRunner {
                bus: rig.team_bus(),
                on_ix: 0,
            }),
            fast(&rig),
        );
        let e = Engine {
            events: core.subscribe(),
            core,
            runner: Arc::new(CountingRunner::default()),
            db: db.clone(),
        };
        launch_team(&e, "t6od");
        wait_status(&e, "t6od", SessionStatus::AwaitingHuman);
    }
    std::thread::sleep(Duration::from_millis(300));
    // The state a crash between the fold's unit write and the pause leaves behind.
    edit_store(&db, "t6od", |s, _| {
        s.status = SessionStatus::Executing;
        if let Some(t) = s.team.as_mut() {
            t.dispute = None;
        }
    });
    let e = engine_on(&db, fast(&rig));
    e.core.resume_run("t6od").unwrap();
    wait_status(&e, "t6od", SessionStatus::AwaitingHuman);
    assert!(awaiting_kinds(&drain_events(&e), "t6od")
        .iter()
        .any(|k| k == "team_dispute"));
    assert_eq!(
        e.runner.0.load(AtomicOrdering::SeqCst),
        0,
        "the unit is not re-run"
    );
    e.core.confirm_gate("t6od", approve(None)).unwrap();
    wait_status(&e, "t6od", SessionStatus::Completed);
}

/// Restart between a member's fold and the PA's review (the unit is done, not counted, and no
/// review is under way): the resumed run dispatches the PA's review — the member's work is not
/// re-run and the step does not count on its own.
#[test]
fn t6_restart_between_a_members_fold_and_the_review_dispatches_the_review() {
    let (rig, e, _worker, _) = member_engine(
        "t6-owed-r",
        |_| "STEP write: ACCEPT — ok".into(),
        |_, _| Ok("DONE".into()),
        FakeCouncil::yes(),
    );
    launch_member_run(&e, "t6or");
    wait_status(&e, "t6or", SessionStatus::Completed);
    let db = e.db.clone();
    drop(e);
    std::thread::sleep(Duration::from_millis(300));
    // The run never ended in the state being reconstructed: no `path.ended` on the bus or record.
    rig.conn()
        .execute(
            "DELETE FROM events WHERE event_type = ?1",
            [tev::PATH_ENDED],
        )
        .unwrap();
    edit_store(&db, "t6or", |s, units| {
        s.status = SessionStatus::Executing;
        s.finished_at = None;
        if let Some(t) = s.team.as_mut() {
            t.ended = false;
        }
        let ix = units.iter().position(|u| u.ord == WRITE).unwrap();
        s.unit_ix = ix;
        let ms = units[ix].member_step.as_mut().unwrap();
        ms.counted = false;
        ms.reviewing = None;
        ms.reviews.clear();
        for u in units.iter_mut().skip(ix + 1) {
            u.status = crate::domain::UnitStatus::Pending;
        }
    });
    let worker = MemberRunner::new(|_| "STEP write: ACCEPT — ok".into());
    let e2 = supervised(
        &rig,
        &db,
        worker.clone(),
        Arc::new(FakeHost::new(|_, _| Ok("DONE".into()))),
        Arc::new(FakeCouncil::yes()),
        false,
    );
    e2.core.resume_run("t6or").unwrap();
    wait_status(&e2, "t6or", SessionStatus::Completed);
    let runs = worker.inputs(WRITE);
    assert_eq!(runs.len(), 1, "only the PA's review ran");
    assert!(runs[0]
        .unit
        .member_step
        .as_ref()
        .is_some_and(|m| m.reviewing.is_some()));
    assert!(unit_view(&e2, "t6or", WRITE).member_step.unwrap().counted);
}

// ── Review round 2 on #628, D2: the PA's review attempt is read-only and cannot mutate ungated ──

/// A worker whose PA review WRITES to the worktree it reviews (a new file and an edit to a
/// committed one) and then accepts the step.
struct WritingReviewer {
    seen: std::sync::Mutex<Vec<StepInput>>,
}

impl StepRunner for WritingReviewer {
    fn run_unit(&self, i: &StepInput) -> StepOutput {
        self.seen.lock().unwrap().push(i.clone());
        let reviewing = i
            .unit
            .member_step
            .as_ref()
            .is_some_and(|m| m.reviewing.is_some());
        let output = if reviewing {
            let wd = i
                .workdir
                .as_deref()
                .expect("a bound run hands the worktree");
            std::fs::write(wd.join("pa-edit.txt"), "the PA edited during review\n").unwrap();
            std::fs::write(wd.join("src/lib.rs"), "fn rewritten_by_the_pa() {}\n").unwrap();
            "reviewed\nSTEP write: ACCEPT — looks right".to_string()
        } else {
            "done".to_string()
        };
        StepOutput {
            run_id: i.run_id.clone(),
            unit_ix: i.unit_ix,
            attempt: i.attempt,
            output,
            status: StepStatus::Ok,
            usage: None,
            files: Vec::new(),
            tools: Vec::new(),
            governed: false,
        }
    }
}

/// D2 (review round 2 on #628): the PA's review of a member step is dispatched READ-ONLY (the
/// same posture every evaluator turn gets), and a review that changes the tree anyway is never
/// accepted: the tree is restored to what the member left, and the step pauses `team_dispute`
/// instead of counting the member attempt's evidence for a tree that no longer exists.
#[test]
fn t6_d2_a_pa_review_that_writes_the_tree_is_restored_and_disputed_never_accepted() {
    let rig = rig("t6-d2");
    let fx = crate::team::supervisor::tests::Fixture::new("t6-d2", "src/lib.rs", "fn a() {}\n");
    let worker = Arc::new(WritingReviewer {
        seen: Default::default(),
    });
    let host = Arc::new(FakeHost::new(|_, _| Ok("DONE".into())));
    let db = rig.dir.join("core.db").to_string_lossy().into_owned();
    let e = supervised(
        &rig,
        &db,
        worker.clone(),
        host,
        Arc::new(FakeCouncil::yes()),
        false,
    );
    let entry = e
        .core
        .register_repo(crate::repo::RepoSpec {
            name: "t6-d2".into(),
            root_path: fx.dir.to_string_lossy().into_owned(),
            registered_at: 1,
        })
        .expect("register");
    launch_member_run_bound(&e, "t6d2", &entry.id);
    wait_for("t6d2 to settle", || {
        matches!(
            status(&e, "t6d2"),
            Some(SessionStatus::AwaitingHuman | SessionStatus::Completed | SessionStatus::Failed)
        )
    });
    let u = unit_view(&e, "t6d2", WRITE);
    assert_eq!(
        status(&e, "t6d2"),
        Some(SessionStatus::AwaitingHuman),
        "the review that wrote the tree must pause, never count the step: {:?}",
        u.member_step
    );

    let runs: Vec<StepInput> = worker
        .seen
        .lock()
        .unwrap()
        .iter()
        .filter(|i| i.unit.ord == WRITE)
        .cloned()
        .collect();
    assert_eq!(runs.len(), 2, "the member's work, then the PA's review");
    // Read-only by construction: the review turn carries the evaluator posture.
    assert_eq!(
        crate::write_posture::WritePosture::of(&runs[1].unit, true),
        crate::write_posture::WritePosture::ReadOnly,
        "the PA's review is a read-only turn"
    );
    // Never accepted: not done, not counted, a team_dispute open.
    assert_ne!(u.status, crate::domain::UnitStatus::Done, "{u:?}");
    assert!(!u.member_step.as_ref().is_some_and(|m| m.counted));
    assert!(payloads(&rig, "t6d2", tev::GATE_OPENED)
        .iter()
        .any(|p| p["kind"] == "team_dispute"));
    // The tree is back to what the member left.
    let wd = runs[1].workdir.clone().unwrap();
    assert!(
        !wd.join("pa-edit.txt").exists(),
        "the PA's new file was removed"
    );
    assert_eq!(
        std::fs::read_to_string(wd.join("src/lib.rs")).unwrap(),
        "fn a() {}\n",
        "the PA's edit was reverted"
    );
}

/// Absence row 32 (found re-auditing the table on #628): an ACCEPT line counts a member's step
/// only when the bus-teamed review attempt's ledger holds its `step.reviewed{accepted}`. A row
/// lost outright (never on the bus, never spooled) pauses instead of counting on the output alone;
/// an un-teamed (no-bus) review has no stream to hold it and is judged on its line.
#[test]
fn row32_an_accept_counts_only_with_its_step_reviewed_on_the_record() {
    use crate::team::events::StepVerdict;
    let rec = |verdict: StepVerdict| crate::team::StepReviewRecord {
        step_id: "write".into(),
        reviewed_attempt: 0,
        verdict,
        to: None,
        reason: "r".into(),
        held: None,
        member_reason: None,
        dispute: None,
    };
    assert_eq!(
        super::accept_record_refusal(true, "write", Some(&rec(StepVerdict::Accepted))),
        None
    );
    let missing = super::accept_record_refusal(true, "write", None).expect("no record: pause");
    assert!(missing.contains("step.reviewed"), "{missing}");
    assert!(
        super::accept_record_refusal(true, "write", Some(&rec(StepVerdict::Rejected))).is_some(),
        "the stream says rejected: the line and the record disagree"
    );
    assert_eq!(super::accept_record_refusal(false, "write", None), None);
}
