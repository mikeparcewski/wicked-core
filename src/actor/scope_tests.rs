//! Seam X1 (DES-TEAMING-002 §8.2, §8.4, rev 13) through the REAL engine: a preset launch declares
//! no `touch`, so its first unit is the PA's read-only `pa-scope` step; the plan is scored from the
//! PA's answer (`SCOPE` on a repo, `RISK` with none) and decided as the run's initial plan at that
//! step's boundary, before any creator step dispatches. Every team fact is read off a real bus db
//! and waited for (`settled`): the plan facts are fire-and-forget, so they may land after the
//! pause or dispatch that follows them.
//!
//! Expected values are fixed literals from §8.5's table, never re-derived from the code under
//! test. The worker answers by unit (`<run>:scope`, …) and holds anything it is not scripted for.

use super::*;

/// A preset's steps, written through the engine's own preset API.
fn put(e: &Engine, name: &str, steps: Value) {
    e.core
        .put_preset(crate::preset::PresetSpec {
            name: name.into(),
            project_id: None,
            steps: serde_json::from_value(steps).expect("preset steps"),
            created_by: "api".into(),
        })
        .expect("put_preset");
}

/// Launch the preset `workflow` (no plan, so no `touch`) on `repo_ref`, on seats `a` (the PA), `b`.
fn launch_preset(e: &Engine, run: &str, workflow: &str, repo_ref: Option<&str>, hc: HumanConfirm) {
    e.core
        .launch_run(LaunchSpec {
            base_ref: None,
            project_id: None,
            problem: "x1 preset launch".into(),
            clis: vec![cli("a"), cli("b")],
            entity_mode: crate::EntityMode::Shared,
            session_id: run.into(),
            human_confirm: hc,
            auto_deliver: false,
            repo_ref: repo_ref.map(str::to_string),
            workflow: Some(workflow.into()),
            extra_write_roots: Vec::new(),
            extra_read_roots: Vec::new(),
            project_graph: None,
            plan: None,
            deliver_step: None,
        })
        .expect("launch");
}

/// A worker that answers per unit (by the phase id after `<run>:`) and holds the rest.
fn by_unit(script: impl Fn(&str) -> Option<Turn> + Send + Sync + 'static) -> Arc<Worker> {
    Worker::scripted(move |i, _| {
        let phase = i.unit.id.rsplit(':').next().unwrap_or("").to_string();
        script(&phase).unwrap_or_else(hold)
    })
}

/// Register the fixture repo (`git_repo`), returning its id and HEAD (the run's base commit).
fn repo(e: &Engine, name: &str) -> (String, String) {
    let (path, head) = git_repo(&e.rig.dir);
    let entry = e
        .core
        .register_repo(crate::RepoSpec {
            name: name.into(),
            root_path: path.to_string_lossy().into_owned(),
            registered_at: 0,
        })
        .unwrap();
    (entry.id, head)
}

/// Index a graph for the registered repo at `head` where `src/auth/login.rs` is imported by forty
/// files and reached by no test (§8.2's blast radius: reach 60 for 21-100 dependents, test gap
/// +20 → 80, the 70-100 band).
fn index_auth_graph(e: &Engine, repo_id: &str, head: &str) {
    use wicked_apps_core::{
        Descriptor, Edge, EdgeKind, GraphWrite, Language, Location, Node, NodeKind, ResolutionTier,
        Span, Symbol,
    };
    let sym = |n: &str| Symbol::global("test", None, vec![Descriptor::method(n, None)]).id();
    let node = |n: &str, file: &str| {
        Node::new(
            sym(n),
            NodeKind::File,
            n,
            Language::new("rust"),
            Location::new(
                file,
                Span {
                    start_byte: 0,
                    end_byte: 0,
                    start_line: 1,
                    start_col: 0,
                    end_line: 200,
                    end_col: 0,
                },
            ),
        )
    };
    let root_path = e
        .core
        .list_repos()
        .unwrap()
        .into_iter()
        .find(|r| r.id == repo_id)
        .expect("the repo")
        .root_path;
    let db = e.rig.dir.join("core.db");
    let root = crate::code_graph::repo_graph_root_for_store(db.to_str().unwrap()).unwrap();
    let graph_db = crate::code_graph::repo_graph_db_at(&root, std::path::Path::new(&root_path));
    std::fs::create_dir_all(graph_db.parent().unwrap()).unwrap();
    let mut nodes = vec![node("login_file", "src/auth/login.rs")];
    let mut edges = Vec::new();
    for i in 0..40u32 {
        let importer = format!("importer{i}");
        nodes.push(node(&importer, &format!("src/user{i}.rs")));
        edges.push(Edge::new(
            sym(&importer),
            sym("login_file"),
            EdgeKind::Imports,
            ResolutionTier::Parsed,
            "fixture",
        ));
    }
    let mut g = wicked_apps_core::open_store(Some(graph_db.to_str().unwrap())).unwrap();
    g.begin_batch().unwrap();
    g.upsert_nodes(&nodes).unwrap();
    g.upsert_edges(&edges).unwrap();
    g.commit_batch().unwrap();
    g.set_repo_info(&wicked_estate_core::RepoInfo {
        commit: Some(head.to_string()),
        ..Default::default()
    })
    .unwrap();
}

fn catalogs(steps: &Value) -> Vec<String> {
    steps
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["catalog"].as_str().unwrap().to_string())
        .collect()
}

/// The run's `gate.opened{kind:"plan_approval"}`, once it is on the bus (it is fire-and-forget, and
/// the scope step's own `unit_review` gate facts ride the same type).
fn plan_gate_opened(e: &Engine, run: &str) -> Value {
    wait_for(&format!("{run}'s plan_approval gate.opened"), || {
        payloads(e, run, tev::GATE_OPENED)
            .iter()
            .any(|p| p["kind"] == "plan_approval")
    });
    payloads(e, run, tev::GATE_OPENED)
        .into_iter()
        .find(|p| p["kind"] == "plan_approval")
        .unwrap()
}

/// The run's plan_approval pauses.
fn plan_pauses(e: &mut Engine, run: &str) -> Vec<u32> {
    e.awaiting(run)
        .into_iter()
        .filter(|(_, k)| k == crate::plan_gate::GATE_KIND)
        .map(|(o, _)| o)
        .collect()
}

/// (X1 a) The default `feature` preset in AUTO mode on a repo: the PA's scope step runs first
/// (read-only `understand`, on the PA seat, with the `SCOPE` grammar in its instructions); it
/// declares a small docs-only scope, which scores 0 (docs have no symbols) — the light 0-19
/// floor, not high risk — so the plan is accepted by the engine with NO plan_approval pause and
/// the next unit dispatches. Rev 1 was the scope step alone.
#[test]
fn x1_a_a_small_docs_only_scope_in_auto_mode_proceeds_on_the_light_floor() {
    let w = by_unit(|phase| {
        (phase == "pa-scope").then(|| {
            turn(
                &pa_output("SCOPE {\"touch\":[\"docs/guide.md\",\"README.md\"]}"),
                None,
            )
        })
    });
    let mut e = engine("x1a", w.clone());
    let (repo_id, _) = repo(&e, "x1a");
    launch_preset(&e, "xa", "feature", Some(&repo_id), HumanConfirm::None);
    wait_for("the unit after the scope step to dispatch", || {
        e.worker.calls().len() >= 2
    });
    let calls = e.worker.calls();
    assert_eq!(calls[0].2, "xa:pa-scope", "the PA's scope step runs first");
    assert_eq!(calls[1].2, "xa:clarify", "then the preset's first step");
    let v = view(&e, "xa");
    let scope = &v.units[0];
    assert_eq!(scope.assigned_cli.as_deref(), Some("a"), "on the PA seat");
    assert_eq!(scope.stage, crate::domain::StageKind::Recon, "read-only");
    // Rev 1 (the scope step alone, by the engine), then rev 2 (the scoped plan, by the engine).
    let accepted = settled(&e, "xa", tev::PLAN_ACCEPTED, 2);
    assert_eq!(catalogs(&accepted[0]["steps"]), ["understand"]);
    assert_eq!(
        (accepted[0]["plan_rev"].clone(), accepted[0]["by"].clone()),
        (json!(1), json!("engine"))
    );
    let a = &accepted[1];
    assert_eq!(
        (a["plan_rev"].clone(), a["by"].clone(), a["band"].clone()),
        (json!(2), json!("engine"), json!("0-19"))
    );
    assert_eq!(a["high_risk"], false);
    assert_eq!(a["mode"], "auto");
    assert_eq!(
        step_ids(&a["steps"]),
        [
            "pa-scope",
            "clarify",
            "design",
            "build",
            "adversarial-review",
            "test",
            "review"
        ],
        "the 0-19 floor adds nothing to a plan that builds"
    );
    // The PA proposed it (from its understand turn), with the touch set it declared; scored 0.
    let proposed = settled(&e, "xa", tev::PLAN_PROPOSED, 1);
    assert_eq!(proposed[0]["by"], "a");
    assert_eq!(proposed[0]["kind"], "initial");
    assert_eq!(proposed[0]["preset"], "feature");
    assert_eq!(proposed[0]["touch"], json!(["docs/guide.md", "README.md"]));
    let scored = settled(&e, "xa", tev::PATH_SCORED, 1);
    assert_eq!(scored[0]["basis"], "intent");
    assert_eq!(scored[0]["score"], 0);
    assert_eq!(
        scored[0]["score_source"],
        tev::score_source_intent(proposed[0]["proposal_id"].as_str().unwrap())
    );
    assert!(
        plan_pauses(&mut e, "xa").is_empty(),
        "no plan_approval pause"
    );
    let tp = view(&e, "xa").session.team_plan.expect("plan state");
    assert!(tp.scope.is_none(), "the hold is taken");
    assert_eq!((tp.accepted_rev, tp.max_score), (2, 0));
    release_all(&w);
}

/// (X1 b) The PA declares a scope touching an auth module with a high blast radius (forty
/// importers, no test): the same scorer as a declared touch reads the repo's graph — 80, the
/// 70-100 band, high risk — so an AUTO-mode run pauses plan_approval before the first unit after
/// the scope step, reviewing the scope step, with nothing else dispatched.
#[test]
fn x1_b_a_scope_on_a_high_blast_radius_auth_module_pauses_as_high_risk() {
    let _env = crate::code_graph::REPO_GRAPH_ROOT_ENV_LOCK
        .read()
        .unwrap_or_else(|p| p.into_inner());
    let w = by_unit(|phase| {
        (phase == "pa-scope").then(|| {
            turn(
                &pa_output("SCOPE {\"touch\":[\"src/auth/login.rs\"]}"),
                None,
            )
        })
    });
    let mut e = engine("x1b", w.clone());
    let (repo_id, head) = repo(&e, "x1b");
    index_auth_graph(&e, &repo_id, &head);
    launch_preset(&e, "xb", "feature", Some(&repo_id), HumanConfirm::None);
    e.wait_awaiting("xb", crate::plan_gate::GATE_KIND, 1);
    let scored = settled(&e, "xb", tev::PATH_SCORED, 1);
    assert_eq!(scored[0]["score"], 80, "{:?}", scored[0]["reasons"]);
    assert!(
        !scored[0]["signals"].is_null(),
        "the graph was read: {:?}",
        scored[0]["reasons"]
    );
    let gate = plan_gate_opened(&e, "xb");
    assert_eq!(gate["reason"], "high_risk");
    assert_eq!(gate["band"], "70-100");
    assert_eq!(gate["high_risk"], true);
    assert_eq!(gate["ord"], 2);
    assert_eq!(gate["reviewing_ord"], 1);
    assert_eq!(plan_pauses(&mut e, "xb"), [2]);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        e.worker.calls().len(),
        1,
        "only the scope step ran: {:?}",
        e.worker.calls()
    );
    // Only rev 1 (the scope step) is accepted; rev 2 is held.
    assert_eq!(payloads(&e, "xb", tev::PLAN_ACCEPTED).len(), 1);
    let tp = view(&e, "xb").session.team_plan.unwrap();
    assert_eq!(tp.pending.as_ref().map(|p| p.rev), Some(2));
    release_all(&w);
}

/// (X1 c) The PA's answer is missing (no `SCOPE` line): the scope fails CLOSED — 100, the first
/// reason "the PA declared no scope" — and the run pauses plan_approval (high risk) before any
/// creator step, even in auto mode.
#[test]
fn x1_c_a_missing_answer_fails_closed_at_100_and_pauses() {
    let w = by_unit(|phase| {
        (phase == "pa-scope").then(|| turn(&pa_output("I looked around; it is all fine."), None))
    });
    let mut e = engine("x1c", w.clone());
    let (repo_id, _) = repo(&e, "x1c");
    launch_preset(&e, "xc", "feature", Some(&repo_id), HumanConfirm::None);
    e.wait_awaiting("xc", crate::plan_gate::GATE_KIND, 1);
    let scored = settled(&e, "xc", tev::PATH_SCORED, 1);
    assert_eq!(scored[0]["score"], 100);
    assert_eq!(scored[0]["deterministic"], 100);
    assert_eq!(scored[0]["reasons"][0], "the PA declared no scope");
    assert!(scored[0]["signals"].is_null());
    let gate = plan_gate_opened(&e, "xc");
    assert_eq!(
        (gate["reason"].clone(), gate["band"].clone()),
        (json!("high_risk"), json!("70-100"))
    );
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(e.worker.calls().len(), 1, "{:?}", e.worker.calls());
    // Approve: the first unit after the scope step dispatches, once.
    e.core.confirm_gate("xc", approve()).unwrap();
    wait_for("the released unit", || e.worker.calls().len() >= 2);
    assert_eq!(
        e.worker.calls()[1].2,
        "xc:clarify",
        "the unit after the scope step"
    );
    release_all(&w);
}

/// (X1 d) A preset with NO repo: the PA rates the content and audience; a client-facing RFP answer
/// rated 45 lands in 40-69 (raised from the lowest-band baseline 0 through the model part — no
/// graph) and proceeds in auto mode (not high risk). A later, LOWER rating from the PA changes
/// nothing: only the scope step's answer is read, and the floor only ratchets up.
#[test]
fn x1_d_a_repo_less_client_facing_rating_raises_the_band_and_cannot_be_lowered() {
    let w = by_unit(|phase| {
        match phase {
        "pa-scope" => Some(turn(
            &pa_output(
                "RISK {\"score\":45,\"reasons\":[\"client-facing: an RFP answer sent to the customer\"]}",
            ),
            None,
        )),
        "test_plan" => Some(turn(
            &pa_output("RISK {\"score\":0,\"reasons\":[\"on reflection it is trivial\"]}"),
            None,
        )),
        _ => None,
    }
    });
    let mut e = engine("x1d", w.clone());
    put(
        &e,
        "rfp-answer",
        json!([{"catalog": "produce", "id": "draft"}, {"catalog": "critique", "id": "check"}]),
    );
    launch_preset(&e, "xd", "rfp-answer", None, HumanConfirm::None);
    wait_for("the unit after test_plan to dispatch", || {
        e.worker.calls().len() >= 3
    });
    let calls: Vec<String> = e.worker.calls().into_iter().map(|c| c.2).collect();
    assert_eq!(calls, ["xd:pa-scope", "xd:test_plan", "xd:design"]);
    let scored = settled(&e, "xd", tev::PATH_SCORED, 1);
    assert_eq!(scored[0]["deterministic"], 0, "the lowest-band baseline");
    assert_eq!(scored[0]["model"]["add"], 45, "the PA's rating only adds");
    assert_eq!(scored[0]["score"], 45);
    assert!(scored[0]["signals"].is_null(), "no graph is involved");
    let accepted = settled(&e, "xd", tev::PLAN_ACCEPTED, 2);
    let a = &accepted[1];
    assert_eq!(
        (a["band"].clone(), a["high_risk"].clone(), a["by"].clone()),
        (json!("40-69"), json!(false), json!("engine"))
    );
    assert_eq!(
        catalogs(&a["steps"]),
        ["understand", "test_plan", "design", "produce", "critique"],
        "the 40-69 floor on a non-code run"
    );
    assert!(plan_pauses(&mut e, "xd").is_empty());
    // The later, lower RISK line lowered nothing and published nothing.
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(payloads(&e, "xd", tev::PATH_SCORED).len(), 1);
    assert!(payloads(&e, "xd", tev::PLAN_REVISED).is_empty());
    let tp = view(&e, "xd").session.team_plan.unwrap();
    assert_eq!(tp.max_score, 45);
    assert_eq!(tp.accepted.as_ref().unwrap().band, "40-69");
    release_all(&w);
}

/// (X1 e) T4's diff re-score still ratchets after a small declared scope: the PA declares a
/// docs-only scope (0-19, accepted with no pause), then the creator's settled diff touches `src/`
/// (a behavioural diff with no graph fails closed at 100) — `path.scored{basis:"diff"}`, then
/// `plan.revised{floor_raised}` 0-19 → 70-100, and a plan_approval pause (into high risk) before
/// the next unit.
#[test]
fn x1_e_the_diff_rescore_still_ratchets_after_a_small_declared_scope() {
    let w = by_unit(|phase| match phase {
        "pa-scope" => Some(turn(&pa_output("SCOPE {\"touch\":[\"README.md\"]}"), None)),
        "draft" => Some(turn(&pa_output("drafted"), Some(&["src/lib.rs"]))),
        _ => None,
    });
    let mut e = engine("x1e", w.clone());
    let (repo_id, _) = repo(&e, "x1e");
    put(
        &e,
        "docs-refresh",
        json!([{"catalog": "produce", "id": "draft"}, {"catalog": "critique", "id": "check"}]),
    );
    launch_preset(&e, "xe", "docs-refresh", Some(&repo_id), HumanConfirm::None);
    e.wait_awaiting("xe", crate::plan_gate::GATE_KIND, 1);
    let accepted = settled(&e, "xe", tev::PLAN_ACCEPTED, 2);
    assert_eq!(
        accepted[1]["band"], "0-19",
        "the small scope was accepted light"
    );
    let revised = settled(&e, "xe", tev::PLAN_REVISED, 1);
    assert_eq!(revised[0]["reason"], "floor_raised");
    assert_eq!(revised[0]["from_band"], "0-19");
    assert_eq!(revised[0]["to_band"], "70-100");
    assert_eq!(revised[0]["plan_rev"], 3);
    let diff = settled(&e, "xe", tev::PATH_SCORED, 2)
        .into_iter()
        .find(|p| p["basis"] == "diff")
        .expect("path.scored{basis:diff}");
    assert_eq!(diff["score"], 100);
    let gate = plan_gate_opened(&e, "xe");
    assert_eq!(gate["reason"], "into_high_risk");
    let calls: Vec<String> = e.worker.calls().into_iter().map(|c| c.2).collect();
    assert_eq!(calls, ["xe:pa-scope", "xe:draft"]);
    let tp = view(&e, "xe").session.team_plan.unwrap();
    assert_eq!(tp.max_score, 100, "the ratchet");
    release_all(&w);
}

/// (X1) A mid-run edit while the PA is still scoping is refused (propose it once the plan is
/// decided, or at its gate), and a manual-mode run pauses ONCE — at the plan gate after the scope
/// step, never before the read-only scope step itself.
#[test]
fn x1_manual_mode_pauses_once_after_the_scope_step_and_edits_wait_for_the_plan() {
    let (w, go) = gated_worker_with(
        pa_output("RISK {\"score\":10,\"reasons\":[\"internal\"]}"),
        None,
    );
    let mut e = engine("x1m", w.clone());
    put(&e, "notes", json!([{"catalog": "produce", "id": "draft"}]));
    launch_preset(&e, "xm", "notes", None, HumanConfirm::Before(1));
    wait_for("the scope step to dispatch", || e.worker.calls().len() == 1);
    assert!(
        e.awaiting("xm").is_empty(),
        "no pause before the scope step"
    );
    let refused = e
        .core
        .propose_plan(
            "xm",
            plan(json!({"steps": [{"catalog": "critique"}]})),
            "req-scoping",
        )
        .unwrap_err();
    assert!(format!("{refused:#}").contains("scoping"), "{refused:#}");
    go.store(true, AtomicOrdering::SeqCst);
    e.wait_awaiting("xm", crate::plan_gate::GATE_KIND, 1);
    let gate = plan_gate_opened(&e, "xm");
    assert_eq!(gate["reason"], "manual_mode");
    assert_eq!(gate["band"], "0-19");
    assert_eq!(e.awaiting("xm").len(), 1, "{:?}", e.awaiting("xm"));
    release_all(&w);
}

// ── core#633 round 2 ─────────────────────────────────────────────────────────────────────────────

/// (round 2, M1) An UN-TEAMED run (no bus: `transport: none`) on a repo gets no diff re-score
/// (the supervisor never measures its diffs), so a declared scope would never be corrected: even a
/// docs-only `SCOPE` fails closed at 100 and pauses plan_approval as high risk.
#[test]
fn x1_r2_m1_an_unteamed_repo_run_fails_its_scope_closed() {
    let w = by_unit(|phase| {
        (phase == "pa-scope")
            .then(|| turn(&pa_output("SCOPE {\"touch\":[\"docs/guide.md\"]}"), None))
    });
    let rig = rig("x1m1");
    let db = rig.dir.join("core.db").to_string_lossy().into_owned();
    let cfg = TeamConfig::new(None, Some(rig.outbox.clone()))
        .with_final_pass_budget(Duration::from_millis(300));
    let core = Core::spawn_with_engine_team(db, Arc::new(StubDispatcher), w.clone(), cfg);
    let mut e = wire(core, w.clone(), rig);
    let (repo_id, _) = repo(&e, "x1m1");
    launch_preset(&e, "xm1", "feature", Some(&repo_id), HumanConfirm::None);
    e.wait_awaiting("xm1", crate::plan_gate::GATE_KIND, 1);
    let s = view(&e, "xm1").session;
    assert!(
        s.team.as_ref().is_some_and(|t| t.is_unteamed()),
        "the run is un-teamed: {:?}",
        s.team
    );
    let tp = s.team_plan.expect("plan state");
    assert_eq!(tp.max_score, 100, "failed closed, not the docs-only 0");
    let held = tp.pending.expect("held for approval");
    assert_eq!((held.reason.as_str(), held.high_risk), ("high_risk", true));
    assert_eq!(e.worker.calls().len(), 1, "only the scope step ran");
    release_all(&w);
}

/// (round 2, L2) Only the PA seat's answer is the scope: the same `SCOPE` output from any other
/// seat holding the scope unit is not recorded (so the boundary fails it closed).
#[test]
fn x1_r2_l2_only_the_pa_seat_answers_the_scope() {
    let w = by_unit(|_| None);
    let e = engine("x1l2", w.clone());
    put(&e, "notes", json!([{"catalog": "produce", "id": "draft"}]));
    launch_preset(&e, "xl2", "notes", None, HumanConfirm::None);
    wait_for("the scope step to dispatch", || e.worker.calls().len() == 1);
    let v = view(&e, "xl2");
    let unit = v.units[0].clone();
    assert_eq!(unit.assigned_cli.as_deref(), Some("a"));
    let output = |attempt| StepOutput {
        run_id: "xl2".into(),
        unit_ix: 0,
        attempt,
        output: pa_output("RISK {\"score\":5,\"reasons\":[\"x\"]}"),
        status: StepStatus::Ok,
        usage: None,
        files: Vec::new(),
        tools: Vec::new(),
        governed: false,
    };
    let mut other = unit.clone();
    other.assigned_cli = Some("b".into());
    let mut s = v.session.clone();
    super::super::record_scope_answer(&mut s, &other, &output(0));
    assert!(
        s.team_plan
            .as_ref()
            .unwrap()
            .scope
            .as_ref()
            .unwrap()
            .answer
            .is_none(),
        "a non-PA seat's answer is not the scope"
    );
    super::super::record_scope_answer(&mut s, &unit, &output(0));
    let a = s
        .team_plan
        .unwrap()
        .scope
        .unwrap()
        .answer
        .expect("the PA's answer");
    assert_eq!(a.by, "a");
    release_all(&w);
}

/// (round 2, M2) A restart between the scope step's fold and its boundary's write (the run
/// persisted with the scope step Done, the cursor past it, and the plan still held) must not
/// finalize the run with only `pa-scope` run: the exec-mode restart re-drive takes it through the
/// boundary (`advance_or_pause`), which decides the plan and puts its units in. (The restarted
/// engine has no team publisher, so the run then waits on its transport — never "completed".)
#[test]
fn x1_r2_m2_a_restart_after_the_scope_fold_decides_the_plan_not_completes_the_run() {
    let w = by_unit(|_| None);
    let e = engine("x1m2", w.clone());
    put(&e, "notes", json!([{"catalog": "produce", "id": "draft"}]));
    launch_preset(&e, "xm2", "notes", None, HumanConfirm::None);
    wait_for("the scope step to dispatch", || e.worker.calls().len() == 1);
    settled(&e, "xm2", tev::PLAN_ACCEPTED, 1);
    let Engine {
        core, worker, rig, ..
    } = e;
    let db = rig.dir.join("core.db").to_string_lossy().into_owned();
    drop(core);
    std::thread::sleep(Duration::from_millis(500));
    // The state a crash after the fold leaves: the scope unit Done with its answer recorded, the
    // cursor past it, the plan still held (apply_scope's write never landed).
    {
        use wicked_apps_core::ToNode;
        let mut store = wicked_apps_core::open_store_any(Some(&db)).expect("store");
        let mut s = crate::domain::get_session(&store, "xm2").unwrap().unwrap();
        let mut units = crate::domain::session_units(&store, "xm2").unwrap();
        assert_eq!(units.len(), 1, "rev 1 is the scope step alone");
        units[0].status = crate::domain::UnitStatus::Done;
        units[0].last_attempt = Some(0);
        crate::domain::put_node(&mut store, units[0].to_node()).unwrap();
        s.unit_ix = 1;
        s.status = SessionStatus::Executing;
        s.team_plan.as_mut().unwrap().scope.as_mut().unwrap().answer =
            Some(crate::plan_gate::ScopeAnswer {
                ord: 1,
                attempt: 0,
                by: "a".into(),
                lines: "RISK {\"score\":10,\"reasons\":[\"internal notes\"]}".into(),
            });
        crate::domain::put_node(&mut store, s.to_node()).unwrap();
    }
    release_all(&worker);
    drop(worker);
    // The restart, in exec-mediation mode (the one mode that re-drives `Executing` runs at boot),
    // with the team publisher on the same bus as before.
    let w2 = by_unit(|_| None);
    let exec_bus = rig.dir.join("exec-bus.db").to_string_lossy().into_owned();
    let cfg = team_cfg(&rig, Duration::from_millis(300));
    let core = Core::spawn_inner(
        db,
        Arc::new(StubDispatcher),
        w2.clone(),
        Some(exec_bus),
        Some(cfg),
    );
    let e = wire(core, w2.clone(), rig);
    wait_for(
        "the re-drive to decide the plan (or finalize the run)",
        || {
            let v = view(&e, "xm2");
            v.session.status == SessionStatus::Completed
                || v.session
                    .team_plan
                    .as_ref()
                    .is_some_and(|t| t.scope.is_none())
        },
    );
    let v = view(&e, "xm2");
    assert_ne!(
        v.session.status,
        SessionStatus::Completed,
        "never completed with only pa-scope run"
    );
    let tp = v.session.team_plan.unwrap();
    assert_eq!((tp.rev, tp.max_score), (2, 10), "the scoped plan is rev 2");
    let ids: Vec<String> = v.units.iter().map(|u| u.id.clone()).collect();
    assert_eq!(ids, ["xm2:pa-scope", "xm2:draft"]);
    release_all(&w2);
}
