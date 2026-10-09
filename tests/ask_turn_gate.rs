//! DES-ASK-TEAM-CHAT-001 §3 (ASK-K2a) through the REAL engine over a REAL bus: the turn gate.
//!
//! An ask is a one-step plan — `understand` with its gate raised to `human_confirm{unconditional:
//! true}` — paused at its terminal gate after the PA answers. The operator's next message is two
//! commands: `Core::propose_plan` adding `answer-2` (held) and `Core::confirm_gate(Approve)`. The
//! Approve arm must apply the held edit BEFORE it chooses the cursor unit: `plan.accepted{rev:2}`,
//! `unitDispatched` for `answer-2`, no `sessionCompleted`. Red before K2a: the approve ran
//! `finalize_run` (no cursor unit) and the run completed with the edit unapplied.
//!
//! Control: with nothing held, approving the terminal gate finalizes exactly as before.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use wicked_council::types::{Category, Confidence, Dispatcher, InputMode, Vote};
use wicked_council::{AgenticCli, CouncilTask};

use wicked_core::{
    BusDb, Core, CoreEvent, EntityMode, HumanConfirm, HumanDecision, LaunchSpec, PlanSteps,
    SessionStatus, StepInput, StepOutput, StepRunner, StepStatus, TeamConfig, TEAM_OUTBOX_FILE,
};

const DEADLINE: Duration = Duration::from_secs(90);
const ACCEPTED: &str = "wicked.team.plan.accepted";
const PROPOSED: &str = "wicked.team.plan.proposed";

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

/// Answers every unit at once with a short reply (the PA's answer), recording each dispatch.
struct Answers {
    calls: Calls,
}
impl StepRunner for Answers {
    fn run_unit(&self, input: &StepInput) -> StepOutput {
        self.calls.lock().unwrap().push((
            input.run_id.clone(),
            input.unit_ix as u32,
            input.attempt,
            input.unit.id.clone(),
        ));
        StepOutput {
            run_id: input.run_id.clone(),
            unit_ix: input.unit_ix,
            attempt: input.attempt,
            output: format!(
                "The retire flow cancels its fetch. (answer for {})",
                input.unit.id
            ),
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
        headless_invocation: format!("run-{key} {{PROMPT}}"),
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
        logout_invocation: None,
        governance_class: None,
        credential: None,
        free_tier: None,
        health: None,
    }
}

fn tmp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("wicked-core-k2a-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

struct Rig {
    core: Core,
    calls: Calls,
    tap: Tap,
    bus: String,
}

fn spawn(db: &str) -> Rig {
    let calls: Calls = Arc::default();
    let dir = std::path::Path::new(db)
        .parent()
        .expect("db dir")
        .to_path_buf();
    let bus = dir.join("bus.db").to_string_lossy().into_owned();
    BusDb::shared(&bus).expect("bus db");
    let core = Core::spawn_with_engine_team(
        db.to_string(),
        Arc::new(StubDispatcher),
        Arc::new(Answers {
            calls: calls.clone(),
        }),
        // A finished unit folds without a supervisor on the bus after this budget.
        TeamConfig::new(Some(bus.clone()), Some(dir.join(TEAM_OUTBOX_FILE)))
            .with_final_pass_budget(Duration::from_millis(300)),
    );
    let tap = Tap {
        rx: core.subscribe(),
        seen: Vec::new(),
    };
    Rig {
        core,
        calls,
        tap,
        bus,
    }
}

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
    fn until(&mut self, what: &str, mut cond: impl FnMut(&[CoreEvent]) -> bool) {
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
    fn settle(&mut self) {
        std::thread::sleep(Duration::from_millis(800));
        self.drain();
    }
}

fn of_type(bus: &str, run: &str, ty: &str) -> Vec<Value> {
    BusDb::shared(bus)
        .expect("bus")
        .poll("*", 0, 100_000)
        .expect("poll")
        .into_iter()
        .filter(|e| e.event_type == ty && e.payload["run_id"] == run)
        .map(|e| e.payload)
        .collect()
}

fn plan(v: Value) -> PlanSteps {
    serde_json::from_value(v).expect("a plan")
}

/// The ask's answer step: `understand` with its gate raised (DES-ASK-TEAM-CHAT-001 §4.4).
fn answer(id: &str) -> Value {
    json!({"catalog": "understand", "id": id,
           "gate": {"human_confirm": {"unconditional": true}}})
}

fn spec(run: &str, steps: Vec<Value>) -> LaunchSpec {
    LaunchSpec {
        problem: "what does the retire flow do".into(),
        clis: vec![cli("a"), cli("b")],
        entity_mode: EntityMode::Shared,
        session_id: run.into(),
        human_confirm: HumanConfirm::None,
        auto_deliver: false,
        repo_ref: None,
        base_ref: None,
        workflow: None,
        project_id: None,
        extra_write_roots: Vec::new(),
        extra_read_roots: Vec::new(),
        project_graph: None,
        plan: Some(plan(json!({"steps": steps}))),
        deliver_step: None,
        exclude_seats: Vec::new(),
        evidence_root: None,
        // (ASK-K1a) The test reads the PA as `a`, so it chooses it.
        primary: Some("a".into()),
    }
}

fn approve() -> HumanDecision {
    HumanDecision::Approve {
        amend: None,
        amend_scope: Default::default(),
    }
}

fn status(core: &Core, run: &str) -> SessionStatus {
    core.sessions_detail()
        .unwrap()
        .into_iter()
        .find(|v| v.session.id == run)
        .expect("run")
        .session
        .status
}

fn awaiting(seen: &[CoreEvent], run: &str) -> usize {
    seen.iter()
        .filter(|e| matches!(e, CoreEvent::AwaitingHuman { session, .. } if session == run))
        .count()
}

fn completed(seen: &[CoreEvent], run: &str) -> bool {
    seen.iter()
        .any(|e| matches!(e, CoreEvent::SessionCompleted { session } if session == run))
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

/// §3: propose `answer-2` at the terminal turn gate, approve → the held edit is applied first:
/// `plan.accepted{rev:2}`, `answer-2` dispatched, the run pauses again at its gate; never
/// `sessionCompleted`.
#[test]
fn approve_at_the_turn_gate_applies_the_held_edit_before_choosing_the_cursor_unit() {
    let dir = tmp_dir("turn");
    let db = dir.join("estate.db").to_str().unwrap().to_string();
    let mut rig = spawn(&db);
    rig.core
        .launch_run(spec("rt", vec![answer("answer-1")]))
        .unwrap();
    rig.tap
        .until("the first turn gate", |seen| awaiting(seen, "rt") >= 1);
    assert_eq!(status(&rig.core, "rt"), SessionStatus::AwaitingHuman);
    assert_eq!(dispatched_ords(&rig.tap.seen, "rt"), vec![(1, 0)]);
    let bus = rig.bus.clone();
    assert_eq!(
        of_type(&bus, "rt", ACCEPTED).len(),
        1,
        "rev 1 accepted at launch"
    );

    // The operator's next message: a held edit, then the approve.
    let proposal = rig
        .core
        .propose_plan("rt", plan(json!({"steps": [answer("answer-2")]})), "turn-2")
        .unwrap();
    assert!(!proposal.duplicate);
    let st = rig.core.confirm_gate("rt", approve()).unwrap();
    assert_ne!(
        st,
        SessionStatus::Completed,
        "the approve must not finalize past the step the operator just added"
    );

    rig.tap.until("answer-2 dispatched", |seen| {
        dispatched_ords(seen, "rt").len() >= 2
    });
    assert_eq!(dispatched_ords(&rig.tap.seen, "rt"), vec![(1, 0), (2, 0)]);
    rig.tap
        .until("the second turn gate", |seen| awaiting(seen, "rt") >= 2);
    rig.tap.settle();
    assert!(
        !completed(&rig.tap.seen, "rt"),
        "the path stays open for the next turn"
    );
    assert_eq!(status(&rig.core, "rt"), SessionStatus::AwaitingHuman);
    // The facts ride the publisher asynchronously: wait for the rows, then read them.
    rig.tap.until("rev 2 accepted on the bus", |_| {
        of_type(&bus, "rt", ACCEPTED).len() >= 2
    });
    rig.tap.until("the edit proposed on the bus", |_| {
        of_type(&bus, "rt", PROPOSED).len() >= 2
    });
    let accepted = of_type(&bus, "rt", ACCEPTED);
    assert_eq!(accepted.len(), 2, "rev 2 accepted on approve: {accepted:?}");
    assert_eq!(accepted[1]["plan_rev"], json!(2));
    assert_eq!(accepted[1]["by"], json!("human"));
    let steps: Vec<&str> = accepted[1]["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap())
        .collect();
    assert_eq!(steps, ["answer-1", "answer-2"]);
    let proposed = of_type(&bus, "rt", PROPOSED);
    assert_eq!(proposed.len(), 2);
    assert_eq!(proposed[1]["kind"], json!("edit"));
    assert_eq!(proposed[1]["proposal_id"], json!(proposal.proposal_id));
    let calls = rig.calls.lock().unwrap().clone();
    assert_eq!(
        calls.iter().map(|c| c.3.as_str()).collect::<Vec<_>>(),
        ["rt:answer-1", "rt:answer-2"]
    );
}

/// Control: nothing held — approving the terminal gate finalizes the run exactly as before.
#[test]
fn approve_at_the_turn_gate_with_nothing_held_finalizes_as_before() {
    let dir = tmp_dir("plain");
    let db = dir.join("estate.db").to_str().unwrap().to_string();
    let mut rig = spawn(&db);
    rig.core
        .launch_run(spec("rp", vec![answer("answer-1")]))
        .unwrap();
    rig.tap
        .until("the turn gate", |seen| awaiting(seen, "rp") >= 1);
    let st = rig.core.confirm_gate("rp", approve()).unwrap();
    assert_eq!(st, SessionStatus::Completed);
    rig.tap
        .until("sessionCompleted", |seen| completed(seen, "rp"));
    assert_eq!(dispatched_ords(&rig.tap.seen, "rp"), vec![(1, 0)]);
    assert_eq!(of_type(&rig.bus, "rp", ACCEPTED).len(), 1);
}

/// Arm the hermetic emit spool (core#311) before `main`.
///
/// SAFETY (`ctor(unsafe)`): runs before `main` on one thread and only sets one process env var
/// via the std API — no allocator setup, no threads, no panics across the FFI boundary.
#[ctor::ctor(unsafe)]
fn arm_hermetic_emit_spool() {
    wicked_apps_core::emit::hermetic_test_spool();
}
