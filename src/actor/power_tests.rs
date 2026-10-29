//! Power mode through the REAL engine (program DES-POWER-MODE): a preset launch on a repo in power
//! mode drops the pre-build phases, its PA scope step is the intake QE plan (the `RIGOR` line rides
//! the answer to the boundary), and its `build` step runs as ONE multi-turn orchestrator: the
//! worker sees turn 1, a checkpoint, then turn 2 of the same unit and attempt, whose prompt carries
//! the operator's message queued in between. Every checkpoint is a rigor-history row.

use super::*;

fn launch_power(e: &Engine, run: &str, repo_ref: &str) {
    e.core
        .launch_run(LaunchSpec {
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
            project_graph: None,
            plan: None,
            deliver_step: None,
            exclude_seats: Vec::new(),
            evidence_root: None,
            primary: Some("a".into()),
            reduced_assurance: false,
            power: true,
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
            project_graph: None,
            plan: None,
            deliver_step: None,
            exclude_seats: Vec::new(),
            evidence_root: None,
            primary: Some("a".into()),
            reduced_assurance: false,
            power: false,
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
