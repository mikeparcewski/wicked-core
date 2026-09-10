//! core#396 / codex round 6 (item 6): the run-wide skills EXISTENCE admission runs before the
//! FIRST unit of ANY kind — a TOOL-COMMAND first unit must not execute when a later agent unit's
//! required skill is missing from the snapshot.
//!
//! Driven through a REAL `Core` and a REAL actor (`dispatch_unit` is where the tool-command path
//! bypasses both worker runners): a two-phase workflow whose first phase is a Tool executor that
//! writes a marker file and whose second phase is an agent unit with a `skill_ref`. With
//! `WICKED_SKILLS_SNAPSHOT` pointing at a fixture generation that LACKS that skill, the run must
//! fail at unit 1 with the skills refusal, the marker must never appear (the command never ran),
//! and the agent runner must never be called. With a generation that HOLDS it, the same workflow's
//! tool command runs (marker present) and the run completes — the admission is a gate, not a wall.
//!
//! One test in its own binary: it sets a process-global variable, and integration tests run one
//! process per file.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use wicked_core::{
    Core, CoreEvent, EntityMode, HumanConfirm, LaunchSpec, StepInput, StepOutput, StepRunner,
    StepStatus,
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

/// Completes every AGENT unit with Ok — and records that it was asked to. A tool-command unit
/// never reaches a runner, so this being called means the run advanced PAST the tool phase.
struct RecordingOkRunner(Arc<AtomicBool>);
impl StepRunner for RecordingOkRunner {
    fn run_unit(&self, i: &StepInput) -> StepOutput {
        self.0.store(true, Ordering::SeqCst);
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
                    CoreEvent::RunCancelled { session: s } if s == session)
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

/// The crew-shaped snapshot fixture shared with `domain_extraction_e2e.rs`.
#[path = "support/skills_snapshot_fixture.rs"]
mod skills_fixture;

/// The platform's shell writing `ran` into `marker` — the observable that the tool command
/// EXECUTED. Absolute path, so the unit's working directory is irrelevant; spelled for the shell
/// (no Windows `\\?\` verbatim prefix — `cmd.exe` rejects it; review pass 9).
fn marker_cmd(marker: &std::path::Path) -> Vec<String> {
    let spelled = skills_fixture::shell_spelling(marker);
    if cfg!(windows) {
        // No inner quotes: the whole `/c` argument is quoted by the process spawner because it
        // holds spaces, and cmd.exe strips only the OUTER pair — inner `\"…\"` would stay in the
        // redirect target ("The filename, directory name, or volume label syntax is incorrect",
        // Windows CI on 4ead6aa). The runner's temp path carries no spaces.
        vec!["cmd".into(), "/c".into(), format!("echo ran > {spelled}")]
    } else {
        vec![
            "sh".into(),
            "-c".into(),
            format!("echo ran > \"{spelled}\""),
        ]
    }
}

fn spec(session_id: &str, workflow: &str) -> LaunchSpec {
    LaunchSpec {
        project_id: None,
        problem: "Index, then extract.".into(),
        clis: vec![cli("stub")],
        entity_mode: EntityMode::Shared,
        session_id: session_id.into(),
        human_confirm: HumanConfirm::None,
        repo_ref: None,
        workflow: Some(workflow.into()),
        extra_write_roots: Vec::new(),
        extra_read_roots: Vec::new(),
        project_graph: None,
    }
}

#[test]
fn a_tool_command_first_unit_does_not_execute_when_a_later_unit_s_skill_is_missing() {
    let base = std::fs::canonicalize(std::env::temp_dir())
        .unwrap()
        .join(format!("wicked-core-planadm-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    // The handed generation holds `domain` only; the run below also names `mem`.
    let snapshot =
        skills_fixture::publish_fixture_snapshot(&base, "000001", &["wicked-garden-domain"]);
    std::env::set_var("WICKED_SKILLS_SNAPSHOT", &snapshot);
    std::env::remove_var("WICKED_WORKER_INHERIT_OPERATOR_CONFIG");

    let db = base.join("core.db").to_str().unwrap().to_string();
    let agent_ran = Arc::new(AtomicBool::new(false));
    let core = Core::spawn_with_engine(
        db,
        Arc::new(NumericDispatcher),
        Arc::new(RecordingOkRunner(agent_ran.clone())),
    );
    let ev = core.subscribe();

    // ── Refused: the tool command must NOT run when a later unit's skill is missing. ──
    let marker = base.join("ran-missing.txt");
    core.register_workflow(
        serde_json::json!({
            "id": "tool-then-missing-skill",
            "phases": [
                {"id": "index", "kind": "build",
                 "executor": {"type": "tool", "cmd": marker_cmd(&marker)}},
                {"id": "extract", "kind": "build", "skill_ref": "wicked-garden-mem"}
            ]
        })
        .to_string(),
    )
    .expect("register workflow");
    core.launch_run(spec("plan-missing", "tool-then-missing-skill"))
        .expect("launch");
    let events = drain_until_terminal(&ev, "plan-missing");
    assert!(
        events.iter().any(
            |e| matches!(e, CoreEvent::SessionFailed { session, .. } if session == "plan-missing")
        ),
        "the run fails at its first unit: {events:?}"
    );
    let refusal = events
        .iter()
        .find_map(|e| match e {
            CoreEvent::StepFailed {
                session, detail, ..
            } if session == "plan-missing" => Some(detail.clone()),
            _ => None,
        })
        .expect("a StepFailed carries the refusal");
    assert!(
        refusal.contains("skills snapshot refused the launch")
            && refusal.contains("wicked-garden-mem"),
        "the refusal names the missing skill: {refusal}"
    );
    // The generation judged against is compared by IDENTITY (both sides canonicalized), not by
    // spelling: the engine reports the canonical real path without the Windows `\\?\` prefix,
    // while the fixture's own spelling keeps it (review pass 8).
    let named = skills_fixture::refused_snapshot_path(&refusal)
        .expect("the refusal names the snapshot it judged the run against");
    assert!(
        skills_fixture::names_generation(named, &snapshot),
        "the refusal names the fixture generation: `{named}` vs `{}`",
        snapshot.display()
    );
    assert!(
        !marker.exists(),
        "the tool command executed before the missing skill was discovered: {}",
        marker.display()
    );
    assert!(
        !agent_ran.load(Ordering::SeqCst),
        "no agent unit ran — the run was refused at unit 1"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, CoreEvent::SkillsSnapshotHanded { session, .. } if session == "plan-missing")),
        "a refused plan reports no generation: {events:?}"
    );

    // ── Admitted: the same shape with a skill the generation HOLDS runs the command. ──
    let marker_ok = base.join("ran-present.txt");
    core.register_workflow(
        serde_json::json!({
            "id": "tool-then-present-skill",
            "phases": [
                {"id": "index", "kind": "build",
                 "executor": {"type": "tool", "cmd": marker_cmd(&marker_ok)}},
                {"id": "extract", "kind": "build", "skill_ref": "wicked-garden-domain"}
            ]
        })
        .to_string(),
    )
    .expect("register workflow");
    core.launch_run(spec("plan-present", "tool-then-present-skill"))
        .expect("launch");
    let events = drain_until_terminal(&ev, "plan-present");
    assert!(
        events.iter().any(
            |e| matches!(e, CoreEvent::SessionCompleted { session } if session == "plan-present")
        ),
        "a run whose skills all exist completes: {events:?}"
    );
    assert!(
        marker_ok.exists(),
        "the admitted tool command ran: {}",
        marker_ok.display()
    );
    assert!(
        agent_ran.load(Ordering::SeqCst),
        "the agent unit ran after the admitted tool command"
    );
    // The generation the run was judged against is reported like a handoff (`path: "tool_cmd"`),
    // so crew's ledger pins it for this session from the first unit on.
    let handed: Vec<(String, Option<String>, String, String)> = events
        .iter()
        .filter_map(|e| match e {
            CoreEvent::SkillsSnapshotHanded {
                session,
                path,
                cli,
                gen,
                root,
                ..
            } if session == "plan-present" => {
                Some((path.clone(), gen.clone(), cli.clone(), root.clone()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        handed.len(),
        1,
        "one report per tool-command unit: {handed:?}"
    );
    let (path, gen, cli, root) = &handed[0];
    assert_eq!(
        (path.as_str(), gen.as_deref(), cli.as_str()),
        ("tool_cmd", Some("000001"), "tool")
    );
    assert!(
        skills_fixture::names_generation(root, &snapshot),
        "the admitted plan reports the verified generation it was judged against: `{root}` vs `{}`",
        snapshot.display()
    );

    std::env::remove_var("WICKED_SKILLS_SNAPSHOT");
    let _ = std::fs::remove_dir_all(&base);
}

// ── Test-harness hygiene (core#311) — not a test ─────────────────────────────────────────────
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
