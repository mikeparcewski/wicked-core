//! Seam T3 (DES-TEAMING-002 §8.4-§8.6, §14 T3): the plan approval gate, driven through the real
//! engine (`Core` actor → launch → score → floor fill → compose → approval matrix → pause →
//! `confirm_gate`), plus T2 acceptance (g) end to end.
//!
//! The runner RECORDS every dispatch and then holds it, so "nothing dispatched before approval"
//! and "approve dispatches the cursor unit exactly once" are read off the runner itself, not only
//! off the event stream. The team facts are read off `CoreEvent::TeamFact` — the one hand-off the
//! engine uses until P1's `TeamBus::publish` lands — and replayed into a real `BusDb` where the
//! acceptance talks about bus rows, so key distinctness is proven through the bus's own dedup.
//!
//! Expected values are fixed literals from DES-TEAMING-002 §8.5's table, never re-derived from the
//! code under test. Waits use generous deadlines that return as soon as the condition holds.

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use wicked_council::types::{Category, Confidence, Dispatcher, InputMode, Vote};
use wicked_council::{AgenticCli, CouncilTask};

use wicked_core::{
    BusDb, BusEmit, Core, CoreEvent, EntityMode, HumanConfirm, HumanDecision, LaunchSpec,
    PlanSteps, PresetSpec, SessionStatus, StepInput, StepOutput, StepRunner, StepStatus,
};

/// Generous: a loaded CI host (and Windows) must never flake on a slow actor.
const DEADLINE: Duration = Duration::from_secs(90);

struct StubDispatcher;
impl Dispatcher for StubDispatcher {
    fn dispatch(&self, cli: &AgenticCli, _t: &CouncilTask) -> Option<Vote> {
        Some(Vote {
            cli: cli.key.clone(),
            recommendation: "x".into(),
            top_risk: "none".into(),
            change_my_mind: "no".into(),
            disqualifier: None,
            confidence: Confidence::default(),
            provenance: "stub".into(),
        })
    }
}

type Calls = Arc<Mutex<Vec<(String, u32, u32, String)>>>;

/// Records `(run, unit_ix, attempt, unit id)` for every dispatch, then holds the unit until the
/// test ends, so exactly the dispatches the engine made are observable and nothing runs on.
struct RecordAndHold {
    calls: Calls,
    gate: Arc<(Mutex<bool>, Condvar)>,
}
impl StepRunner for RecordAndHold {
    fn run_unit(&self, input: &StepInput) -> StepOutput {
        self.calls.lock().unwrap().push((
            input.run_id.clone(),
            input.unit_ix as u32,
            input.attempt,
            input.unit.id.clone(),
        ));
        let (lock, cv) = &*self.gate;
        let mut done = lock.lock().unwrap();
        while !*done {
            let (g, _) = cv.wait_timeout(done, Duration::from_millis(50)).unwrap();
            done = g;
        }
        StepOutput {
            run_id: input.run_id.clone(),
            unit_ix: input.unit_ix,
            attempt: input.attempt,
            output: "held".into(),
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

fn tmp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("wicked-core-t3-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

struct Rig {
    core: Core,
    calls: Calls,
    gate: Arc<(Mutex<bool>, Condvar)>,
    tap: Tap,
}
impl Drop for Rig {
    fn drop(&mut self) {
        let (lock, cv) = &*self.gate;
        *lock.lock().unwrap() = true;
        cv.notify_all();
    }
}

fn spawn(db: &str) -> Rig {
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let calls: Calls = Arc::default();
    let core = Core::spawn_with_engine(
        db.to_string(),
        Arc::new(StubDispatcher),
        Arc::new(RecordAndHold {
            calls: calls.clone(),
            gate: gate.clone(),
        }),
    );
    let tap = Tap {
        rx: core.subscribe(),
        seen: Vec::new(),
    };
    Rig {
        core,
        calls,
        gate,
        tap,
    }
}

/// A restart: drop the rig (its actor exits once the last handle drops), then a fresh one over the
/// same store.
fn restart(rig: Rig, db: &str) -> Rig {
    drop(rig);
    std::thread::sleep(Duration::from_millis(300));
    spawn(db)
}

fn plan(v: Value) -> PlanSteps {
    serde_json::from_value(v).expect("a plan")
}

fn spec(run: &str, human_confirm: HumanConfirm, plan: Option<PlanSteps>) -> LaunchSpec {
    LaunchSpec {
        problem: "add SSO login".into(),
        clis: vec![cli("a"), cli("b")],
        entity_mode: EntityMode::Shared,
        session_id: run.into(),
        human_confirm,
        auto_deliver: false,
        repo_ref: None,
        base_ref: None,
        workflow: None,
        project_id: None,
        extra_write_roots: Vec::new(),
        extra_read_roots: Vec::new(),
        project_graph: None,
        plan,
        deliver_step: None,
    }
}

/// Everything the engine emitted, drained as it arrives.
struct Tap {
    rx: std::sync::mpsc::Receiver<CoreEvent>,
    seen: Vec<CoreEvent>,
}
impl Tap {
    fn drain(&mut self) {
        while let Ok(ev) = self.rx.try_recv() {
            self.seen.push(ev);
        }
    }
    /// Wait (generous deadline, early return) until `cond` holds over everything seen.
    fn until(&mut self, what: &str, cond: impl Fn(&[CoreEvent]) -> bool) {
        let end = Instant::now() + DEADLINE;
        loop {
            self.drain();
            if cond(&self.seen) {
                return;
            }
            if Instant::now() > end {
                panic!(
                    "timed out waiting for {what}; saw {:#?}",
                    self.seen
                        .iter()
                        .map(CoreEvent::to_json)
                        .map(|j| j["type"].clone())
                        .collect::<Vec<_>>()
                );
            }
            if let Ok(ev) = self.rx.recv_timeout(Duration::from_millis(50)) {
                self.seen.push(ev);
            }
        }
    }
    /// Let anything still in flight arrive (used before asserting an absence).
    fn settle(&mut self) {
        std::thread::sleep(Duration::from_millis(500));
        self.drain();
    }
}

/// One published team fact: `(event_type, key, payload)`.
fn facts(seen: &[CoreEvent], run: &str) -> Vec<(String, String, Value)> {
    seen.iter()
        .filter_map(|e| match e {
            CoreEvent::TeamFact {
                session,
                event_type,
                key,
                payload,
            } if session == run => Some((event_type.clone(), key.clone(), payload.clone())),
            _ => None,
        })
        .collect()
}

fn of_type(seen: &[CoreEvent], run: &str, ty: &str) -> Vec<Value> {
    facts(seen, run)
        .into_iter()
        .filter(|(t, _, _)| t == ty)
        .map(|(_, _, p)| p)
        .collect()
}

fn fact_types(seen: &[CoreEvent], run: &str) -> Vec<String> {
    facts(seen, run).into_iter().map(|(t, _, _)| t).collect()
}

const PROPOSED: &str = "wicked.team.plan.proposed";
const SCORED: &str = "wicked.team.path.scored";
const ACCEPTED: &str = "wicked.team.plan.accepted";
const REFUSED: &str = "wicked.team.plan.refused";
const OPENED: &str = "wicked.team.gate.opened";
const DECIDED: &str = "wicked.team.gate.decided";

fn paused_on_plan(seen: &[CoreEvent], run: &str) -> Option<(u32, Option<u32>, String)> {
    seen.iter().rev().find_map(|e| match e {
        CoreEvent::AwaitingHuman {
            session,
            ord,
            reviewing_ord,
            prompt,
            gate_kind,
        } if session == run && gate_kind == "plan_approval" => {
            Some((*ord, *reviewing_ord, prompt.clone()))
        }
        _ => None,
    })
}

fn plan_pauses(seen: &[CoreEvent], run: &str) -> usize {
    seen.iter()
        .filter(|e| {
            matches!(e, CoreEvent::AwaitingHuman { session, gate_kind, .. }
                if session == run && gate_kind == "plan_approval")
        })
        .count()
}

fn dispatched_ords(seen: &[CoreEvent], run: &str) -> Vec<(u32, u32)> {
    seen.iter()
        .filter_map(|e| match e {
            CoreEvent::UnitDispatched {
                session,
                ord,
                attempt,
                ..
            } if session == run => Some((*ord, *attempt)),
            _ => None,
        })
        .collect()
}

fn unit_ids(core: &Core, run: &str) -> Vec<String> {
    let views = core.sessions_detail().unwrap();
    let v = views.iter().find(|v| v.session.id == run).expect("run");
    v.units
        .iter()
        .map(|u| {
            u.id.strip_prefix(&format!("{run}:"))
                .unwrap_or(&u.id)
                .to_string()
        })
        .collect()
}

fn session(core: &Core, run: &str) -> wicked_core::AgentSession {
    core.sessions_detail()
        .unwrap()
        .into_iter()
        .find(|v| v.session.id == run)
        .expect("run")
        .session
}

fn catalogs(steps: &Value) -> Vec<String> {
    steps
        .as_array()
        .expect("steps")
        .iter()
        .map(|s| s["catalog"].as_str().unwrap().to_string())
        .collect()
}

/// The 70-100 floor for a code plan that does not deliver (§8.5: `deliver` only for a run that
/// delivers), filled around a lone `build`.
const FLOOR_70_AROUND_BUILD: [&str; 6] = [
    "test_plan",
    "design",
    "architecture",
    "build",
    "review",
    "security_review",
];

fn approve() -> HumanDecision {
    HumanDecision::Approve {
        amend: None,
        amend_scope: Default::default(),
    }
}

// ── T2 (g) end to end ────────────────────────────────────────────────────────────────────────────

/// T2 (g) + T3 (c): an auto-mode `{plan:{steps:[{catalog:"build"}]}}` with `touch` omitted, and
/// again with `touch: []`, scores 100 with reason "no declared scope", gets the 70-100 floor, and
/// pauses `plan_approval` (high risk) BEFORE `build` dispatches. The step's `id` is omitted too,
/// exactly as the acceptance spells the payload.
#[test]
fn t2_g_c_a_build_plan_with_no_declared_scope_pauses_before_build() {
    for (i, p) in [
        json!({"steps": [{"catalog": "build"}]}),
        json!({"steps": [{"catalog": "build"}], "touch": []}),
    ]
    .into_iter()
    .enumerate()
    {
        let dir = tmp_dir(&format!("g{i}"));
        let db = dir.join("estate.db").to_str().unwrap().to_string();
        let mut rig = spawn(&db);
        let run = format!("rg{i}");
        rig.core
            .launch_run(spec(&run, HumanConfirm::None, Some(plan(p))))
            .unwrap();
        rig.tap.until("the plan_approval pause", |s| {
            paused_on_plan(s, &run).is_some()
        });
        rig.tap.settle();
        let seen = &rig.tap.seen;

        let scored = of_type(seen, &run, SCORED);
        assert_eq!(scored.len(), 1, "one intent score");
        assert_eq!(scored[0]["basis"], "intent");
        assert_eq!(scored[0]["score"], 100);
        assert_eq!(scored[0]["reasons"], json!(["no declared scope"]));

        let proposed = of_type(seen, &run, PROPOSED);
        assert_eq!(proposed.len(), 1);
        assert_eq!(proposed[0]["by"], "human");
        assert_eq!(proposed[0]["kind"], "initial");
        assert_eq!(catalogs(&proposed[0]["steps"]), ["build"]);

        let opened = of_type(seen, &run, OPENED);
        assert_eq!(opened.len(), 1);
        let g = &opened[0];
        assert_eq!(g["kind"], "plan_approval");
        assert_eq!(g["band"], "70-100");
        assert_eq!(g["high_risk"], true);
        assert_eq!(g["mode"], "auto");
        assert_eq!(g["reason"], "high_risk");
        assert_eq!(g["plan_rev"], 1);
        assert_eq!(g["gate_id"], format!("g-{run}-1"));
        assert_eq!(
            g["diff"]["added"],
            json!([
                "test_plan",
                "design",
                "architecture",
                "review",
                "security_review"
            ])
        );
        assert!(
            of_type(seen, &run, ACCEPTED).is_empty(),
            "nothing accepted yet"
        );

        // The facts, in order: proposed → scored → gate opened.
        assert_eq!(fact_types(seen, &run), [PROPOSED, SCORED, OPENED]);

        // Paused BEFORE the first unit: nothing dispatched, the cursor has not moved.
        assert_eq!(unit_ids(&rig.core, &run), FLOOR_70_AROUND_BUILD);
        let (ord, reviewing, _) = paused_on_plan(seen, &run).unwrap();
        assert_eq!((ord, reviewing), (1, None));
        assert!(dispatched_ords(seen, &run).is_empty(), "no unit dispatched");
        assert!(
            rig.calls.lock().unwrap().is_empty(),
            "the runner ran nothing"
        );
        let s = session(&rig.core, &run);
        assert_eq!(s.status, SessionStatus::AwaitingHuman);
        assert_eq!((s.unit_ix, s.gate_seq), (0, 1));
        // A launched plan is a team run: its units come from `<run>:plan-1`.
        let views = rig.core.sessions_detail().unwrap();
        let v = views.iter().find(|v| v.session.id == run).unwrap();
        assert!(
            v.units.iter().all(|u| u.team_run),
            "every unit is a team-run unit"
        );
    }
}

/// T2 (g): the same launch with `touch: ["src/x.rs"]` takes the GRAPH path, not the no-scope rule.
/// This rig has no repo, so the graph is unusable and S4 fails closed with the graph's reason
/// (never "no declared scope"); the graph-backed score itself is pinned in the plan_gate unit
/// tests against an indexed store.
#[test]
fn t2_g_a_declared_touch_set_scores_from_the_graph_path() {
    let dir = tmp_dir("gt");
    let db = dir.join("estate.db").to_str().unwrap().to_string();
    let mut rig = spawn(&db);
    let p = plan(json!({"steps": [{"catalog": "build"}], "touch": ["src/x.rs"]}));
    rig.core
        .launch_run(spec("rgt", HumanConfirm::None, Some(p)))
        .unwrap();
    rig.tap.until("the plan_approval pause", |s| {
        paused_on_plan(s, "rgt").is_some()
    });
    let scored = of_type(&rig.tap.seen, "rgt", SCORED);
    assert_eq!(scored[0]["score"], 100);
    let reasons: Vec<String> = serde_json::from_value(scored[0]["reasons"].clone()).unwrap();
    assert!(
        !reasons.iter().any(|r| r == "no declared scope"),
        "{reasons:?}"
    );
    assert!(
        reasons[0].starts_with("fail closed at 100: "),
        "the graph path fails closed with its reason: {reasons:?}"
    );
    assert_eq!(scored[0]["signals"], Value::Null);
}

/// T2 (g) + T3 (b): an understand-only plan with `touch` omitted scores 0, has an empty floor, is
/// not high risk, and in auto mode is released by the engine without a pause.
#[test]
fn t2_g_b_an_understand_only_plan_scores_0_and_proceeds_in_auto_mode() {
    let dir = tmp_dir("gu");
    let db = dir.join("estate.db").to_str().unwrap().to_string();
    let mut rig = spawn(&db);
    let p = plan(json!({"steps": [{"catalog": "understand"}]}));
    rig.core
        .launch_run(spec("rgu", HumanConfirm::None, Some(p)))
        .unwrap();
    rig.tap.until("the first dispatch", |s| {
        !dispatched_ords(s, "rgu").is_empty()
    });
    rig.tap.settle();
    let seen = &rig.tap.seen;
    let scored = of_type(seen, "rgu", SCORED);
    assert_eq!(scored[0]["score"], 0);
    let accepted = of_type(seen, "rgu", ACCEPTED);
    assert_eq!(accepted.len(), 1);
    let a = &accepted[0];
    assert_eq!(a["by"], "engine");
    assert_eq!(a["plan_rev"], 1);
    assert_eq!(a["mode"], "auto");
    assert_eq!(a["high_risk"], false);
    assert_eq!(a["band"], "0-19");
    assert_eq!(a["workflow_id"], "rgu:plan-1");
    assert_eq!(catalogs(&a["steps"]), ["understand"]);
    assert_eq!(fact_types(seen, "rgu"), [PROPOSED, SCORED, ACCEPTED]);
    assert!(of_type(seen, "rgu", OPENED).is_empty());
    assert_eq!(plan_pauses(seen, "rgu"), 0, "no plan_approval pause");
    assert_eq!(dispatched_ords(seen, "rgu"), [(1, 0)]);
}

// ── T3 (a) manual mode ───────────────────────────────────────────────────────────────────────────

/// T3 (a): manual mode (`before:1` and `all`) pauses `plan_approval` before the first execution
/// unit, even for a plan that is not high risk (score 0), and again for a high-risk one.
#[test]
fn a_manual_mode_pauses_plan_approval_before_the_first_unit() {
    let cases = [
        (
            "rab",
            HumanConfirm::Before(1),
            json!({"steps": [{"catalog": "understand"}]}),
            false,
        ),
        (
            "raa",
            HumanConfirm::All,
            json!({"steps": [{"catalog": "understand"}]}),
            false,
        ),
        (
            "rbb",
            HumanConfirm::Before(1),
            json!({"steps": [{"catalog": "build"}]}),
            true,
        ),
    ];
    for (run, hc, p, high) in cases {
        let dir = tmp_dir(run);
        let db = dir.join("estate.db").to_str().unwrap().to_string();
        let mut rig = spawn(&db);
        rig.core.launch_run(spec(run, hc, Some(plan(p)))).unwrap();
        rig.tap.until("the plan_approval pause", |s| {
            paused_on_plan(s, run).is_some()
        });
        rig.tap.settle();
        let seen = &rig.tap.seen;
        let g = &of_type(seen, run, OPENED)[0];
        assert_eq!(g["mode"], "manual", "{run}");
        assert_eq!(g["reason"], "manual_mode", "{run}");
        assert_eq!(g["high_risk"], high, "{run}");
        assert_eq!(paused_on_plan(seen, run).unwrap().0, 1, "{run}");
        assert!(dispatched_ords(seen, run).is_empty(), "{run}");
        assert!(of_type(seen, run, ACCEPTED).is_empty(), "{run}");
    }
}

// ── T3 (d) approve ───────────────────────────────────────────────────────────────────────────────

/// T3 (d): approve publishes `gate.decided{human_approved}` then `plan.accepted{by:"human"}`,
/// dispatches exactly the `Pending` cursor unit once, and leaves `session.attempt` unchanged.
/// (The mid-run shape — the cursor's predecessor `Done` — is pinned by the actor unit test
/// `plan_gate_approve_dispatches_the_pending_cursor_once_after_a_done_predecessor`.)
#[test]
fn d_approve_dispatches_the_cursor_unit_once_and_keeps_the_attempt() {
    let dir = tmp_dir("d");
    let db = dir.join("estate.db").to_str().unwrap().to_string();
    let mut rig = spawn(&db);
    let p = plan(json!({"steps": [{"catalog": "build", "id": "build"}]}));
    rig.core
        .launch_run(spec("rd", HumanConfirm::None, Some(p)))
        .unwrap();
    rig.tap.until("the plan_approval pause", |s| {
        paused_on_plan(s, "rd").is_some()
    });
    let before = session(&rig.core, "rd").attempt;
    let status = rig.core.confirm_gate("rd", approve()).unwrap();
    assert_eq!(status, SessionStatus::Executing);
    rig.tap.until("the first dispatch", |s| {
        !dispatched_ords(s, "rd").is_empty()
    });
    rig.tap.settle();
    let seen = &rig.tap.seen;
    let decided = of_type(seen, "rd", DECIDED);
    assert_eq!(decided.len(), 1);
    assert_eq!(decided[0]["kind"], "plan_approval");
    assert_eq!(decided[0]["decision"], "human_approved");
    assert_eq!(decided[0]["gate_id"], "g-rd-1");
    assert_eq!(decided[0]["by"], "human");
    let accepted = of_type(seen, "rd", ACCEPTED);
    assert_eq!(accepted.len(), 1);
    assert_eq!(accepted[0]["by"], "human");
    assert_eq!(accepted[0]["plan_rev"], 1);
    assert_eq!(accepted[0]["high_risk"], true);
    assert_eq!(catalogs(&accepted[0]["steps"]), FLOOR_70_AROUND_BUILD);
    assert_eq!(
        fact_types(seen, "rd"),
        [PROPOSED, SCORED, OPENED, DECIDED, ACCEPTED]
    );
    assert_eq!(
        dispatched_ords(seen, "rd"),
        [(1, 0)],
        "the cursor unit, once"
    );
    let calls = rig.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(calls[0].3, "rd:test_plan");
    assert_eq!(
        session(&rig.core, "rd").attempt,
        before,
        "attempt unchanged"
    );
    // A second answer is refused: the gate is closed.
    assert!(rig.core.confirm_gate("rd", approve()).is_err());
}

// ── T3 (e) approve with edit ─────────────────────────────────────────────────────────────────────

/// T3 (e): approve-with-edit publishes `plan.proposed{by:"human", kind:"edit"}` then
/// `plan.accepted{plan_rev: n+1}`; the edit is below the floor, so the floor phases are ADDED,
/// not refused, and the run re-plans onto `<run>:plan-2` and dispatches once.
#[test]
fn e_an_edit_below_the_floor_is_accepted_as_rev_2_with_floor_phases_added() {
    let dir = tmp_dir("e");
    let db = dir.join("estate.db").to_str().unwrap().to_string();
    let mut rig = spawn(&db);
    let p = plan(json!({"steps": [{"catalog": "build", "id": "build"}]}));
    rig.core
        .launch_run(spec("re", HumanConfirm::None, Some(p)))
        .unwrap();
    rig.tap.until("the plan_approval pause", |s| {
        paused_on_plan(s, "re").is_some()
    });
    let edit = plan(json!({"steps": [
        {"catalog": "build", "id": "make"},
        {"catalog": "test", "id": "prove"}
    ]}));
    let status = rig
        .core
        .confirm_gate("re", HumanDecision::EditPlan { plan: edit })
        .unwrap();
    assert_eq!(status, SessionStatus::Executing);
    rig.tap.until("the first dispatch", |s| {
        !dispatched_ords(s, "re").is_empty()
    });
    rig.tap.settle();
    let seen = &rig.tap.seen;
    let proposed = of_type(seen, "re", PROPOSED);
    assert_eq!(proposed.len(), 2);
    assert_eq!(proposed[1]["by"], "human");
    assert_eq!(proposed[1]["kind"], "edit");
    assert_ne!(proposed[0]["proposal_id"], proposed[1]["proposal_id"]);
    let decided = of_type(seen, "re", DECIDED);
    assert_eq!(decided.len(), 1);
    assert_eq!(decided[0]["decision"], "human_amended");
    let accepted = of_type(seen, "re", ACCEPTED);
    assert_eq!(accepted.len(), 1);
    let a = &accepted[0];
    assert_eq!(a["plan_rev"], 2);
    assert_eq!(a["by"], "human");
    assert_eq!(a["proposal_id"], proposed[1]["proposal_id"]);
    assert_eq!(a["workflow_id"], "re:plan-2");
    assert_eq!(
        catalogs(&a["steps"]),
        [
            "test_plan",
            "design",
            "architecture",
            "build",
            "test",
            "review",
            "security_review"
        ]
    );
    let added: Vec<(String, String)> = a["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            (
                s["id"].as_str().unwrap().to_string(),
                s["added_by"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        added,
        [
            ("test_plan".into(), "floor".into()),
            ("design".into(), "floor".into()),
            ("architecture".into(), "floor".into()),
            ("make".into(), "plan".into()),
            ("prove".into(), "plan".into()),
            ("review".into(), "floor".into()),
            ("security_review".into(), "floor".into()),
        ]
    );
    // The order DES §8.6 gives: the edit is proposed, then accepted as rev n+1.
    let types = fact_types(seen, "re");
    let pos = |t: &str, nth: usize| {
        types
            .iter()
            .enumerate()
            .filter(|(_, x)| *x == t)
            .nth(nth)
            .map(|(i, _)| i)
            .unwrap()
    };
    assert!(pos(PROPOSED, 1) < pos(ACCEPTED, 0));
    assert_eq!(
        unit_ids(&rig.core, "re"),
        [
            "test_plan",
            "design",
            "architecture",
            "make",
            "prove",
            "review",
            "security_review"
        ]
    );
    assert_eq!(dispatched_ords(seen, "re"), [(1, 0)]);
    assert_eq!(rig.calls.lock().unwrap().len(), 1);
}

// ── T3 (f) reject ────────────────────────────────────────────────────────────────────────────────

/// T3 (f): reject cancels, after publishing `gate.decided{human_rejected}`; nothing dispatches.
#[test]
fn f_reject_cancels() {
    let dir = tmp_dir("f");
    let db = dir.join("estate.db").to_str().unwrap().to_string();
    let mut rig = spawn(&db);
    let p = plan(json!({"steps": [{"catalog": "build", "id": "build"}]}));
    rig.core
        .launch_run(spec("rf", HumanConfirm::None, Some(p)))
        .unwrap();
    rig.tap.until("the plan_approval pause", |s| {
        paused_on_plan(s, "rf").is_some()
    });
    let status = rig.core.confirm_gate("rf", HumanDecision::Reject).unwrap();
    assert_eq!(status, SessionStatus::Cancelled);
    rig.tap.settle();
    let decided = of_type(&rig.tap.seen, "rf", DECIDED);
    assert_eq!(decided.len(), 1);
    assert_eq!(decided[0]["decision"], "human_rejected");
    assert!(of_type(&rig.tap.seen, "rf", ACCEPTED).is_empty());
    assert!(dispatched_ords(&rig.tap.seen, "rf").is_empty());
    assert_eq!(session(&rig.core, "rf").status, SessionStatus::Cancelled);
}

// ── T3 (g) restart ───────────────────────────────────────────────────────────────────────────────

/// T3 (g): a restart while paused keeps the gate open (same `gate_id`, no second opening) and
/// resumable: approving after the restart dispatches the cursor unit once.
#[test]
fn g_a_restart_while_paused_keeps_the_gate_open_and_resumable() {
    let dir = tmp_dir("gr");
    let db = dir.join("estate.db").to_str().unwrap().to_string();
    let mut rig = spawn(&db);
    let p = plan(json!({"steps": [{"catalog": "build", "id": "build"}]}));
    rig.core
        .launch_run(spec("rr", HumanConfirm::None, Some(p)))
        .unwrap();
    rig.tap.until("the plan_approval pause", |s| {
        paused_on_plan(s, "rr").is_some()
    });
    let mut rig = restart(rig, &db);
    let s = session(&rig.core, "rr");
    assert_eq!(s.status, SessionStatus::AwaitingHuman);
    assert_eq!(s.gate_seq, 1);
    let pending = s
        .team_plan
        .as_ref()
        .and_then(|t| t.pending.as_ref())
        .expect("pending");
    assert_eq!(pending.gate_id.as_deref(), Some("g-rr-1"));
    let status = rig.core.confirm_gate("rr", approve()).unwrap();
    assert_eq!(status, SessionStatus::Executing);
    rig.tap.until("the first dispatch", |s| {
        !dispatched_ords(s, "rr").is_empty()
    });
    rig.tap.settle();
    let decided = of_type(&rig.tap.seen, "rr", DECIDED);
    assert_eq!(
        decided[0]["gate_id"], "g-rr-1",
        "the gate opened before the restart"
    );
    assert!(
        of_type(&rig.tap.seen, "rr", OPENED).is_empty(),
        "no second opening"
    );
    assert_eq!(dispatched_ords(&rig.tap.seen, "rr"), [(1, 0)]);
}

// ── T3 (i) re-open ───────────────────────────────────────────────────────────────────────────────

/// Replay facts into a real bus: `(event_type, key) -> event_id`, through `BusDb::emit`'s own
/// key dedup.
fn bus_ids(dir: &std::path::Path, facts: &[(String, String, Value)]) -> Vec<i64> {
    let bus = BusDb::open(dir.join("bus.db").to_str().unwrap()).unwrap();
    facts
        .iter()
        .map(|(t, k, p)| {
            bus.emit(&BusEmit::new(t.clone(), "wicked-core", "core.team", p.clone()).with_key(k))
                .unwrap()
        })
        .collect()
}

/// An edit the engine refuses in auto mode: it carries a floor override (§8.5, refused in auto
/// mode whatever the steps).
fn refused_edit() -> PlanSteps {
    plan(json!({
        "steps": [{"catalog": "build", "id": "build"}],
        "override": {"remove": ["review"], "reason": "trust me"}
    }))
}

fn run_reopen(restart_between: bool) {
    let name = if restart_between { "ir" } else { "i" };
    let run = if restart_between { "rir" } else { "ri" };
    let dir = tmp_dir(name);
    let db = dir.join("estate.db").to_str().unwrap().to_string();
    let mut rig = spawn(&db);
    let mut all: Vec<CoreEvent> = Vec::new();
    let p = plan(json!({"steps": [{"catalog": "build", "id": "build"}]}));
    rig.core
        .launch_run(spec(run, HumanConfirm::None, Some(p)))
        .unwrap();
    rig.tap.until("the first plan_approval pause", |s| {
        paused_on_plan(s, run).is_some()
    });

    // First edit: refused → plan.refused + a SECOND gate.opened with a different gate_id.
    let status = rig
        .core
        .confirm_gate(
            run,
            HumanDecision::EditPlan {
                plan: refused_edit(),
            },
        )
        .unwrap();
    assert_eq!(status, SessionStatus::AwaitingHuman);
    rig.tap
        .until("the re-opened gate", |s| of_type(s, run, OPENED).len() == 2);
    let (_, _, prompt) = paused_on_plan(&rig.tap.seen, run).unwrap();
    assert!(prompt.contains("override in auto mode"), "{prompt}");
    let refused = of_type(&rig.tap.seen, run, REFUSED);
    assert_eq!(refused.len(), 1);
    assert_eq!(refused[0]["reason"], "override in auto mode");
    let opened = of_type(&rig.tap.seen, run, OPENED);
    assert_eq!(opened[0]["gate_id"], format!("g-{run}-1"));
    assert_eq!(opened[1]["gate_id"], format!("g-{run}-2"));
    let first_decided = of_type(&rig.tap.seen, run, DECIDED);
    assert_eq!(first_decided.len(), 1);
    assert_eq!(first_decided[0]["gate_id"], format!("g-{run}-1"));
    assert_eq!(first_decided[0]["decision"], "human_amended");

    if restart_between {
        rig.tap.settle();
        all.append(&mut rig.tap.seen);
        rig = restart(rig, &db);
        let s = session(&rig.core, run);
        assert_eq!((s.status, s.gate_seq), (SessionStatus::AwaitingHuman, 2));
    }

    // Second edit on the re-opened gate: a NEW proposal (its id derives from the second gate_id).
    let status = rig
        .core
        .confirm_gate(
            run,
            HumanDecision::EditPlan {
                plan: plan(json!({"steps": [{"catalog": "build", "id": "build"}]})),
            },
        )
        .unwrap();
    assert_eq!(status, SessionStatus::Executing);
    rig.tap.until("the first dispatch", |s| {
        !dispatched_ords(s, run).is_empty()
    });
    rig.tap.settle();
    all.append(&mut rig.tap.seen);

    let proposed = of_type(&all, run, PROPOSED);
    assert_eq!(proposed.len(), 3, "initial + two edits");
    let (e1, e2) = (&proposed[1]["proposal_id"], &proposed[2]["proposal_id"]);
    assert_ne!(e1, e2, "the second edit is a distinct proposal");
    assert_eq!(proposed[1]["kind"], "edit");
    assert_eq!(proposed[2]["kind"], "edit");
    let decided = of_type(&all, run, DECIDED);
    assert_eq!(decided.len(), 2);
    assert_eq!(
        decided[1]["gate_id"],
        format!("g-{run}-2"),
        "decides the SECOND gate"
    );
    assert_eq!(decided[1]["re"], format!("gate.opened#g-{run}-2"));
    let accepted = of_type(&all, run, ACCEPTED);
    assert_eq!(accepted.len(), 1);
    assert_eq!(accepted[0]["proposal_id"], *e2);
    assert_eq!(dispatched_ords(&all, run), [(1, 0)], "dispatched once");

    // Both edit rows exist on the bus: distinct keys, so distinct event ids; neither resolves to
    // the other. Same for the two gate openings.
    let fs = facts(&all, run);
    let ids = bus_ids(&dir, &fs);
    let id_of = |ty: &str, nth: usize| {
        fs.iter()
            .zip(&ids)
            .filter(|((t, _, _), _)| t == ty)
            .nth(nth)
            .map(|(_, id)| *id)
            .unwrap()
    };
    assert_ne!(id_of(PROPOSED, 1), id_of(PROPOSED, 2));
    assert_ne!(id_of(OPENED, 0), id_of(OPENED, 1));
    let mut unique = ids.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), ids.len(), "every fact is its own bus row");
}

/// T3 (i): a refused edit re-opens the gate with a new `gate_id`; a second edit is a distinct
/// proposal; approving it decides the second gate and dispatches once.
#[test]
fn i_a_refused_edit_reopens_the_gate_with_a_new_gate_id() {
    run_reopen(false);
}

/// T3 (i), after a restart between the two openings.
#[test]
fn i_the_reopened_gate_survives_a_restart_between_the_openings() {
    run_reopen(true);
}

// ── T3 (h) override ──────────────────────────────────────────────────────────────────────────────

/// T3 (h): in manual mode a floor override is recorded on `plan.accepted.override` and shown in
/// the gate prompt; the same override in auto mode is refused (`plan.refused`, "override in auto
/// mode") and the launch fails with no run.
#[test]
fn h_a_manual_override_is_recorded_and_an_auto_override_is_refused() {
    let dir = tmp_dir("h");
    let db = dir.join("estate.db").to_str().unwrap().to_string();
    let mut rig = spawn(&db);
    let with_override = || {
        plan(json!({
            "steps": [{"catalog": "build", "id": "build"}],
            "override": {"remove": ["architecture"], "reason": "a one-line fix"}
        }))
    };
    rig.core
        .launch_run(spec("rhm", HumanConfirm::Before(1), Some(with_override())))
        .unwrap();
    rig.tap
        .until("the plan_approval pause and its gate.opened", |s| {
            paused_on_plan(s, "rhm").is_some() && !of_type(s, "rhm", OPENED).is_empty()
        });
    let (_, _, prompt) = paused_on_plan(&rig.tap.seen, "rhm").unwrap();
    assert!(prompt.contains("override"), "{prompt}");
    assert!(prompt.contains("architecture"), "{prompt}");
    assert!(prompt.contains("a one-line fix"), "{prompt}");
    let g = &of_type(&rig.tap.seen, "rhm", OPENED)[0];
    assert_eq!(g["reason"], "override");
    rig.core.confirm_gate("rhm", approve()).unwrap();
    rig.tap
        .until("plan.accepted", |s| !of_type(s, "rhm", ACCEPTED).is_empty());
    let a = &of_type(&rig.tap.seen, "rhm", ACCEPTED)[0];
    assert_eq!(
        a["override"],
        json!({"remove": ["architecture"], "reason": "a one-line fix"})
    );
    assert!(!catalogs(&a["steps"]).contains(&"architecture".to_string()));

    let err = rig
        .core
        .launch_run(spec("rha", HumanConfirm::None, Some(with_override())))
        .expect_err("an override in auto mode is refused");
    assert!(err.to_string().contains("override in auto mode"), "{err}");
    rig.tap.settle();
    let refused = of_type(&rig.tap.seen, "rha", REFUSED);
    assert_eq!(refused.len(), 1);
    assert_eq!(refused[0]["reason"], "override in auto mode");
    assert!(
        rig.core
            .sessions_detail()
            .unwrap()
            .iter()
            .all(|v| v.session.id != "rha"),
        "no run was persisted"
    );
}

// ── presets become team runs ─────────────────────────────────────────────────────────────────────

/// A run launched from a preset is a TEAM run once its plan registers as `<run>:plan-<rev>`: its
/// preset steps go through the same pipeline (proposed with `preset`, floor-filled, gated), its
/// units carry `team_run`, and the accepted plan names the per-run def.
#[test]
fn a_preset_launch_becomes_a_team_run() {
    let dir = tmp_dir("p");
    let db = dir.join("estate.db").to_str().unwrap().to_string();
    let mut rig = spawn(&db);
    rig.core
        .put_preset(PresetSpec {
            name: "my-flow".into(),
            project_id: None,
            steps: plan(json!({"steps": [
                {"catalog": "understand", "id": "scope"},
                {"catalog": "build", "id": "make", "depends_on": ["scope"]},
                {"catalog": "review", "id": "check", "depends_on": ["make"]}
            ]}))
            .steps,
            created_by: "api".into(),
        })
        .unwrap();
    let mut s = spec("rp", HumanConfirm::None, None);
    s.workflow = Some("my-flow".into());
    rig.core.launch_run(s).unwrap();
    rig.tap.until("the plan_approval pause", |s| {
        paused_on_plan(s, "rp").is_some()
    });
    let proposed = &of_type(&rig.tap.seen, "rp", PROPOSED)[0];
    assert_eq!(proposed["preset"], "my-flow");
    assert_eq!(proposed["by"], "human");
    let views = rig.core.sessions_detail().unwrap();
    let v = views.iter().find(|v| v.session.id == "rp").unwrap();
    assert!(
        v.units.iter().all(|u| u.team_run),
        "a preset run is a team run"
    );
    assert_eq!(
        unit_ids(&rig.core, "rp"),
        [
            "scope",
            "test_plan",
            "design",
            "architecture",
            "make",
            "check",
            "security_review"
        ]
    );
    rig.core.confirm_gate("rp", approve()).unwrap();
    rig.tap
        .until("plan.accepted", |s| !of_type(s, "rp", ACCEPTED).is_empty());
    let a = &of_type(&rig.tap.seen, "rp", ACCEPTED)[0];
    assert_eq!(a["workflow_id"], "rp:plan-1");
    assert_eq!(a["plan_rev"], 1);
}

/// A launch names a plan OR a preset, never both: refused synchronously, no run persisted.
#[test]
fn a_launch_with_both_a_plan_and_a_workflow_is_refused() {
    let dir = tmp_dir("both");
    let db = dir.join("estate.db").to_str().unwrap().to_string();
    let rig = spawn(&db);
    let mut s = spec(
        "rboth",
        HumanConfirm::None,
        Some(plan(json!({"steps": [{"catalog": "build"}]}))),
    );
    s.workflow = Some("feature".into());
    let err = rig.core.launch_run(s).expect_err("refused");
    assert!(err.to_string().contains("plan"), "{err}");
}

fn deliver_step() -> wicked_core::PlanStep {
    serde_json::from_value(json!({
        "catalog": "deliver", "id": "deliver", "instructions": "push and open the PR",
        "executor": {"type": "tool", "cmd": ["true"]}
    }))
    .unwrap()
}

/// A DELIVERING preset launch (crew's `deliver: "pr"`): the launcher's deliver step rides the
/// preset's plan — appended last, in the floor (§8.5: `deliver` for a run that delivers) — so the
/// run stays ONE team plan behind the plan_approval gate instead of a separately composed def.
#[test]
fn a_delivering_preset_launch_carries_its_deliver_step_behind_the_gate() {
    let dir = tmp_dir("dlv");
    let db = dir.join("estate.db").to_str().unwrap().to_string();
    let mut rig = spawn(&db);
    let mut s = spec("rdlv", HumanConfirm::None, None);
    s.workflow = Some("feature".into());
    s.deliver_step = Some(deliver_step());
    rig.core.launch_run(s).unwrap();
    rig.tap
        .until("the plan_approval pause and its gate.opened", |s| {
            paused_on_plan(s, "rdlv").is_some() && !of_type(s, "rdlv", OPENED).is_empty()
        });
    let ids = unit_ids(&rig.core, "rdlv");
    assert_eq!(ids.last().map(String::as_str), Some("deliver"), "{ids:?}");
    assert_eq!(ids.iter().filter(|i| *i == "deliver").count(), 1);
    assert_eq!(
        of_type(&rig.tap.seen, "rdlv", PROPOSED)[0]["preset"],
        "feature"
    );
    assert!(dispatched_ords(&rig.tap.seen, "rdlv").is_empty());

    // A deliver step with no plan or preset to ride is refused, never dropped …
    let mut bare = spec("rdlv2", HumanConfirm::None, None);
    bare.deliver_step = Some(deliver_step());
    let err = rig.core.launch_run(bare).expect_err("refused");
    assert!(err.to_string().contains("deliver step"), "{err}");
    // … and one that is not the catalog deliver entry with a command is refused.
    let mut wrong = spec(
        "rdlv3",
        HumanConfirm::None,
        Some(plan(json!({"steps": [{"catalog": "build"}]}))),
    );
    wrong.deliver_step = Some(
        serde_json::from_value(json!({"catalog": "run", "id": "deliver",
            "executor": {"type": "tool", "cmd": ["true"]}}))
        .unwrap(),
    );
    let err = rig.core.launch_run(wrong).expect_err("refused");
    assert!(err.to_string().contains("catalog `deliver`"), "{err}");
}

/// The straight-through `Core::launch` honours no gate, so it refuses a preset (and a plan)
/// instead of running one past its plan_approval gate.
#[test]
fn the_straight_through_launch_refuses_a_preset() {
    let dir = tmp_dir("legacy");
    let db = dir.join("estate.db").to_str().unwrap().to_string();
    let mut rig = spawn(&db);
    let mut s = spec("rleg", HumanConfirm::None, None);
    s.workflow = Some("feature".into());
    rig.core.launch(s);
    rig.tap.until("the refusal", |seen| {
        seen.iter().any(|e| {
            matches!(e, CoreEvent::Error { session: Some(id), message }
                if id == "rleg" && message.contains("launch_run"))
        })
    });
    assert!(dispatched_ords(&rig.tap.seen, "rleg").is_empty());
}

/// Arm the hermetic emit spool (core#311) before `main`.
///
/// SAFETY (`ctor(unsafe)`): runs before `main` on one thread and only sets one process env var
/// via the std API — no allocator setup, no threads, no panics across the FFI boundary.
#[ctor::ctor(unsafe)]
fn arm_hermetic_emit_spool() {
    wicked_apps_core::emit::hermetic_test_spool();
}
