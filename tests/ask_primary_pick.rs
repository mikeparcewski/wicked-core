//! DES-ASK-TEAM-CHAT-001 §4.1 (ASK-K1a; DES-TEAMING-002 §8.1, D1's selection half) through the
//! REAL engine over a REAL bus: the PA pick is on `path.started` (`cli`, `selection`), every PA
//! step of the run lands on it, and a PA step whose seat fails or times out re-picks the PA —
//! `path.repicked` keyed by `pick_seq`, the step redispatched as attempt+1 on the other seat —
//! while a one-seat roster keeps the engine's terminal backstop.
//!
//! Expected values are fixed literals from the design; waits are generous and return early.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use wicked_council::types::{Category, Confidence, Dispatcher, InputMode, Vote};
use wicked_council::{AgenticCli, CouncilTask};

use wicked_core::{
    BusDb, Core, CoreEvent, EntityMode, HumanConfirm, LaunchSpec, PlanSteps, StepInput, StepOutput,
    StepRunner, StepStatus, TeamConfig, TEAM_OUTBOX_FILE,
};

const DEADLINE: Duration = Duration::from_secs(90);
const STARTED: &str = "wicked.team.path.started";
const REPICKED: &str = "wicked.team.path.repicked";

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

type Calls = Arc<Mutex<Vec<(String, u32, u32, String, String)>>>;

/// What the first dispatch of the run does before every later one is recorded and held.
#[derive(Clone, Copy)]
enum First {
    Hold,
    /// A worker-originated failure (the wrapped runner's nonzero-exit shape): the ladder's input.
    WorkerError,
    /// The engine's own turn ceiling.
    TimedOut,
}

/// Records `(run, unit_ix, attempt, unit id, seat)` for every dispatch; the first dispatch may
/// fail as `first` says; every other dispatch is held until the test ends.
struct Runner {
    calls: Calls,
    gate: Arc<(Mutex<bool>, Condvar)>,
    first: First,
    n: AtomicU32,
}
impl StepRunner for Runner {
    fn run_unit(&self, input: &StepInput) -> StepOutput {
        let seat = input.unit.assigned_cli.clone().unwrap_or_default();
        self.calls.lock().unwrap().push((
            input.run_id.clone(),
            input.unit_ix as u32,
            input.attempt,
            input.unit.id.clone(),
            seat,
        ));
        let n = self.n.fetch_add(1, Ordering::SeqCst);
        let fail = |status: StepStatus, output: &str| StepOutput {
            run_id: input.run_id.clone(),
            unit_ix: input.unit_ix,
            attempt: input.attempt,
            output: output.into(),
            status,
            usage: None,
            files: Vec::new(),
            tools: Vec::new(),
            governed: false,
        };
        if n == 0 {
            match self.first {
                First::Hold => {}
                First::WorkerError => {
                    return fail(
                        StepStatus::Failed,
                        "(cli `x` exited 1) timeout waiting for response",
                    )
                }
                First::TimedOut => {
                    return fail(
                        StepStatus::TimedOut,
                        "ACP timeout waiting for response id=7",
                    )
                }
            }
        }
        let (lock, cv) = &*self.gate;
        let mut done = lock.lock().unwrap();
        while !*done {
            done = cv.wait(done).unwrap();
        }
        fail(StepStatus::Cancelled, "released at test end")
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
        seat_eligible_for_work: true,
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
    let dir = std::env::temp_dir().join(format!("wicked-core-k1a-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

struct Rig {
    core: Core,
    calls: Calls,
    gate: Arc<(Mutex<bool>, Condvar)>,
    tap: Tap,
    bus: String,
}
impl Drop for Rig {
    fn drop(&mut self) {
        let (lock, cv) = &*self.gate;
        *lock.lock().unwrap() = true;
        cv.notify_all();
    }
}

fn spawn(db: &str, first: First) -> Rig {
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
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
        Arc::new(Runner {
            calls: calls.clone(),
            gate: gate.clone(),
            first,
            n: AtomicU32::new(0),
        }),
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
        gate,
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
        std::thread::sleep(Duration::from_millis(600));
        self.drain();
    }
}

/// The run's team facts of `ty` ON THE BUS, in `event_id` order.
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

/// Two read-only PA steps: no creator ⇒ score 0, an empty floor, accepted at once in auto mode.
fn two_answers() -> PlanSteps {
    plan(json!({"steps": [
        {"catalog": "understand", "id": "answer-1"},
        {"catalog": "understand", "id": "answer-2"}
    ]}))
}

fn spec(run: &str, clis: Vec<AgenticCli>, primary: Option<&str>) -> LaunchSpec {
    LaunchSpec {
        problem: "what does the retire flow do".into(),
        clis,
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
        plan: Some(two_answers()),
        deliver_step: None,
        exclude_seats: Vec::new(),
        evidence_root: None,
        primary: primary.map(str::to_string),
        reduced_assurance: false,
    }
}

fn view(core: &Core, run: &str) -> wicked_core::SessionView {
    core.sessions_detail()
        .unwrap()
        .into_iter()
        .find(|v| v.session.id == run)
        .expect("run")
}

fn calls_of(calls: &Calls, run: &str) -> Vec<(u32, u32, String, String)> {
    calls
        .lock()
        .unwrap()
        .iter()
        .filter(|c| c.0 == run)
        .map(|c| (c.1, c.2, c.3.clone(), c.4.clone()))
        .collect()
}

/// A chosen PA: `path.started{cli, selection:"chosen"}` names it, the roster carries it first,
/// the team state records it, and every PA step is seated on it.
#[test]
fn a_chosen_pa_is_on_path_started_and_every_pa_step_lands_on_it() {
    let dir = tmp_dir("chosen");
    let db = dir.join("estate.db").to_str().unwrap().to_string();
    let mut rig = spawn(&db, First::Hold);
    let calls = rig.calls.clone();
    rig.core
        .launch_run(spec("rc", vec![cli("a"), cli("b")], Some("b")))
        .unwrap();
    let bus = rig.bus.clone();
    rig.tap
        .until("path.started", |_| !of_type(&bus, "rc", STARTED).is_empty());
    rig.tap
        .until("the first dispatch", |_| !calls_of(&calls, "rc").is_empty());
    let started = &of_type(&bus, "rc", STARTED)[0];
    assert_eq!(started["cli"], json!("b"));
    assert_eq!(started["selection"], json!("chosen"));
    assert_eq!(started["roster"], json!(["b", "a"]), "the pick is first");
    let v = view(&rig.core, "rc");
    assert_eq!(v.session.clis, ["b", "a"]);
    let pick = v
        .session
        .team
        .as_ref()
        .and_then(|t| t.primary.clone())
        .expect("a pick");
    assert_eq!(
        (pick.cli.as_str(), pick.selection.as_str(), pick.pick_seq),
        ("b", "chosen", 0)
    );
    assert!(
        v.units
            .iter()
            .all(|u| u.assigned_cli.as_deref() == Some("b")),
        "every PA step on the pick: {:?}",
        v.units
            .iter()
            .map(|u| (u.id.clone(), u.assigned_cli.clone()))
            .collect::<Vec<_>>()
    );
    assert_eq!(calls_of(&calls, "rc")[0].3, "b");
}

/// No choice on a team run: the engine draws one (`selection:"random"`), from the roster, and
/// seats every PA step on it.
#[test]
fn an_unchosen_pa_is_drawn_at_random_and_recorded_so() {
    let dir = tmp_dir("random");
    let db = dir.join("estate.db").to_str().unwrap().to_string();
    let mut rig = spawn(&db, First::Hold);
    let calls = rig.calls.clone();
    rig.core
        .launch_run(spec("rr", vec![cli("a"), cli("b"), cli("c")], None))
        .unwrap();
    let bus = rig.bus.clone();
    rig.tap
        .until("path.started", |_| !of_type(&bus, "rr", STARTED).is_empty());
    rig.tap
        .until("the first dispatch", |_| !calls_of(&calls, "rr").is_empty());
    let started = &of_type(&bus, "rr", STARTED)[0];
    let pa = started["cli"].as_str().expect("a cli").to_string();
    assert!(["a", "b", "c"].contains(&pa.as_str()), "{pa}");
    assert_eq!(started["selection"], json!("random"));
    assert_eq!(
        started["roster"][0],
        json!(pa),
        "the draw is first on the roster"
    );
    let v = view(&rig.core, "rr");
    assert_eq!(
        v.session
            .team
            .as_ref()
            .and_then(|t| t.primary.as_ref())
            .map(|p| p.selection.as_str()),
        Some("random")
    );
    assert!(v
        .units
        .iter()
        .all(|u| u.assigned_cli.as_deref() == Some(pa.as_str())));
    assert_eq!(calls_of(&calls, "rr")[0].3, pa);
}

/// A PA step whose seat FAILS (a worker-originated failure) on a two-seat roster: the PA is
/// re-picked — `path.repicked{from, to, reason, selection:"random", pick_seq:1}` on the bus, the
/// roster re-ordered, the later PA step moved, the failed step redispatched as attempt+1 on the
/// new PA.
#[test]
fn a_pa_seat_failure_repicks_the_pa_and_redispatches_on_the_other_seat() {
    let dir = tmp_dir("repick");
    let db = dir.join("estate.db").to_str().unwrap().to_string();
    let mut rig = spawn(&db, First::WorkerError);
    let calls = rig.calls.clone();
    rig.core
        .launch_run(spec("rp", vec![cli("a"), cli("b")], Some("a")))
        .unwrap();
    let bus = rig.bus.clone();
    rig.tap
        .until("the redispatch", |_| calls_of(&calls, "rp").len() >= 2);
    rig.tap.until("path.repicked", |_| {
        !of_type(&bus, "rp", REPICKED).is_empty()
    });
    let calls = calls_of(&calls, "rp");
    assert_eq!(
        (calls[0].0, calls[0].1, calls[0].3.as_str()),
        (0, 0, "a"),
        "attempt 0 on the PA"
    );
    assert_eq!(
        (calls[1].0, calls[1].1, calls[1].3.as_str()),
        (0, 1, "b"),
        "the same step, attempt+1, on the other seat"
    );
    let rows = of_type(&bus, "rp", REPICKED);
    assert_eq!(rows.len(), 1);
    let r = &rows[0];
    assert_eq!(r["from"], json!("a"));
    assert_eq!(r["to"], json!("b"));
    assert_eq!(r["reason"], json!("failed"));
    assert_eq!(r["selection"], json!("random"));
    assert_eq!(r["pick_seq"], json!(1));
    assert_eq!(
        (r["ord"].clone(), r["attempt"].clone()),
        (json!(1), json!(0))
    );
    assert_eq!(r["by"], json!("engine"));
    let v = view(&rig.core, "rp");
    assert_eq!(v.session.clis, ["b", "a"], "the roster follows the pick");
    let plan_roster: Vec<&str> = v
        .session
        .team_plan
        .as_ref()
        .expect("a team plan")
        .roster
        .iter()
        .map(|c| c["key"].as_str().unwrap())
        .collect();
    assert_eq!(
        plan_roster,
        ["b", "a"],
        "the plan state's roster (the re-plan's) follows too"
    );
    let pick = v
        .session
        .team
        .as_ref()
        .and_then(|t| t.primary.clone())
        .expect("a pick");
    assert_eq!(
        (pick.cli.as_str(), pick.selection.as_str(), pick.pick_seq),
        ("b", "random", 1)
    );
    assert!(
        v.units
            .iter()
            .all(|u| u.assigned_cli.as_deref() == Some("b")),
        "the pending PA step followed the PA: {:?}",
        v.units
            .iter()
            .map(|u| (u.id.clone(), u.assigned_cli.clone()))
            .collect::<Vec<_>>()
    );
}

/// A PA step that hits the turn ceiling (`timed_out`) on a two-seat roster re-picks too (F2),
/// with the reason on the row; the run is not cancelled.
#[test]
fn a_pa_timeout_repicks_when_another_seat_is_eligible() {
    let dir = tmp_dir("timeout");
    let db = dir.join("estate.db").to_str().unwrap().to_string();
    let mut rig = spawn(&db, First::TimedOut);
    let calls = rig.calls.clone();
    rig.core
        .launch_run(spec("rt", vec![cli("a"), cli("b")], Some("a")))
        .unwrap();
    let bus = rig.bus.clone();
    rig.tap
        .until("the redispatch", |_| calls_of(&calls, "rt").len() >= 2);
    rig.tap.until("path.repicked", |_| {
        !of_type(&bus, "rt", REPICKED).is_empty()
    });
    let r = &of_type(&bus, "rt", REPICKED)[0];
    assert_eq!(
        (r["from"].clone(), r["to"].clone()),
        (json!("a"), json!("b"))
    );
    assert_eq!(r["reason"], json!("timed_out"));
    assert_eq!(calls_of(&calls, "rt")[1].3, "b");
    rig.tap.settle();
    assert!(
        !rig.tap
            .seen
            .iter()
            .any(|e| matches!(e, CoreEvent::RunCancelled { session, .. } if session == "rt")),
        "a re-picked timeout never takes the cancel backstop"
    );
    assert_eq!(
        view(&rig.core, "rt").session.status,
        wicked_core::SessionStatus::Executing
    );
}

/// One seat: a PA timeout has nobody to re-pick, so it is an ordinary timeout (core#744) — with
/// no operator in the loop the run FAILS (`sessionFailed`, `stepFailed{failureKind: timedOut}`,
/// `timed_out` on the wire), it is never cancelled by its own ceiling, and no `path.repicked` is
/// published.
#[test]
fn a_pa_timeout_on_a_one_seat_roster_fails_the_run_and_never_cancels_it() {
    let dir = tmp_dir("oneseat");
    let db = dir.join("estate.db").to_str().unwrap().to_string();
    let mut rig = spawn(&db, First::TimedOut);
    let calls = rig.calls.clone();
    rig.core
        .launch_run(spec("r1s", vec![cli("a")], None))
        .unwrap();
    rig.tap.until("the failed terminal", |seen| {
        seen.iter()
            .any(|e| matches!(e, CoreEvent::SessionFailed { session, .. } if session == "r1s"))
    });
    rig.tap.settle();
    assert_eq!(calls_of(&calls, "r1s").len(), 1, "no redispatch");
    assert!(of_type(&rig.bus, "r1s", REPICKED).is_empty());
    assert!(
        rig.tap.seen.iter().any(|e| matches!(
            e,
            CoreEvent::StepFailed {
                session,
                failure_kind: wicked_core::StepFailureKind::TimedOut,
                ..
            } if session == "r1s"
        )),
        "the cause is named on the wire"
    );
    assert!(
        !rig.tap
            .seen
            .iter()
            .any(|e| matches!(e, CoreEvent::RunCancelled { session, .. } if session == "r1s")),
        "a timeout never ends the run as cancelled"
    );
    assert_eq!(
        view(&rig.core, "r1s").session.status,
        wicked_core::SessionStatus::Failed
    );
}

/// Arm the hermetic emit spool (core#311) before `main`.
///
/// SAFETY (`ctor(unsafe)`): runs before `main` on one thread and only sets one process env var
/// via the std API — no allocator setup, no threads, no panics across the FFI boundary.
#[ctor::ctor(unsafe)]
fn arm_hermetic_emit_spool() {
    wicked_apps_core::emit::hermetic_test_spool();
}
