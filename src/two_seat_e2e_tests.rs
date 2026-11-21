//! (crew#972 decision 2 on core#1543) THE TWO-SEAT JOURNEY, through the real actor: a repo-bound
//! `bug` run with only Claude Code and Codex. The fix is claude's (codex judges it); the verify
//! step runs on codex, and its pinned judge must be neither codex (its author) nor claude (the
//! validator's author). Before: that gate held `judge_unavailable` and the run parked. Now: claude
//! judges it, disclosed, because the seat gateway OBSERVED the verify launch calling codex's
//! provider and the judge's launch calling claude's. When codex's launch is observed calling
//! claude's provider, the verdict is discarded and the gate holds — never a self-grade.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::{
    Core, CoreEvent, HumanConfirm, LaunchSpec, RepoSpec, StepInput, StepOutput, StepRunner,
    StepStatus,
};
use wicked_council::types::{Category, Confidence, Dispatcher, InputMode, Vote};
use wicked_council::{AgenticCli, CouncilTask};

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

/// Each seat's launches record the provider the seat gateway saw them call (`program` per seat key).
struct Seat {
    program: std::collections::HashMap<&'static str, &'static str>,
    /// (seat, phase) launches observed calling another provider than the seat's usual one.
    at_phase: Option<(&'static str, &'static str, &'static str)>,
    judged_on: Arc<Mutex<Vec<String>>>,
}
impl StepRunner for Seat {
    fn run_unit(&self, input: &StepInput) -> StepOutput {
        let seat = input.unit.assigned_cli.clone().unwrap_or_default();
        let phase = input.unit.phase_id().unwrap_or("").to_string();
        let provider = match self.at_phase {
            Some((s, p, provider)) if s == seat && p == phase => Some(provider),
            _ => self
                .program
                .get(crate::team::runner::seat_key(&seat))
                .copied(),
        };
        crate::observed_provider::record_receipt(&seat, provider.map(|p| vec![p.to_string()]));
        let mut output = format!("did: {phase}");
        if phase == "verify" {
            output.push_str("\nthe fix is right and src/app.ts reads `fixed`\nVERDICT: PASS");
        }
        if input.unit.session_id == "validator" {
            self.judged_on.lock().unwrap().push(seat.clone());
            output = "PASS\nthe fix divides the percentage by 100\nPASS".to_string();
        } else if let (Some(wd), "fix") = (&input.workdir, phase.as_str()) {
            std::fs::write(wd.join("src/app.ts"), "fixed\n").unwrap();
        }
        StepOutput {
            run_id: input.run_id.clone(),
            unit_ix: input.unit_ix,
            attempt: input.attempt,
            output,
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
        binary: key.into(),
        headless_invocation: format!("{key} {{PROMPT}}"),
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

fn git(repo: &Path, args: &[&str]) {
    // spawn-audit: test-only — a git fixture building the layout under test.
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .expect("git runs");
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("wcore-two-seat-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn make_repo(name: &str) -> PathBuf {
    let repo = tmp(name);
    std::fs::create_dir_all(repo.join("src")).unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.email", "t@example.invalid"]);
    git(&repo, &["config", "user.name", "t"]);
    git(&repo, &["config", "commit.gpgsign", "false"]);
    git(&repo, &["config", "core.autocrlf", "false"]);
    std::fs::write(repo.join("README.md"), "tiny tip\n").unwrap();
    std::fs::write(repo.join("src/app.ts"), "buggy\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "init"]);
    repo
}

fn bug_run(session_id: &str, repo_ref: &str) -> LaunchSpec {
    LaunchSpec {
        base_ref: None,
        project_id: None,
        problem: "perPerson(120, 20, 3) returns 840 instead of 48: fix it".into(),
        clis: vec![cli("claude"), cli("codex")],
        entity_mode: crate::EntityMode::Shared,
        session_id: session_id.into(),
        human_confirm: HumanConfirm::None,
        auto_deliver: false,
        repo_ref: Some(repo_ref.to_string()),
        workflow: Some("bug-def".into()),
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
        qe_acceptance: crate::assurance::QeOverride::Skip(
            "the two-seat journey test does not exercise QE acceptance".into(),
        ),
    }
}

const WAIT_BUDGET: Duration = Duration::from_secs(240);

fn until_ended(
    events: &std::sync::mpsc::Receiver<CoreEvent>,
    sid: &str,
) -> Result<Vec<CoreEvent>, String> {
    let start = Instant::now();
    let mut out = Vec::new();
    while start.elapsed() < WAIT_BUDGET {
        match events.recv_timeout(Duration::from_millis(250)) {
            Ok(ev) => {
                let done = matches!(&ev, CoreEvent::SessionCompleted { session, .. } if session == sid)
                    || matches!(&ev, CoreEvent::SessionFailed { session, .. } if session == sid)
                    || matches!(&ev, CoreEvent::AwaitingHuman { session, .. } if session == sid);
                out.push(ev);
                if done {
                    return Ok(out);
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    Err(format!(
        "run {sid} did not stop within {WAIT_BUDGET:?}; saw {} events",
        out.len()
    ))
}

fn escalations(evs: &[CoreEvent], sid: &str) -> Vec<(String, String)> {
    evs.iter()
        .filter_map(|e| match e {
            CoreEvent::GateEscalated {
                session,
                condition,
                verdict_summary,
                ..
            } if session == sid => Some((condition.clone(), verdict_summary.clone())),
            _ => None,
        })
        .collect()
}

/// macOS and Linux only (the module is): the repo-checks floor the verify gate runs has no OS write
/// boundary to arm on Windows, where a code-verifying phase fails closed at its gate
/// (`floor_failed`) by design, whatever its judge — not the journey this test proves.
#[test]
fn two_seats_complete_a_bug_run_and_a_self_grade_never_passes() {
    let _env = crate::test_env::ENV_LOCK
        .write()
        .unwrap_or_else(|p| p.into_inner());
    // On the host floor codex is kept for a repo-bound run only when the operator says so.
    // Restored on drop, so a failed assertion never leaks it into the next test.
    struct Restore(Option<std::ffi::OsString>);
    impl Drop for Restore {
        fn drop(&mut self) {
            match self.0.take() {
                Some(v) => std::env::set_var("WICKED_CODEX_WORKTREE_OK", v),
                None => std::env::remove_var("WICKED_CODEX_WORKTREE_OK"),
            }
        }
    }
    let _restore = Restore(std::env::var_os("WICKED_CODEX_WORKTREE_OK"));
    std::env::set_var("WICKED_CODEX_WORKTREE_OK", "1");
    let repo = make_repo("repo");
    let db_dir = tmp("db");
    let launch = |programs: [(&'static str, &'static str); 2],
                  at_phase: Option<(&'static str, &'static str, &'static str)>,
                  sid: &'static str| {
        let judged_on = Arc::new(Mutex::new(Vec::new()));
        let core = Core::spawn_with_engine(
            db_dir
                .join(format!("{sid}.db"))
                .to_str()
                .unwrap()
                .to_string(),
            Arc::new(StubDispatcher),
            Arc::new(Seat {
                program: programs.into_iter().collect(),
                at_phase,
                judged_on: judged_on.clone(),
            }),
        );
        core.register_workflow(include_str!("../tests/fixtures/workflows/bug-def.json"))
            .expect("the bug def stand-in registers");
        let entry = core
            .register_repo(RepoSpec {
                name: format!("tiny-tip-{sid}"),
                root_path: repo.to_str().unwrap().into(),
                registered_at: 1,
            })
            .expect("register");
        let events = core.subscribe();
        core.launch_run(bug_run(sid, &entry.id)).expect("launch");
        let evs = until_ended(&events, sid).expect("the run stops somewhere");
        let judged = judged_on.lock().unwrap().clone();
        (evs, judged)
    };

    let gates = |evs: &[CoreEvent], sid: &str| -> Vec<(u32, Option<String>, bool, String)> {
        evs.iter()
            .filter_map(|e| match e {
                CoreEvent::GateEvaluated {
                    session,
                    ord,
                    judge_cli,
                    combined,
                    assurance,
                    ..
                } if session == sid => Some((
                    *ord,
                    judge_cli.clone(),
                    *combined,
                    assurance
                        .judge_fallbacks
                        .iter()
                        .map(|f| f.label.clone())
                        .collect::<Vec<_>>()
                        .join(" | "),
                )),
                _ => None,
            })
            .collect()
    };

    // Two seats, two providers: the run COMPLETES on its own. Codex judges claude's fix; claude
    // judges codex's verify step — the gate that held before — disclosed as verified.
    let claude_codex = [
        ("claude", "api.anthropic.com:443"),
        ("codex", "chatgpt.com:443"),
    ];
    let (evs, judged) = launch(claude_codex, None, "two-seats");
    let held = escalations(&evs, "two-seats");
    assert!(held.is_empty(), "no gate holds: {held:?}");
    assert!(
        matches!(evs.last(), Some(CoreEvent::SessionCompleted { session, .. }) if session == "two-seats"),
        "the two-seat run completes on its own: {:?}",
        evs.last()
    );
    let g = gates(&evs, "two-seats");
    assert!(
        g.iter()
            .any(|(_, j, pass, _)| j.as_deref() == Some("codex") && *pass),
        "codex judged and passed claude's fix: {g:?}"
    );
    assert!(
        g.iter()
            .any(|(_, j, pass, label)| j.as_deref() == Some("claude")
                && *pass
                && label.contains("pinned validator's author")
                && label.contains("verified by observation")),
        "claude judged and passed codex's verify, disclosed and verified: {g:?}"
    );
    assert!(judged.contains(&"claude".to_string()) && judged.contains(&"codex".to_string()));

    // The attacker's roster: the fix is independent, but codex's VERIFY launch is observed calling
    // claude's provider. (core#1540) That launch is the work's author by identity, so the verify
    // gate holds as a same-seat evaluator, naming the provider; its judge (claude, re-admitted by
    // name) is still asked, and its approval is passed over as not independent.
    let (evs, judged) = launch(
        claude_codex,
        Some(("codex", "verify", "api.anthropic.com:443")),
        "alias",
    );
    let held = escalations(&evs, "alias");
    let verify_ord = gates(&evs, "alias")
        .iter()
        .find(|(_, j, _, _)| j.as_deref() == Some("codex"))
        .map(|(o, ..)| o + 1);
    assert!(
        held.iter().any(|(c, s)| c == "dead_seat"
            && s.contains("ran on `codex`, observed calling `api.anthropic.com:443`")),
        "the self-grade is held: {held:?}"
    );
    assert!(
        evs.iter().any(
            |e| matches!(e, CoreEvent::GateEscalated { session, ord, condition, .. }
            if session == "alias" && condition == "dead_seat" && Some(*ord) == verify_ord)
        ),
        "it is the VERIFY gate that holds (after the fix gate passed): {held:?}"
    );
    assert!(
        judged.contains(&"claude".to_string()),
        "claude was asked: {judged:?}"
    );
}
