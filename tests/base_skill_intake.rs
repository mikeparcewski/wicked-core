//! core#468: the run's BASE skill — a role-keyed discipline directive on EVERY agent unit — is
//! admitted at INTAKE and carried onto every unit.
//!
//! Driven through a REAL `Core` and a REAL actor (the intake admission lives on the actor's
//! synchronous launch path and in `pre_distribute`): with `WICKED_SKILLS_SNAPSHOT` pointing at a
//! fixture generation that LACKS the def's `base_skill_ref`, `launch_run` must return `Err`
//! naming the skill as the base skill — synchronously, before any unit is planned, with NO
//! session persisted and no event for that run (never "unit 1 failed"). With a generation that
//! HOLDS it, the run completes, every `unitDispatched` carries `baseSkill {name, role}` keyed on
//! the unit's role, and every worker input carries the skill on the unit AND in the plan-wide
//! `required_skills` set. The engine-config default (`WICKED_BASE_SKILL_REF`) applies to a def
//! that declares none, is refused the same way when missing, and is overridden by the def's
//! explicit `""` opt-out.
//!
//! One test in its own binary: it sets process-global variables, and integration tests run one
//! process per file.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use wicked_core::{
    BaseSkill, Core, CoreEvent, EntityMode, HumanConfirm, LaunchSpec, StepInput, StepOutput,
    StepRunner, StepStatus,
};
use wicked_council::types::{Category, Confidence, Dispatcher, InputMode, Vote};
use wicked_council::{AgenticCli, CouncilTask};

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

struct NumericDispatcher;
impl Dispatcher for NumericDispatcher {
    fn dispatch(&self, c: &AgenticCli, _: &CouncilTask) -> Option<Vote> {
        Some(Vote {
            cli: c.key.clone(),
            recommendation: "1".into(),
            top_risk: "none".into(),
            change_my_mind: "no".into(),
            disqualifier: None,
            confidence: Confidence::default(),
            provenance: "numeric".into(),
        })
    }
}

/// What the worker was handed per unit: `(run, ord, base_skill_ref, required_skills)`.
type Seen = Arc<Mutex<Vec<(String, u32, Option<String>, Vec<String>)>>>;

/// Completes every agent unit with Ok — and records what it was handed.
struct RecordingOkRunner(Seen);
impl StepRunner for RecordingOkRunner {
    fn run_unit(&self, i: &StepInput) -> StepOutput {
        self.0.lock().unwrap().push((
            i.run_id.clone(),
            i.unit.ord,
            i.unit.base_skill_ref.clone(),
            i.required_skills.clone(),
        ));
        StepOutput {
            run_id: i.run_id.clone(),
            unit_ix: i.unit_ix,
            attempt: i.attempt,
            // DES-L1 PR-1A (D-9): the def's `review` phase is an Evaluator agent unit — answer the
            // verdict contract the fold parses, or the run parks at the escalation gate.
            output: if i.unit.role == wicked_core::PhaseRole::Evaluator && i.unit.tool_cmd.is_none()
            {
                "ok\nVERDICT: PASS".into()
            } else {
                "ok".into()
            },
            status: StepStatus::Ok,
            usage: None,
            files: vec![],
            tools: Vec::new(),
            governed: false,
        }
    }
}

/// Drain events until a terminal event for `session` is observed or the deadline expires.
fn drain_until_terminal(
    events: &std::sync::mpsc::Receiver<CoreEvent>,
    session: &str,
) -> Vec<CoreEvent> {
    let mut collected = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or(Duration::ZERO);
        if remaining.is_zero() {
            break;
        }
        match events.recv_timeout(remaining.min(Duration::from_millis(500))) {
            Ok(ev) => {
                let terminal = matches!(&ev,
                    CoreEvent::SessionCompleted { session: s } if s == session)
                    || matches!(&ev,
                    CoreEvent::SessionFailed { session: s, .. } if s == session)
                    || matches!(&ev,
                    CoreEvent::RunCancelled { session: s, .. } if s == session)
                    || matches!(&ev,
                    CoreEvent::AwaitingHuman { session: s, .. } if s == session);
                collected.push(ev);
                if terminal {
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
        }
    }
    collected
}

/// The crew-shaped snapshot fixture shared with the other skills tests.
#[path = "support/skills_snapshot_fixture.rs"]
mod skills_fixture;

fn spec(session_id: &str, workflow: &str) -> LaunchSpec {
    LaunchSpec {
        base_ref: None,
        project_id: None,
        problem: "Triage, then review.".into(),
        clis: vec![cli("stub")],
        entity_mode: EntityMode::Shared,
        session_id: session_id.into(),
        human_confirm: HumanConfirm::None,
        auto_deliver: false,
        repo_ref: None,
        workflow: Some(workflow.into()),
        extra_write_roots: Vec::new(),
        extra_read_roots: Vec::new(),
        project_graph: None,
    }
}

/// Two agent phases, no code, no gates — a neutral rung and an evaluator, so the directive's
/// `§<role>` is exercised on two roles.
fn phases() -> serde_json::Value {
    serde_json::json!([
        {"id": "triage", "kind": "recon", "role": "neutral"},
        {"id": "review", "kind": "review", "role": "evaluator", "depends_on": ["triage"]}
    ])
}

/// The `baseSkill` of every `unitDispatched` for `session`, by ord.
fn dispatched_base_skills(events: &[CoreEvent], session: &str) -> Vec<(u32, Option<BaseSkill>)> {
    let mut out: Vec<(u32, Option<BaseSkill>)> = events
        .iter()
        .filter_map(|e| match e {
            CoreEvent::UnitDispatched {
                session: s,
                ord,
                base_skill,
                ..
            } if s == session => Some((*ord, base_skill.clone())),
            _ => None,
        })
        .collect();
    out.sort_by_key(|(ord, _)| *ord);
    out
}

#[test]
fn the_base_skill_is_refused_at_intake_when_missing_and_rides_every_unit_when_present() {
    let base = std::fs::canonicalize(std::env::temp_dir())
        .unwrap()
        .join(format!("wicked-core-baseskill-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    // The handed generation holds `domain` only.
    let snapshot =
        skills_fixture::publish_fixture_snapshot(&base, "000001", &["wicked-garden-domain"]);
    std::env::set_var("WICKED_SKILLS_SNAPSHOT", &snapshot);
    std::env::remove_var("WICKED_WORKER_INHERIT_OPERATOR_CONFIG");
    std::env::remove_var("WICKED_BASE_SKILL_REF");

    let db = base.join("core.db").to_str().unwrap().to_string();
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let core = Core::spawn_with_engine(
        db,
        Arc::new(NumericDispatcher),
        Arc::new(RecordingOkRunner(seen.clone())),
    );
    let ev = core.subscribe();

    // ── 1. Refused at INTAKE: the def names a base skill the generation lacks. ──
    core.register_workflow(
        serde_json::json!({
            "id": "base-missing",
            "base_skill_ref": "wicked-garden-governed-worker",
            "phases": phases()
        })
        .to_string(),
    )
    .expect("register workflow");
    let err = core
        .launch_run(spec("base-missing-run", "base-missing"))
        .expect_err("a snapshot that lacks the base skill refuses the launch synchronously");
    let text = err.to_string();
    for needle in [
        "base skill \"wicked-garden-governed-worker\"",
        "base_skill_ref",
        "refused at intake, before any unit was planned",
        "does not hold the skills this run requires: wicked-garden-governed-worker",
    ] {
        assert!(text.contains(needle), "missing {needle:?} in: {text}");
    }
    // The generation judged against is named — compared by identity (canonicalized both sides).
    let named = skills_fixture::refused_snapshot_path(&text)
        .expect("the refusal names the snapshot it judged the run against");
    assert!(
        skills_fixture::names_generation(named, &snapshot),
        "the refusal names the fixture generation: `{named}` vs `{}`",
        snapshot.display()
    );
    assert!(
        seen.lock().unwrap().is_empty(),
        "no worker was ever handed a unit of the refused run"
    );

    // ── 2. Present: the same shape with a base skill the generation HOLDS runs to completion,
    //       and the directive rides every unit — on the wire and in the worker's input. ──
    core.register_workflow(
        serde_json::json!({
            "id": "base-present",
            "base_skill_ref": "wicked-garden-domain",
            "phases": phases()
        })
        .to_string(),
    )
    .expect("register workflow");
    core.launch_run(spec("base-present-run", "base-present"))
        .expect("launch");
    let events = drain_until_terminal(&ev, "base-present-run");
    assert!(
        !events.iter().any(|e| matches!(e,
            CoreEvent::SessionStarted { session, .. } if session == "base-missing-run")),
        "the refused run never started — no session was persisted, no event carries its id: {events:?}"
    );
    assert!(
        events.iter().any(
            |e| matches!(e, CoreEvent::SessionCompleted { session } if session == "base-present-run")
        ),
        "a run whose base skill exists completes: {events:?}"
    );
    let domain = |role: &str| {
        Some(BaseSkill {
            name: "wicked-garden-domain".to_string(),
            role: role.to_string(),
            // (#479, DES-L4 PR-⑥) The `stub` seat has no per-launch skills lever
            // (`SkillsLever::for_binary("stub")` is `Absent` ⇒ `SkillForm::Unloaded`), so the
            // dispatch truthfully reports the discipline as NOT handed.
            handed: false,
        })
    };
    assert_eq!(
        dispatched_base_skills(&events, "base-present-run"),
        vec![(1, domain("neutral")), (2, domain("evaluator"))],
        "every dispatch carries the base skill keyed on ITS unit's role"
    );
    {
        let handed = seen.lock().unwrap();
        let ours: Vec<_> = handed
            .iter()
            .filter(|(run, ..)| run == "base-present-run")
            .collect();
        assert_eq!(
            ours.len(),
            2,
            "both agent units reached the worker: {ours:?}"
        );
        for (_, ord, base_skill, required) in ours {
            assert_eq!(
                base_skill.as_deref(),
                Some("wicked-garden-domain"),
                "unit {ord} carries the base skill"
            );
            assert!(
                required.iter().any(|r| r == "wicked-garden-domain"),
                "unit {ord}: the base skill rides the plan-wide required set: {required:?}"
            );
        }
    }

    // ── 3. The engine-config default: a def that declares NO base skill inherits it… ──
    core.register_workflow(
        serde_json::json!({ "id": "base-inherit", "phases": phases() }).to_string(),
    )
    .expect("register workflow");
    std::env::set_var("WICKED_BASE_SKILL_REF", "wicked-garden-domain");
    core.launch_run(spec("base-inherit-run", "base-inherit"))
        .expect("launch");
    let events = drain_until_terminal(&ev, "base-inherit-run");
    assert_eq!(
        dispatched_base_skills(&events, "base-inherit-run"),
        vec![(1, domain("neutral")), (2, domain("evaluator"))],
        "the default lands on every unit of a def that declares none"
    );
    // …is refused the same way when the default names a skill the generation lacks…
    std::env::set_var("WICKED_BASE_SKILL_REF", "wicked-garden-mem");
    let err = core
        .launch_run(spec("base-inherit-missing-run", "base-inherit"))
        .expect_err("a missing default base skill is refused at intake too");
    assert!(
        err.to_string().contains("base skill \"wicked-garden-mem\"")
            && err.to_string().contains("WICKED_BASE_SKILL_REF"),
        "{err}"
    );
    // …and the def's explicit `""` opt-out wins over it.
    core.register_workflow(
        serde_json::json!({ "id": "base-off", "base_skill_ref": "", "phases": phases() })
            .to_string(),
    )
    .expect("register workflow");
    core.launch_run(spec("base-off-run", "base-off"))
        .expect("an opted-out def launches whatever the default names");
    let events = drain_until_terminal(&ev, "base-off-run");
    assert!(
        events.iter().any(
            |e| matches!(e, CoreEvent::SessionCompleted { session } if session == "base-off-run")
        ),
        "{events:?}"
    );
    assert_eq!(
        dispatched_base_skills(&events, "base-off-run"),
        vec![(1, None), (2, None)],
        "an explicit `\"\"` is no base skill: `baseSkill` is null on every dispatch"
    );
    std::env::remove_var("WICKED_BASE_SKILL_REF");

    drop(core);
    let _ = std::fs::remove_dir_all(&base);
}

/// Arm the hermetic emit spool BEFORE main (pre-main is single-threaded, so no test thread can
/// race it): engine paths under test fire coarse fire-and-forget `wicked.*` emissions, and with
/// no shared store configured those spool — which must land in a per-process temp file, never in
/// the operator's real `~/.something-wicked/wicked-apps/emit-outbox.ndjson` replay queue. Every
/// binary in this suite carries this block; `harness_hygiene.rs` fails the suite if one is missing.
///
/// SAFETY (`ctor(unsafe)`): runs before `main` on one thread and only sets one process env var
/// via the std API — no allocator setup, no threads, no panics across the FFI boundary.
#[ctor::ctor(unsafe)]
fn arm_hermetic_emit_spool() {
    wicked_apps_core::emit::hermetic_test_spool();
}
