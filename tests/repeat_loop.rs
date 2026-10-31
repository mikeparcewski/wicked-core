//! domain-graph-3 — a BOUNDED LOOP over a run's own steps, through the real engine.
//!
//! A Tool phase that declares `repeat_from` decides, each time it passes, whether the run goes back
//! for another pass (`wicked-repeat {"next":"continue"}`), stops to ask the operator
//! (`"ask"`, with its own choices), or moves on (`"done"`). This is what lets a domain-extraction
//! run keep mapping page after page until the target, instead of one page per run.
//!
//! POSIX only: the phases are shell scripts (the suite's idiom for script-driven tests).

#![cfg(unix)]

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use wicked_core::{
    Core, CoreEvent, EntityMode, HumanConfirm, HumanDecision, LaunchSpec, RepoSpec, StepInput,
    StepOutput, StepRunner, StepStatus,
};
use wicked_council::types::{Dispatcher, Vote};
use wicked_council::{AgenticCli, CouncilTask};

#[ctor::ctor(unsafe)]
fn arm_hermetic_emit_spool() {
    wicked_apps_core::emit::hermetic_test_spool();
}

struct NoCouncil;
impl Dispatcher for NoCouncil {
    fn dispatch(&self, _: &AgenticCli, _: &CouncilTask) -> Option<Vote> {
        None
    }
}

/// Tool units never reach the runner; a turn here would be a bug the test should see.
struct NoTurns;
impl StepRunner for NoTurns {
    fn run_unit(&self, i: &StepInput) -> StepOutput {
        StepOutput {
            run_id: i.run_id.clone(),
            unit_ix: i.unit_ix,
            attempt: i.attempt,
            output: "an agent turn ran in a tool-only loop".into(),
            status: StepStatus::Failed,
            usage: None,
            files: vec![],
            tools: Vec::new(),
            governed: false,
        }
    }
}

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("wicked-core-repeat-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn make_git_repo(dir: &std::path::Path) -> PathBuf {
    let repo = dir.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let git = |args: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(args)
            .output()
            .unwrap()
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "t@t"]);
    git(&["config", "user.name", "t"]);
    git(&["config", "commit.gpgsign", "false"]);
    git(&["config", "core.fsmonitor", "false"]);
    std::fs::write(repo.join("README.md"), "hello").unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "init"]);
    repo
}

/// `page` (a), `next` (decides the loop back to `page`), `finish` (c). `next` counts its runs in
/// a file outside the worktree: continue, continue, ask (choices `more` / `lower`), then done; a
/// `more` choice answers continue once. Every run logs what the engine handed it.
fn workflow(id: &str, dir: &std::path::Path) -> String {
    let log = dir.join("log.txt");
    let n = dir.join("n.txt");
    let next = format!(
        r#"n=$(cat '{n}' 2>/dev/null || echo 0); n=$((n+1)); echo $n > '{n}'
echo "next pass=$WICKED_REPEAT_PASS choice=${{WICKED_REPEAT_CHOICE:-}} n=$n" >> '{log}'
if [ "$WICKED_REPEAT_CHOICE" = more ]; then echo 'wicked-repeat {{"next":"continue","line":"granted"}}'; exit 0; fi
if [ $n -lt 3 ]; then echo "wicked-repeat {{\"next\":\"continue\",\"line\":\"Mapping x · page $n\"}}"
elif [ $n -eq 3 ]; then echo 'wicked-repeat {{"next":"ask","line":"budget reached","choices":[{{"id":"more","label":"Map more"}},{{"id":"lower","label":"Lower the target"}}],"recommended":"more"}}'
else echo 'wicked-repeat {{"next":"done","line":"target reached"}}'; fi"#,
        n = n.display(),
        log = log.display()
    );
    serde_json::json!({
        "id": id,
        "phases": [
            {"id": "page", "kind": "recon",
             "executor": {"type": "tool", "cmd": ["sh", "-c", format!("echo page >> '{}'", log.display())]}},
            {"id": "next", "kind": "recon", "depends_on": ["page"], "repeat_from": "page",
             "executor": {"type": "tool", "cmd": ["sh", "-c", next]}},
            {"id": "finish", "kind": "recon", "depends_on": ["next"],
             "executor": {"type": "tool", "cmd": ["sh", "-c", format!("echo finish >> '{}'", log.display())]}}
        ]
    })
    .to_string()
}

fn spec(session_id: &str, workflow: &str, repo_ref: String) -> LaunchSpec {
    LaunchSpec {
        base_ref: None,
        project_id: None,
        problem: format!("loop {session_id}"),
        clis: Vec::new(),
        entity_mode: EntityMode::Shared,
        session_id: session_id.into(),
        human_confirm: HumanConfirm::None,
        auto_deliver: false,
        repo_ref: Some(repo_ref),
        workflow: Some(workflow.into()),
        extra_write_roots: Vec::new(),
        extra_read_roots: Vec::new(),
        project_graph: None,
        plan: None,
        deliver_step: None,
        exclude_seats: Vec::new(),
        evidence_root: None,
        primary: None,
        reduced_assurance: false,
        power: false,
        voice: None,
        deliverables: Vec::new(),
        qe_acceptance: Default::default(),
    }
}

/// Events until the run completes, is cancelled or fails, or pauses for a human.
fn drain(events: &std::sync::mpsc::Receiver<CoreEvent>, sid: &str) -> Vec<CoreEvent> {
    let mut out = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(120);
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match events.recv_timeout(left.min(Duration::from_millis(500))) {
            Ok(ev) => {
                let stop = matches!(&ev, CoreEvent::SessionCompleted { session } if session == sid)
                    || matches!(&ev, CoreEvent::SessionFailed { session, .. } if session == sid)
                    || matches!(&ev, CoreEvent::RunCancelled { session, .. } if session == sid)
                    || matches!(&ev, CoreEvent::AwaitingHuman { session, .. } if session == sid);
                out.push(ev);
                if stop {
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    out
}

fn repeats(events: &[CoreEvent]) -> Vec<(u32, u32, u32, String)> {
    events
        .iter()
        .filter_map(|e| match e {
            CoreEvent::StepRepeated {
                ord,
                from_ord,
                pass,
                line,
                ..
            } => Some((*ord, *from_ord, *pass, line.clone())),
            _ => None,
        })
        .collect()
}

fn setup(
    name: &str,
) -> (
    Core,
    std::sync::mpsc::Receiver<CoreEvent>,
    PathBuf,
    String,
    String,
) {
    let dir = scratch(name);
    let repo = make_git_repo(&dir);
    let core = Core::spawn_with_engine(
        dir.join("estate.db").to_string_lossy().into_owned(),
        Arc::new(NoCouncil),
        Arc::new(NoTurns),
    );
    let ev = core.subscribe();
    let entry = core
        .register_repo(RepoSpec {
            name: format!("repeat {name}"),
            root_path: repo.to_string_lossy().into_owned(),
            registered_at: 0,
        })
        .expect("register the repo");
    let wf = core
        .register_workflow(workflow(&format!("loop-{name}"), &dir))
        .expect("a loop over tool phases registers");
    (core, ev, dir, entry.id, wf)
}

fn log_lines(dir: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("log.txt"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn a_loop_runs_its_range_again_asks_at_its_budget_and_goes_on_with_the_choice() {
    let (core, ev, dir, repo, wf) = setup("choice");
    let sid = "repeat-choice";
    core.launch_run(spec(sid, &wf, repo)).expect("launch");

    // Two passes go back to `page`, then the command asks.
    let events = drain(&ev, sid);
    assert_eq!(
        repeats(&events),
        vec![
            (2, 1, 1, "Mapping x · page 1".to_string()),
            (2, 1, 2, "Mapping x · page 2".to_string())
        ],
        "events: {events:?}"
    );
    let ask = events
        .iter()
        .rev()
        .find_map(|e| match e {
            CoreEvent::AwaitingHuman {
                ord,
                gate_kind,
                prompt,
                ..
            } => Some((*ord, gate_kind.clone(), prompt.clone(), e.to_json())),
            _ => None,
        })
        .expect("the ask paused the run");
    assert_eq!((ask.0, ask.1.as_str()), (2, "repeat"));
    assert!(
        ask.2.starts_with("budget reached Choose one:") && ask.2.contains("Map more (recommended)"),
        "{}",
        ask.2
    );
    assert_eq!(
        ask.3["choices"],
        serde_json::json!(["consent:more", "consent:lower", "reject"])
    );
    assert_eq!(ask.3["choiceLabels"]["consent:more"], "Map more");
    assert_eq!(ask.3["repeatLine"], "budget reached");

    // An answer the ask does not offer is refused, and the ask stays open.
    let refused = core.confirm_gate(
        sid,
        HumanDecision::ConsentChoice {
            choice: "consent:forever".into(),
        },
    );
    assert!(
        format!("{:#}", refused.unwrap_err()).contains("is not a choice of the ask on unit 2"),
        "a choice outside the ask is refused"
    );
    let refused = core.confirm_gate(sid, HumanDecision::RequestChanges { note: None });
    assert!(refused.is_err(), "a send-back does not answer an ask");
    // A resume never runs past the ask (codex r1 on #1226): the step does not run again.
    let before = log_lines(&dir).len();
    assert_eq!(
        core.resume_run(sid).expect("resume"),
        wicked_core::SessionStatus::AwaitingHuman
    );
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(log_lines(&dir).len(), before, "nothing ran on resume");

    // The choice re-runs the deciding step with it; its `continue` makes one more pass, then done.
    core.confirm_gate(
        sid,
        HumanDecision::ConsentChoice {
            choice: "consent:more".into(),
        },
    )
    .expect("the operator's choice");
    let events = drain(&ev, sid);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, CoreEvent::SessionCompleted { session } if session == sid)),
        "the run completes after the loop: {events:?}"
    );
    assert_eq!(
        repeats(&events),
        vec![(2, 1, 3, "granted".to_string())],
        "the granted pass"
    );
    let log = log_lines(&dir);
    assert_eq!(
        log,
        vec![
            "page",
            "next pass=1 choice= n=1",
            "page",
            "next pass=2 choice= n=2",
            "page",
            "next pass=3 choice= n=3",
            // the re-run after the ask: the same pass, with the operator's choice
            "next pass=3 choice=more n=4",
            "page",
            // a choice answers one ask: the next pass decides without it
            "next pass=4 choice= n=5",
            "finish",
        ],
        "what each run was handed"
    );
}

#[test]
fn stop_at_the_ask_ends_the_run_and_nothing_after_the_loop_runs() {
    let (core, ev, dir, repo, wf) = setup("stop");
    let sid = "repeat-stop";
    core.launch_run(spec(sid, &wf, repo)).expect("launch");
    let events = drain(&ev, sid);
    assert!(events
        .iter()
        .any(|e| matches!(e, CoreEvent::AwaitingHuman { gate_kind, .. } if gate_kind == "repeat")));
    core.confirm_gate(sid, HumanDecision::Reject)
        .expect("stop here");
    let events = drain(&ev, sid);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, CoreEvent::RunCancelled { session, .. } if session == sid)),
        "{events:?}"
    );
    assert!(!log_lines(&dir).iter().any(|l| l == "finish"));
}

#[test]
fn a_loop_only_a_tool_can_decide_and_only_backward_is_refused_at_registration() {
    let dir = scratch("refuse");
    let core = Core::spawn_with_engine(
        dir.join("estate.db").to_string_lossy().into_owned(),
        Arc::new(NoCouncil),
        Arc::new(NoTurns),
    );
    let tool = serde_json::json!({"type": "tool", "cmd": ["true"]});
    let cases = [
        (
            serde_json::json!([{"id": "a", "kind": "recon"},
                               {"id": "b", "kind": "recon", "repeat_from": "a"}]),
            "only a Tool phase decides a loop",
        ),
        (
            serde_json::json!([{"id": "a", "kind": "recon", "executor": tool, "repeat_from": "b"},
                               {"id": "b", "kind": "recon", "executor": tool}]),
            "b is not declared before it",
        ),
        (
            serde_json::json!([{"id": "deliver", "kind": "build", "executor": tool},
                               {"id": "b", "kind": "recon", "executor": tool, "repeat_from": "deliver"}]),
            "the range holds the deliver phase deliver",
        ),
        (
            serde_json::json!([{"id": "a", "kind": "recon", "executor": tool},
                               {"id": "b", "kind": "recon", "executor": tool,
                                "gate": "consent_before"},
                               {"id": "c", "kind": "recon", "executor": tool, "repeat_from": "a"}]),
            "the range holds b, which asks consent first",
        ),
        (
            serde_json::json!([{"id": "a", "kind": "recon", "executor": tool},
                               {"id": "b", "kind": "recon", "executor": tool, "repeat_from": "a",
                                "gate": {"human_confirm": {"unconditional": true}}}]),
            "it takes no human gate",
        ),
        (
            serde_json::json!([{"id": "a", "kind": "recon", "executor": tool, "owner": "team"},
                               {"id": "b", "kind": "recon", "executor": tool, "repeat_from": "a"}]),
            "the range holds a, a team member's step",
        ),
    ];
    for (i, (phases, why)) in cases.into_iter().enumerate() {
        let err = core
            .register_workflow(
                serde_json::json!({"id": format!("bad-{i}"), "phases": phases}).to_string(),
            )
            .expect_err(why);
        assert!(format!("{err:#}").contains(why), "{why}: {err:#}");
    }
}
