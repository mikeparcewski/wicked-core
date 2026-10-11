//! Seam C2 (DES-TEAMING-002 §8.4, §14): presets — rows in the core store, the built-in seeding,
//! `Core::{put,delete,list}_preset(s)`, and `launch_run` resolving `workflow` as a preset name.
//!
//! Every acceptance item is driven through the real engine (`Core` actor → resolve → compose →
//! plan → persist), with a stub council and a runner that holds every unit, so the persisted unit
//! list is what the launch planned and nothing ran. Expected values are fixed literals, not
//! re-derived from the code under test.
//!
//! - (a) built-in `feature` launches the unit list C1(a) fixed (the §11.2 composition, bold cells
//!   included: `test` and `review` on the evaluator role);
//! - (b) `put_preset("my-flow")` then a launch naming it launches that selection;
//! - (c) a project-scoped preset shadows a built-in only for that project;
//! - (d) a built-in cannot be deleted (nor overwritten globally);
//! - (e) a bus `wicked.crew.run.requested {workflow:"my-flow"}` and a campaign node naming it both
//!   launch it — resolution is the engine's, no crew involved;
//! - (f) presets survive a restart (a fresh `Core` over the same store).

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use wicked_council::types::{Category, Confidence, Dispatcher, InputMode, Vote};
use wicked_council::{AgenticCli, CouncilTask};

use wicked_core::{
    BusDb, BusEmit, CampaignDef, CampaignNode, Core, CoreEvent, DenialGatePolicy, EntityMode,
    FailurePolicy, HumanConfirm, LaunchSpec, PlanStep, PresetSpec, RunSpec, StepInput, StepOutput,
    StepRunner, StepStatus, WorkUnit,
};

const EVIDENCE_FLOOR_PIN: &str = "e2e7af1db9e48454";

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

/// Holds every unit until the test ends: the plan is persisted, nothing completes, so the unit
/// list read back is exactly what the launch planned. The one exception is the PA's read-only
/// `pa-scope` step (DES-TEAMING-002 X1: a preset declares no touch set, so its PA scopes it
/// first): it answers at once with no `SCOPE` line, so the plan fails closed at 100 — the top
/// band these tests pin — and is decided at that step's boundary.
struct Hold(Arc<(Mutex<bool>, Condvar)>);
impl StepRunner for Hold {
    fn run_unit(&self, input: &StepInput) -> StepOutput {
        if input.unit.id.ends_with(":pa-scope") {
            return StepOutput {
                run_id: input.run_id.clone(),
                unit_ix: input.unit_ix,
                attempt: input.attempt,
                output: "looked around; nothing to declare".into(),
                status: StepStatus::Ok,
                usage: None,
                files: Vec::new(),
                tools: Vec::new(),
                governed: false,
            };
        }
        let (lock, cv) = &*self.0;
        let mut done = lock.lock().unwrap();
        while !*done {
            let (g, _) = cv.wait_timeout(done, Duration::from_millis(50)).unwrap();
            done = g;
        }
        StepOutput {
            run_id: input.run_id.clone(),
            unit_ix: input.unit_ix,
            attempt: input.attempt,
            output: "held".into(),
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

fn tmp_dir(name: &str) -> std::path::PathBuf {
    let dir =
        std::env::temp_dir().join(format!("wicked-core-presets-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

struct Rig {
    core: Core,
    gate: Arc<(Mutex<bool>, Condvar)>,
}
impl Drop for Rig {
    fn drop(&mut self) {
        let (lock, cv) = &*self.gate;
        *lock.lock().unwrap() = true;
        cv.notify_all();
    }
}

fn spawn(db: &str) -> Rig {
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let core = Core::spawn_with_engine(
        db.to_string(),
        Arc::new(StubDispatcher),
        Arc::new(Hold(gate.clone())),
    );
    Rig { core, gate }
}

fn spec(run: &str, workflow: &str, project_id: Option<&str>) -> LaunchSpec {
    LaunchSpec {
        problem: "add SSO login".into(),
        clis: vec![cli("a"), cli("b")],
        entity_mode: EntityMode::Shared,
        session_id: run.into(),
        human_confirm: HumanConfirm::None,
        auto_deliver: false,
        repo_ref: None,
        base_ref: None,
        workflow: Some(workflow.into()),
        project_id: project_id.map(str::to_string),
        extra_write_roots: Vec::new(),
        extra_read_roots: Vec::new(),
        project_graph: None,
        plan: None,
        deliver_step: None,
        exclude_seats: Vec::new(),
        evidence_root: None,
        primary: None,
        reduced_assurance: false,
        deliverables: Vec::new(),
        qe_acceptance: Default::default(),
    }
}

/// The run's persisted units, once planning has written them (the launch returns before the
/// deferred plan lands) — for a plan its PA scopes (X1), once the scoped plan is in, not just the
/// `pa-scope` step of rev 1.
fn units_of(core: &Core, run: &str) -> Vec<WorkUnit> {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if let Ok(views) = core.sessions_detail() {
            if let Some(v) = views.iter().find(|v| v.session.id == run) {
                let scoping = v
                    .session
                    .team_plan
                    .as_ref()
                    .is_some_and(|t| t.scope.is_some());
                if !v.units.is_empty() && !scoping {
                    return v.units.clone();
                }
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("run {run} planned no units within 10 s");
}

/// One unit as the acceptance compares it: `(phase id, stage, role, gate, validator pin)`.
fn row(run: &str, u: &WorkUnit) -> (String, String, String, String, Option<String>) {
    let j = |v: serde_json::Value| match v {
        serde_json::Value::String(s) => s,
        other => other.to_string(),
    };
    (
        u.id.strip_prefix(&format!("{run}:"))
            .unwrap_or(&u.id)
            .to_string(),
        j(serde_json::to_value(u.stage).unwrap()),
        j(serde_json::to_value(u.role).unwrap()),
        j(serde_json::to_value(u.gate).unwrap()),
        u.validator.as_ref().map(wicked_core::pin),
    )
}

fn rows(run: &str, units: &[WorkUnit]) -> Vec<(String, String, String, String, Option<String>)> {
    units.iter().map(|u| row(run, u)).collect()
}

fn r(
    id: &str,
    stage: &str,
    role: &str,
    gate: &str,
    pin: Option<&str>,
) -> (String, String, String, String, Option<String>) {
    (
        id.into(),
        stage.into(),
        role.into(),
        gate.into(),
        pin.map(str::to_string),
    )
}

/// C1(a)'s `feature` composition as a unit list (fixed values: §11.2's row, with its two bold
/// cells — `test` and `review` on the evaluator role), launched as a TEAM plan (DES-TEAMING-002
/// T3): a preset declares no `touch`, so (X1) its PA scopes it first — the read-only `pa-scope`
/// step, ord 1 — and this rig's PA declares nothing, so the plan fails closed at 100 and floor
/// fill inserts the 70-100 floor phases it lacks (`test_plan`, `architecture`,
/// `security_review`; `deliver` only for a delivering run) at their catalog-order positions.
fn feature_units() -> Vec<(String, String, String, String, Option<String>)> {
    let hc = r#"{"human_confirm":{"unconditional":false}}"#;
    let hci = r#"{"human_confirm_if":"verdict_not_pass"}"#;
    let f = Some(EVIDENCE_FLOOR_PIN);
    vec![
        r("pa-scope", "recon", "neutral", "auto", None),
        r("clarify", "recon", "neutral", hc, None),
        r("test_plan", "test", "neutral", "auto", None),
        r("design", "recon", "neutral", "auto", None),
        r("architecture", "recon", "neutral", "auto", None),
        r("build", "build", "creator", "auto", f),
        r("adversarial-review", "review", "evaluator", hc, f),
        r("test", "test", "evaluator", hci, f),
        r("review", "review", "evaluator", "auto", None),
        r("security_review", "review", "evaluator", "auto", f),
    ]
}

fn step(catalog: &str, id: &str, depends_on: &[&str]) -> PlanStep {
    PlanStep {
        catalog: catalog.into(),
        id: id.into(),
        depends_on: if depends_on.is_empty() {
            None
        } else {
            Some(depends_on.iter().map(|s| s.to_string()).collect())
        },
        ..Default::default()
    }
}

/// `my-flow`: understand → build → review.
fn my_flow_steps() -> Vec<PlanStep> {
    vec![
        step("understand", "scope", &[]),
        step("build", "make", &["scope"]),
        step("review", "check", &["make"]),
    ]
}

/// `my-flow`'s units as a team plan (T3): the 70-100 floor fills `test_plan`, `design`,
/// `architecture` and `security_review` around its three steps.
fn my_flow_units() -> Vec<(String, String, String, String, Option<String>)> {
    let f = Some(EVIDENCE_FLOOR_PIN);
    vec![
        r("pa-scope", "recon", "neutral", "auto", None),
        r("scope", "recon", "neutral", "auto", None),
        r("test_plan", "test", "neutral", "auto", None),
        r("design", "recon", "neutral", "auto", None),
        r("architecture", "recon", "neutral", "auto", None),
        r("make", "build", "creator", "auto", f),
        r("check", "review", "evaluator", "auto", f),
        r("security_review", "review", "evaluator", "auto", f),
    ]
}

fn put(core: &Core, name: &str, project: Option<&str>, steps: Vec<PlanStep>) {
    core.put_preset(PresetSpec {
        name: name.into(),
        project_id: project.map(str::to_string),
        steps,
        created_by: "api".into(),
    })
    .unwrap_or_else(|e| panic!("put_preset {name}: {e}"));
}

/// (a) — and "every existing workflow id still launches": the built-in `feature` preset is what a
/// launch naming `feature` runs.
#[test]
fn a_builtin_feature_launches_the_c1_unit_list() {
    let dir = tmp_dir("a");
    let db = dir.join("estate.db").to_str().unwrap().to_string();
    let rig = spawn(&db);
    let listed = rig.core.list_presets(None).unwrap();
    let feature = listed
        .iter()
        .find(|p| p.name == "feature")
        .expect("the built-in feature preset is listed");
    assert_eq!(feature.scope, "global");
    assert_eq!(feature.created_by, "builtin");

    rig.core.launch_run(spec("ra", "feature", None)).unwrap();
    assert_eq!(rows("ra", &units_of(&rig.core, "ra")), feature_units());
}

/// A workflow id with no preset of that name still launches its registered def (every existing
/// id keeps launching until its M-seam turns it into a preset).
#[test]
fn a_workflow_without_a_preset_still_launches_its_def() {
    let dir = tmp_dir("wf");
    let db = dir.join("estate.db").to_str().unwrap().to_string();
    let rig = spawn(&db);
    // (M1) `bug` is a preset now, so a registered def stands in for "an id with no preset".
    rig.core
        .register_workflow(
            r#"{"id":"no-preset-flow","phases":[
              {"id":"look","kind":"recon","gate":"auto"},
              {"id":"note","kind":"recon","gate":"auto","depends_on":["look"]}]}"#,
        )
        .unwrap();
    rig.core
        .launch_run(spec("rw", "no-preset-flow", None))
        .unwrap();
    let ids: Vec<String> = rows("rw", &units_of(&rig.core, "rw"))
        .into_iter()
        .map(|r| r.0)
        .collect();
    assert_eq!(ids, ["look", "note"]);
    let err = rig
        .core
        .launch_run(spec("rx", "no-such-flow", None))
        .expect_err("an unknown name is refused");
    assert!(
        err.to_string().contains("unknown workflow `no-such-flow`"),
        "{err}"
    );
}

/// (b) preset launch.
#[test]
fn b_put_then_launch_runs_the_selection() {
    let dir = tmp_dir("b");
    let db = dir.join("estate.db").to_str().unwrap().to_string();
    let rig = spawn(&db);
    put(&rig.core, "my-flow", None, my_flow_steps());
    let events = rig.core.subscribe();
    rig.core.launch_run(spec("rb", "my-flow", None)).unwrap();
    assert_eq!(rows("rb", &units_of(&rig.core, "rb")), my_flow_units());
    // The run announces the preset name it launched (studio's workflow label).
    let started = events
        .try_iter()
        .find_map(|e| match e {
            CoreEvent::SessionStarted {
                session,
                workflow_id,
                ..
            } if session == "rb" => Some(workflow_id),
            _ => None,
        })
        .expect("SessionStarted for rb");
    assert_eq!(started.as_deref(), Some("my-flow"));
}

/// (b), refusals: a put whose steps do not compose is refused with the catalog's named reason,
/// and a bad name is refused.
#[test]
fn b_put_refuses_steps_that_do_not_compose_and_bad_names() {
    let dir = tmp_dir("b2");
    let db = dir.join("estate.db").to_str().unwrap().to_string();
    let rig = spawn(&db);
    let weak = PlanStep {
        validator_pin: Some(None),
        ..step("build", "b", &[])
    };
    let err = rig
        .core
        .put_preset(PresetSpec {
            name: "weak".into(),
            project_id: None,
            steps: vec![weak],
            created_by: "api".into(),
        })
        .expect_err("a pin removal is refused");
    assert!(
        err.to_string()
            .starts_with("preset_invalid_steps: pin_removed"),
        "{err}"
    );
    let err = rig
        .core
        .put_preset(PresetSpec {
            name: "has space".into(),
            project_id: None,
            steps: my_flow_steps(),
            created_by: "api".into(),
        })
        .expect_err("a bad name is refused");
    assert!(err.to_string().starts_with("preset_invalid_name"), "{err}");
    assert!(rig
        .core
        .list_presets(None)
        .unwrap()
        .iter()
        .all(|p| p.name != "weak" && p.name != "has space"));
}

/// (c) a project-scoped preset shadows a built-in only for that project.
#[test]
fn c_a_project_preset_shadows_a_builtin_only_there() {
    let dir = tmp_dir("c");
    let db = dir.join("estate.db").to_str().unwrap().to_string();
    let rig = spawn(&db);
    let project = rig.core.project_create("shadow", None).unwrap();
    put(
        &rig.core,
        "feature",
        Some(&project.id),
        vec![step("understand", "only", &[])],
    );

    rig.core
        .launch_run(spec("rc1", "feature", Some(&project.id)))
        .unwrap();
    assert_eq!(
        rows("rc1", &units_of(&rig.core, "rc1")),
        vec![r("only", "recon", "neutral", "auto", None)]
    );
    rig.core.launch_run(spec("rc2", "feature", None)).unwrap();
    assert_eq!(rows("rc2", &units_of(&rig.core, "rc2")), feature_units());

    let in_project = rig.core.list_presets(Some(&project.id)).unwrap();
    let f = in_project.iter().find(|p| p.name == "feature").unwrap();
    assert_eq!(f.scope, format!("project:{}", project.id));
    let global = rig.core.list_presets(None).unwrap();
    let f = global.iter().find(|p| p.name == "feature").unwrap();
    assert_eq!(f.scope, "global");

    // Deleting the shadow restores the built-in for that project.
    assert!(rig
        .core
        .delete_preset("feature", Some(&project.id))
        .unwrap());
    rig.core
        .launch_run(spec("rc3", "feature", Some(&project.id)))
        .unwrap();
    assert_eq!(rows("rc3", &units_of(&rig.core, "rc3")), feature_units());

    // A project scope must name a project.
    let err = rig
        .core
        .put_preset(PresetSpec {
            name: "x".into(),
            project_id: Some("proj_nope".into()),
            steps: my_flow_steps(),
            created_by: "api".into(),
        })
        .expect_err("unknown project");
    assert!(
        err.to_string().starts_with("preset_unknown_project"),
        "{err}"
    );
}

/// (d) a built-in cannot be deleted, nor overwritten globally; a user preset can be deleted.
#[test]
fn d_a_builtin_cannot_be_deleted() {
    let dir = tmp_dir("d");
    let db = dir.join("estate.db").to_str().unwrap().to_string();
    let rig = spawn(&db);
    let err = rig
        .core
        .delete_preset("feature", None)
        .expect_err("a built-in is refused");
    assert!(
        err.to_string().starts_with("preset_builtin_readonly"),
        "{err}"
    );
    let err = rig
        .core
        .put_preset(PresetSpec {
            name: "feature".into(),
            project_id: None,
            steps: my_flow_steps(),
            created_by: "api".into(),
        })
        .expect_err("a global overwrite of a built-in is refused");
    assert!(
        err.to_string().starts_with("preset_builtin_readonly"),
        "{err}"
    );
    rig.core.launch_run(spec("rd", "feature", None)).unwrap();
    assert_eq!(rows("rd", &units_of(&rig.core, "rd")), feature_units());

    put(&rig.core, "mine", None, my_flow_steps());
    assert!(rig.core.delete_preset("mine", None).unwrap());
    assert!(
        !rig.core.delete_preset("mine", None).unwrap(),
        "already gone"
    );
    assert!(rig
        .core
        .list_presets(None)
        .unwrap()
        .iter()
        .all(|p| p.name != "mine"));
    let err = rig
        .core
        .launch_run(spec("rd2", "mine", None))
        .expect_err("a deleted preset no longer launches");
    assert!(err.to_string().contains("unknown workflow `mine`"), "{err}");
}

/// (e) the bus launch bridge resolves a preset name with no crew involvement.
#[test]
fn e_a_bus_run_requested_naming_a_preset_launches_it() {
    let dir = tmp_dir("e-bus");
    let db = dir.join("estate.db").to_str().unwrap().to_string();
    let bus_db = dir.join("bus.db").to_str().unwrap().to_string();
    let rig = spawn(&db);
    put(&rig.core, "my-flow", None, my_flow_steps());
    let bridge = rig
        .core
        .connect_bus(bus_db.clone(), vec![cli("a"), cli("b")]);
    let bus = BusDb::open(&bus_db).unwrap();
    bus.emit(&BusEmit::new(
        wicked_core::RUN_REQUESTED,
        "wicked-cli",
        "cli.run",
        serde_json::json!({
            "workflow": "my-flow",
            "problem": "add SSO login",
            "args": { "session_id": "re-bus" }
        }),
    ))
    .unwrap();
    assert_eq!(
        rows("re-bus", &units_of(&rig.core, "re-bus")),
        my_flow_units()
    );
    bridge.stop();
}

/// (e) a campaign node naming a preset launches it (the campaign driver resolves through the
/// same engine path).
#[test]
fn e_a_campaign_node_naming_a_preset_launches_it() {
    let dir = tmp_dir("e-camp");
    let db = dir.join("estate.db").to_str().unwrap().to_string();
    let rig = spawn(&db);
    put(&rig.core, "my-flow", None, my_flow_steps());
    let def = CampaignDef {
        id: "camp".into(),
        nodes: vec![CampaignNode {
            node_id: "n1".into(),
            run_spec: RunSpec {
                problem: "add SSO login".into(),
                clis: vec![cli("a"), cli("b")],
                entity_mode: EntityMode::Shared,
                human_confirm: HumanConfirm::None,
                repo_ref: None,
                workflow_id: Some("my-flow".into()),
            },
        }],
        name: "camp".into(),
        edges: vec![],
        policy: FailurePolicy::FailFast,
        max_concurrency: 1,
        denial_gate: DenialGatePolicy::default(),
    };
    rig.core.launch_campaign(def).unwrap();
    let run = "camp:n1:a0";
    assert_eq!(rows(run, &units_of(&rig.core, run)), my_flow_units());
}

/// (f) presets survive a restart: a fresh Core over the same store lists and launches them.
#[test]
fn f_presets_survive_a_restart() {
    let dir = tmp_dir("f");
    let db = dir.join("estate.db").to_str().unwrap().to_string();
    {
        let rig = spawn(&db);
        put(&rig.core, "my-flow", None, my_flow_steps());
    }
    // The actor exits once its last handle drops; give it a moment to release the store.
    std::thread::sleep(Duration::from_millis(300));
    let rig = spawn(&db);
    let listed = rig.core.list_presets(None).unwrap();
    let mine = listed
        .iter()
        .find(|p| p.name == "my-flow")
        .expect("my-flow survives the restart");
    assert_eq!(mine.steps, my_flow_steps());
    assert_eq!(mine.created_by, "api");
    // Seeding at the second boot is idempotent: still exactly one feature, still the built-in.
    assert_eq!(listed.iter().filter(|p| p.name == "feature").count(), 1);
    rig.core.launch_run(spec("rf", "my-flow", None)).unwrap();
    assert_eq!(rows("rf", &units_of(&rig.core, "rf")), my_flow_units());
}

/// The built-in `feature` preset's steps ARE C1's §11.2 mapping fixture — the preset cannot drift
/// from the composition C1(a) pinned against today's def.
#[test]
fn the_builtin_feature_steps_are_the_c1_mapping() {
    let raw = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/catalog/mappings.json"),
    )
    .unwrap();
    let maps: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let builtins = wicked_core::builtin_presets();
    let names: Vec<&str> = builtins.iter().map(|(n, _)| *n).collect();
    assert_eq!(
        names,
        [
            "bug",
            "capture-learnings",
            "chat",
            "demo",
            "domain-extraction",
            "editor-plugin",
            "feature",
            "interactive-chat",
            "interactive-draft",
            "interactive-edit",
            "learn",
            "mcp-server",
            "migration",
            "onboarding",
            "qe-author-tests",
            "steering-author"
        ]
    );
    // `demo` (M9b) replaces interactive-demo with the garden demo skill's flow instead of mapping
    // its phases, so it has no §11.2 row; `m9b_demo_*` pins its steps.
    // `editor-plugin` (X3) is a new workflow, not a migrated consumer: it has no §11.2 row either;
    // `x3_editor_plugin_*` pins its steps. `learn` (DES-learn-workflow) is new too; `learn_*` pins it.
    for (name, steps) in builtins
        .into_iter()
        .filter(|(n, _)| *n != "demo" && *n != "editor-plugin" && *n != "learn")
    {
        let want: Vec<PlanStep> = serde_json::from_value(maps[name]["steps"].clone())
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(steps, want, "the built-in `{name}` is its C1 mapping");
    }
}

/// M3 (DES-TEAMING-002 §14 (ii)): a launch naming `chat` runs the built-in preset and plans the
/// C1(a) unit list for chat — explore → `understand`, identical to the deleted def (§11.2 has no
/// bold cell for chat). Chat is NOT a creator plan (one read-only step), so X1 adds no `pa-scope`
/// step: the plan is decided at launch as rev 1 (`preset: "chat"`, nothing held for the PA), the
/// floor is empty (no phase added) and nothing pauses in auto mode.
///
/// Evaluator ≠ creator (§11.3 "Evaluator seat"): chat has no evaluator unit, so there is nothing
/// to move off the creator seat — a ONE-seat roster launches and plans, it is not refused
/// `NoEligibleSeat`.
#[test]
fn m3_chat_launches_its_c1_unit_list_with_no_scope_step() {
    let dir = tmp_dir("chat");
    let db = dir.join("estate.db").to_str().unwrap().to_string();
    let rig = spawn(&db);
    let chat = rig
        .core
        .list_presets(None)
        .unwrap()
        .into_iter()
        .find(|p| p.name == "chat")
        .expect("the built-in chat preset is listed");
    assert_eq!(
        (chat.scope.as_str(), chat.created_by.as_str()),
        ("global", "builtin")
    );

    let mut one_seat = spec("rchat", "chat", None);
    one_seat.clis = vec![cli("a")];
    rig.core.launch_run(one_seat).unwrap();
    let units = units_of(&rig.core, "rchat");
    assert_eq!(
        rows("rchat", &units),
        vec![r("explore", "recon", "neutral", "auto", None)]
    );
    assert!(units.iter().all(|u| !u.executes_code), "chat is read-only");
    let view = rig
        .core
        .sessions_detail()
        .unwrap()
        .into_iter()
        .find(|v| v.session.id == "rchat")
        .unwrap();
    let plan = view
        .session
        .team_plan
        .expect("a preset launch is a team plan");
    assert_eq!(plan.preset.as_deref(), Some("chat"));
    assert!(
        plan.scope.is_none(),
        "no PA scope step for a read-only plan"
    );
    assert!(plan.pending.is_none(), "nothing held for approval");
    assert_eq!((plan.rev, plan.accepted_rev), (1, 1));
    assert!(!plan.accepted_high_risk);
    assert_eq!(
        plan.max_score, 0,
        "a plan with no creator step scores 0 at launch"
    );
}

/// M2 (DES-TEAMING-002 §14): a launch naming `migration` runs the built-in preset, not the
/// shadowed def. §11.2's bold cells reach the units: cutover and cleanup are creators carrying the
/// evidence floor, and cleanup now executes code. cutover keeps its UNCONDITIONAL human gate. As a
/// creator plan with no declared `touch`, the PA scopes it first (`pa-scope`, ord 1, X1), and this
/// rig's PA declares nothing, so the plan fails closed at 100 and floor fill adds the 70-100 phases
/// migration lacks (`test_plan`, `architecture`, `review`, `security_review`) at their catalog-order
/// positions.
#[test]
fn m2_migration_launches_the_preset_with_its_bold_cells() {
    let dir = tmp_dir("migration");
    let db = dir.join("estate.db").to_str().unwrap().to_string();
    let rig = spawn(&db);
    let preset = rig
        .core
        .list_presets(None)
        .unwrap()
        .into_iter()
        .find(|p| p.name == "migration")
        .expect("the built-in migration preset is listed");
    assert_eq!(
        (preset.scope.as_str(), preset.created_by.as_str()),
        ("global", "builtin")
    );

    rig.core
        .launch_run(spec("rmig", "migration", None))
        .unwrap();
    let units = units_of(&rig.core, "rmig");
    let hc = r#"{"human_confirm":{"unconditional":false}}"#;
    let hcu = r#"{"human_confirm":{"unconditional":true}}"#;
    let hci = r#"{"human_confirm_if":"verdict_not_pass"}"#;
    let f = Some(EVIDENCE_FLOOR_PIN);
    assert_eq!(
        rows("rmig", &units),
        vec![
            r("pa-scope", "recon", "neutral", "auto", None),
            r("test_plan", "test", "neutral", "auto", None),
            r("plan", "recon", "neutral", hc, None),
            r("architecture", "recon", "neutral", "auto", None),
            r("execute", "build", "creator", "auto", f),
            r("cutover", "build", "creator", hcu, f),
            r("verify", "test", "evaluator", hci, f),
            r("cleanup", "build", "creator", "auto", f),
            r("review", "review", "evaluator", "auto", f),
            r("security_review", "review", "evaluator", "auto", f),
        ]
    );
    let cleanup = units
        .iter()
        .find(|u| u.id == "rmig:cleanup")
        .expect("cleanup is planned");
    assert!(
        cleanup.executes_code,
        "cleanup removes the old path: code work"
    );
    let view = rig
        .core
        .sessions_detail()
        .unwrap()
        .into_iter()
        .find(|v| v.session.id == "rmig")
        .unwrap();
    let plan = view
        .session
        .team_plan
        .expect("a preset launch is a team plan");
    assert_eq!(plan.preset.as_deref(), Some("migration"));
}

/// M10 (DES-TEAMING-002 §14): a launch naming `qe-author-tests` runs the built-in preset: the
/// author is the code-writing creator, verify the evidence-pinned Tool step, review the evaluator
/// gate. As a creator plan the PA scopes it first and floor fill applies (§11.3).
#[test]
fn m10_qe_author_tests_launches_the_preset() {
    let dir = tmp_dir("qe");
    let db = dir.join("estate.db").to_str().unwrap().to_string();
    let rig = spawn(&db);
    rig.core
        .launch_run(spec("rqe", "qe-author-tests", None))
        .unwrap();
    let units = units_of(&rig.core, "rqe");
    let hci = r#"{"human_confirm_if":"verdict_not_pass"}"#;
    let f = Some(EVIDENCE_FLOOR_PIN);
    // With no declared scope the rig's PA fails closed at 100: floor fill adds `test_plan`,
    // `design`, `architecture` and `security_review` (the plan already has build and review).
    assert_eq!(
        rows("rqe", &units),
        vec![
            r("pa-scope", "recon", "neutral", "auto", None),
            r("recon", "recon", "neutral", "auto", None),
            r("test_plan", "test", "neutral", "auto", None),
            r("design", "recon", "neutral", "auto", None),
            r("architecture", "recon", "neutral", "auto", None),
            r("author", "build", "creator", "auto", f),
            r("verify", "test", "neutral", "auto", f),
            r("review", "review", "evaluator", hci, f),
            r("security_review", "review", "evaluator", "auto", f),
        ]
    );
    let by = |id: &str| {
        units
            .iter()
            .find(|u| u.id == format!("rqe:{id}"))
            .unwrap_or_else(|| panic!("no `{id}` in {:?}", rows("rqe", &units)))
    };
    assert_eq!(units[0].id, "rqe:pa-scope");
    assert_eq!(
        row("rqe", by("author")),
        r(
            "author",
            "build",
            "creator",
            "auto",
            Some(EVIDENCE_FLOOR_PIN)
        )
    );
    let verify = by("verify");
    assert!(verify.tool_cmd.is_some(), "verify is a Tool step");
    assert_eq!(
        verify.validator.as_ref().map(wicked_core::pin).as_deref(),
        Some(EVIDENCE_FLOOR_PIN)
    );
    assert_eq!(row("rqe", by("review")).2, "evaluator");
}

/// M7 (DES-TEAMING-002 §14): a launch naming `capture-learnings` runs the built-in preset. It is
/// a creator plan on a repo (capture → `produce`), so the PA scopes it first (`pa-scope`, ord 1, X1),
/// and this rig's PA declares nothing, so floor fill adds the 70-100 phases it lacks. Every step
/// keeps the repo-learn skill, and capture keeps the capture-report floor (BC-80): a run that
/// submits nothing cannot report `completed`.
#[test]
fn m7_capture_learnings_launches_the_preset_with_its_capture_report_floor() {
    let dir = tmp_dir("capture");
    let db = dir.join("estate.db").to_str().unwrap().to_string();
    let rig = spawn(&db);
    rig.core
        .launch_run(spec("rcap", "capture-learnings", None))
        .unwrap();
    let units = units_of(&rig.core, "rcap");
    // capture → `produce` is the one creator, and it writes nothing (core#649 option A): its PA may
    // answer `SCOPE {"touch":[]}`, which scores 0. This rig's PA answers nothing, so the plan fails
    // closed at 100 and floor fill adds the 70-100 phases a non-code run owes (no diff-floored
    // `security_review`, core#649 / #847).
    assert_eq!(
        rows("rcap", &units),
        vec![
            r("pa-scope", "recon", "neutral", "auto", None),
            r("churn", "recon", "neutral", "auto", None),
            r("hotspots", "recon", "neutral", "auto", None),
            r("test_plan", "test", "neutral", "auto", None),
            r("design", "recon", "neutral", "auto", None),
            r("architecture", "recon", "neutral", "auto", None),
            r("capture", "build", "creator", "auto", None),
            r("critique", "review", "evaluator", "auto", None),
        ]
    );
    let by = |id: &str| {
        units
            .iter()
            .find(|u| u.id == format!("rcap:{id}"))
            .unwrap_or_else(|| panic!("{id} is planned"))
    };
    for id in ["churn", "hotspots", "capture"] {
        assert_eq!(
            by(id).skill_ref.as_deref(),
            Some("wicked-garden-repo-learn"),
            "{id}"
        );
    }
    assert!(by("capture").requires_capture_report);
    let capture = wicked_core::builtin_presets()
        .into_iter()
        .find(|(n, _)| *n == "capture-learnings")
        .unwrap()
        .1;
    let plan = wicked_core::PlanSteps {
        steps: capture,
        ..Default::default()
    };
    assert!(plan.writes_nothing(), "every creator step writes nothing");
    assert!(!by("churn").requires_capture_report);
    assert!(
        by("capture")
            .instructions
            .as_deref()
            .is_some_and(|t| t.contains("wicked-capture-report")),
        "capture's instruction mandates the report marker the floor reads"
    );
}

/// M8 (DES-TEAMING-002 §14): a launch naming `steering-author` runs the built-in preset. propose
/// is now a `produce` creator (the bold cell: kind recon → build) and keeps its UNCONDITIONAL human
/// gate, the TH-12 propose-as-gate crew lands the approved rules on (crew#388), from the `propose`
/// unit's reply (#789). As a creator plan with no declared scope, the PA scopes it first and floor
/// fill applies (§11.3).
#[test]
fn m8_steering_author_launches_the_preset_with_its_propose_gate() {
    let dir = tmp_dir("steering");
    let db = dir.join("estate.db").to_str().unwrap().to_string();
    let rig = spawn(&db);
    rig.core
        .launch_run(spec("rsa", "steering-author", None))
        .unwrap();
    let units = units_of(&rig.core, "rsa");
    let hcu = r#"{"human_confirm":{"unconditional":true}}"#;
    assert_eq!(
        rows("rsa", &units),
        vec![
            r("pa-scope", "recon", "neutral", "auto", None),
            r("analyze", "recon", "neutral", "auto", None),
            r("test_plan", "test", "neutral", "auto", None),
            r("design", "recon", "neutral", "auto", None),
            r("architecture", "recon", "neutral", "auto", None),
            r("propose", "build", "creator", hcu, None),
            r("critique", "review", "evaluator", "auto", None),
        ]
    );
    let propose = units
        .iter()
        .find(|u| u.id == "rsa:propose")
        .expect("propose is planned under its own id: crew's landing finds it by id");
    assert_eq!(
        row("rsa", propose),
        r("propose", "build", "creator", hcu, None)
    );
    assert!(!propose.executes_code, "the run writes nothing to the tree");
    // (core#649 option A) Its one creator writes nothing, so its PA may scope it
    // `SCOPE {"touch":[]}` (0); this rig's PA answers nothing, so it floors at 100 as a non-code
    // run (no diff-floored `security_review`, #847).
    let steps = wicked_core::builtin_presets()
        .into_iter()
        .find(|(n, _)| *n == "steering-author")
        .unwrap()
        .1;
    assert!(wicked_core::PlanSteps {
        steps,
        ..Default::default()
    }
    .writes_nothing());
    assert!(
        propose
            .instructions
            .as_deref()
            .is_some_and(|t| t.contains("```json") && t.contains("Do not write it to any file")),
        "the reply is the proposal (#789)"
    );
}

/// (X-MIG M9) A preset launch's declared deliverables reach the engine's deliverable floor: they
/// join the last creator step's `required_deliverables` on its unit. A launch that declares them
/// with neither a plan nor a preset is refused at launch, never dropped.
#[test]
fn a_preset_launchs_declared_deliverables_ride_its_last_creator_unit() {
    let dir = tmp_dir("deliverables");
    let db = dir.join("estate.db").to_str().unwrap().to_string();
    let rig = spawn(&db);
    let mut s = spec("rdl", "feature", None);
    s.deliverables = vec!["docs/out.md".into()];
    rig.core.launch_run(s).unwrap();
    let units = units_of(&rig.core, "rdl");
    let build = units
        .iter()
        .find(|u| u.id == "rdl:build")
        .expect("feature's creator is planned");
    assert!(
        build
            .required_deliverables
            .iter()
            .any(|d| d == "docs/out.md"),
        "{:?}",
        build.required_deliverables
    );
    let mut legacy = spec("rdl2", "feature", None);
    legacy.workflow = None;
    legacy.deliverables = vec!["docs/out.md".into()];
    let e = rig.core.launch_run(legacy).unwrap_err().to_string();
    assert!(e.contains("names neither"), "{e}");
}

/// M9 (DES-TEAMING-002 §14, §11.3): a launch naming `interactive-chat`, `interactive-draft` or
/// `interactive-edit` runs the built-in preset. Each is a repo-less creator plan (`produce`), so the
/// PA rates its RISK first (`pa-scope`, ord 1); this rig's PA answers nothing, so it floors at 100
/// as a non-code run (no `build`, no diff-floored `security_review`). Every preset step keeps the
/// draft skill, and the steps are crew's, in the form crew registered them with the skill held.
#[test]
fn m9_interactive_presets_launch_repo_less_with_the_draft_skill() {
    let dir = tmp_dir("interactive");
    let db = dir.join("estate.db").to_str().unwrap().to_string();
    let rig = spawn(&db);
    for (name, run, own) in [
        ("interactive-chat", "richat", &["understand", "revise"][..]),
        ("interactive-draft", "ridraft", &["draft"][..]),
        ("interactive-edit", "riedit", &["edit"][..]),
    ] {
        rig.core.launch_run(spec(run, name, None)).unwrap();
        let units = units_of(&rig.core, run);
        assert_eq!(units[0].id, format!("{run}:pa-scope"), "{name}");
        assert!(
            units[0]
                .instructions
                .as_deref()
                .is_some_and(|t| t.contains("RISK {")),
            "{name}: a repo-less run is rated, not scoped"
        );
        let ids: Vec<&str> = units
            .iter()
            .map(|u| u.id.strip_prefix(&format!("{run}:")).unwrap())
            .collect();
        for id in own {
            let u = units
                .iter()
                .find(|u| u.id == format!("{run}:{id}"))
                .unwrap_or_else(|| panic!("{name}: {id} is planned in {ids:?}"));
            assert_eq!(
                u.skill_ref.as_deref(),
                Some("wicked-garden-draft"),
                "{name}/{id}"
            );
            assert!(!u.executes_code, "{name}/{id}");
        }
        assert!(
            !ids.iter().any(|i| *i == "build" || *i == "security_review"),
            "{name}: a non-code run owes no build and no security_review: {ids:?}"
        );
    }
}

/// M1 (DES-TEAMING-002 §14, §11.3): a launch naming `bug` runs the built-in preset. The PA's
/// `pa-scope` comes first; triage → reproduce → fix → verify keep their order; fix keeps the
/// retired-behaviour sweep instructions, the evidence-floor pin and the creator role; verify keeps
/// its `human_confirm_if` gate, pin and evaluator role. Floor fill may add steps by band (this rig's
/// PA answers nothing, so it floors at 100) but never reorders the preset's own.
#[test]
fn m1_bug_launches_the_preset_in_its_order_with_its_gates_and_pins() {
    let dir = tmp_dir("bug");
    let db = dir.join("estate.db").to_str().unwrap().to_string();
    let rig = spawn(&db);
    rig.core.launch_run(spec("rbug", "bug", None)).unwrap();
    let units = units_of(&rig.core, "rbug");
    let all = rows("rbug", &units);
    assert_eq!(all[0].0, "pa-scope", "{all:?}");
    let own: Vec<_> = all
        .iter()
        .filter(|r| ["triage", "reproduce", "fix", "verify"].contains(&r.0.as_str()))
        .cloned()
        .collect();
    let floor = Some(EVIDENCE_FLOOR_PIN);
    let hci = r#"{"human_confirm_if":"verdict_not_pass"}"#;
    assert_eq!(
        own,
        vec![
            r("triage", "recon", "neutral", "auto", None),
            r("reproduce", "test", "neutral", "auto", None),
            r("fix", "build", "creator", "auto", floor),
            r("verify", "test", "evaluator", hci, floor),
        ],
        "{all:?}"
    );
    let fix = units.iter().find(|u| u.id == "rbug:fix").unwrap();
    assert!(fix.executes_code);
    assert!(fix
        .description
        .contains("Update every consumer of behaviour this fix retires or changes"));
}

/// M12 (DES-W7-M12, core#649): a launch naming `mcp-server` runs the built-in preset — the def's
/// nine phases in order under their own ids, each review keeping its specialist skill and raised
/// gate (security-review is a `review`, not the catalog's `security_review`), test on the evaluator
/// role (the bold cell), install-plan and install as Tool `run` steps with install gated
/// `consent_before`. The PA's `pa-scope` comes first; floor fill may add steps by band.
#[test]
fn m12_mcp_server_launches_the_preset_with_its_skills_gates_and_consent() {
    let dir = tmp_dir("mcp");
    let db = dir.join("estate.db").to_str().unwrap().to_string();
    let rig = spawn(&db);
    rig.core
        .launch_run(spec("rmcp", "mcp-server", None))
        .unwrap();
    let units = units_of(&rig.core, "rmcp");
    let all = rows("rmcp", &units);
    assert_eq!(all[0].0, "pa-scope", "{all:?}");
    let own_ids = [
        "scope",
        "source-discovery",
        "design",
        "build",
        "test",
        "security-review",
        "observability-review",
        "install-plan",
        "install",
    ];
    let own: Vec<_> = all
        .iter()
        .filter(|r| own_ids.contains(&r.0.as_str()))
        .cloned()
        .collect();
    let f = Some(EVIDENCE_FLOOR_PIN);
    let h = r#"{"human_confirm":{"unconditional":false}}"#;
    let v = r#"{"human_confirm_if":"verdict_not_pass"}"#;
    assert_eq!(
        own,
        vec![
            r("scope", "recon", "neutral", h, None),
            r("source-discovery", "recon", "neutral", "auto", None),
            r("design", "recon", "neutral", h, None),
            r("build", "build", "creator", "auto", f),
            r("test", "test", "evaluator", v, f),
            r("security-review", "review", "evaluator", h, f),
            r("observability-review", "review", "evaluator", v, f),
            r("install-plan", "build", "neutral", "auto", None),
            r("install", "build", "neutral", "consent_before", None),
        ],
        "{all:?}"
    );
    let skill = |id: &str| {
        units
            .iter()
            .find(|u| u.id == format!("rmcp:{id}"))
            .and_then(|u| u.skill_ref.clone())
    };
    assert_eq!(
        skill("security-review").as_deref(),
        Some("wicked-garden-platform-security-engineer")
    );
    assert_eq!(
        skill("observability-review").as_deref(),
        Some("wicked-garden-qe-observability-test-engineer")
    );
    assert_eq!(
        skill("build").as_deref(),
        Some("wicked-garden-mcp-scaffold")
    );
    let install = units.iter().find(|u| u.id == "rmcp:install").unwrap();
    assert_eq!(install.depends_on, vec!["install-plan".to_string()]);
}

/// X3 (operator ruling 2026-10-10): a launch naming `editor-plugin` runs the built-in preset — scope
/// and design gated, build on the creator with the editor-scaffold skill, test on the evaluator role
/// (the catalog's test entry) running the conformance harness, security-review keeping the platform
/// specialist and its raised gate, install-plan and install as Tool `run` steps with install gated
/// `consent_before` on its dry run. The PA's `pa-scope` comes first.
#[test]
fn x3_editor_plugin_launches_the_preset_with_its_skills_gates_and_consent() {
    let dir = tmp_dir("editor");
    let db = dir.join("estate.db").to_str().unwrap().to_string();
    let rig = spawn(&db);
    rig.core
        .launch_run(spec("redit", "editor-plugin", None))
        .unwrap();
    let units = units_of(&rig.core, "redit");
    let all = rows("redit", &units);
    assert_eq!(all[0].0, "pa-scope", "{all:?}");
    let own_ids = [
        "scope",
        "design",
        "build",
        "test",
        "security-review",
        "install-plan",
        "install",
    ];
    let own: Vec<_> = all
        .iter()
        .filter(|r| own_ids.contains(&r.0.as_str()))
        .cloned()
        .collect();
    let f = Some(EVIDENCE_FLOOR_PIN);
    let h = r#"{"human_confirm":{"unconditional":false}}"#;
    let v = r#"{"human_confirm_if":"verdict_not_pass"}"#;
    assert_eq!(
        own,
        vec![
            r("scope", "recon", "neutral", h, None),
            r("design", "recon", "neutral", h, None),
            r("build", "build", "creator", "auto", f),
            r("test", "test", "evaluator", v, f),
            r("security-review", "review", "evaluator", h, f),
            r("install-plan", "build", "neutral", "auto", None),
            r("install", "build", "neutral", "consent_before", None),
        ],
        "{all:?}"
    );
    let skill = |id: &str| {
        units
            .iter()
            .find(|u| u.id == format!("redit:{id}"))
            .and_then(|u| u.skill_ref.clone())
    };
    for id in ["scope", "design", "build", "test"] {
        assert_eq!(
            skill(id).as_deref(),
            Some("wicked-garden-editor-scaffold"),
            "{id}"
        );
    }
    assert_eq!(
        skill("security-review").as_deref(),
        Some("wicked-garden-platform-security-engineer")
    );
    let install = units.iter().find(|u| u.id == "redit:install").unwrap();
    assert_eq!(install.depends_on, vec!["install-plan".to_string()]);
}

/// M9b (studio#373): a launch naming `demo` runs the built-in preset — the wicked-garden demo
/// skill's plan → record → review, meant for the launch's one declared write root. `plan` and `review`
/// are the plan gate and the review gate (`human_confirm`); `plan` and `record` are creators and
/// `review` is an evaluator, so evaluator ≠ creator puts the reviewer on another seat; every
/// deliverable is relative, so it resolves inside the demo root; nothing executes code.
#[test]
fn m9b_demo_launches_plan_record_review_on_the_demo_skill() {
    let dir = tmp_dir("demo");
    let db = dir.join("estate.db").to_str().unwrap().to_string();
    let rig = spawn(&db);
    let demo = rig
        .core
        .list_presets(None)
        .unwrap()
        .into_iter()
        .find(|p| p.name == "demo")
        .expect("the built-in demo preset is listed");
    assert_eq!(
        (demo.scope.as_str(), demo.created_by.as_str()),
        ("global", "builtin")
    );

    let mut launch = spec("rdemo", "demo", None);
    launch.problem = "Make a demo of http://127.0.0.1:5173 for new users".into();
    // No declared root here: the demo root is the launcher's (crew mints one per run), and a root
    // needs $HOME to validate, which a Windows runner lacks. The steps are what this pins.
    rig.core.launch_run(launch).unwrap();
    let units = units_of(&rig.core, "rdemo");
    let hc = r#"{"human_confirm":{"unconditional":false}}"#;
    let got = |id: &str| {
        units
            .iter()
            .find(|u| u.id == format!("rdemo:{id}"))
            .unwrap_or_else(|| panic!("no `{id}` unit in {:?}", rows("rdemo", &units)))
    };
    let (plan, record, review) = (got("plan"), got("record"), got("review"));
    assert_eq!(row("rdemo", plan), r("plan", "build", "creator", hc, None));
    assert_eq!(
        row("rdemo", record),
        r("record", "build", "creator", "auto", None)
    );
    assert_eq!(
        row("rdemo", review),
        r("review", "review", "evaluator", hc, None)
    );
    for u in [plan, record, review] {
        assert_eq!(
            u.skill_ref.as_deref(),
            Some("wicked-garden-demo"),
            "{}",
            u.id
        );
        assert!(!u.executes_code, "{} changes no tree", u.id);
    }
    assert!(
        plan.ord < record.ord && record.ord < review.ord,
        "plan, then record, then review"
    );
    assert_eq!(
        plan.required_deliverables,
        ["script.md", "chapters.json", "storyline.mjs"]
    );
    assert!(record
        .required_deliverables
        .iter()
        .any(|d| d == "demo-video/demo.mp4"));
    assert!(record
        .required_deliverables
        .iter()
        .any(|d| d == "review/chapters.png"));
    assert!(
        review.required_deliverables.is_empty(),
        "the reviewer writes nothing: its verdict is its output"
    );
    let all = plan
        .required_deliverables
        .iter()
        .chain(&record.required_deliverables);
    for d in all {
        assert!(
            !std::path::Path::new(d).is_absolute() && !d.contains(".."),
            "{d} resolves inside the demo root"
        );
    }
    for (u, action) in [(plan, "`plan`"), (record, "`record`"), (review, "`review`")] {
        assert!(
            u.description.contains(action),
            "{} names the skill's {action} action: {}",
            u.id,
            u.description
        );
    }
    assert!(
        record.description.contains("read-only"),
        "the recorder is read-only against the app"
    );
    let view = rig
        .core
        .sessions_detail()
        .unwrap()
        .into_iter()
        .find(|v| v.session.id == "rdemo")
        .unwrap();
    assert_eq!(
        view.session
            .team_plan
            .expect("a preset launch is a team plan")
            .preset
            .as_deref(),
        Some("demo")
    );
}

/// Arm the hermetic emit spool (core#311) before `main`.
///
/// SAFETY (`ctor(unsafe)`): runs before `main` on one thread and only sets one process env var
/// via the std API — no allocator setup, no threads, no panics across the FFI boundary.
#[ctor::ctor(unsafe)]
fn arm_hermetic_emit_spool() {
    wicked_apps_core::emit::hermetic_test_spool();
}

/// DES-learn-workflow: a launch naming `learn` runs the built-in preset — scope gated, research and
/// synthesize ungated neutral `understand` steps, the walkthrough behind an UNCONDITIONAL human gate
/// (the chat walkthrough the engagement dial can never skip), author on the creator `build` entry
/// (evidence floor), review on the evaluator `review` entry. Every step runs `wicked-garden-learn`.
#[test]
fn learn_launches_the_preset_with_its_skill_and_the_unconditional_walkthrough() {
    let dir = tmp_dir("learn");
    let db = dir.join("estate.db").to_str().unwrap().to_string();
    let rig = spawn(&db);
    rig.core.launch_run(spec("rlearn", "learn", None)).unwrap();
    let units = units_of(&rig.core, "rlearn");
    let all = rows("rlearn", &units);
    assert_eq!(all[0].0, "pa-scope", "{all:?}");
    let own_ids = [
        "scope",
        "research",
        "synthesize",
        "walkthrough",
        "author",
        "review",
    ];
    let own: Vec<_> = all
        .iter()
        .filter(|r| own_ids.contains(&r.0.as_str()))
        .cloned()
        .collect();
    let f = Some(EVIDENCE_FLOOR_PIN);
    let h = r#"{"human_confirm":{"unconditional":false}}"#;
    let hu = r#"{"human_confirm":{"unconditional":true}}"#;
    assert_eq!(
        own,
        vec![
            r("scope", "recon", "neutral", h, None),
            r("research", "recon", "neutral", "auto", None),
            r("synthesize", "recon", "neutral", "auto", None),
            r("walkthrough", "recon", "neutral", hu, None),
            r("author", "build", "creator", "auto", f),
            r("review", "review", "evaluator", h, f),
        ],
        "{all:?}"
    );
    for id in own_ids {
        let u = units
            .iter()
            .find(|u| u.id == format!("rlearn:{id}"))
            .unwrap_or_else(|| panic!("{id} in {all:?}"));
        assert_eq!(u.skill_ref.as_deref(), Some("wicked-garden-learn"), "{id}");
    }
    let author = units.iter().find(|u| u.id == "rlearn:author").unwrap();
    assert_eq!(author.depends_on, vec!["walkthrough".to_string()]);
}
