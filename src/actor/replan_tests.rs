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

/// What the scripted worker does for the FIRST dispatch of a run: the PA's output, and the paths
/// the supervisor would report for its settled diff (`None` = no re-score).
struct Script {
    output: String,
    diff: Option<Vec<String>>,
}

/// Runs the first dispatch from its script and HOLDS every later one (recorded), so "what ran
/// after the revision" is read off the worker itself and nothing runs on past it.
struct Worker {
    tx: OnceLock<Sender<Command>>,
    script: Mutex<Script>,
    calls: Mutex<Vec<(u32, u32, String)>>,
    first: AtomicUsize,
    release: Arc<(Mutex<bool>, std::sync::Condvar)>,
}

impl Worker {
    fn new(output: &str, diff: Option<&[&str]>) -> Arc<Self> {
        Arc::new(Self {
            tx: OnceLock::new(),
            script: Mutex::new(Script {
                output: output.to_string(),
                diff: diff.map(|d| d.iter().map(|p| p.to_string()).collect()),
            }),
            calls: Mutex::default(),
            first: AtomicUsize::new(0),
            release: Arc::new((Mutex::new(false), std::sync::Condvar::new())),
        })
    }
    /// `(ord, attempt, unit id)` of every dispatch, in order.
    fn calls(&self) -> Vec<(u32, u32, String)> {
        self.calls.lock().unwrap().clone()
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let (l, cv) = &*self.release;
        *l.lock().unwrap() = true;
        cv.notify_all();
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
        let output = if self.first.fetch_add(1, AtomicOrdering::SeqCst) == 0 {
            let s = self.script.lock().unwrap();
            if let (Some(paths), Some(tx)) = (&s.diff, self.tx.get()) {
                // The supervisor's measurement of this attempt's settled diff.
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
            s.output.clone()
        } else {
            let (l, cv) = &*self.release;
            let mut done = l.lock().unwrap();
            while !*done {
                done = cv.wait_timeout(done, Duration::from_millis(50)).unwrap().0;
            }
            "held".to_string()
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

fn engine(name: &str, worker: Arc<Worker>) -> Engine {
    let rig = rig(name);
    let db = rig.dir.join("core.db").to_string_lossy().into_owned();
    let cfg = TeamConfig::new(Some(rig.bus.clone()), Some(rig.outbox.clone()))
        .with_schedule(vec![Duration::from_millis(40); 3])
        .with_attempt_wait(Duration::from_millis(30))
        // No supervisor in these rigs: the worker synthesizes its (empty) ledger after this.
        .with_final_pass_budget(Duration::from_millis(300))
        .with_gate_poll(Duration::from_millis(20));
    let core = Core::spawn_with_engine_team(db, Arc::new(StubDispatcher), worker.clone(), cfg);
    let _ = worker.tx.set(core.tx.clone());
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

fn plan(v: Value) -> PlanSteps {
    serde_json::from_value(v).expect("a plan")
}

fn launch(e: &Engine, run: &str, hc: HumanConfirm, p: PlanSteps) {
    e.core
        .launch_run(LaunchSpec {
            base_ref: None,
            project_id: None,
            problem: "t4 re-plan".into(),
            clis: vec![cli("a"), cli("b")],
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
        wait_for(&format!("{run}: {n} `{kind}` pause(s)"), || {
            self.awaiting(run).iter().filter(|(_, k)| k == kind).count() >= n
        });
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

/// (a) second clause: a later LOWER re-score publishes nothing — no `path.scored`, no revision.
#[test]
fn t4_a_a_lower_rescore_publishes_nothing() {
    // The PA's own addition keeps the run going past the first boundary without a gate.
    let w = Worker::new(&pa_output("docs only"), Some(&["docs/guide.md"]));
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
    wait_for("the second unit to dispatch", || {
        e.worker.calls().len() >= 2
    });
    std::thread::sleep(Duration::from_millis(300));
    assert!(payloads(&e, "low", tev::PATH_SCORED)
        .iter()
        .all(|p| p["basis"] == "intent"));
    assert!(payloads(&e, "low", tev::PLAN_REVISED).is_empty());
    assert!(e.awaiting("low").is_empty());
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
    let revised = payloads(&e, "man", tev::PLAN_REVISED);
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
    let revised = payloads(&e, "mem", tev::PLAN_REVISED);
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
