//! DES-TEAMING-002 T4 (§8.7) through the REAL engine: a plan launched through `Core`, its units
//! run by a scripted worker, the revision applied at the step boundary, and every team fact read
//! off a real bus db. The diff re-score reaches the actor exactly as the supervisor sends it
//! (`Command::TeamRescored` on the actor's channel); the worker sends it BEFORE it returns, so it
//! is on the channel ahead of the unit's `ApplyStepResult`, as the supervisor's final-pass
//! re-score is ahead of the fold the worker waits for. The supervisor's own half (the snapshot,
//! the bound, the paths of a real git diff) is `team::supervisor_tests`.
//!
//! The steps are the non-code catalog pair (`produce` → `critique`): the plan-growth machinery is
//! identical for `build`, whose evidence-floor pin needs a worktree and a judge seat these rigs
//! do not have. Every run has no graph, so a behavioural diff fails closed at 100 (S4's rule),
//! and a docs-only touch set scores 0.

use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use wicked_council::types::{Category, Confidence, Dispatcher, InputMode, Vote};
use wicked_council::{AgenticCli, CouncilTask};

use crate::command::Command;
use crate::team::events as tev;
use crate::team::publish::tests::{rig, Rig};
use crate::team::publish::TeamConfig;
use crate::workflow::{HumanDecision, StepInput, StepOutput, StepRunner, StepStatus};
use crate::{Core, CoreEvent, HumanConfirm, LaunchSpec, PlanSteps, SessionStatus};

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

/// What the scripted worker does on one dispatch.
#[derive(Default)]
struct Turn {
    output: String,
    /// The paths the supervisor would report for this attempt's settled diff (`None` = none).
    diff: Option<Vec<String>>,
    /// Raise one HIGH finding on the attempt (an unresolved HIGH pauses `team_dispute`).
    raise_high: bool,
    /// Hold the turn until the test ends (nothing runs on past it).
    hold: bool,
}

fn turn(output: &str, diff: Option<&[&str]>) -> Turn {
    Turn {
        output: output.to_string(),
        diff: diff.map(|d| d.iter().map(|p| p.to_string()).collect()),
        ..Turn::default()
    }
}

fn hold() -> Turn {
    Turn {
        hold: true,
        ..Turn::default()
    }
}

type ScriptFn = Box<dyn Fn(&StepInput, usize) -> Turn + Send + Sync>;

/// A scripted worker: `script(input, dispatch_index)` decides each turn; every dispatch is
/// recorded, so "what ran after the revision" (and at which attempt) is read off the worker.
struct Worker {
    tx: OnceLock<Sender<Command>>,
    bus: OnceLock<crate::team::publish::TeamBus>,
    script: ScriptFn,
    calls: Mutex<Vec<(u32, u32, String)>>,
    n: AtomicUsize,
    release: Arc<(Mutex<bool>, std::sync::Condvar)>,
}

impl Worker {
    fn scripted(script: impl Fn(&StepInput, usize) -> Turn + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            tx: OnceLock::new(),
            bus: OnceLock::new(),
            script: Box::new(script),
            calls: Mutex::default(),
            n: AtomicUsize::new(0),
            release: Arc::new((Mutex::new(false), std::sync::Condvar::new())),
        })
    }
    /// The first dispatch runs `output` (with `diff`); every later one is held.
    fn new(output: &str, diff: Option<&[&str]>) -> Arc<Self> {
        let first: Vec<String> = diff
            .unwrap_or_default()
            .iter()
            .map(|p| p.to_string())
            .collect();
        let has_diff = diff.is_some();
        let output = output.to_string();
        Self::scripted(move |_, n| {
            if n == 0 {
                Turn {
                    output: output.clone(),
                    diff: has_diff.then(|| first.clone()),
                    ..Turn::default()
                }
            } else {
                hold()
            }
        })
    }
    /// `(ord, attempt, unit id)` of every dispatch, in order.
    fn calls(&self) -> Vec<(u32, u32, String)> {
        self.calls.lock().unwrap().clone()
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        release_all(self);
    }
}

fn release_all(w: &Worker) {
    let (l, cv) = &*w.release;
    *l.lock().unwrap() = true;
    cv.notify_all();
}

impl StepRunner for Worker {
    fn run_unit(&self, i: &StepInput) -> StepOutput {
        self.calls
            .lock()
            .unwrap()
            .push((i.unit.ord, i.attempt, i.unit.id.clone()));
        let t = (self.script)(i, self.n.fetch_add(1, AtomicOrdering::SeqCst));
        if t.raise_high {
            let f =
                crate::team::publish::tests::fixture_with(tev::FINDING_RAISED, 0, &i.run_id, |p| {
                    p["ord"] = json!(i.unit.ord);
                    p["attempt"] = json!(i.attempt);
                    p["raise_seq"] = json!(1);
                });
            self.bus
                .get()
                .expect("bus")
                .publish(&f)
                .expect("the finding is on the bus");
        }
        if let (Some(paths), Some(tx)) = (&t.diff, self.tx.get()) {
            // The supervisor's measurement of this attempt's settled diff, ahead of the result.
            tx.send(Command::TeamRescored {
                run_id: i.run_id.clone(),
                ord: i.unit.ord,
                attempt: i.attempt,
                rescore_seq: 1,
                tree: "0123456789abcdef0123456789abcdef01234567".into(),
                paths: paths.clone(),
            })
            .unwrap();
        }
        let output = if t.hold {
            let (l, cv) = &*self.release;
            let mut done = l.lock().unwrap();
            while !*done {
                done = cv.wait_timeout(done, Duration::from_millis(50)).unwrap().0;
            }
            "held".to_string()
        } else {
            t.output
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

struct Engine {
    core: Core,
    worker: Arc<Worker>,
    events: Receiver<CoreEvent>,
    seen: Vec<CoreEvent>,
    rig: Rig,
}

fn team_cfg(rig: &Rig, final_pass: Duration) -> TeamConfig {
    TeamConfig::new(Some(rig.bus.clone()), Some(rig.outbox.clone()))
        .with_schedule(vec![Duration::from_millis(40); 3])
        .with_attempt_wait(Duration::from_millis(30))
        .with_final_pass_budget(final_pass)
        .with_gate_poll(Duration::from_millis(20))
}

fn wire(core: Core, worker: Arc<Worker>, rig: Rig) -> Engine {
    let _ = worker.tx.set(core.tx.clone());
    let _ = worker.bus.set(rig.team_bus());
    let events = core.subscribe();
    core.ping();
    Engine {
        core,
        worker,
        events,
        seen: Vec::new(),
        rig,
    }
}

/// No supervisor: the worker synthesizes its (empty, or its own raise's) ledger after 300 ms.
fn engine(name: &str, worker: Arc<Worker>) -> Engine {
    let rig = rig(name);
    let db = rig.dir.join("core.db").to_string_lossy().into_owned();
    let cfg = team_cfg(&rig, Duration::from_millis(300));
    let core = Core::spawn_with_engine_team(db, Arc::new(StubDispatcher), worker.clone(), cfg);
    wire(core, worker, rig)
}

/// With the team supervisor on the bus (T6's member steps and the PA's review of them).
fn engine_supervised(name: &str, worker: Arc<Worker>) -> Engine {
    use crate::team::supervisor::tests::{FakeCouncil, FakeHost};
    let rig = rig(name);
    let db = rig.dir.join("core.db").to_string_lossy().into_owned();
    let cfg = team_cfg(&rig, Duration::from_secs(90));
    let core = Core::spawn_with_engine_team_supervised(
        db,
        Arc::new(StubDispatcher),
        worker.clone(),
        cfg,
        None,
        Arc::new(FakeHost::new(|_, _| Ok("DONE".into()))),
        Arc::new(FakeCouncil::yes()),
        |c| c.poll = Duration::from_millis(20),
    );
    wire(core, worker, rig)
}

fn plan(v: Value) -> PlanSteps {
    serde_json::from_value(v).expect("a plan")
}

fn launch(e: &Engine, run: &str, hc: HumanConfirm, p: PlanSteps) {
    launch_on(e, run, hc, p, &["a", "b"]);
}

/// Launch on the named seats (the first is the PA).
fn launch_on(e: &Engine, run: &str, hc: HumanConfirm, p: PlanSteps, seats: &[&str]) {
    e.core
        .launch_run(LaunchSpec {
            base_ref: None,
            project_id: None,
            problem: "t4 re-plan".into(),
            clis: seats.iter().map(|k| cli(k)).collect(),
            entity_mode: crate::EntityMode::Shared,
            session_id: run.into(),
            human_confirm: hc,
            auto_deliver: false,
            repo_ref: None,
            workflow: None,
            extra_write_roots: Vec::new(),
            extra_read_roots: Vec::new(),
            project_graph: None,
            plan: Some(p),
            deliver_step: None,
        })
        .expect("launch");
}

/// Poll until `cond` holds, generously (slow Windows runners), returning early.
fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(90);
    while Instant::now() < deadline {
        if cond() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting for {what}");
}

fn view(e: &Engine, run: &str) -> crate::domain::SessionView {
    e.core
        .sessions_detail()
        .unwrap()
        .into_iter()
        .find(|v| v.session.id == run)
        .expect("the run")
}

fn payloads(e: &Engine, run: &str, event_type: &str) -> Vec<Value> {
    crate::bus::BusDb::shared(&e.rig.bus)
        .unwrap()
        .poll(event_type, 0, 10_000)
        .unwrap()
        .into_iter()
        .filter(|r| r.payload["run_id"] == run)
        .map(|r| r.payload)
        .collect()
}

/// The run's `event_type` payloads once `n` are on the bus. A revision's facts are published
/// fire-and-forget, so they can land after the pause or dispatch that follows them.
fn settled(e: &Engine, run: &str, event_type: &str, n: usize) -> Vec<Value> {
    wait_for(&format!("{n} `{event_type}` of {run} on the bus"), || {
        payloads(e, run, event_type).len() >= n
    });
    payloads(e, run, event_type)
}

/// Every team row of the run, `(event_id, type, payload)`, in bus order.
fn rows(e: &Engine, run: &str) -> Vec<(i64, String, Value)> {
    crate::bus::BusDb::shared(&e.rig.bus)
        .unwrap()
        .poll("wicked.team.**", 0, 10_000)
        .unwrap()
        .into_iter()
        .filter(|r| r.payload["run_id"] == run)
        .map(|r| (r.event_id, r.event_type, r.payload))
        .collect()
}

impl Engine {
    fn drain(&mut self) {
        self.seen.extend(self.events.try_iter());
    }
    fn awaiting(&mut self, run: &str) -> Vec<(u32, String)> {
        self.drain();
        self.seen
            .iter()
            .filter_map(|ev| match ev {
                CoreEvent::AwaitingHuman {
                    session,
                    ord,
                    gate_kind,
                    ..
                } if session == run => Some((*ord, gate_kind.clone())),
                _ => None,
            })
            .collect()
    }
    fn wait_awaiting(&mut self, run: &str, kind: &str, n: usize) {
        let deadline = Instant::now() + Duration::from_secs(90);
        while Instant::now() < deadline {
            if self.awaiting(run).iter().filter(|(_, k)| k == kind).count() >= n {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!(
            "timed out waiting for {run}: {n} `{kind}` pause(s); paused {:?}; dispatched {:?}; \
             status {:?}",
            self.awaiting(run),
            self.worker.calls(),
            view(self, run).session.status
        );
    }
}

fn approve() -> HumanDecision {
    HumanDecision::Approve {
        amend: None,
        amend_scope: crate::workflow::AmendScope::Cursor,
    }
}

fn step_ids(steps: &Value) -> Vec<String> {
    steps
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap().to_string())
        .collect()
}

/// The PA's step output: 200+ characters of prose, then its lines.
fn pa_output(lines: &str) -> String {
    format!(
        "{}\n{lines}\n",
        "The step is done; the work is described here. ".repeat(6)
    )
}

// ── the operator item + (b) + (c): the diff re-score raises the floor into high risk ─────────────

/// (Operator item) The declared touch set is trusted at launch; the checkpoint re-score of the
/// settled diff corrects it. An AUTO-mode plan whose touch set is `README.md` (docs-only: band
/// 0-19, accepted with no pause) and whose creator step then changes `src/` files (a behavioural
/// diff with no graph fails closed at 100): at the step boundary the engine publishes
/// `path.scored{basis:"diff"}` and then `plan.revised{reason:"floor_raised"}` with the added floor
/// phases after the cursor, and — the new band being high risk — pauses `plan_approval` before
/// the next unit, auto mode included. (c): approving dispatches the first NEW unit; the done
/// creator is never dispatched again.
#[test]
fn t4_diff_rescore_into_high_risk_revises_and_pauses_before_the_next_unit_in_auto_mode() {
    let w = Worker::new(&pa_output("built it"), Some(&["src/lib.rs", "src/auth.rs"]));
    let mut e = engine("t4op", w.clone());
    launch(
        &e,
        "op",
        HumanConfirm::None,
        plan(
            json!({"steps": [{"catalog": "produce"}, {"catalog": "critique"}],
                    "touch": ["README.md"]}),
        ),
    );
    e.wait_awaiting("op", crate::plan_gate::GATE_KIND, 1);
    settled(&e, "op", tev::PLAN_REVISED, 1);
    // The initial plan went through with no approval: the only plan gate is the revision's.
    let accepted = payloads(&e, "op", tev::PLAN_ACCEPTED);
    assert_eq!(accepted.len(), 1, "rev 1 accepted once, by the engine");
    assert_eq!(accepted[0]["band"], "0-19");
    assert_eq!(accepted[0]["by"], "engine");
    // Only the creator step ran; the run holds before the next unit.
    assert_eq!(e.worker.calls().len(), 1, "{:?}", e.worker.calls());
    let v = view(&e, "op");
    assert_eq!(v.session.status, SessionStatus::AwaitingHuman);
    // path.scored{diff} then plan.revised{floor_raised}, in that order on the bus.
    let rs = rows(&e, "op");
    let scored = rs
        .iter()
        .position(|(_, t, p)| t == tev::PATH_SCORED && p["basis"] == "diff")
        .expect("path.scored{basis:diff}");
    let revised = rs
        .iter()
        .position(|(_, t, _)| t == tev::PLAN_REVISED)
        .expect("plan.revised");
    assert!(scored < revised, "path.scored precedes plan.revised");
    let d = &rs[scored].2;
    assert_eq!(d["score"], 100);
    assert_eq!(d["ord"], 1);
    let r = &rs[revised].2;
    assert_eq!(r["reason"], "floor_raised");
    assert_eq!(r["from_band"], "0-19");
    assert_eq!(r["to_band"], "70-100");
    assert_eq!(r["high_risk"], true);
    assert_eq!(r["plan_rev"], 2);
    assert_eq!(r["proposal_id"], Value::Null);
    let added: Vec<(String, bool)> = r["added"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            (
                s["catalog"].as_str().unwrap().to_string(),
                s["late"].as_bool().unwrap(),
            )
        })
        .collect();
    // §8.5's 70-100 floor on a non-code run, minus what the plan had; the three that precede the
    // done `produce` in the catalog are late.
    assert_eq!(
        added,
        vec![
            ("test_plan".to_string(), true),
            ("design".to_string(), true),
            ("architecture".to_string(), true),
            ("security_review".to_string(), false),
        ]
    );
    // The gate: `plan_approval`, before the first new unit, reviewing the creator.
    let gate = payloads(&e, "op", tev::GATE_OPENED)
        .into_iter()
        .find(|p| p["kind"] == "plan_approval")
        .expect("gate.opened{plan_approval}");
    assert_eq!(gate["reason"], "into_high_risk");
    assert_eq!(gate["ord"], 2);
    assert_eq!(gate["reviewing_ord"], 1);
    // Units: the done prefix untouched, the late phases at the cursor, the rest after.
    let ids: Vec<String> = v.units.iter().map(|u| u.id.clone()).collect();
    assert_eq!(
        ids,
        [
            "op:produce",
            "op:test_plan",
            "op:design",
            "op:architecture",
            "op:critique",
            "op:security_review"
        ]
    );
    assert_eq!(v.units[0].status, crate::domain::UnitStatus::Done);
    // (c) approve: the first new unit dispatches, once; the done creator never again.
    e.core.confirm_gate("op", approve()).unwrap();
    wait_for("the first new unit to dispatch", || {
        e.worker.calls().len() >= 2
    });
    std::thread::sleep(Duration::from_millis(300));
    let calls = e.worker.calls();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_eq!(calls[1].2, "op:test_plan");
    assert_eq!(calls[1].1, 0, "a new unit runs at its own attempt 0");
    assert!(
        calls.iter().filter(|c| c.2 == "op:produce").count() == 1,
        "the done unit is never re-dispatched: {calls:?}"
    );
    // The released rev is accepted by the human, after the revision.
    let accepted = payloads(&e, "op", tev::PLAN_ACCEPTED);
    assert_eq!(accepted.last().unwrap()["plan_rev"], 2);
    assert_eq!(accepted.last().unwrap()["by"], "human");
    release_all(&w);
}

/// (a) second clause: the floor RISES first (a behavioural diff: 0-19 → 70-100, one
/// `path.scored{diff}` and one `plan.revised`), then a later LOWER re-score (a docs-only diff)
/// publishes nothing more — no second `path.scored`, no second revision, no second gate.
#[test]
fn t4_a_a_lower_rescore_after_a_raise_publishes_nothing() {
    let w = Worker::scripted(|i, _| {
        let id = i.unit.id.as_str();
        if id.ends_with(":produce") {
            turn(&pa_output("built it"), Some(&["src/lib.rs"]))
        } else if id.ends_with(":test_plan") {
            turn(&pa_output("planned the tests"), Some(&["docs/tests.md"]))
        } else {
            hold()
        }
    });
    let mut e = engine("t4low", w.clone());
    launch(
        &e,
        "low",
        HumanConfirm::None,
        plan(
            json!({"steps": [{"catalog": "produce"}, {"catalog": "critique"}],
                    "touch": ["README.md"]}),
        ),
    );
    e.wait_awaiting("low", crate::plan_gate::GATE_KIND, 1);
    let diff_scores = |e: &Engine| {
        payloads(e, "low", tev::PATH_SCORED)
            .into_iter()
            .filter(|p| p["basis"] == "diff")
            .count()
    };
    wait_for("the raise on the bus", || {
        diff_scores(&e) == 1 && payloads(&e, "low", tev::PLAN_REVISED).len() == 1
    });
    e.core.confirm_gate("low", approve()).unwrap();
    // test_plan (the lower re-score) runs, and the run moves on to the next unit.
    wait_for("the unit after test_plan to dispatch", || {
        e.worker.calls().len() >= 3
    });
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        diff_scores(&e),
        1,
        "the lower re-score publishes no path.scored"
    );
    assert_eq!(
        payloads(&e, "low", tev::PLAN_REVISED).len(),
        1,
        "no second revision"
    );
    assert_eq!(
        e.awaiting("low")
            .iter()
            .filter(|(_, k)| k == crate::plan_gate::GATE_KIND)
            .count(),
        1,
        "no second plan gate"
    );
    release_all(&w);
}

// ── HIGH-1: every advance applies a held revision — a dispute answer, a member acceptance ─────────

/// The raise on a step whose unit pauses `team_dispute` (an unresolved HIGH) is applied when the
/// dispute is answered and the run advances: `path.scored{diff}` → `plan.revised{floor_raised}` →
/// `plan_approval` before the next unit — never a dispatch past it.
#[test]
fn t4_high1_a_raise_on_a_disputed_step_is_applied_when_the_dispute_is_answered() {
    let w = Worker::scripted(|i, n| {
        if n == 0 {
            Turn {
                raise_high: true,
                ..turn(&pa_output("built it"), Some(&["src/lib.rs"]))
            }
        } else {
            let _ = i;
            hold()
        }
    });
    let mut e = engine("t4disp", w.clone());
    launch(
        &e,
        "disp",
        HumanConfirm::None,
        plan(
            json!({"steps": [{"catalog": "produce"}, {"catalog": "critique"}],
                    "touch": ["README.md"]}),
        ),
    );
    e.wait_awaiting("disp", "team_dispute", 1);
    e.core.confirm_gate("disp", approve()).unwrap();
    e.wait_awaiting("disp", crate::plan_gate::GATE_KIND, 1);
    assert_eq!(
        e.worker.calls().len(),
        1,
        "nothing dispatched past the raise"
    );
    wait_for("the raise on the bus", || {
        payloads(&e, "disp", tev::PLAN_REVISED).len() == 1
    });
    let revised = &payloads(&e, "disp", tev::PLAN_REVISED)[0];
    assert_eq!(revised["reason"], "floor_raised");
    assert_eq!(revised["to_band"], "70-100");
    assert!(payloads(&e, "disp", tev::PATH_SCORED)
        .iter()
        .any(|p| p["basis"] == "diff"));
    release_all(&w);
}

/// The step is the team's (`owner:"team"`): the member's work changes `src/`, the PA's review
/// accepts it, and the acceptance's advance applies the raise before the next unit.
#[test]
fn t4_high1_a_raise_on_a_member_step_is_applied_when_the_pa_accepts_it() {
    let w = Worker::scripted(|_, n| match n {
        0 => turn(&pa_output("the member built it"), Some(&["src/lib.rs"])),
        1 => turn("reviewed\nSTEP produce: ACCEPT — it is right", None),
        _ => hold(),
    });
    let mut e = engine_supervised("t4mem-acc", w.clone());
    // Three seats: on two, the revised high-risk plan cannot be staffed (a team run never grades
    // on a creator seat, seam D1) and the revision is refused as unplannable.
    launch_on(
        &e,
        "macc",
        HumanConfirm::None,
        plan(
            json!({"steps": [{"catalog": "produce", "owner": "team"}, {"catalog": "critique"}],
                    "touch": ["README.md"]}),
        ),
        &["a", "b", "c"],
    );
    e.wait_awaiting("macc", crate::plan_gate::GATE_KIND, 1);
    let calls = e.worker.calls();
    assert_eq!(
        calls.len(),
        2,
        "the member's work and the PA's review only: {calls:?}"
    );
    let revised = settled(&e, "macc", tev::PLAN_REVISED, 1);
    assert_eq!(revised.len(), 1);
    assert_eq!(revised[0]["reason"], "floor_raised");
    release_all(&w);
}

// ── MEDIUM-1: the PA answers a member's request in its REVIEW of the member's step ──────────────

/// The render asks the PA to answer `change.requested` at its next boundary; on a member step
/// that turn is its review. `PLAN <change_id>: ACCEPT` + `PLAN+` there is a revision
/// (`plan.revised{reason:"member_request"}`) applied when the step is accepted.
#[test]
fn t4_medium1_the_pa_review_accepting_a_member_request_revises_the_plan() {
    let w = Worker::scripted(|_, n| match n {
        0 => turn(&pa_output("the member built it"), None),
        1 => turn(
            "reviewed\nSTEP produce: ACCEPT — it is right\n\
             PLAN c-00000000000000aa: ACCEPT — the member is right\n\
             PLAN+ {\"steps\":[{\"catalog\":\"test\"}]}",
            None,
        ),
        _ => hold(),
    });
    let e = engine_supervised("t4mem-req", w.clone());
    launch(
        &e,
        "mreq",
        HumanConfirm::None,
        plan(
            json!({"steps": [{"catalog": "produce", "owner": "team"}, {"catalog": "critique"}],
                    "touch": ["README.md"]}),
        ),
    );
    wait_for("the unit after the member step to dispatch", || {
        e.worker.calls().len() >= 3
    });
    let revised = settled(&e, "mreq", tev::PLAN_REVISED, 1);
    assert_eq!(revised.len(), 1, "{revised:#?}");
    assert_eq!(revised[0]["reason"], "member_request");
    let want = tev::mint_proposal_id(
        "mreq",
        "a",
        &tev::ProposalSource::Change {
            change_id: "c-00000000000000aa".into(),
        },
    );
    assert_eq!(revised[0]["proposal_id"], want.as_str());
    release_all(&w);
}

// ── MEDIUM-2: a revision after a request_changes rewind keeps the rewound units' attempts ──────

/// The review requests changes (a def gate on `critique`): the creator re-runs at attempt 1 and
/// its settled diff re-scores into high risk. The revision inserts the floor phases, and the
/// review — already run once — re-dispatches at its own NEXT attempt (1), never a reused 0.
#[test]
fn t4_medium2_a_revision_after_a_rewind_keeps_the_review_attempt() {
    let w = Worker::scripted(|i, _| {
        let id = i.unit.id.as_str();
        let reran = i.attempt > 0;
        if id.ends_with(":produce") && reran {
            turn(&pa_output("reworked it"), Some(&["src/lib.rs"]))
        } else if id.ends_with(":critique") && reran {
            hold()
        } else {
            // An evaluator ends with its verdict (a creator ignores the line).
            turn(&pa_output("did the step\nVERDICT: PASS"), None)
        }
    });
    let mut e = engine("t4rew", w.clone());
    launch(
        &e,
        "rew",
        HumanConfirm::None,
        plan(json!({"steps": [
            {"catalog": "produce"},
            {"catalog": "critique", "gate": {"human_confirm": {"unconditional": false}}},
            {"catalog": "understand", "id": "wrapup"}],
            "touch": ["README.md"]})),
    );
    // The def gate after the review: send it back to the creator.
    e.wait_awaiting("rew", "def", 1);
    e.core
        .confirm_gate(
            "rew",
            HumanDecision::RequestChanges {
                note: Some("cover the edge case".into()),
            },
        )
        .unwrap();
    e.wait_awaiting("rew", crate::plan_gate::GATE_KIND, 1);
    e.core.confirm_gate("rew", approve()).unwrap();
    wait_for("the review to re-dispatch", || {
        e.worker
            .calls()
            .iter()
            .filter(|c| c.2 == "rew:critique")
            .count()
            == 2
    });
    let calls = e.worker.calls();
    let reviews: Vec<u32> = calls
        .iter()
        .filter(|c| c.2 == "rew:critique")
        .map(|c| c.1)
        .collect();
    assert_eq!(
        reviews,
        [0, 1],
        "the review re-runs at attempt 1: {calls:?}"
    );
    release_all(&w);
}

// ── MEDIUM-3: a revision that cannot be planned changes nothing ─────────────────────────────────

/// The PA adds a Tool step whose binary does not exist: planning refuses it. The run keeps its
/// accepted rev and its floor (nothing persisted ahead of the units), says why, and goes on.
#[test]
fn t4_medium3_a_revision_that_cannot_be_planned_leaves_the_plan_as_it_was() {
    let w = Worker::new(
        &pa_output(
            r#"PLAN+ {"steps":[{"catalog":"run","id":"run","executor":{"type":"tool","cmd":["wicked-t4-no-such-binary"]}}]}"#,
        ),
        None,
    );
    let e = engine("t4plan", w.clone());
    launch(
        &e,
        "pln",
        HumanConfirm::None,
        plan(
            json!({"steps": [{"catalog": "produce"}, {"catalog": "critique"}],
                    "touch": ["README.md"]}),
        ),
    );
    wait_for("the next unit to dispatch", || e.worker.calls().len() >= 2);
    let v = view(&e, "pln");
    let tp = v.session.team_plan.as_ref().unwrap();
    assert_eq!(
        (tp.rev, tp.accepted_rev),
        (1, 1),
        "no rev persisted ahead of its units"
    );
    assert_eq!(v.units.len(), 2);
    assert_eq!(e.worker.calls()[1].2, "pln:critique");
    assert!(payloads(&e, "pln", tev::PLAN_ACCEPTED)
        .iter()
        .all(|p| p["plan_rev"] == 1));
    release_all(&w);
}

// ── (b) a revision in auto mode below high risk does not pause ──────────────────────────────────

/// (b) + (d) first half: in auto mode the PA's `PLAN+` on a user-composed plan is
/// `plan.proposed{kind:"change", by:<PA>}` then `plan.revised{reason:"pa_added"}`, below high risk,
/// so the run does not pause; the user's steps are all still present, in order.
#[test]
fn t4_b_d_a_pa_revision_in_auto_mode_below_high_risk_does_not_pause() {
    let w = Worker::new(
        &pa_output(r#"PLAN+ {"steps":[{"catalog":"security_review"}],"reason":"auth code"}"#),
        None,
    );
    let mut e = engine("t4pa", w.clone());
    launch(
        &e,
        "pa",
        HumanConfirm::None,
        plan(
            json!({"steps": [{"catalog": "produce"}, {"catalog": "critique"}],
                    "touch": ["README.md"]}),
        ),
    );
    wait_for("the next unit to dispatch", || e.worker.calls().len() >= 2);
    settled(&e, "pa", tev::PLAN_REVISED, 1);
    let proposed = payloads(&e, "pa", tev::PLAN_PROPOSED);
    let change = proposed
        .iter()
        .find(|p| p["kind"] == "change")
        .expect("plan.proposed{kind:change}");
    assert_eq!(change["by"], "a", "the PA seat proposes");
    assert_eq!(change["base_rev"], 1);
    let revised = payloads(&e, "pa", tev::PLAN_REVISED);
    assert_eq!(revised.len(), 1);
    assert_eq!(revised[0]["reason"], "pa_added");
    assert_eq!(revised[0]["proposal_id"], change["proposal_id"]);
    assert_eq!(revised[0]["high_risk"], false);
    assert!(
        e.awaiting("pa").is_empty(),
        "no pause below high risk in auto mode"
    );
    let accepted = payloads(&e, "pa", tev::PLAN_ACCEPTED);
    let last = accepted.last().unwrap();
    assert_eq!(last["plan_rev"], 2);
    assert_eq!(
        step_ids(&last["steps"]),
        ["produce", "critique", "security_review"],
        "the user's steps, in order, then the PA's"
    );
    assert_eq!(e.worker.calls()[1].2, "pa:critique");
    release_all(&w);
}

// ── (b) manual mode + (d) two concurrent proposals against one base_rev ─────────────────────────

/// (b) manual mode: every revision pauses. (d) second half: the PA's `PLAN+` and a human edit
/// made against the SAME `base_rev` (the revision's gate is open, rev 1 still the accepted one)
/// are two `plan.proposed` rows with distinct `proposal_id`s and two successive revisions (rev 2
/// the PA's, rev 3 the human's); neither is dropped: rev 3 carries both additions, and the done
/// unit is kept as it ran.
#[test]
fn t4_b_d_manual_every_revision_pauses_and_two_concurrent_proposals_both_land() {
    let w = Worker::new(
        &pa_output(r#"PLAN+ {"steps":[{"catalog":"security_review"}]}"#),
        None,
    );
    let mut e = engine("t4man", w.clone());
    // Manual mode (`Before(_)`), with no run-level pause on these units.
    launch(
        &e,
        "man",
        HumanConfirm::Before(99),
        plan(
            json!({"steps": [{"catalog": "produce"}, {"catalog": "critique"}],
                    "touch": ["README.md"]}),
        ),
    );
    e.wait_awaiting("man", crate::plan_gate::GATE_KIND, 1);
    e.core.confirm_gate("man", approve()).unwrap();
    // The PA's revision pauses, even below high risk.
    e.wait_awaiting("man", crate::plan_gate::GATE_KIND, 2);
    assert_eq!(e.worker.calls().len(), 1);
    let revised = settled(&e, "man", tev::PLAN_REVISED, 1);
    assert_eq!(revised.len(), 1);
    assert_eq!(revised[0]["reason"], "pa_added");
    assert_eq!(revised[0]["high_risk"], false);
    // The human edits at that gate, adding a step of their own.
    e.core
        .confirm_gate(
            "man",
            HumanDecision::EditPlan {
                plan: plan(json!({"steps": [{"catalog": "test_plan"}]})),
            },
        )
        .unwrap();
    wait_for("the first unit after the edit", || {
        e.worker.calls().len() >= 2
    });
    wait_for("rev 3 on the bus", || {
        payloads(&e, "man", tev::PLAN_ACCEPTED)
            .last()
            .is_some_and(|p| p["plan_rev"] == 3)
    });
    let proposed: Vec<Value> = payloads(&e, "man", tev::PLAN_PROPOSED)
        .into_iter()
        .filter(|p| p["kind"] != "initial")
        .collect();
    assert_eq!(proposed.len(), 2, "{proposed:#?}");
    assert_eq!(proposed[0]["kind"], "change");
    assert_eq!(proposed[1]["kind"], "edit");
    assert_eq!(proposed[0]["base_rev"], 1);
    assert_eq!(proposed[1]["base_rev"], 1, "both against the same base rev");
    assert_ne!(proposed[0]["proposal_id"], proposed[1]["proposal_id"]);
    let accepted = payloads(&e, "man", tev::PLAN_ACCEPTED);
    let last = accepted.last().unwrap();
    assert_eq!(last["plan_rev"], 3);
    assert_eq!(last["by"], "human");
    let ids = step_ids(&last["steps"]);
    for want in ["produce", "critique", "security_review", "test_plan"] {
        assert!(ids.contains(&want.to_string()), "{want} kept: {ids:?}");
    }
    let (p, c) = (
        ids.iter().position(|s| s == "produce").unwrap(),
        ids.iter().position(|s| s == "critique").unwrap(),
    );
    assert!(p < c, "the user's steps stay in order: {ids:?}");
    // The late `test_plan` is at the cursor; the done `produce` is not dispatched again.
    let calls = e.worker.calls();
    assert_eq!(calls[1].2, "man:test_plan");
    assert_eq!(calls.iter().filter(|c| c.2 == "man:produce").count(), 1);
    release_all(&w);
}

// ── (e) a member's request becomes a revision only through the PA ──────────────────────────────

/// (e) The PA's `PLAN <change_id>: ACCEPT` and its `PLAN+` restatement, in the step output, are
/// `plan.proposed{kind:"change"}` sourced by the change id, then
/// `plan.revised{reason:"member_request"}`.
#[test]
fn t4_e_an_accepted_member_request_is_a_revision_from_the_pa_output() {
    let w = Worker::new(
        &pa_output(
            "PLAN c-0123456789abcdef: ACCEPT — the member is right\n\
             PLAN+ {\"steps\":[{\"catalog\":\"security_review\"}]}",
        ),
        None,
    );
    let mut e = engine("t4mem", w.clone());
    launch(
        &e,
        "mem",
        HumanConfirm::None,
        plan(
            json!({"steps": [{"catalog": "produce"}, {"catalog": "critique"}],
                    "touch": ["README.md"]}),
        ),
    );
    wait_for("the next unit to dispatch", || e.worker.calls().len() >= 2);
    let revised = settled(&e, "mem", tev::PLAN_REVISED, 1);
    assert_eq!(revised.len(), 1);
    assert_eq!(revised[0]["reason"], "member_request");
    let want = tev::mint_proposal_id(
        "mem",
        "a",
        &tev::ProposalSource::Change {
            change_id: "c-0123456789abcdef".into(),
        },
    );
    assert_eq!(revised[0]["proposal_id"], want.as_str());
    assert!(e.awaiting("mem").is_empty());
    release_all(&w);
}

/// (e) A member's `change.requested` on the bus with no PA answer in the step output changes
/// nothing: the engine never reads the member's row.
#[test]
fn t4_e_a_change_requested_without_the_pa_answer_is_no_revision() {
    let w = Worker::new(&pa_output("no plan lines"), None);
    let e = engine("t4req", w.clone());
    let req = crate::team::publish::tests::fixture_with(tev::CHANGE_REQUESTED, 0, "req", |p| {
        p["steps"] = json!([{"catalog": "security_review", "id": "security_review"}]);
    });
    e.rig.team_bus().publish(&req).unwrap();
    launch(
        &e,
        "req",
        HumanConfirm::None,
        plan(
            json!({"steps": [{"catalog": "produce"}, {"catalog": "critique"}],
                    "touch": ["README.md"]}),
        ),
    );
    wait_for("the next unit to dispatch", || e.worker.calls().len() >= 2);
    std::thread::sleep(Duration::from_millis(300));
    assert!(payloads(&e, "req", tev::PLAN_REVISED).is_empty());
    assert_eq!(e.worker.calls()[1].2, "req:critique");
    release_all(&w);
}

/// (e) The actor holds no bus connection for team facts and never polls the bus: every team
/// fact it reads arrives on its command channel. Read off the actor's and the plan gate's source.
#[test]
fn t4_e_the_actor_never_polls_the_bus() {
    for (name, src) in [
        ("actor.rs", include_str!("../actor.rs")),
        ("actor/team_gate.rs", include_str!("team_gate.rs")),
        ("plan_gate.rs", include_str!("../plan_gate.rs")),
        (
            "plan_gate/revise.rs",
            include_str!("../plan_gate/revise.rs"),
        ),
    ] {
        for needle in [".poll(", "read_run(", "TEAM_FILTER", "BusDb::open"] {
            assert!(!src.contains(needle), "{name} must not call `{needle}`");
        }
    }
}

// ── T8 (c), (e): Core::propose_plan and Core::preview_plan through the real engine ─────────────

/// A worker whose first dispatch waits until the test says `go` (so an edit arrives mid-unit);
/// every later dispatch is held.
fn gated_worker() -> (Arc<Worker>, Arc<std::sync::atomic::AtomicBool>) {
    gated_worker_with(pa_output("built it"), None)
}

/// [`gated_worker`] whose first turn ends with `output` and, when given, the supervisor's re-score
/// of `diff` (sent before the result, as the supervisor's final pass is).
fn gated_worker_with(
    output: String,
    diff: Option<Vec<String>>,
) -> (Arc<Worker>, Arc<std::sync::atomic::AtomicBool>) {
    let go = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let g = go.clone();
    let w = Worker::scripted(move |_, n| {
        if n == 0 {
            wait_for("the test to release the first unit", || {
                g.load(AtomicOrdering::SeqCst)
            });
            Turn {
                output: output.clone(),
                diff: diff.clone(),
                ..Turn::default()
            }
        } else {
            hold()
        }
    });
    (w, go)
}

fn docs_plan() -> PlanSteps {
    plan(
        json!({"steps": [{"catalog": "produce"}, {"catalog": "critique"}],
                "touch": ["README.md"]}),
    )
}

fn human_edits(e: &Engine, run: &str) -> Vec<Value> {
    payloads(e, run, tev::PLAN_PROPOSED)
        .into_iter()
        .filter(|p| p["kind"] == "edit")
        .collect()
}

/// (T8 (c)) An edit proposed mid-unit is HELD (nothing published while the unit runs) and applied
/// at the step boundary through the revision path: `plan.proposed{by:"human", kind:"edit"}`
/// sourced by the request id, then `plan.accepted{by:"human"}` (its author approved it) as rev 2, the added step at the cursor, the done unit never re-run. The same request id, before
/// or after the boundary, is a `duplicate`: no second row. (T8 (e)) the preview of the launch plan
/// is the floor fill the launch computed.
#[test]
fn t8_c_propose_plan_applies_at_the_boundary_once_per_request_id() {
    let (w, go) = gated_worker();
    let e = engine("t8prop", w.clone());
    let preview = e
        .core
        .preview_plan(docs_plan(), None, None, None, HumanConfirm::None)
        .unwrap();
    launch(&e, "prop", HumanConfirm::None, docs_plan());
    wait_for("the creator to dispatch", || e.worker.calls().len() == 1);
    let accepted = settled(&e, "prop", tev::PLAN_ACCEPTED, 1);
    // (e) The preview is what the launch computed.
    let shape = |steps: &Value| -> Vec<(String, String, String)> {
        steps
            .as_array()
            .unwrap()
            .iter()
            .map(|s| {
                (
                    s["catalog"].as_str().unwrap().to_string(),
                    s["id"].as_str().unwrap().to_string(),
                    s["added_by"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    };
    let previewed = serde_json::to_value(&preview).unwrap();
    assert_eq!(shape(&previewed["steps"]), shape(&accepted[0]["steps"]));
    assert_eq!(previewed["band"], accepted[0]["band"]);
    assert_eq!(previewed["high_risk"], accepted[0]["high_risk"]);
    assert!(!preview.pauses);

    let edit = plan(json!({"steps": [{"catalog": "test_plan"}]}));
    let first = e.core.propose_plan("prop", edit.clone(), "req-1").unwrap();
    let again = e.core.propose_plan("prop", edit.clone(), "req-1").unwrap();
    let want = tev::mint_proposal_id(
        "prop",
        "human",
        &tev::ProposalSource::Edit {
            request_id: "req-1".into(),
        },
    );
    assert_eq!(
        (first.proposal_id.as_str(), first.duplicate),
        (want.as_str(), false)
    );
    assert_eq!(
        (again.proposal_id.as_str(), again.duplicate),
        (want.as_str(), true)
    );
    // Held while the unit runs: nothing of the edit is on the bus yet.
    std::thread::sleep(Duration::from_millis(300));
    assert!(human_edits(&e, "prop").is_empty());
    go.store(true, AtomicOrdering::SeqCst);
    // The boundary applies it; the facts are fire-and-forget, so wait for them.
    let accepted = settled(&e, "prop", tev::PLAN_ACCEPTED, 2);
    wait_for("the added unit to dispatch", || e.worker.calls().len() >= 2);
    wait_for("the edit's plan.proposed", || {
        human_edits(&e, "prop").len() == 1
    });
    let proposed = &human_edits(&e, "prop")[0];
    assert_eq!(proposed["by"], "human");
    assert_eq!(proposed["proposal_id"], want.as_str());
    assert_eq!(proposed["base_rev"], 1);
    assert_eq!(accepted[1]["plan_rev"], 2);
    assert_eq!(
        accepted[1]["by"], "human",
        "a human edit is approved by its author"
    );
    assert_eq!(accepted[1]["proposal_id"], want.as_str());
    let ids: Vec<String> = view(&e, "prop")
        .units
        .iter()
        .map(|u| u.id.clone())
        .collect();
    assert_eq!(ids, ["prop:produce", "prop:test_plan", "prop:critique"]);
    let calls = e.worker.calls();
    assert_eq!(calls[1].2, "prop:test_plan", "{calls:?}");
    // After the boundary the request id is still spent: no second row of any kind.
    let late = e.core.propose_plan("prop", edit, "req-1").unwrap();
    assert!(late.duplicate);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(human_edits(&e, "prop").len(), 1);
    assert_eq!(payloads(&e, "prop", tev::PLAN_ACCEPTED).len(), 2);
    assert!(payloads(&e, "prop", tev::PLAN_REFUSED).is_empty());
    release_all(&w);
}

/// (T8 (c), operator decision) A HUMAN's mid-run edit is approved by its author, like an edit at
/// the gate: in manual mode it opens NO second `plan_approval` gate — it is accepted directly as
/// rev 2 `by:"human"` — while floor fill and the ratchet still apply. The launch plan has no
/// creator but a behavioural touch set, so the run's ratcheted score is 100 with an empty floor;
/// the edit adds a creator (`produce`), and the 70-100 floor it now owes is added with it.
#[test]
fn t8_c_a_human_edit_in_manual_mode_is_accepted_by_its_author_and_floor_filled() {
    let (w, go) = gated_worker();
    let mut e = engine("t8man", w.clone());
    // Manual mode that pauses only for plans (no unit is at ord 99).
    launch_on(
        &e,
        "man",
        HumanConfirm::Before(99),
        plan(
            json!({"steps": [{"catalog": "understand"}, {"catalog": "critique"}],
                    "touch": ["src/lib.rs"]}),
        ),
        &["a", "b", "c"],
    );
    e.wait_awaiting("man", crate::plan_gate::GATE_KIND, 1);
    e.core.confirm_gate("man", approve()).unwrap();
    wait_for("the first unit to dispatch", || e.worker.calls().len() == 1);
    let p = e
        .core
        .propose_plan(
            "man",
            plan(json!({"steps": [{"catalog": "produce"}]})),
            "req-m",
        )
        .unwrap();
    assert!(!p.duplicate);
    go.store(true, AtomicOrdering::SeqCst);
    // No second plan gate: the next unit dispatches straight away.
    wait_for("the unit after the edit to dispatch", || {
        e.worker.calls().len() >= 2
    });
    let accepted = settled(&e, "man", tev::PLAN_ACCEPTED, 2);
    wait_for("the edit's plan.proposed", || {
        human_edits(&e, "man").len() == 1
    });
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        e.awaiting("man")
            .iter()
            .filter(|(_, k)| k == crate::plan_gate::GATE_KIND)
            .count(),
        1,
        "only the launch plan's gate"
    );
    assert_eq!(
        payloads(&e, "man", tev::GATE_OPENED)
            .iter()
            .filter(|g| g["kind"] == "plan_approval")
            .count(),
        1
    );
    assert_eq!(accepted[1]["plan_rev"], 2);
    assert_eq!(accepted[1]["by"], "human");
    let mut floor_added: Vec<String> = accepted[1]["steps"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["added_by"] == "floor")
        .map(|s| s["catalog"].as_str().unwrap().to_string())
        .collect();
    floor_added.sort();
    assert_eq!(
        floor_added,
        ["architecture", "design", "security_review", "test_plan"],
        "the floor the edit owes at the ratcheted score"
    );
    let ids: Vec<String> = view(&e, "man").units.iter().map(|u| u.id.clone()).collect();
    assert!(ids.contains(&"man:produce".to_string()), "{ids:?}");
    release_all(&w);
}

/// (T8 (c)) What `propose_plan` refuses synchronously, holding nothing: an unknown run, an empty
/// request id, and an edit carrying `touch` or `override` (launch-plan fields) or no steps.
#[test]
fn t8_c_propose_plan_refusals() {
    let (w, go) = gated_worker();
    let e = engine("t8ref", w.clone());
    launch(&e, "ref", HumanConfirm::None, docs_plan());
    wait_for("the creator to dispatch", || e.worker.calls().len() == 1);
    let add = || plan(json!({"steps": [{"catalog": "test_plan"}]}));
    assert!(e.core.propose_plan("nope", add(), "r").is_err());
    assert!(e.core.propose_plan("ref", add(), " ").is_err());
    for bad in [
        json!({"steps": [{"catalog": "test_plan"}], "touch": ["src/x.rs"]}),
        json!({"steps": [{"catalog": "test_plan"}], "override": {"remove": ["design"], "reason": "x"}}),
        json!({"steps": []}),
    ] {
        let err = e
            .core
            .propose_plan("ref", plan(bad.clone()), "r2")
            .unwrap_err();
        assert!(!err.to_string().is_empty(), "{bad}");
    }
    // None of the refused calls spent its request id.
    assert!(!e.core.propose_plan("ref", add(), "r2").unwrap().duplicate);
    go.store(true, AtomicOrdering::SeqCst);
    release_all(&w);
}

// ── core#630 round 3 ─────────────────────────────────────────────────────────────────────────────

/// (round 3, HIGH) A human edit never carries a held floor raise past the approval matrix: in auto
/// mode, a human edit and a diff re-score into 70-100 at the SAME boundary still pause
/// `plan_approval` (into high risk) before the next unit. The edit (a step the raise's floor does
/// not add) is applied first, as its own rev; the raise is judged by the matrix against it.
#[test]
fn t8_r3_a_human_edit_does_not_carry_a_held_floor_raise_past_the_matrix() {
    let (w, go) = gated_worker_with(pa_output("built it"), Some(vec!["src/lib.rs".into()]));
    let mut e = engine("t8r3fl", w.clone());
    launch(&e, "rfl", HumanConfirm::None, docs_plan());
    wait_for("the creator to dispatch", || e.worker.calls().len() == 1);
    e.core
        .propose_plan(
            "rfl",
            plan(json!({"steps": [{"catalog": "understand"}]})),
            "req-f",
        )
        .unwrap();
    go.store(true, AtomicOrdering::SeqCst);
    e.wait_awaiting("rfl", crate::plan_gate::GATE_KIND, 1);
    assert_eq!(
        e.worker.calls().len(),
        1,
        "nothing dispatched past the raise"
    );
    let gate = wait_plan_gate(&e, "rfl", 1);
    assert_eq!(gate["reason"], "into_high_risk");
    let v = view(&e, "rfl");
    let tp = v.session.team_plan.as_ref().unwrap();
    assert!(
        !tp.approved_high_risk,
        "no human approved the high-risk raise"
    );
    let pending = tp.pending.as_ref().expect("the raise is held");
    assert!(pending.high_risk);
    // The human's step is in the held plan, and so is the raise's floor.
    let cats: Vec<&str> = pending
        .steps
        .steps
        .iter()
        .map(|s| s.catalog.as_str())
        .collect();
    assert!(
        cats.contains(&"understand") && cats.contains(&"security_review"),
        "{cats:?}"
    );
    release_all(&w);
}

/// (round 3, HIGH) Same for a held PA revision that needs approval: in manual mode the PA's
/// `PLAN+` and a human edit at the same boundary still pause `plan_approval` (manual mode) — the
/// human's own edit does not approve the PA's step.
#[test]
fn t8_r3_a_human_edit_does_not_carry_a_held_pa_revision_past_the_matrix() {
    let (w, go) = gated_worker_with(pa_output(r#"PLAN+ {"steps":[{"catalog":"design"}]}"#), None);
    let mut e = engine("t8r3pa", w.clone());
    launch(&e, "rpa", HumanConfirm::Before(99), docs_plan());
    e.wait_awaiting("rpa", crate::plan_gate::GATE_KIND, 1);
    e.core.confirm_gate("rpa", approve()).unwrap();
    wait_for("the creator to dispatch", || e.worker.calls().len() == 1);
    e.core
        .propose_plan(
            "rpa",
            plan(json!({"steps": [{"catalog": "test_plan"}]})),
            "req-p",
        )
        .unwrap();
    go.store(true, AtomicOrdering::SeqCst);
    e.wait_awaiting("rpa", crate::plan_gate::GATE_KIND, 2);
    assert_eq!(
        e.worker.calls().len(),
        1,
        "nothing dispatched past the PA's revision"
    );
    let gate = wait_plan_gate(&e, "rpa", 2);
    assert_eq!(gate["reason"], "manual_mode");
    let v = view(&e, "rpa");
    let tp = v.session.team_plan.as_ref().unwrap();
    // The human's edit is the accepted rev; the PA's is the held one.
    let accepted: Vec<&str> = tp
        .accepted
        .as_ref()
        .unwrap()
        .steps
        .steps
        .iter()
        .map(|s| s.catalog.as_str())
        .collect();
    assert!(
        accepted.contains(&"test_plan") && !accepted.contains(&"design"),
        "{accepted:?}"
    );
    let held: Vec<&str> = tp
        .pending
        .as_ref()
        .unwrap()
        .steps
        .steps
        .iter()
        .map(|s| s.catalog.as_str())
        .collect();
    assert!(held.contains(&"design"), "{held:?}");
    release_all(&w);
}

/// The run's `n`-th `gate.opened{plan_approval}` payload, once it is on the bus (fire-and-forget).
fn wait_plan_gate(e: &Engine, run: &str, n: usize) -> Value {
    let gates = |e: &Engine| -> Vec<Value> {
        payloads(e, run, tev::GATE_OPENED)
            .into_iter()
            .filter(|g| g["kind"] == "plan_approval")
            .collect()
    };
    wait_for("the plan_approval gate.opened", || gates(e).len() >= n);
    gates(e)[n - 1].clone()
}

/// (round 3, MEDIUM) An edit that passes the synchronous checks but cannot be PLANNED at the
/// boundary (a Tool step whose binary does not exist) is never silently spent: the engine
/// publishes its `plan.proposed` and a `plan.refused` for its proposal id, and the run goes on.
#[test]
fn t8_r3_an_edit_that_cannot_be_planned_is_refused_on_the_bus() {
    let (w, go) = gated_worker();
    let e = engine("t8r3pl", w.clone());
    launch(&e, "rpl", HumanConfirm::None, docs_plan());
    wait_for("the creator to dispatch", || e.worker.calls().len() == 1);
    let p = e
        .core
        .propose_plan(
            "rpl",
            plan(json!({"steps": [{"catalog": "run", "id": "run",
                "executor": {"type": "tool", "cmd": ["wicked-r3-no-such-binary"]}}]})),
            "req-x",
        )
        .unwrap();
    go.store(true, AtomicOrdering::SeqCst);
    wait_for("the run to go on", || e.worker.calls().len() >= 2);
    let refused = settled(&e, "rpl", tev::PLAN_REFUSED, 1);
    assert_eq!(refused[0]["proposal_id"], p.proposal_id.as_str());
    wait_for("the edit's plan.proposed", || {
        human_edits(&e, "rpl").len() == 1
    });
    let tp = view(&e, "rpl").session.team_plan.unwrap();
    assert_eq!((tp.rev, tp.accepted_rev), (1, 1));
    assert!(tp.edits.is_empty());
    release_all(&w);
}

/// A throwaway git repo (README + `src/lib.rs`, one commit) and its HEAD commit.
fn git_repo(dir: &std::path::Path) -> (std::path::PathBuf, String) {
    let repo = dir.join("repo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    let git = |args: &[&str]| {
        use wicked_apps_core::spawn::HardenedCommand;
        let out = std::process::Command::new("git")
            .hardened()
            .arg("-C")
            .arg(&repo)
            .args(args)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "t@t"]);
    git(&["config", "user.name", "t"]);
    git(&["config", "commit.gpgsign", "false"]);
    std::fs::write(repo.join("README.md"), "hello").unwrap();
    std::fs::write(repo.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "init"]);
    let head = git(&["rev-parse", "HEAD"]);
    (repo, head)
}

/// (round 3, MEDIUM) The preview scores against the repo the launch would run on: on the same
/// indexed repo, `preview_plan(repoRef)` and the launch give the same score, band and floor-filled
/// steps — and the score is the graph's (not the fail-closed 100), with `graph: "ready"`.
#[test]
fn t8_r3_the_preview_scores_against_the_launch_repo_graph() {
    use wicked_apps_core::{GraphWrite, Language, Location, Node, NodeKind, Span, Symbol};
    let _env = crate::code_graph::REPO_GRAPH_ROOT_ENV_LOCK
        .read()
        .unwrap_or_else(|p| p.into_inner());
    let w = Worker::new(&pa_output("built it"), None);
    let e = engine("t8r3repo", w.clone());
    let (repo, head) = git_repo(&e.rig.dir);
    let entry = e
        .core
        .register_repo(crate::RepoSpec {
            name: "r3repo".into(),
            root_path: repo.to_string_lossy().into_owned(),
            registered_at: 0,
        })
        .unwrap();
    // The repo's code graph, where the engine reads it, indexed at HEAD: `f` in src/lib.rs.
    let db = e.rig.dir.join("core.db");
    let root = crate::code_graph::repo_graph_root_for_store(db.to_str().unwrap()).unwrap();
    let graph_db =
        crate::code_graph::repo_graph_db_at(&root, std::path::Path::new(&entry.root_path));
    std::fs::create_dir_all(graph_db.parent().unwrap()).unwrap();
    {
        let mut g = wicked_apps_core::open_store(Some(graph_db.to_str().unwrap())).unwrap();
        let f = Node::new(
            Symbol::global(
                "test",
                None,
                vec![wicked_apps_core::Descriptor::method("f", None)],
            )
            .id(),
            NodeKind::Function,
            "f",
            Language::new("rust"),
            Location::new(
                "src/lib.rs",
                Span {
                    start_byte: 0,
                    end_byte: 0,
                    start_line: 1,
                    start_col: 0,
                    end_line: 1,
                    end_col: 0,
                },
            ),
        );
        g.begin_batch().unwrap();
        g.upsert_nodes(&[f]).unwrap();
        g.commit_batch().unwrap();
        g.set_repo_info(&wicked_estate_core::RepoInfo {
            commit: Some(head.clone()),
            ..Default::default()
        })
        .unwrap();
    }
    let p = plan(
        json!({"steps": [{"catalog": "produce"}, {"catalog": "critique"}],
                        "touch": ["src/lib.rs"]}),
    );
    let preview = e
        .core
        .preview_plan(p.clone(), None, Some(&entry.id), None, HumanConfirm::None)
        .unwrap();
    assert_eq!(preview.graph, "ready", "{:?}", preview.reasons);
    assert!(
        preview.score < 100,
        "the graph's score, not the fail-closed one: {}",
        preview.score
    );
    e.core
        .launch_run(LaunchSpec {
            base_ref: None,
            project_id: None,
            problem: "r3 repo".into(),
            clis: vec![cli("a"), cli("b")],
            entity_mode: crate::EntityMode::Shared,
            session_id: "rrepo".into(),
            human_confirm: HumanConfirm::None,
            auto_deliver: false,
            repo_ref: Some(entry.id.clone()),
            workflow: None,
            extra_write_roots: Vec::new(),
            extra_read_roots: Vec::new(),
            project_graph: None,
            plan: Some(p),
            deliver_step: None,
        })
        .unwrap();
    let scored = settled(&e, "rrepo", tev::PATH_SCORED, 1);
    assert_eq!(scored[0]["score"], preview.score);
    let accepted = settled(&e, "rrepo", tev::PLAN_ACCEPTED, 1);
    assert_eq!(accepted[0]["band"], preview.band.as_str());
    let launched: Vec<String> = step_ids(&accepted[0]["steps"]);
    let previewed: Vec<String> = preview.steps.iter().map(|s| s.id.clone()).collect();
    assert_eq!(launched, previewed);
    release_all(&w);
}

// ── core#630 round 4 ─────────────────────────────────────────────────────────────────────────────

/// (round 4) A held edit is never lost to the run ending before its boundary: cancelling the run
/// while the edit waits publishes the edit's `plan.proposed` and a `plan.refused` for it.
#[test]
fn t8_r4_a_held_edit_is_refused_on_the_bus_when_the_run_ends() {
    let (w, go) = gated_worker();
    let e = engine("t8r4end", w.clone());
    launch(&e, "rend", HumanConfirm::None, docs_plan());
    wait_for("the creator to dispatch", || e.worker.calls().len() == 1);
    settled(&e, "rend", tev::PLAN_ACCEPTED, 1);
    let p = e
        .core
        .propose_plan(
            "rend",
            plan(json!({"steps": [{"catalog": "test_plan"}]})),
            "req-end",
        )
        .unwrap();
    e.core.cancel_run("rend").unwrap();
    let refused = settled(&e, "rend", tev::PLAN_REFUSED, 1);
    assert_eq!(refused[0]["proposal_id"], p.proposal_id.as_str());
    wait_for("the edit's plan.proposed", || {
        human_edits(&e, "rend").len() == 1
    });
    // The id stays spent: a retry is a duplicate, and publishes nothing more.
    assert!(
        e.core
            .propose_plan(
                "rend",
                plan(json!({"steps": [{"catalog": "test_plan"}]})),
                "req-end"
            )
            .unwrap()
            .duplicate
    );
    go.store(true, AtomicOrdering::SeqCst);
    release_all(&w);
}

/// (round 4) The preview never fetches: on a repo whose `origin` is unreachable it answers
/// promptly and leaves `refs/remotes` and `FETCH_HEAD` untouched (the base is the local tip).
#[test]
fn t8_r4_the_preview_does_not_fetch_the_remote() {
    let w = Worker::new(&pa_output("built it"), None);
    let e = engine("t8r4fetch", w.clone());
    let (repo, _head) = git_repo(&e.rig.dir);
    {
        use wicked_apps_core::spawn::HardenedCommand;
        let ok = std::process::Command::new("git")
            .hardened()
            .arg("-C")
            .arg(&repo)
            .args([
                "remote",
                "add",
                "origin",
                "https://192.0.2.1/unreachable.git",
            ])
            .status()
            .unwrap()
            .success();
        assert!(ok);
    }
    let entry = e
        .core
        .register_repo(crate::RepoSpec {
            name: "r4fetch".into(),
            root_path: repo.to_string_lossy().into_owned(),
            registered_at: 0,
        })
        .unwrap();
    let started = Instant::now();
    let preview = e
        .core
        .preview_plan(
            plan(json!({"steps": [{"catalog": "produce"}], "touch": ["src/lib.rs"]})),
            None,
            Some(&entry.id),
            None,
            HumanConfirm::All,
        )
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "the preview waited on the network: {:?}",
        started.elapsed()
    );
    assert_eq!(
        preview.graph, "unavailable",
        "no graph is indexed for this repo"
    );
    let git_dir = repo.join(".git");
    assert!(!git_dir.join("FETCH_HEAD").exists(), "the preview fetched");
    assert!(
        !git_dir.join("refs/remotes").join("origin").exists(),
        "refs/remotes were touched"
    );
    release_all(&w);
}
