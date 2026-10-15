//! (operator ruling 2026-10-14) The fallback judge, through the real actor.
//!
//! Crew run 59232ebc (verify-e2e round 3): a `bug` run seated claude + codex + pi on a repository.
//! codex is benched for worktree-bound runs on that host (its forced approval policy has no
//! approver), and every other seat was a party to the work, so every gate escalated
//! `judge_unavailable` and the operator approved each one by hand. This drives the same shape —
//! a repo-bound `bug` run, codex benched by the engine's own worktree bench, the other run seats
//! excluded from judging — first with the fallback OFF (`WICKED_JUDGE_FALLBACK=off`: the
//! pre-ruling behaviour, reproduced), then with it on: Antigravity judges in codex's place, the
//! run completes, and every gate's receipt discloses `judge: agy (fallback; codex benched: …)`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use wicked_core::{
    Core, CoreEvent, HumanConfirm, LaunchSpec, RepoSpec, SessionStatus, StepInput, StepOutput,
    StepRunner, StepStatus,
};
use wicked_council::types::{Category, Confidence, Dispatcher, InputMode, Vote};
use wicked_council::{AgenticCli, CouncilTask};

/// Route every fire-and-forget `wicked.*` emission this binary triggers to a per-process temp
/// spool instead of the operator's real replay queue (core#311); `harness_hygiene.rs` requires it.
///
/// SAFETY (`ctor(unsafe)`): runs before `main` on one thread and only sets one process env var
/// via the std API — no allocator setup, no threads, no panics across the FFI boundary.
#[ctor::ctor(unsafe)]
fn arm_hermetic_emit_spool() {
    wicked_apps_core::emit::hermetic_test_spool();
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

/// The fix phase writes the fix; judge sessions record their seat and answer PASS.
struct Seat {
    judged_on: Arc<Mutex<Vec<String>>>,
}
impl StepRunner for Seat {
    fn run_unit(&self, input: &StepInput) -> StepOutput {
        let phase = input.unit.phase_id().unwrap_or("").to_string();
        let mut output = format!("did: {phase}");
        if phase == "verify" {
            // The evaluator's contract: its own verdict line.
            output.push_str("\nthe fix is right and src/app.ts reads `fixed`\nVERDICT: PASS");
        }
        if input.unit.session_id == "validator" {
            self.judged_on
                .lock()
                .unwrap()
                .push(input.unit.assigned_cli.clone().unwrap_or_default());
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

fn git(repo: &Path, args: &[&str]) {
    // spawn-audit: test-only — a git fixture building the layout under test.
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .expect("git runs");
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "wicked-core-judge-fallback-it-{name}-{}",
        std::process::id()
    ));
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

/// 59232ebc's launch: claude + codex + pi on a repository; claude and pi excluded from judging
/// (there, claude created the fix and pi checked it — every seat but codex was a party).
fn bug_run(session_id: &str, repo_ref: &str) -> LaunchSpec {
    LaunchSpec {
        base_ref: None,
        project_id: None,
        problem: "perPerson(120, 20, 3) returns 840 instead of 48: fix it".into(),
        clis: vec![cli("claude"), cli("codex"), cli("pi")],
        entity_mode: wicked_core::EntityMode::Shared,
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
        exclude_seats: vec!["claude".into(), "pi".into()],
        evidence_root: None,
        primary: None,
        reduced_assurance: false,
        deliverables: Vec::new(),
        qe_acceptance: wicked_core::assurance::QeOverride::Skip(
            "the judge-fallback test does not exercise QE acceptance".into(),
        ),
    }
}

const WAIT_BUDGET: Duration = Duration::from_secs(240);

/// Collect events until one satisfies `done` (returned WITH the match) or the budget runs out.
fn wait_for_event(
    events: &std::sync::mpsc::Receiver<CoreEvent>,
    done: impl Fn(&CoreEvent) -> bool,
) -> Result<Vec<CoreEvent>, String> {
    let start = Instant::now();
    let mut out = Vec::new();
    while start.elapsed() < WAIT_BUDGET {
        match events.recv_timeout(Duration::from_millis(250)) {
            Ok(ev) => {
                let hit = done(&ev);
                out.push(ev);
                if hit {
                    return Ok(out);
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    Err(format!(
        "the awaited event did not arrive within {WAIT_BUDGET:?} (a timeout, not an outcome); \
         saw {} events",
        out.len()
    ))
}

fn ended(sid: &'static str) -> impl Fn(&CoreEvent) -> bool {
    move |ev| {
        matches!(ev, CoreEvent::SessionCompleted { session, .. } if session == sid)
            || matches!(ev, CoreEvent::SessionFailed { session, .. } if session == sid)
            || matches!(ev, CoreEvent::AwaitingHuman { session, .. } if session == sid)
    }
}

fn status_of(core: &Core, sid: &str) -> Option<SessionStatus> {
    core.sessions_detail()
        .ok()?
        .into_iter()
        .find(|v| v.session.id == sid)
        .map(|v| v.session.status)
}

#[test]
fn run_59232ebc_judge_unavailable_today_and_a_disclosed_fallback_judge_after() {
    // This host's codex-runs-commands opt-out would unbench codex: the test pins the bench.
    std::env::remove_var("WICKED_CODEX_WORKTREE_OK");
    let repo = make_repo("repo");
    let db_dir = tmp("db");
    let judged_on = Arc::new(Mutex::new(Vec::new()));
    let core = Core::spawn_with_engine(
        db_dir.join("estate.db").to_str().unwrap().to_string(),
        Arc::new(StubDispatcher),
        Arc::new(Seat {
            judged_on: judged_on.clone(),
        }),
    );
    core.register_workflow(include_str!("fixtures/workflows/bug-def.json"))
        .expect("the bug def stand-in registers");
    let entry = core
        .register_repo(RepoSpec {
            name: "tiny-tip".into(),
            root_path: repo.to_str().unwrap().into(),
            registered_at: 1,
        })
        .expect("register");
    let events = core.subscribe();

    // TODAY (the fallback off): the fix gate's only distinct judge is benched — the hold.
    std::env::set_var("WICKED_JUDGE_FALLBACK", "off");
    core.launch_run(bug_run("r-today", &entry.id))
        .expect("launch");
    let evs = wait_for_event(&events, ended("r-today")).expect("the run stops somewhere");
    let escalated = evs.iter().find_map(|e| match e {
        CoreEvent::GateEscalated {
            session,
            condition,
            verdict_summary,
            ..
        } if session == "r-today" => Some((condition.clone(), verdict_summary.clone())),
        _ => None,
    });
    let (condition, summary) = escalated.unwrap_or_else(|| panic!("no escalation: {evs:?}"));
    assert_eq!(condition, "judge_unavailable", "{summary}");
    assert!(
        summary.contains("codex (benched for this run: codex cannot work in this run's worktree"),
        "the hold names the bench in its own words: {summary}"
    );
    assert_eq!(
        status_of(&core, "r-today"),
        Some(SessionStatus::AwaitingHuman)
    );
    assert!(
        judged_on.lock().unwrap().is_empty(),
        "no judge was asked: {:?}",
        judged_on.lock().unwrap()
    );

    // AFTER (the default fallback seats): Antigravity judges in codex's place, disclosed.
    std::env::remove_var("WICKED_JUDGE_FALLBACK");
    core.launch_run(bug_run("r-after", &entry.id))
        .expect("launch");
    let evs = wait_for_event(&events, ended("r-after")).expect("the run stops somewhere");
    let gates: Vec<(
        u32,
        Option<String>,
        wicked_core::assurance::AssuranceReceipt,
    )> = evs
        .iter()
        .filter_map(|e| match e {
            CoreEvent::GateEvaluated {
                session,
                ord,
                judge_cli,
                assurance,
                ..
            } if session == "r-after" && judge_cli.is_some() => {
                Some((*ord, judge_cli.clone(), (**assurance).clone()))
            }
            _ => None,
        })
        .collect();
    let escalations: Vec<(String, String)> = evs
        .iter()
        .filter_map(|e| match e {
            CoreEvent::GateEscalated {
                session,
                condition,
                verdict_summary,
                ..
            } if session == "r-after" => Some((condition.clone(), verdict_summary.clone())),
            _ => None,
        })
        .collect();
    assert!(
        !escalations.iter().any(|(c, _)| c == "judge_unavailable"),
        "no gate holds for a judge once a fallback judge can rule: {escalations:?}"
    );
    if escalations.is_empty() {
        assert!(
            matches!(evs.last(), Some(CoreEvent::SessionCompleted { .. })),
            "the run completes on its own: {:?}",
            evs.last()
        );
        assert!(
            gates.len() >= 2,
            "the fix and verify gates were judged: {gates:?}"
        );
    } else {
        // The one other way this run stops: a host with no OS write boundary, where the repo
        // checks floor refuses (fail-closed) — the fix gate was still judged, by the fallback.
        eprintln!("r-after stopped at a non-judge escalation: {escalations:?}");
        assert!(!gates.is_empty(), "the fix gate was judged: {gates:?}");
    }
    for (ord, judge, receipt) in &gates {
        assert_eq!(judge.as_deref(), Some("agy"), "gate {ord}");
        assert_eq!(receipt.judge.as_deref(), Some("agy"), "gate {ord}");
        let fb = receipt
            .judge_fallbacks
            .first()
            .unwrap_or_else(|| panic!("gate {ord} discloses the fallback: {receipt:?}"));
        assert_eq!(
            fb.label,
            "judge: agy (fallback; codex benched: codex cannot work in this run's worktree on \
             this host)",
            "gate {ord}"
        );
        assert_eq!(fb.preferred, vec!["codex".to_string()]);
        // The receipt's wire shape: camelCase, beside the other assurance fields.
        let wire = serde_json::to_value(receipt).unwrap();
        assert_eq!(wire["judgeFallbacks"][0]["seat"], "agy", "{wire}");
    }
    let judged = judged_on.lock().unwrap().clone();
    assert!(
        !judged.is_empty() && judged.iter().all(|s| s == "agy"),
        "evaluator ≠ creator holds: only the fallback seat judged: {judged:?}"
    );
    let _ = std::fs::remove_dir_all(&repo);
    let _ = std::fs::remove_dir_all(&db_dir);
}
