//! DES-CATALOG-002 §2.4, §4.6 and §11 Lane A: the plan-gate SEND-BACK and the expected revision in
//! the gate decision, driven through the real engine (`Core` actor → launch → floor fill → held
//! plan → `confirm_gate_expecting`).
//!
//! The runner records every dispatch and holds it, so "no unit dispatched" is read off the runner.
//! The lead's re-plan turn is the one engine call the runner answers itself (workflow
//! `wf-send-back`), with the scripted answer, and only once the test releases it, so an approval
//! sent WHILE the re-plan runs is observable.

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use wicked_council::types::{Category, Confidence, Dispatcher, InputMode, Vote};
use wicked_council::{AgenticCli, CouncilTask};

use wicked_core::{
    Core, CoreEvent, EntityMode, GateExpect, HumanConfirm, HumanDecision, LaunchSpec, PlanSteps,
    SessionStatus, StepInput, StepOutput, StepRunner, StepStatus, TeamConfig, GATE_CHANGED,
    TEAM_OUTBOX_FILE,
};

const DEADLINE: Duration = Duration::from_secs(90);

#[path = "support/skills_snapshot_fixture.rs"]
mod skills_fixture;

/// Hermetic: emissions go to a temp spool, and the skills root is a published fixture generation
/// (never the developer's live garden cache, whose skills may be Claude-only and refuse the
/// fixture seats). The held plan's floor runs the security specialist.
/// SAFETY (`ctor(unsafe)`): runs before `main` on one thread and only sets process env vars.
#[ctor::ctor(unsafe)]
fn arm_env() {
    wicked_apps_core::emit::hermetic_test_spool();
    let base = std::fs::canonicalize(std::env::temp_dir())
        .unwrap()
        .join(format!(
            "wicked-core-sendback-skills-{}",
            std::process::id()
        ));
    let _ = std::fs::remove_dir_all(&base);
    let snapshot = skills_fixture::publish_fixture_snapshot(
        &base,
        "000001",
        &["wicked-garden-qe-security-test-engineer"],
    );
    std::env::set_var("WICKED_SKILLS_SNAPSHOT", &snapshot);
}

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

/// The lead's scripted answer: `(status, output)`, handed out once `released`.
#[derive(Default)]
struct Lead {
    answer: Option<(StepStatus, String)>,
    released: bool,
    prompts: Vec<String>,
}

struct Runner {
    calls: Arc<Mutex<Vec<String>>>,
    lead: Arc<(Mutex<Lead>, Condvar)>,
    hold: Arc<(Mutex<bool>, Condvar)>,
}

fn out(input: &StepInput, status: StepStatus, output: String) -> StepOutput {
    StepOutput {
        run_id: input.run_id.clone(),
        unit_ix: input.unit_ix,
        attempt: input.attempt,
        output,
        status,
        usage: None,
        files: Vec::new(),
        tools: Vec::new(),
        governed: false,
    }
}

impl StepRunner for Runner {
    fn run_unit(&self, input: &StepInput) -> StepOutput {
        if input.workflow_id == "wf-send-back" {
            let (lock, cv) = &*self.lead;
            let mut lead = lock.lock().unwrap();
            lead.prompts.push(input.unit.description.clone());
            while !lead.released {
                let (g, _) = cv.wait_timeout(lead, Duration::from_millis(50)).unwrap();
                lead = g;
            }
            let (status, output) = lead
                .answer
                .clone()
                .unwrap_or((StepStatus::Failed, "no answer scripted".into()));
            return out(input, status, output);
        }
        // The PA's read-only scope step answers at once (no SCOPE line: the plan fails closed at
        // 100 and is held); every other unit is recorded and held.
        if input.unit.id.ends_with(":pa-scope") {
            return out(input, StepStatus::Ok, "looked around".into());
        }
        self.calls.lock().unwrap().push(input.unit.id.clone());
        let (lock, cv) = &*self.hold;
        let mut done = lock.lock().unwrap();
        while !*done {
            let (g, _) = cv.wait_timeout(done, Duration::from_millis(50)).unwrap();
            done = g;
        }
        out(input, StepStatus::Ok, "held".into())
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

struct Rig {
    /// Taken on drop and on a restart, so the actor is gone before the held lead is released.
    core: Option<Core>,
    calls: Arc<Mutex<Vec<String>>>,
    lead: Arc<(Mutex<Lead>, Condvar)>,
    hold: Arc<(Mutex<bool>, Condvar)>,
    rx: std::sync::mpsc::Receiver<CoreEvent>,
    seen: Vec<CoreEvent>,
}
impl Drop for Rig {
    /// The actor stops FIRST (its last handle drops), then the held workers are released: a lead
    /// released before the stop could still answer it (codex r2).
    fn drop(&mut self) {
        drop(self.core.take());
        *self.hold.0.lock().unwrap() = true;
        self.hold.1.notify_all();
        let mut l = self.lead.0.lock().unwrap();
        l.released = true;
        self.lead.1.notify_all();
    }
}

impl Rig {
    fn dir(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "wicked-core-sendback-{name}-{}",
            std::process::id()
        ))
    }

    fn new(name: &str) -> Rig {
        let dir = Rig::dir(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Rig::over(&dir)
    }

    /// A restart: this rig's actor stops (its handles drop) while its lead is still held, so no
    /// answer reaches it; a fresh one boots over its store.
    fn restart(mut self, name: &str) -> Rig {
        drop(self.core.take());
        std::thread::sleep(Duration::from_millis(500));
        let next = Rig::over(&Rig::dir(name));
        drop(self);
        next
    }

    fn core(&self) -> &Core {
        self.core.as_ref().expect("a running engine")
    }

    fn over(dir: &std::path::Path) -> Rig {
        let db = dir.join("estate.db").to_string_lossy().into_owned();
        let calls: Arc<Mutex<Vec<String>>> = Arc::default();
        let lead: Arc<(Mutex<Lead>, Condvar)> = Arc::default();
        let hold: Arc<(Mutex<bool>, Condvar)> = Arc::default();
        let core = Core::spawn_with_engine_team(
            db,
            Arc::new(StubDispatcher),
            Arc::new(Runner {
                calls: calls.clone(),
                lead: lead.clone(),
                hold: hold.clone(),
            }),
            TeamConfig::new(None, Some(dir.join(TEAM_OUTBOX_FILE)))
                .with_final_pass_budget(Duration::from_millis(300)),
        );
        let rx = core.subscribe();
        Rig {
            core: Some(core),
            calls,
            lead,
            hold,
            rx,
            seen: Vec::new(),
        }
    }

    fn until(&mut self, what: &str, cond: impl Fn(&Rig) -> bool) {
        let end = Instant::now() + DEADLINE;
        loop {
            while let Ok(ev) = self.rx.try_recv() {
                self.seen.push(ev);
            }
            if cond(self) {
                return;
            }
            // Fail fast: a run that ended will never reach the condition.
            if let Some(j) = self
                .seen
                .iter()
                .map(CoreEvent::to_json)
                .find(|j| j["type"] == "sessionFailed")
            {
                let errors: Vec<Value> = self
                    .seen
                    .iter()
                    .map(CoreEvent::to_json)
                    .filter(|j| j["type"] == "error")
                    .collect();
                panic!("the run failed waiting for {what}: {j} {errors:#?}");
            }
            if Instant::now() > end {
                let errors: Vec<Value> = self
                    .seen
                    .iter()
                    .map(CoreEvent::to_json)
                    .filter(|j| j["type"] == "error" || j["type"] == "sessionFailed")
                    .collect();
                panic!("timed out waiting for {what}; errors: {errors:#?}");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn plan_pauses(&self, run: &str) -> usize {
        self.seen
            .iter()
            .filter(|e| {
                matches!(e, CoreEvent::AwaitingHuman { session, gate_kind, .. }
                    if session == run && gate_kind == "plan_approval")
            })
            .count()
    }

    fn dispatched(&self, run: &str) -> usize {
        self.seen
            .iter()
            .filter(|e| matches!(e, CoreEvent::UnitDispatched { session, .. } if session == run))
            .count()
    }

    fn session(&self, run: &str) -> wicked_core::AgentSession {
        self.core()
            .sessions_detail()
            .unwrap()
            .into_iter()
            .find(|v| v.session.id == run)
            .expect("run")
            .session
    }

    fn held(&self, run: &str) -> wicked_core::PendingPlan {
        self.session(run)
            .team_plan
            .and_then(|t| t.pending)
            .expect("a held plan")
    }

    fn script(&self, status: StepStatus, output: String) {
        self.lead.0.lock().unwrap().answer = Some((status, output));
    }

    fn release_lead(&self) {
        self.lead.0.lock().unwrap().released = true;
        self.lead.1.notify_all();
    }
}

/// The held plan these tests send back: a writer, the parity test after it, the cited-findings
/// check, with a declared touch so no PA scope step runs first. Manual mode holds it.
fn launch(rig: &mut Rig, run: &str) {
    launch_with(rig, run, true)
}

/// `declared`: the plan declares its touch (no PA scope step); else the PA scopes it first, so
/// the held plan's first unit already ran. The plan authors its own deliver step.
fn launch_with(rig: &mut Rig, run: &str, declared: bool) {
    let mut plan = json!({
        "steps": [
            {"catalog": "build", "id": "build"},
            {"catalog": "test", "id": "parity", "rubric": "parity", "depends_on": ["build"]},
            {"catalog": "critique", "id": "check", "rubric": "cited-findings",
             "depends_on": ["build"], "gate": {"human_confirm_if": "verdict_not_pass"}},
            {"catalog": "deliver", "id": "deliver",
             "executor": {"type": "tool", "cmd": ["true"]}}
        ]
    });
    if declared {
        plan["touch"] = json!(["src/sso.rs"]);
    }
    let plan: PlanSteps = serde_json::from_value(plan).unwrap();
    rig.core()
        .launch_run(LaunchSpec {
            problem: "add SSO login".into(),
            clis: vec![cli("a"), cli("b"), cli("c")],
            entity_mode: EntityMode::Shared,
            session_id: run.into(),
            human_confirm: HumanConfirm::Before(1),
            auto_deliver: false,
            repo_ref: None,
            base_ref: None,
            workflow: None,
            project_id: None,
            extra_write_roots: Vec::new(),
            extra_read_roots: Vec::new(),
            project_graph: None,
            plan: Some(plan),
            deliver_step: None,
            exclude_seats: Vec::new(),
            evidence_root: None,
            primary: Some("a".into()),
            reduced_assurance: false,
            power: false,
            voice: None,
            deliverables: Vec::new(),
            qe_acceptance: Default::default(),
        })
        .unwrap();
    rig.until("the plan gate", |r| r.plan_pauses(run) == 1);
}

/// The held plan's own steps (no provenance), as the lead would restate them.
fn restated(held: &wicked_core::PendingPlan) -> Vec<Value> {
    held.steps
        .steps
        .iter()
        .map(|s| {
            let mut v = serde_json::to_value(s).unwrap();
            let o = v.as_object_mut().unwrap();
            for k in ["added_by", "floor_reason", "floor_rule"] {
                o.remove(k);
            }
            v
        })
        .collect()
}

fn plan_line(steps: Vec<Value>) -> String {
    format!(
        "Added a rollback step.\nPLAN {}",
        serde_json::to_string(&json!({ "steps": steps })).unwrap()
    )
}

fn approve() -> HumanDecision {
    HumanDecision::Approve {
        amend: None,
        amend_scope: Default::default(),
    }
}

fn expect(ord: u32, rev: u32) -> Option<GateExpect> {
    Some(GateExpect {
        ord: Some(ord),
        plan_rev: Some(rev),
    })
}

fn send_back(note: &str) -> HumanDecision {
    HumanDecision::RequestChanges {
        note: Some(note.into()),
    }
}

fn gate_changed(r: anyhow::Result<SessionStatus>) -> bool {
    r.is_err_and(|e| format!("{e:#}").starts_with(GATE_CHANGED))
}

/// The send-back dispatches no unit; while the lead re-plans every answer (old rev or none) is
/// `gate_changed`; the new rev is held for a fresh approval with every check of the old one, the
/// floor and the parity freshness re-applied (a writer added after the parity step owes parity
/// again, even behind an ordinary test); the old rev is no longer approvable, the new one is.
#[test]
fn a_send_back_replans_holds_a_new_rev_and_dispatches_nothing() {
    let run = "rsb";
    let mut rig = Rig::new(run);
    launch(&mut rig, run);
    let held = rig.held(run);
    assert_eq!(held.rev, 1);
    let ord = 1;
    // The lead adds a writer at the end and an ordinary test after it.
    let mut steps = restated(&held);
    steps.push(
        json!({"catalog": "build", "id": "rollback", "instructions": "roll back if parity fails"}),
    );
    steps.push(json!({"catalog": "test", "id": "smoke", "depends_on": ["rollback"]}));
    rig.script(StepStatus::Ok, plan_line(steps));

    let status = rig
        .core()
        .confirm_gate_expecting(run, send_back("add a rollback step"), None, expect(ord, 1))
        .unwrap();
    assert_eq!(status, SessionStatus::AwaitingHuman);
    // While the re-plan runs: the old rev, and any answer, is refused `gate_changed`.
    rig.until("the lead's turn", |r| {
        !r.lead.0.lock().unwrap().prompts.is_empty()
    });
    assert!(gate_changed(rig.core().confirm_gate_expecting(
        run,
        approve(),
        None,
        expect(ord, 1)
    )));
    assert!(gate_changed(rig.core().confirm_gate(run, approve())));
    assert!(gate_changed(
        rig.core().confirm_gate(run, HumanDecision::Reject)
    ));
    let prompt = rig.lead.0.lock().unwrap().prompts[0].clone();
    assert!(
        prompt.contains("NOTE: ```add a rollback step```"),
        "{prompt}"
    );
    assert!(prompt.contains("KEEP: "), "{prompt}");

    rig.release_lead();
    rig.until("the new rev's gate", |r| r.plan_pauses(run) == 2);
    let next = rig.held(run);
    assert_eq!(next.rev, 2);
    assert_eq!(next.refusal, None);
    let by = |id: &str| next.steps.steps.iter().find(|s| s.id == id).cloned();
    assert!(by("rollback").is_some());
    // The plan's own deliver step is kept (the lead never sees it).
    assert!(by("deliver").is_some(), "the authored deliver step stays");
    assert!(!prompt.contains("\"deliver\""), "{prompt}");
    assert!(!prompt.contains('\n'), "one line on every carrier");
    // Every check of rev 1 is in rev 2, unchanged.
    for id in ["parity", "check"] {
        let old = held.steps.steps.iter().find(|s| s.id == id).unwrap();
        let new = by(id).unwrap_or_else(|| panic!("{id} kept"));
        assert_eq!(
            (&old.catalog, &old.rubric, &old.gate),
            (&new.catalog, &new.rubric, &new.gate)
        );
    }
    for floor in &held.floor_added {
        assert!(
            next.steps.steps.iter().any(|s| &s.catalog == floor),
            "floor phase {floor} kept"
        );
    }
    // Parity is owed again after the new writer, and the ordinary `smoke` test does not count.
    let ids: Vec<&str> = next.steps.steps.iter().map(|s| s.id.as_str()).collect();
    let rollback = ids.iter().position(|i| *i == "rollback").unwrap();
    let fresh = next
        .steps
        .steps
        .iter()
        .enumerate()
        .find(|(i, s)| *i > rollback && s.rubric.as_deref() == Some("parity"))
        .unwrap_or_else(|| panic!("a parity test after rollback: {ids:?}"));
    assert_eq!(fresh.1.catalog, "test");
    // Nothing dispatched: no unit of the run reached the runner.
    assert_eq!(rig.dispatched(run), 0);
    assert!(rig.calls.lock().unwrap().is_empty());
    // The old rev is no longer approvable; the new one is.
    assert!(gate_changed(rig.core().confirm_gate_expecting(
        run,
        approve(),
        None,
        expect(ord, 1)
    )));
    let s = rig
        .core()
        .confirm_gate_expecting(run, approve(), None, expect(ord, 2))
        .unwrap();
    assert_eq!(s, SessionStatus::Executing);
}

/// A re-plan that removes or dilutes the parity or the cited-findings step — the floor and the
/// instruments untouched — is refused, and so is a lead that fails: the held rev re-opens
/// approvable, with the failure recorded.
#[test]
fn a_failed_or_check_losing_replan_reopens_the_old_rev() {
    type Mutate = Box<dyn Fn(&mut Vec<Value>)>;
    let cases: Vec<(&str, Mutate, Option<StepStatus>)> = vec![
        (
            "drop-parity",
            Box::new(|s| s.retain(|v| v["id"] != "parity")),
            None,
        ),
        (
            "unrubric-check",
            Box::new(|s| {
                for v in s.iter_mut().filter(|v| v["id"] == "check") {
                    v.as_object_mut().unwrap().remove("rubric");
                }
            }),
            None,
        ),
        (
            "move-parity-rubric",
            Box::new(|s| {
                for v in s.iter_mut().filter(|v| v["id"] == "parity") {
                    v["instructions"] = json!("just run the tests");
                }
            }),
            None,
        ),
        (
            "weaken-floor-review",
            Box::new(|s| {
                for v in s.iter_mut().filter(|v| v["id"] == "review") {
                    v["instructions"] = json!("review whitespace only");
                }
            }),
            None,
        ),
        (
            "check-before-its-work",
            Box::new(|s| {
                let i = s.iter().position(|v| v["id"] == "check").unwrap();
                let mut c = s.remove(i);
                c["depends_on"] = json!([]);
                s.insert(0, c);
            }),
            None,
        ),
        (
            // The work the check followed, renamed and moved after it (codex r2).
            "rename-the-build",
            Box::new(|s| {
                let b = s.iter().position(|v| v["id"] == "build").unwrap();
                let mut build = s.remove(b);
                build["id"] = json!("impl");
                let i = s.iter().position(|v| v["id"] == "check").unwrap();
                let mut c = s.remove(i);
                c["depends_on"] = json!([]);
                s.insert(0, c);
                s.push(build);
                for v in s.iter_mut().filter(|v| v["id"] == "parity") {
                    v["depends_on"] = json!(["impl"]);
                }
            }),
            None,
        ),
        ("lead-fails", Box::new(|_| {}), Some(StepStatus::Failed)),
        ("no-plan-line", Box::new(|_| {}), Some(StepStatus::Ok)),
    ];
    for (name, mutate, raw) in cases {
        let run = format!("rf{}", name.replace('-', ""));
        let mut rig = Rig::new(name);
        launch(&mut rig, &run);
        let held = rig.held(&run);
        let mut steps = restated(&held);
        mutate(&mut steps);
        match raw {
            None => rig.script(StepStatus::Ok, plan_line(steps)),
            Some(st) => rig.script(st, "I could not make a plan".into()),
        }
        rig.release_lead();
        rig.core()
            .confirm_gate_expecting(&run, send_back("make it better"), None, expect(1, 1))
            .unwrap();
        rig.until("the old rev re-opened", |r| r.plan_pauses(&run) == 2);
        let back = rig.held(&run);
        assert_eq!(back.rev, 1, "{name}: the same rev");
        let why = back.refusal.clone().unwrap_or_default();
        assert!(
            why.starts_with("Couldn't update the plan: "),
            "{name}: {why}"
        );
        // (A renamed build after its review also breaks the floor's own order: refused either way.)
        if raw.is_none() && name != "rename-the-build" {
            assert!(why.contains("drops or weakens a check"), "{name}: {why}");
        }
        assert_eq!(rig.dispatched(&run), 0, "{name}");
        // The old rev is approvable again.
        let s = rig
            .core()
            .confirm_gate_expecting(&run, approve(), None, expect(1, 1))
            .unwrap();
        assert_eq!(s, SessionStatus::Executing, "{name}");
    }
}

/// A send-back without the plan revision is refused as before, and an answer whose expectation
/// does not hold (another rev, another unit) is `gate_changed`; the gate stays open through all
/// of them.
#[test]
fn a_send_back_needs_the_plan_rev_and_a_stale_answer_is_gate_changed() {
    let run = "rnr";
    let mut rig = Rig::new(run);
    launch(&mut rig, run);
    let e = rig
        .core()
        .confirm_gate(run, send_back("add a rollback step"))
        .unwrap_err();
    assert!(
        format!("{e:#}").contains("a plan_approval gate reviews a plan"),
        "{e:#}"
    );
    assert!(gate_changed(rig.core().confirm_gate_expecting(
        run,
        approve(),
        None,
        expect(1, 7)
    )));
    assert!(gate_changed(rig.core().confirm_gate_expecting(
        run,
        approve(),
        None,
        expect(9, 1)
    )));
    assert!(gate_changed(rig.core().confirm_gate_expecting(
        run,
        send_back("x"),
        None,
        expect(1, 2)
    )));
    // An empty note is no send-back.
    assert!(rig
        .core()
        .confirm_gate_expecting(run, send_back("  "), None, expect(1, 1))
        .is_err());
    assert_eq!(rig.held(run).rev, 1);
    let s = rig
        .core()
        .confirm_gate_expecting(run, approve(), None, expect(1, 1))
        .unwrap();
    assert_eq!(s, SessionStatus::Executing);
    // A second tab's answer to the same gate, after it was answered: `gate_changed`.
    assert!(gate_changed(rig.core().confirm_gate_expecting(
        run,
        approve(),
        None,
        expect(1, 1)
    )));
}

/// A PA-scoped plan (its `pa-scope` step already ran) is sent back: the lead never sees the step
/// that ran, a restated one is dropped, and the new rev keeps it as the done prefix it is.
#[test]
fn a_scoped_plan_keeps_its_done_scope_step_through_a_send_back() {
    let run = "rsc";
    let mut rig = Rig::new(run);
    launch_with(&mut rig, run, false);
    let held = rig.held(run);
    assert_eq!(held.steps.steps[0].id, "pa-scope");
    // The lead restates everything, pa-scope included, and adds a step.
    let mut steps = restated(&held);
    steps.push(json!({"catalog": "understand", "id": "survey"}));
    rig.script(StepStatus::Ok, plan_line(steps));
    rig.release_lead();
    rig.core()
        .confirm_gate_expecting(run, send_back("survey first"), None, expect(2, held.rev))
        .unwrap();
    rig.until("the new rev's gate", |r| r.plan_pauses(run) == 2);
    let next = rig.held(run);
    assert_eq!(next.rev, held.rev + 1, "{:?}", next.refusal);
    let ids: Vec<&str> = next.steps.steps.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(
        ids.iter().filter(|i| **i == "pa-scope").count(),
        1,
        "{ids:?}"
    );
    assert!(ids.contains(&"survey"), "{ids:?}");
    let prompt = rig.lead.0.lock().unwrap().prompts[0].clone();
    assert!(prompt.contains("already ran (pa-scope)"), "{prompt}");
    assert!(!prompt.contains("\"id\":\"pa-scope\""), "{prompt}");
    assert_eq!(rig.dispatched(run), 1, "only the scope step ever ran");
}

/// A restart while the lead re-plans: no turn is left to answer the send-back, so the held rev
/// re-opens, approvable, with the reason recorded.
#[test]
fn a_restart_while_the_lead_replans_reopens_the_held_rev() {
    let run = "rrs";
    let mut rig = Rig::new(run);
    launch(&mut rig, run);
    rig.core()
        .confirm_gate_expecting(run, send_back("add a rollback step"), None, expect(1, 1))
        .unwrap();
    rig.until("the lead's turn", |r| {
        !r.lead.0.lock().unwrap().prompts.is_empty()
    });
    let rig = rig.restart(run);
    let end = Instant::now() + DEADLINE;
    loop {
        let h = rig.held(run);
        if h.refusal
            .as_deref()
            .is_some_and(|w| w.contains("restarted"))
            && h.gate_id.is_some()
        {
            break;
        }
        assert!(Instant::now() < end, "the held rev never re-opened: {h:?}");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(rig.held(run).rev, 1);
    let s = rig
        .core()
        .confirm_gate_expecting(run, approve(), None, expect(1, 1))
        .unwrap();
    assert_eq!(s, SessionStatus::Executing);
}
