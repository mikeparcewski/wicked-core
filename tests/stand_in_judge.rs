//! DES-STANDIN-001 §3.6, §3.10, T-R5 and T-R12 (coord F1 and F2 on #1854): the stand-in JUDGE
//! through the REAL engine on a repository (`Core::launch_run` → the build unit → the judge →
//! the fold → the gate → `grant_stand_ins` → the re-judge).
//!
//! The roster is one signed-in helper (`claude`) on a repo-bound fix. Every real judge seat this
//! host could reach refuses (signed out); a stand-in judge answers `verdict`. On main such a run
//! lets the creator judge itself, disclosed. A stand-in-aware run never does: with consent the
//! judge stands in (the default judge as well as a pinned one); without it the gate holds at
//! `stand_in_needed`, and a grant re-runs only the judge, never the build.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use wicked_core::{
    Core, CoreEvent, HumanConfirm, HumanDecision, LaunchSpec, LaunchStandIn, RepoSpec,
    StandInConsent, StepInput, StepOutput, StepRunner, StepStatus,
};
use wicked_council::types::{Category, Confidence, Dispatcher, InputMode, Vote};
use wicked_council::{AgenticCli, CouncilTask};

/// Pre-main: arm the hermetic emit spool (core#311). SAFETY (`ctor(unsafe)`): runs before `main`
/// on one thread and only sets process env vars via the std API.
#[ctor::ctor(unsafe)]
fn arm_hermetic_emit_spool() {
    wicked_apps_core::emit::hermetic_test_spool();
    // Every test in this binary reads the run's own seats only: no registry fallback judge.
    std::env::set_var("WICKED_JUDGE_FALLBACK", "off");
}

struct NoBallots;
impl Dispatcher for NoBallots {
    fn dispatch(&self, _c: &AgenticCli, _: &CouncilTask) -> Option<Vote> {
        None
    }
}

/// The fix writes the fix. A judge on a stand-in answers `verdict`; every real judge seat
/// refuses as a signed-out seat does. Records every launch as (session, seat).
struct Seat {
    verdict: &'static str,
    launches: Arc<Mutex<Vec<(String, String)>>>,
}
impl StepRunner for Seat {
    fn run_unit(&self, input: &StepInput) -> StepOutput {
        let seat = input.unit.assigned_cli.clone().unwrap_or_default();
        let phase = input.unit.phase_id().unwrap_or("").to_string();
        self.launches
            .lock()
            .unwrap()
            .push((input.unit.session_id.clone(), seat.clone()));
        let output = if input.unit.session_id == "validator" {
            if seat.contains('~') {
                format!(
                    "{}\nthe fix divides the percentage by 100\n{}",
                    self.verdict, self.verdict
                )
            } else {
                "Not logged in · Please run /login".to_string()
            }
        } else {
            if let (Some(wd), "fix") = (&input.workdir, phase.as_str()) {
                std::fs::write(wd.join("src/app.ts"), "fixed\n").unwrap();
            }
            format!("did: {phase}")
        };
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
        display_name: "Claude Code".into(),
        binary: "unused".into(),
        headless_invocation: format!("{key}-bin -p {{PROMPT}}"),
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
        "wicked-core-standin-judge-{name}-{}",
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

/// One fix phase on the repository: the default judge (no pin), or the pinned evidence floor
/// (`bug`'s own fix phase, `tests/fixtures/workflows/bug-def.json`).
fn fix_def(id: &str, pinned: bool) -> String {
    // An unpinned phase that executes code is refused at registration (its gate would check
    // nothing): the default-judge shape is a fix that changes the tree without declaring it.
    let (pin, executes) = if pinned {
        (r#""e2e7af1db9e48454""#, true)
    } else {
        ("null", false)
    };
    format!(
        r#"{{"id":"{id}","phases":[{{"id":"fix","kind":"build","gate_type":"execution",
        "gate":"auto","executes_code":{executes},"verified_evidence":false,"required_deliverables":[],
        "depends_on":[],"role":"creator","skill_ref":null,"allowed_skills":[],
        "validator_pin":{pin},"executor":{{"type":"agent"}}}}]}}"#
    )
}

fn consent() -> StandInConsent {
    StandInConsent {
        scope: "job".into(),
        by: "operator".into(),
        at: 1,
        via: "launch".into(),
    }
}

fn spec(sid: &str, repo_ref: &str, def: &str, consent: Option<StandInConsent>) -> LaunchSpec {
    LaunchSpec {
        base_ref: None,
        project_id: None,
        problem: "perPerson(120, 20, 3) returns 840 instead of 48: fix it".into(),
        clis: vec![cli("claude")],
        entity_mode: wicked_core::EntityMode::Shared,
        session_id: sid.into(),
        human_confirm: HumanConfirm::None,
        auto_deliver: false,
        repo_ref: Some(repo_ref.to_string()),
        workflow: Some(def.into()),
        extra_write_roots: Vec::new(),
        extra_read_roots: Vec::new(),
        project_graph: None,
        plan: None,
        deliver_step: None,
        exclude_seats: Vec::new(),
        evidence_root: None,
        primary: None,
        reduced_assurance: false,
        stand_in: LaunchStandIn {
            aware: true,
            parties: Vec::new(),
            consent,
        },
        power: false,
        voice: None,
        deliverables: Vec::new(),
        qe_acceptance: Default::default(),
    }
}

struct Rig {
    core: Core,
    repo_ref: String,
    launches: Arc<Mutex<Vec<(String, String)>>>,
    dirs: Vec<PathBuf>,
}

impl Drop for Rig {
    fn drop(&mut self) {
        for d in &self.dirs {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}

fn rig(name: &str, verdict: &'static str, pinned: bool) -> Rig {
    let repo = make_repo(&format!("{name}-repo"));
    let db = tmp(&format!("{name}-db"));
    let launches = Arc::new(Mutex::new(Vec::new()));
    let core = Core::spawn_with_engine(
        db.join("estate.db").to_str().unwrap().to_string(),
        Arc::new(NoBallots),
        Arc::new(Seat {
            verdict,
            launches: launches.clone(),
        }),
    );
    core.register_workflow(fix_def(&format!("{name}-def"), pinned))
        .expect("the fix def registers");
    let entry = core
        .register_repo(RepoSpec {
            name: format!("{name}-tip"),
            root_path: repo.to_str().unwrap().into(),
            registered_at: 1,
        })
        .expect("register");
    Rig {
        core,
        repo_ref: entry.id,
        launches,
        dirs: vec![repo, db],
    }
}

fn collect_until(
    events: &std::sync::mpsc::Receiver<CoreEvent>,
    until: impl Fn(&CoreEvent) -> bool,
) -> Vec<CoreEvent> {
    let mut out = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(120);
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        match events.recv_timeout(remaining.min(Duration::from_millis(200))) {
            Ok(ev) => {
                let done = until(&ev);
                out.push(ev);
                if done {
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
        }
    }
    out
}

fn ends(sid: &str) -> impl Fn(&CoreEvent) -> bool + '_ {
    move |e| {
        matches!(e, CoreEvent::SessionCompleted { session } if session == sid)
            || matches!(e, CoreEvent::SessionFailed { session, .. } if session == sid)
            || matches!(e, CoreEvent::AwaitingHuman { session, .. } if session == sid)
    }
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

/// The fix gate's receipt (`gateEvaluated.assurance` on ord 1) as JSON, the last one.
fn fix_receipt(evs: &[CoreEvent], sid: &str) -> serde_json::Value {
    evs.iter()
        .filter_map(|e| match e {
            CoreEvent::GateEvaluated {
                session,
                ord,
                assurance,
                ..
            } if session == sid && *ord == 1 => Some(serde_json::to_value(assurance).unwrap()),
            _ => None,
        })
        .next_back()
        .unwrap_or_else(|| panic!("the fix's gateEvaluated: {evs:?}"))
}

fn judge_role(r: &serde_json::Value) -> serde_json::Value {
    r["roles"]
        .as_array()
        .and_then(|a| a.iter().find(|x| x["role"] == "judge").cloned())
        .unwrap_or_else(|| panic!("a judge role on the receipt: {r}"))
}

/// The stand-in judge launches (seat keys in the reserved grammar) and the fix's dispatches.
fn tallies(rig: &Rig) -> (Vec<String>, usize) {
    let launches = rig.launches.lock().unwrap().clone();
    let stand_in_judges = launches
        .iter()
        .filter(|(s, k)| s == "validator" && k.contains('~'))
        .map(|(_, k)| k.clone())
        .collect();
    let fixes = launches.iter().filter(|(s, _)| s != "validator").count();
    (stand_in_judges, fixes)
}

/// Coord F1: a consented one-CLI repo run whose fix changed the tree and pins no validator. The
/// DEFAULT judge has no seat distinct from the creator and an aware run never re-admits the
/// creator: the consented stand-in judges, so the gate never holds for a judge.
#[test]
fn a_consented_one_cli_repo_run_has_its_default_judge_stand_in() {
    let rig = rig("default", "PASS", false);
    let sid = "standin-judge-default";
    let ev = rig.core.subscribe();
    rig.core
        .launch_run(spec(sid, &rig.repo_ref, "default-def", Some(consent())))
        .expect("an aware launch with consent is admitted");
    let evs = collect_until(&ev, ends(sid));
    let held = escalations(&evs, sid);
    assert!(
        !held
            .iter()
            .any(|(c, _)| c == "judge_unavailable" || c == "stand_in_needed"),
        "no gate holds for a judge: {held:?}"
    );
    let (judges, fixes) = tallies(&rig);
    assert_eq!(judges.len(), 1, "one stand-in judge: {judges:?}");
    assert!(judges[0].starts_with("claude~"), "{judges:?}");
    assert_eq!(fixes, 1);
    let r = fix_receipt(&evs, sid);
    let role = judge_role(&r);
    assert_eq!(role["filled"], "stand_in", "{r}");
    assert_eq!(role["state"], "answered", "{r}");
    assert_eq!(role["seat"], judges[0].as_str(), "{r}");
    assert!(
        !r["ran"].as_array().unwrap().iter().any(|i| i == "judge"),
        "a stand-in judge is never `ran`: {r}"
    );
    assert!(
        evs.iter().any(|e| matches!(
            e,
            CoreEvent::StandInSeated { session, binding, .. }
                if session == sid && binding.key == judges[0]
        )),
        "standInSeated for the judge: {evs:?}"
    );
}

/// T-R5: a pinned validator, one seat, consent. The stand-in's PASS passes the gate and its
/// REJECT denies it (never a hold for a judge); a stand-in judge is never counted as run.
#[test]
fn t_r5_a_pinned_stand_in_judge_passes_on_pass_and_denies_on_reject() {
    for (word, pass) in [("PASS", true), ("REJECT", false)] {
        let name = format!("pinned-{}", word.to_lowercase());
        let rig = rig(&name, word, true);
        let sid = format!("standin-judge-{name}");
        let ev = rig.core.subscribe();
        rig.core
            .launch_run(spec(
                &sid,
                &rig.repo_ref,
                &format!("{name}-def"),
                Some(consent()),
            ))
            .expect("an aware launch with consent is admitted");
        let evs = collect_until(&ev, ends(&sid));
        let held = escalations(&evs, &sid);
        assert!(
            !held
                .iter()
                .any(|(c, _)| c == "judge_unavailable" || c == "stand_in_needed"),
            "{word}: no gate holds for a judge: {held:?}"
        );
        let (judges, fixes) = tallies(&rig);
        assert_eq!(judges.len(), 1, "{word}: one stand-in judge: {judges:?}");
        assert_eq!(fixes, 1, "{word}");
        let r = fix_receipt(&evs, &sid);
        assert_eq!(judge_role(&r)["filled"], "stand_in", "{word}: {r}");
        let gated = evs.iter().find_map(|e| match e {
            CoreEvent::GateEvaluated {
                session,
                ord: 1,
                agent_verdict,
                ..
            } if *session == sid => Some(agent_verdict.clone()),
            _ => None,
        });
        if pass {
            assert!(held.is_empty(), "PASS: the gate passes: {held:?}");
        } else {
            assert!(
                !held.is_empty(),
                "REJECT: the gate denies into the person's gate: {gated:?}"
            );
        }
    }
}

/// T-R5 (no consent) + T-R12: the judge has no helper and the run has no consent, so the fix's
/// gate holds at `stand_in_needed` after the fix ran once. A retry with still no helper re-opens
/// the gate; a grant for the earlier opening is refused; the current grant re-runs ONLY the judge,
/// on a stand-in, at the same tree: the fix is never re-dispatched.
#[test]
fn t_r12_a_missing_judge_holds_and_a_grant_re_runs_only_the_judge() {
    let rig = rig("hold", "PASS", false);
    let sid = "standin-judge-hold";
    let ev = rig.core.subscribe();
    rig.core
        .launch_run(spec(sid, &rig.repo_ref, "hold-def", None))
        .expect("an aware launch without consent is admitted");
    let evs = collect_until(&ev, ends(sid));
    let held = escalations(&evs, sid);
    assert_eq!(
        held.last().map(|(c, _)| c.as_str()),
        Some("stand_in_needed"),
        "{held:?}"
    );
    assert_eq!(tallies(&rig), (Vec::new(), 1), "the fix ran once, no judge");
    let first = rig.core.gate_actions(sid).unwrap().expect("a gate is open");
    assert!(
        first.actions.iter().any(|a| a.action == "use_stand_ins"),
        "{first:?}"
    );
    let first_seq = first.gate_seq.expect("gate_seq");

    // A retry with still no helper: only the judge is tried again, and the gate re-opens.
    let grant_seq = {
        let status = rig
            .core
            .confirm_gate(
                sid,
                HumanDecision::Approve {
                    amend: None,
                    amend_scope: Default::default(),
                },
            )
            .expect("a retry is taken");
        // Wait on the gate itself, not on an event: the first hold may have left a later
        // `awaitingHuman` in the channel, and the re-judge runs on its own thread. What the run
        // said meanwhile is kept for the failure message (core#1873).
        let deadline = Instant::now() + Duration::from_secs(120);
        let mut seen: Vec<CoreEvent> = Vec::new();
        let now = loop {
            while let Ok(e) = ev.try_recv() {
                seen.push(e);
            }
            let gate = rig.core.gate_actions(sid).unwrap();
            if let Some(g) = gate.clone().filter(|g| {
                g.gate_seq != Some(first_seq)
                    && g.actions.iter().any(|a| a.action == "use_stand_ins")
            }) {
                break g;
            }
            if Instant::now() >= deadline {
                let said: Vec<String> = seen
                    .iter()
                    .filter_map(|e| match e {
                        CoreEvent::GateEscalated {
                            session,
                            condition,
                            denial_source,
                            verdict_summary,
                            attempt,
                            ..
                        } if session == sid => Some(format!(
                            "escalated {condition}/{denial_source} attempt {attempt}: {}",
                            verdict_summary.chars().take(600).collect::<String>()
                        )),
                        CoreEvent::GateEvaluated { session, .. } if session == sid => {
                            Some(format!("{e:?}").chars().take(1500).collect::<String>())
                        }
                        _ => None,
                    })
                    .collect();
                let names: Vec<String> = seen
                    .iter()
                    .map(|e| format!("{e:?}").chars().take(40).collect())
                    .collect();
                panic!(
                    "the gate never re-opened at stand_in_needed after the retry ({status:?}); \
                     gate now: {gate:?}\nsaid:\n{}\nevents: {names:?}",
                    said.join("\n")
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        let now_seq = now.gate_seq.expect("gate_seq");
        assert_ne!(first_seq, now_seq, "the retry is another opening");
        assert_eq!(tallies(&rig), (Vec::new(), 1), "a retry re-ran no build");

        // The earlier opening is gone: its grant is refused; the current one is taken.
        let stale = rig
            .core
            .grant_stand_ins(sid, "job", Some("operator".into()), first_seq, None)
            .expect_err("a grant for an earlier opening is refused");
        assert!(stale.to_string().starts_with("gate_changed"), "{stale:#}");
        now_seq
    };
    // The held gate is idle: what the channel holds is from before the grant.
    while ev.try_recv().is_ok() {}
    rig.core
        .grant_stand_ins(sid, "job", Some("operator".into()), grant_seq, None)
        .expect("the grant is taken");
    let evs = collect_until(&ev, ends(sid));
    assert!(
        evs.iter()
            .any(|e| matches!(e, CoreEvent::SessionCompleted { session } if session == sid)),
        "the run completes on the stand-in's PASS: {evs:?}"
    );
    let (judges, fixes) = tallies(&rig);
    assert_eq!(
        judges.len(),
        1,
        "the judge re-ran once, on a stand-in: {judges:?}"
    );
    assert!(judges[0].starts_with("claude~"), "{judges:?}");
    assert_eq!(fixes, 1, "the fix was never re-dispatched");
    assert_eq!(judge_role(&fix_receipt(&evs, sid))["filled"], "stand_in");
}
