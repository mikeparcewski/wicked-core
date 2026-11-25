//! Power mode through the REAL engine (program DES-POWER-MODE): a preset launch on a repo in power
//! mode drops the pre-build phases, its PA scope step is the intake QE plan (the `RIGOR` line rides
//! the answer to the boundary), and its `build` step runs as ONE multi-turn orchestrator: the
//! worker sees turn 1, a checkpoint, then turn 2 of the same unit and attempt, whose prompt carries
//! the operator's message queued in between. Every checkpoint is a rigor-history row.

use super::*;

fn launch_power(e: &Engine, run: &str, repo_ref: &str) {
    launch_power_voiced(e, run, repo_ref, None);
}

fn launch_power_voiced(e: &Engine, run: &str, repo_ref: &str, voice: Option<String>) {
    e.core
        .launch_run(LaunchSpec {
            scope: None,
            base_ref: None,
            project_id: None,
            problem: "power mode launch".into(),
            clis: vec![cli("a"), cli("b")],
            entity_mode: crate::EntityMode::Shared,
            session_id: run.into(),
            human_confirm: HumanConfirm::None,
            auto_deliver: false,
            repo_ref: Some(repo_ref.to_string()),
            workflow: Some("bug".into()),
            extra_write_roots: Vec::new(),
            extra_read_roots: Vec::new(),
            optional_read_roots: Vec::new(),
            project_graph: None,
            plan: None,
            deliver_step: None,
            exclude_seats: Vec::new(),
            evidence_root: None,
            primary: Some("a".into()),
            reduced_assurance: false,
            stand_in: Default::default(),
            power: true,
            voice,
            deliverables: Vec::new(),
            qe_acceptance: Default::default(),
        })
        .expect("launch");
}

#[test]
fn a_power_build_runs_many_turns_of_one_unit_with_checkpoints_and_the_operator_message() {
    let core_slot: Arc<OnceLock<Core>> = Arc::new(OnceLock::new());
    let prompts: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let fix_turns = Arc::new(AtomicUsize::new(0));
    let (slot, seen, turns) = (core_slot.clone(), prompts.clone(), fix_turns.clone());
    let w = Worker::scripted(move |i, _| {
        let phase = i.unit.id.rsplit(':').next().unwrap_or("").to_string();
        seen.lock()
            .unwrap()
            .push((phase.clone(), i.unit.description.clone()));
        match phase.as_str() {
            "pa-scope" => turn(
                &pa_output(
                    "SCOPE {\"touch\":[\"README.md\"]}\nRIGOR {\"atLeast\":null,\"why\":\"docs\",\
                     \"scenarios\":[\"readme renders\"],\"estimateMinutes\":20,\"mandatoryPhases\":[],\
                     \"phased\":null}",
                ),
                None,
            ),
            "fix" => match turns.fetch_add(1, AtomicOrdering::SeqCst) {
                0 => {
                    // The operator speaks while the orchestrator works: the next turn hears it.
                    slot.get()
                        .expect("core")
                        .inject_worker_message(
                            &i.run_id,
                            "keep the old README heading",
                            crate::InjectTarget::All,
                        )
                        .expect("inject");
                    turn("edited the README\nCHECKPOINT: commit 1; next the heading", None)
                }
                1 => turn("kept the heading\nHANDOFF: README fixed", None),
                _ => hold(),
            },
            _ => hold(),
        }
    });
    let e = engine("pw1", w.clone());
    let _ = core_slot.set(e.core.clone());
    let (path, _head) = git_repo(&e.rig.dir);
    let repo = e
        .core
        .register_repo(crate::RepoSpec {
            name: "pw1".into(),
            root_path: path.to_string_lossy().into_owned(),
            registered_at: 0,
        })
        .unwrap();
    launch_power(&e, "pw", &repo.id);
    wait_for(
        "both orchestrator turns and the pre-hand-off checkpoint",
        || {
            view(&e, "pw")
                .session
                .power
                .as_ref()
                .is_some_and(|p| p.history.len() >= 3)
        },
    );
    let calls = e.worker.calls();
    assert_eq!(
        calls[0].2, "pw:pa-scope",
        "the intake (scope) step runs first"
    );
    assert_eq!(calls[1].2, "pw:fix");
    assert_eq!(calls[2].2, "pw:fix", "turn 2 is the same unit");
    assert_eq!(calls[1].1, calls[2].1, "…at the same attempt");
    let prompts = prompts.lock().unwrap().clone();
    assert!(
        prompts[0].1.contains("RIGOR {"),
        "the scope step asks for the QE plan"
    );
    assert!(prompts[1]
        .1
        .contains("[power mode] You are the ORCHESTRATOR"));
    assert!(
        prompts[2].1.starts_with("[power mode] Turn 2"),
        "{}",
        prompts[2].1
    );
    assert!(
        prompts[2].1.contains("keep the old README heading"),
        "the queued operator message rides the next turn: {}",
        prompts[2].1
    );
    let v = view(&e, "pw");
    let ids: Vec<String> = v
        .units
        .iter()
        .filter_map(|u| u.id.rsplit(':').next().map(str::to_string))
        .collect();
    assert!(
        !ids.iter().any(|i| i == "triage" || i == "reproduce"),
        "power drops the pre-build phases: {ids:?}"
    );
    let p = v.session.power.unwrap();
    let plan = p.plan.expect("the intake rigor record");
    assert_eq!(plan.rigor, crate::power::Rigor::Light, "docs only");
    assert_eq!(plan.estimate_minutes, Some(20));
    assert_eq!(plan.scenarios, ["readme renders".to_string()]);
    let triggers: Vec<&str> = p.history.iter().map(|r| r.trigger.as_str()).collect();
    assert_eq!(&triggers[..3], ["intake", "turn", "pre_handoff"]);
    assert_eq!(p.history[1].checkpoint, Some(1));
    assert_eq!(p.history[1].turn, Some(1));
    assert!(p.orchestrator.turns >= 2);
    assert_eq!(
        p.orchestrator.cli.as_deref(),
        Some("a"),
        "the PA is the orchestrator"
    );
    release_all(&w);
}

#[test]
fn a_phased_launch_carries_no_power_record_and_keeps_its_preset_steps() {
    let w = Worker::scripted(|i, _| {
        if i.unit.id.ends_with(":pa-scope") {
            turn(&pa_output("SCOPE {\"touch\":[\"README.md\"]}"), None)
        } else {
            hold()
        }
    });
    let e = engine("pw2", w.clone());
    let (path, _head) = git_repo(&e.rig.dir);
    let repo = e
        .core
        .register_repo(crate::RepoSpec {
            name: "pw2".into(),
            root_path: path.to_string_lossy().into_owned(),
            registered_at: 0,
        })
        .unwrap();
    e.core
        .launch_run(LaunchSpec {
            scope: None,
            base_ref: None,
            project_id: None,
            problem: "phased".into(),
            clis: vec![cli("a"), cli("b")],
            entity_mode: crate::EntityMode::Shared,
            session_id: "ph".into(),
            human_confirm: HumanConfirm::None,
            auto_deliver: false,
            repo_ref: Some(repo.id.clone()),
            workflow: Some("bug".into()),
            extra_write_roots: Vec::new(),
            extra_read_roots: Vec::new(),
            optional_read_roots: Vec::new(),
            project_graph: None,
            plan: None,
            deliver_step: None,
            exclude_seats: Vec::new(),
            evidence_root: None,
            primary: Some("a".into()),
            reduced_assurance: false,
            stand_in: Default::default(),
            power: false,
            voice: None,
            deliverables: Vec::new(),
            qe_acceptance: Default::default(),
        })
        .expect("launch");
    wait_for("the unit after the scope step", || {
        e.worker.calls().len() >= 2
    });
    let calls = e.worker.calls();
    assert_eq!(
        calls[1].2, "ph:triage",
        "a phased bug run keeps its triage step"
    );
    let v = view(&e, "ph");
    assert!(v.session.power.is_none());
    release_all(&w);
}

// The hand-off's floor runs the repo's checks in an OS write boundary; Windows has none the
// engine can arm, so there a code-verifying phase fails closed before any verdict (see the floor's
// own NOT_RUN). The send-back path is exercised where the floor can run.
#[cfg(not(windows))]
#[test]
fn a_hand_off_not_pass_goes_back_to_the_orchestrator_on_its_own() {
    if crate::validator::skip_in_container_floor(
        "a_hand_off_not_pass_goes_back_to_the_orchestrator_on_its_own",
    ) {
        return;
    }
    if crate::validator::skip_when_nested(
        "a_hand_off_not_pass_goes_back_to_the_orchestrator_on_its_own",
    ) {
        return;
    }
    let fix_turns = Arc::new(AtomicUsize::new(0));
    let reviews = Arc::new(AtomicUsize::new(0));
    let (turns, revs) = (fix_turns.clone(), reviews.clone());
    let w = Worker::scripted(move |i, _| {
        let phase = i.unit.id.rsplit(':').next().unwrap_or("").to_string();
        match phase.as_str() {
            "pa-scope" => turn(&pa_output("SCOPE {\"touch\":[\"README.md\"]}"), None),
            "fix" => match turns.fetch_add(1, AtomicOrdering::SeqCst) {
                0 => {
                    // A real change in the worktree, so the build's own floor has a tree to judge.
                    let wd = i.workdir.as_ref().expect("a worktree");
                    std::fs::write(wd.join("README.md"), "hello, power\n").unwrap();
                    turn("changed README.md\nHANDOFF: first try", None)
                }
                _ => hold(),
            },
            // The judges of the hand-off's pinned gates pass.
            "validator-agent" => turn("PASS", None),
            p if p.starts_with("review") || p == "verify" => {
                match revs.fetch_add(1, AtomicOrdering::SeqCst) {
                    0 => turn(
                        "ITEM 1: FAIL — the heading is gone (README.md:1)\nVERDICT: FAIL",
                        None,
                    ),
                    _ => hold(),
                }
            }
            _ => hold(),
        }
    });
    let e = engine("pw3", w.clone());
    let (path, _head) = git_repo(&e.rig.dir);
    let repo = e
        .core
        .register_repo(crate::RepoSpec {
            name: "pw3".into(),
            root_path: path.to_string_lossy().into_owned(),
            registered_at: 0,
        })
        .unwrap();
    launch_power(&e, "ps", &repo.id);
    wait_for(
        "the build to be sent back after the review's NOT-PASS",
        || e.worker.calls().iter().filter(|c| c.2 == "ps:fix").count() >= 2,
    );
    let fixes: Vec<(u32, u32, String)> = e
        .worker
        .calls()
        .into_iter()
        .filter(|c| c.2 == "ps:fix")
        .collect();
    assert!(
        fixes[1].1 > fixes[0].1,
        "the rework is a new attempt: {fixes:?}"
    );
    let v = view(&e, "ps");
    let p = v.session.power.expect("power");
    assert_eq!(
        p.handoff.as_ref().map(|h| h.round),
        Some(2),
        "round 2 of the hand-off"
    );
    assert_eq!(p.operator_gates, 0, "no person answered anything");
    assert!(
        p.handoff
            .as_ref()
            .unwrap()
            .steps
            .iter()
            .any(|s| (s.step == "review" || s.step == "qe") && s.status == "failed"),
        "{:?}",
        p.handoff
    );
    release_all(&w);
}

/// (core#1216) The person's voice reaches the ORCHESTRATOR and nobody else: its first brief and
/// its rework round carry it; the PA's scope step, the hand-off's reviewers and the gate judges
/// are briefed without it (evaluator independence never depends on the person's tone settings).
/// Requires the hand-off floor's OS write boundary to arm: Windows and the container floor
/// cannot arm it, so the hand-off fails closed before a reviewer runs and no rework round follows.
#[cfg(not(windows))]
#[test]
fn only_the_orchestrator_hears_the_voice_never_the_pa_reviewers_or_judges() {
    if crate::validator::skip_in_container_floor(
        "only_the_orchestrator_hears_the_voice_never_the_pa_reviewers_or_judges",
    ) {
        return;
    }
    if crate::validator::skip_when_nested(
        "only_the_orchestrator_hears_the_voice_never_the_pa_reviewers_or_judges",
    ) {
        return;
    }
    const MARK: &str = "Your name with the person is \"Rook\"";
    let prompts: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let fix_turns = Arc::new(AtomicUsize::new(0));
    let reviews = Arc::new(AtomicUsize::new(0));
    let (seen, turns, revs) = (prompts.clone(), fix_turns.clone(), reviews.clone());
    let w = Worker::scripted(move |i, _| {
        let phase = i.unit.id.rsplit(':').next().unwrap_or("").to_string();
        seen.lock()
            .unwrap()
            .push((phase.clone(), i.unit.description.clone()));
        match phase.as_str() {
            "pa-scope" => turn(&pa_output("SCOPE {\"touch\":[\"README.md\"]}"), None),
            "fix" => match turns.fetch_add(1, AtomicOrdering::SeqCst) {
                0 => {
                    let wd = i.workdir.as_ref().expect("a worktree");
                    std::fs::write(wd.join("README.md"), "hello, voice\n").unwrap();
                    turn("changed README.md\nHANDOFF: first try", None)
                }
                _ => hold(),
            },
            "validator-agent" => turn("PASS", None),
            p if p.starts_with("review") || p == "verify" => {
                match revs.fetch_add(1, AtomicOrdering::SeqCst) {
                    0 => turn(
                        "ITEM 1: FAIL — the heading is gone (README.md:1)\nVERDICT: FAIL",
                        None,
                    ),
                    _ => hold(),
                }
            }
            _ => hold(),
        }
    });
    let e = engine("pwv", w.clone());
    let (path, _head) = git_repo(&e.rig.dir);
    let repo = e
        .core
        .register_repo(crate::RepoSpec {
            name: "pwv".into(),
            root_path: path.to_string_lossy().into_owned(),
            registered_at: 0,
        })
        .unwrap();
    let voice = format!("## Voice\n\n- {MARK}.\n- Keep it short.\n");
    launch_power_voiced(&e, "pv", &repo.id, Some(voice));
    // (core#1794) Wait on the briefs themselves: the worker records a dispatch before its script
    // runs, so a dispatch count can reach 2 while the second brief is not yet recorded.
    wait_for(
        "the rework round's brief after the review's NOT-PASS",
        || {
            prompts
                .lock()
                .unwrap()
                .iter()
                .filter(|(p, _)| p == "fix")
                .count()
                >= 2
        },
    );
    let prompts = prompts.lock().unwrap().clone();
    let fixes: Vec<&String> = prompts
        .iter()
        .filter(|(p, _)| p == "fix")
        .map(|(_, d)| d)
        .collect();
    assert_eq!(fixes.len(), 2, "{prompts:?}");
    for f in &fixes {
        assert!(f.contains(MARK), "the orchestrator hears the voice: {f}");
        assert!(
            f.contains(crate::power::VOICE_GOVERNANCE),
            "behind its governance line: {f}"
        );
    }
    let others: Vec<&(String, String)> = prompts.iter().filter(|(p, _)| p != "fix").collect();
    assert!(
        others.iter().any(|(p, _)| p == "pa-scope"),
        "the PA's scope step ran: {prompts:?}"
    );
    assert!(
        others
            .iter()
            .any(|(p, _)| p.starts_with("review") || p == "verify" || p == "validator-agent"),
        "a hand-off reviewer or judge ran: {prompts:?}"
    );
    for (p, d) in others {
        assert!(!d.contains(MARK), "{p} must not hear the voice: {d}");
        assert!(!d.contains("## Voice"), "{p} must not hear the voice: {d}");
    }
    assert_eq!(
        view(&e, "pv").session.power.unwrap().voice.as_deref(),
        Some(format!("## Voice\n\n- {MARK}.\n- Keep it short.").as_str()),
        "kept on the run (trimmed) for a rework round or a restart"
    );
    release_all(&w);
}

/// (core#1216) A voice over the bound refuses the launch with nothing persisted.
#[test]
fn an_oversized_voice_refuses_the_launch() {
    let w = Worker::scripted(|_, _| hold());
    let e = engine("pwvx", w.clone());
    let (path, _head) = git_repo(&e.rig.dir);
    let repo = e
        .core
        .register_repo(crate::RepoSpec {
            name: "pwvx".into(),
            root_path: path.to_string_lossy().into_owned(),
            registered_at: 0,
        })
        .unwrap();
    let err = e
        .core
        .launch_run(LaunchSpec {
            scope: None,
            base_ref: None,
            project_id: None,
            problem: "too much voice".into(),
            clis: vec![cli("a"), cli("b")],
            entity_mode: crate::EntityMode::Shared,
            session_id: "pvx".into(),
            human_confirm: HumanConfirm::None,
            auto_deliver: false,
            repo_ref: Some(repo.id.clone()),
            workflow: Some("bug".into()),
            extra_write_roots: Vec::new(),
            extra_read_roots: Vec::new(),
            optional_read_roots: Vec::new(),
            project_graph: None,
            plan: None,
            deliver_step: None,
            exclude_seats: Vec::new(),
            evidence_root: None,
            primary: Some("a".into()),
            reduced_assurance: false,
            stand_in: Default::default(),
            power: true,
            voice: Some("x".repeat(crate::power::VOICE_MAX_BYTES + 1)),
            deliverables: Vec::new(),
            qe_acceptance: Default::default(),
        })
        .expect_err("refused");
    assert!(err.to_string().contains("voice"), "{err}");
    assert!(
        !e.core
            .sessions_detail()
            .unwrap()
            .iter()
            .any(|v| v.session.id == "pvx"),
        "nothing persisted"
    );
    assert!(e.worker.calls().is_empty(), "nothing ran");
    release_all(&w);
}

fn power_engine(name: &str, w: Arc<Worker>, run: &str) -> Engine {
    let e = engine(name, w);
    let (path, _head) = git_repo(&e.rig.dir);
    let repo = e
        .core
        .register_repo(crate::RepoSpec {
            name: name.into(),
            root_path: path.to_string_lossy().into_owned(),
            registered_at: 0,
        })
        .unwrap();
    launch_power(&e, run, &repo.id);
    e
}

// The hand-off's floor runs the repo's checks in an OS write boundary; Windows has none the
// engine can arm, so there a code-verifying phase fails closed before any verdict (see the floor's
// own NOT_RUN). The send-back path is exercised where the floor can run.
#[cfg(not(windows))]
#[test]
fn a_hand_off_that_keeps_failing_goes_back_three_rounds_then_waits_for_the_operator() {
    if crate::validator::skip_in_container_floor(
        "a_hand_off_that_keeps_failing_goes_back_three_rounds_then_waits_for_the_operator",
    ) {
        return;
    }
    if crate::validator::skip_when_nested(
        "a_hand_off_that_keeps_failing_goes_back_three_rounds_then_waits_for_the_operator",
    ) {
        return;
    }
    let w = Worker::scripted(move |i, _| {
        let phase = i.unit.id.rsplit(':').next().unwrap_or("").to_string();
        match phase.as_str() {
            "pa-scope" => turn(&pa_output("SCOPE {\"touch\":[\"README.md\"]}"), None),
            "fix" => {
                let wd = i.workdir.as_ref().expect("a worktree");
                std::fs::write(wd.join("README.md"), format!("try {}\n", i.attempt)).unwrap();
                turn("changed README.md\nHANDOFF: try again", None)
            }
            "validator-agent" => turn("PASS", None),
            "verify" => turn(
                "ITEM 1: FAIL — still wrong (README.md:1)\nVERDICT: FAIL",
                None,
            ),
            _ => hold(),
        }
    });
    let e = power_engine("pw4", w.clone(), "pc");
    wait_for(
        "the third round to fail and the run to wait for a person",
        || {
            let v = view(&e, "pc");
            v.session.status == SessionStatus::AwaitingHuman
                && v.session
                    .power
                    .as_ref()
                    .and_then(|p| p.handoff.as_ref())
                    .is_some_and(|h| h.steps.iter().filter(|s| s.status == "failed").count() >= 3)
        },
    );
    let fixes = e.worker.calls().iter().filter(|c| c.2 == "pc:fix").count();
    assert_eq!(fixes, 3, "three hand-off rounds, then no fourth on its own");
    let p = view(&e, "pc").session.power.unwrap();
    assert_eq!(p.handoff.as_ref().unwrap().round, 3);
    assert_eq!(p.operator_gates, 0);
    release_all(&w);
}

#[test]
fn an_operator_raise_is_a_floor_held_for_the_plan_and_rigor_is_never_lowered_on_request() {
    let w = Worker::scripted(move |i, _| {
        if i.unit.id.ends_with(":pa-scope") {
            turn(&pa_output("SCOPE {\"touch\":[\"README.md\"]}"), None)
        } else {
            hold()
        }
    });
    let e = power_engine("pw5", w.clone(), "pr");
    wait_for("the orchestrator's first turn", || {
        e.worker.calls().iter().any(|c| c.2 == "pr:fix")
    });
    let st = e
        .core
        .raise_rigor(
            "pr",
            crate::power::Rigor::Highest,
            "local (human)",
            "touches sign-in",
        )
        .expect("a raise");
    assert_eq!(st.rigor, Some(crate::power::Rigor::Highest));
    assert!(st
        .floors
        .iter()
        .any(|f| f.by == "operator" && f.rigor == crate::power::Rigor::Highest));
    assert_eq!(st.history.last().unwrap().trigger, "operator");
    let tp = view(&e, "pr").session.team_plan.unwrap();
    assert_eq!(
        tp.rescored.as_ref().map(|r| r.score),
        Some(70),
        "the raise is held for the plan's next boundary"
    );
    let err = e
        .core
        .raise_rigor("pr", crate::power::Rigor::High, "local (human)", "lower it")
        .unwrap_err();
    assert!(format!("{err:#}").contains("conflict"), "{err:#}");
    // The switch: off is always allowed and recorded with its actor; on again while available.
    let off = e
        .core
        .set_power_mode("pr", false, "local (human)", Some("needs design".into()))
        .unwrap();
    assert!(!off.on);
    let t = off.toggles.last().unwrap();
    assert_eq!((t.on, t.by.as_str()), (false, "local (human)"));
    assert_eq!(off.history.last().unwrap().trigger, "toggle");
    assert!(
        e.core
            .set_power_mode("pr", true, "local (human)", None)
            .unwrap()
            .on
    );
    release_all(&w);
}

/// (wicked-core#1209, rig run 1d372696) A hand-off step whose unit COUNTED reads "passed" at once,
/// not when the next unit dispatches: the QE step read "running" for the whole deliver-gate wait.
#[cfg(not(windows))]
#[test]
fn a_counted_hand_off_step_reads_passed_before_the_next_unit_dispatches() {
    if crate::validator::skip_in_container_floor(
        "a_counted_hand_off_step_reads_passed_before_the_next_unit_dispatches",
    ) {
        return;
    }
    if crate::validator::skip_when_nested(
        "a_counted_hand_off_step_reads_passed_before_the_next_unit_dispatches",
    ) {
        return;
    }
    let w = Worker::scripted(move |i, _| {
        let phase = i.unit.id.rsplit(':').next().unwrap_or("").to_string();
        match phase.as_str() {
            "pa-scope" => turn(&pa_output("SCOPE {\"touch\":[\"README.md\"]}"), None),
            "fix" => {
                let wd = i.workdir.as_ref().expect("a worktree");
                std::fs::write(wd.join("README.md"), "hello, power\n").unwrap();
                turn("changed README.md\nHANDOFF: done", None)
            }
            // (DES-RULE-CHECK-001) The review's rule check asks the same judge identity.
            "validator-agent" => match crate::rule_check::stub_rule_verdicts(&i.unit.description) {
                Some(v) => turn(&v, None),
                None => turn("PASS", None),
            },
            p if p.starts_with("review") || p == "verify" => turn(
                "ITEM 1: PASS — the heading is right (README.md:1)\nVERDICT: PASS",
                None,
            ),
            _ => hold(),
        }
    });
    let e = power_engine("pw9", w.clone(), "pk");
    wait_for("a hand-off step to count", || {
        let v = view(&e, "pk");
        v.units
            .iter()
            .any(|u| handoff_unit(u) && u.status == crate::domain::UnitStatus::Done)
    });
    let v = view(&e, "pk");
    let p = v.session.power.clone().expect("power");
    let steps = &p.handoff.as_ref().expect("a hand-off").steps;
    for u in v
        .units
        .iter()
        .filter(|u| handoff_unit(u) && u.status == crate::domain::UnitStatus::Done)
    {
        let last = steps
            .iter()
            .rev()
            .find(|s| s.ord == Some(u.ord))
            .unwrap_or_else(|| panic!("unit {} has a step: {steps:?}", u.ord));
        assert_eq!(
            last.status, "passed",
            "unit {} counted, so its step passed: {steps:?}",
            u.ord
        );
    }
    release_all(&w);
}

#[cfg(not(windows))]
fn handoff_unit(u: &crate::domain::WorkUnit) -> bool {
    matches!(
        u.catalog.as_deref(),
        Some("review" | "critique" | "security_review" | "test")
    )
}

/// (core#1520) turn 1 hears only this attempt's notes; persisted amendment history stays intact.
#[test]
fn a_redispatched_build_briefs_only_the_current_attempts_notes() {
    use wicked_apps_core::ToNode;

    let mut store = wicked_apps_core::open_store(Some(":memory:")).unwrap();
    let session: crate::domain::AgentSession = serde_json::from_value(json!({
        "id": "notes", "workflow_id": "wf", "problem": "fix it",
        "entity_mode": crate::EntityMode::Shared, "clis": ["a"],
        "status": SessionStatus::Executing, "attempt": 3,
        "power": crate::power::PowerState::launched(true, 0)
    }))
    .unwrap();
    let mut unit = crate::domain::WorkUnit::pending("notes:fix", "notes", 0, "authored task");
    unit.catalog = Some("build".into());
    unit.role = crate::workflow::PhaseRole::Creator;
    unit.last_attempt = Some(3);
    unit.assigned_cli = Some("a".into());
    unit.description
        .push_str(" (operator amendment: round one)");
    unit.description
        .push_str(" (operator amendment: round two)");
    for (attempt, text) in [(1, "round one"), (2, "round two")] {
        unit.operator_rulings.push(crate::domain::OperatorRuling {
            action: "amend".into(),
            text: text.into(),
            attempt,
            at: 1,
        });
    }
    crate::domain::put_node(&mut store, unit.to_node()).unwrap();
    let mut sink = crate::event_log::EventSink::default();
    let sent =
        crate::actor::power_dispatch(&mut store, &mut sink, &session, unit.clone(), false).unwrap();
    assert!(
        sent.description.starts_with("authored task\n"),
        "{}",
        sent.description
    );
    assert!(
        !sent.description.contains("round one"),
        "{}",
        sent.description
    );
    assert_eq!(sent.description.matches("round two").count(), 1);
    assert_eq!(
        crate::domain::session_units(&store, "notes").unwrap()[0].description,
        unit.description
    );
    crate::power::forget_run("notes");
}

/// (core#1520) a repeated note at a later attempt emits history; replay at that attempt is silent.
#[test]
fn a_repeated_note_emits_an_amendment_for_each_attempt() {
    let mut store = wicked_apps_core::open_store(Some(":memory:")).unwrap();
    let mut sink = crate::event_log::EventSink::default();
    let (tx, rx) = std::sync::mpsc::channel();
    sink.push(tx);
    let mut unit = crate::domain::WorkUnit::pending("repeat:fix", "repeat", 0, "fix it");
    for attempt in [1, 2, 2] {
        unit.last_attempt = Some(attempt);
        crate::actor::amend_unit(
            &mut store,
            &mut sink,
            "repeat",
            &mut unit,
            "keep heading",
            "creator",
        )
        .unwrap();
    }
    let events: Vec<_> = rx
        .try_iter()
        .filter_map(|ev| match ev {
            CoreEvent::UnitReworkAmended {
                amendment,
                updated_description,
                scope,
                ..
            } => Some((amendment, updated_description, scope)),
            _ => None,
        })
        .collect();
    assert_eq!(events.len(), 2, "{events:?}");
    assert_eq!(events[0], events[1]);
    assert_eq!(events[1].2, "creator");
    assert_eq!(
        unit.description
            .matches(" (operator amendment: keep heading)")
            .count(),
        1
    );
    assert_eq!(unit.operator_rulings[0].attempt, 2);
}

/// (core#1178) an unmarked first turn hands off once and its checkpoint counts failed tests.
#[test]
fn an_unmarked_turn_hands_off_once_with_its_failed_test_count() {
    let w = Worker::scripted(|i, _| match i.unit.id.rsplit(':').next().unwrap_or("") {
        "pa-scope" => turn(&pa_output("SCOPE {\"touch\":[\"README.md\"]}"), None),
        "fix" => {
            let mut sink = crate::tool_activity::ToolActivitySink::new(
                &i.run_id,
                i.unit.ord,
                i.attempt,
                i.workdir.as_deref().unwrap(),
                Box::new(|_| {}),
            );
            sink.started("test", "Bash", &json!({"command": "npm test"}));
            sink.finished("test", false);
            turn("The work is done.", None)
        }
        _ => hold(),
    });
    let e = engine("unmarked-turn", w.clone());
    let (path, _) = git_repo(&e.rig.dir);
    let repo = e
        .core
        .register_repo(crate::RepoSpec {
            name: "unmarked".into(),
            root_path: path.to_string_lossy().into_owned(),
            registered_at: 0,
        })
        .unwrap();
    launch_power(&e, "unmarked", &repo.id);
    wait_for("the first turn to finish", || {
        view(&e, "unmarked")
            .session
            .power
            .as_ref()
            .is_some_and(|p| p.turn_log.first().is_some_and(|t| t.ended_at.is_some()))
    });
    let p = view(&e, "unmarked").session.power.unwrap();
    assert_eq!(p.turn_log[0].outcome.as_deref(), Some("handoff"));
    assert_eq!(
        p.turn_log[0].summary.as_deref(),
        Some(
            "the turn ended with no CHECKPOINT or HANDOFF line; \
             the engine read it as the hand-off"
        )
    );
    assert_eq!(
        e.worker
            .calls()
            .iter()
            .filter(|c| c.2 == "unmarked:fix")
            .count(),
        1
    );
    let checkpoint = p
        .history
        .iter()
        .find(|r| r.trigger == "pre_handoff")
        .unwrap();
    assert_eq!(checkpoint.signals.as_ref().unwrap().struggle.failed_runs, 1);
    release_all(&w);
}

/// (core#1178) scope's RIGOR phases become durable floor steps even below their band.
#[test]
fn the_intake_qe_plan_adds_architecture_to_a_light_plan() {
    let w = Worker::scripted(|i, _| match i.unit.id.rsplit(':').next().unwrap_or("") {
        "pa-scope" => turn(
            &pa_output(
                "SCOPE {\"touch\":[\"README.md\"]}\n\
                 RIGOR {\"mandatoryPhases\":[{\"phase\":\"architecture\",\"why\":\"boundary\"}]}",
            ),
            None,
        ),
        _ => hold(),
    });
    let e = engine("intake-architecture", w.clone());
    let (path, _) = git_repo(&e.rig.dir);
    let repo = e
        .core
        .register_repo(crate::RepoSpec {
            name: "intake-architecture".into(),
            root_path: path.to_string_lossy().into_owned(),
            registered_at: 0,
        })
        .unwrap();
    launch_power(&e, "architecture", &repo.id);
    wait_for("the scoped plan", || {
        view(&e, "architecture")
            .session
            .power
            .as_ref()
            .is_some_and(|p| p.plan.is_some())
    });
    let v = view(&e, "architecture");
    let state = v.session.team_plan.unwrap();
    let restored: crate::plan_gate::TeamPlanState =
        serde_json::from_value(serde_json::to_value(&state).unwrap()).unwrap();
    assert_eq!(restored.qe_phases, ["architecture".to_string()]);
    assert_eq!(
        state.max_score, 0,
        "the docs band has no architecture phase"
    );
    let accepted = state.accepted.unwrap();
    let step = accepted
        .steps
        .steps
        .iter()
        .find(|s| s.catalog == "architecture")
        .expect("the QE phase joins the plan");
    assert_eq!(
        step.floor_reason.as_deref(),
        Some("the intake QE plan requires it")
    );
    assert!(step.floor_rule.is_none());
    assert!(accepted.rules.is_empty());
    assert_eq!(state.qe_phases, ["architecture".to_string()]);
    release_all(&w);
}

/// (core#1475) a real ledger-backed QE denial survives the power escalation and build rewind.
#[test]
fn a_qe_failure_sends_its_ledger_findings_back_to_the_build() {
    use wicked_apps_core::ToNode;

    let review = crate::qe_acceptance::review::tests::review_with_ledger_reason(
        "FAIL",
        Some("IMPLEMENTATION_BUG — X"),
    );
    let mut store = wicked_apps_core::open_store(Some(":memory:")).unwrap();
    let mut power = crate::power::PowerState::launched(true, 0);
    power.orchestrator.ord = Some(1);
    let mut session: crate::domain::AgentSession = serde_json::from_value(json!({
        "id": "qe-reason", "workflow_id": "wf", "problem": "fix it",
        "entity_mode": crate::EntityMode::Shared, "clis": ["a", "b"],
        "status": SessionStatus::Executing, "unit_ix": 1, "power": power
    }))
    .unwrap();
    crate::domain::put_node(&mut store, session.to_node()).unwrap();
    let mut build = crate::domain::WorkUnit::pending("qe-reason:fix", "qe-reason", 1, "fix it");
    build.role = crate::workflow::PhaseRole::Creator;
    build.assigned_cli = Some("a".into());
    build.status = crate::domain::UnitStatus::Done;
    build.last_attempt = Some(0);
    crate::domain::put_node(&mut store, build.to_node()).unwrap();
    let mut qe = crate::domain::WorkUnit::pending("qe-reason:qe", "qe-reason", 2, "check it");
    qe.role = crate::workflow::PhaseRole::Evaluator;
    qe.catalog = Some("test".into());
    qe.assigned_cli = Some("b".into());
    qe.status = crate::domain::UnitStatus::Rejected;
    qe.last_attempt = Some(0);
    qe.denial = crate::qe_acceptance::review::denial(&review);
    qe.denial_reason = qe.denial.as_ref().map(|d| d.reason.clone());
    qe.qe_review = Some(review);
    crate::domain::put_node(&mut store, qe.to_node()).unwrap();
    let mut sink = crate::event_log::EventSink::default();
    let (events, seen) = std::sync::mpsc::channel();
    sink.push(events);
    let (tx, rx) = std::sync::mpsc::channel();
    crate::actor::escalate_denied_unit(
        &mut store,
        &mut sink,
        &tx,
        &mut session,
        &qe,
        0,
        false,
        true,
        false,
        "",
    )
    .unwrap();
    let (decision, actor) = rx
        .try_iter()
        .find_map(|cmd| match cmd {
            Command::ConfirmGate {
                decision, actor, ..
            } => Some((decision, actor)),
            _ => None,
        })
        .expect("power answers the escalation");
    assert_eq!(actor.as_deref(), Some(crate::power::HANDOFF_ACTOR));
    assert!(matches!(decision, HumanDecision::RequestChanges { .. }));
    let runner: Arc<dyn StepRunner> = Worker::scripted(|_, _| turn("HANDOFF: done", None));
    crate::actor::confirm_gate_as(
        &mut store,
        &mut sink,
        &runner,
        &tx,
        &mut std::collections::HashSet::new(),
        "qe-reason",
        decision,
        actor,
        &None,
        &None,
        uuid::Uuid::nil(),
        false,
    )
    .unwrap();
    let units = crate::domain::session_units(&store, "qe-reason").unwrap();
    assert!(units[0]
        .rework_amendment
        .as_deref()
        .unwrap()
        .contains("IMPLEMENTATION_BUG — X"));
    assert!(seen.try_iter().any(|ev| matches!(
        ev,
        CoreEvent::UnitReworkAmended { amendment, .. }
            if amendment.contains("IMPLEMENTATION_BUG — X")
    )));
}

/// (core#1585) boot supersedes the parked run's dead attempt without dispatching past its gate.
#[test]
fn boot_supersedes_dispatched_attempts_of_live_parked_runs() {
    use crate::domain::UnitStatus::{Distributed, Done, Pending};
    use wicked_apps_core::ToNode;

    for (status, in_flight, unit_status, last_attempt, bumped) in [
        (
            SessionStatus::AwaitingHuman,
            false,
            Distributed,
            Some(4),
            true,
        ),
        (SessionStatus::Planning, false, Distributed, Some(4), true),
        (
            SessionStatus::Distributing,
            false,
            Distributed,
            Some(4),
            true,
        ),
        (
            SessionStatus::AwaitingHuman,
            true,
            Distributed,
            Some(4),
            false,
        ),
        (SessionStatus::AwaitingHuman, false, Pending, Some(4), false),
        (SessionStatus::AwaitingHuman, false, Done, Some(4), false),
        (
            SessionStatus::AwaitingHuman,
            false,
            Distributed,
            None,
            false,
        ),
        (SessionStatus::Failed, false, Distributed, Some(4), false),
        (SessionStatus::Completed, false, Distributed, Some(4), false),
        (SessionStatus::Cancelled, false, Distributed, Some(4), false),
    ] {
        let mut store = wicked_apps_core::open_store(Some(":memory:")).unwrap();
        let session: crate::domain::AgentSession = serde_json::from_value(json!({
            "id": "parked", "workflow_id": "wf", "problem": "fix it",
            "entity_mode": crate::EntityMode::Shared, "clis": ["a"],
            "status": status, "attempt": 4
        }))
        .unwrap();
        crate::domain::put_node(&mut store, session.to_node()).unwrap();
        let mut unit = crate::domain::WorkUnit::pending("parked:fix", "parked", 1, "fix it");
        unit.status = unit_status;
        unit.last_attempt = last_attempt;
        crate::domain::put_node(&mut store, unit.to_node()).unwrap();
        let w = Worker::scripted(|_, _| turn("unexpected dispatch", None));
        let runner: Arc<dyn StepRunner> = w.clone();
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut sink = crate::event_log::EventSink::default();
        let mut flying = std::collections::HashSet::new();
        if in_flight {
            flying.insert("parked".to_string());
        }
        crate::actor::redrive_executing_sessions(
            &mut store,
            &mut sink,
            &runner,
            &tx,
            &mut flying,
            &None,
            &None,
            uuid::Uuid::nil(),
            false,
        );
        let after = crate::domain::get_session(&store, "parked")
            .unwrap()
            .unwrap();
        assert_eq!(after.status, status);
        assert_eq!(
            after.attempt,
            if bumped { 5 } else { 4 },
            "{status:?}, {in_flight}, {unit_status:?}, {last_attempt:?}"
        );
        assert!(w.calls().is_empty(), "boot must leave the gate held");
        assert_eq!(
            crate::domain::session_units(&store, "parked").unwrap()[0],
            unit
        );
    }
}
