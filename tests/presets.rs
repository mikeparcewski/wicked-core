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
        acp: None,
        capabilities: None,
        login_invocation: None,
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
    rig.core.launch_run(spec("rw", "bug", None)).unwrap();
    let ids: Vec<String> = rows("rw", &units_of(&rig.core, "rw"))
        .into_iter()
        .map(|r| r.0)
        .collect();
    assert_eq!(ids, ["triage", "reproduce", "fix", "verify"]);
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
    let want: Vec<PlanStep> = serde_json::from_value(maps["feature"]["steps"].clone()).unwrap();
    let builtins = wicked_core::builtin_presets();
    let names: Vec<&str> = builtins.iter().map(|(n, _)| *n).collect();
    assert_eq!(names, ["feature"]);
    assert_eq!(builtins[0].1, want);
}

/// Arm the hermetic emit spool (core#311) before `main`.
///
/// SAFETY (`ctor(unsafe)`): runs before `main` on one thread and only sets one process env var
/// via the std API — no allocator setup, no threads, no panics across the FFI boundary.
#[ctor::ctor(unsafe)]
fn arm_hermetic_emit_spool() {
    wicked_apps_core::emit::hermetic_test_spool();
}
