//! core#677 — a napi HOST provisions its own deterministic validator through the engine (vault →
//! approve, both on the single-writer actor) and pins it on an AGENT phase; the run plans without
//! anyone hand-seeding the machine. An unvaulted or unapproved pin still bails at plan time.

use std::sync::Arc;
use std::time::{Duration, Instant};
use wicked_core::{
    Core, CoreEvent, EntityMode, HumanConfirm, LaunchSpec, StepInput, StepOutput, StepRunner,
    StepStatus,
};
use wicked_council::types::{Category, Confidence, Dispatcher, InputMode, Vote};
use wicked_council::{AgenticCli, CouncilTask};

struct StubDispatcher;
impl Dispatcher for StubDispatcher {
    fn dispatch(&self, cli: &AgenticCli, _task: &CouncilTask) -> Option<Vote> {
        Some(Vote {
            cli: cli.key.clone(),
            recommendation: "fake-a".into(),
            top_risk: "none".into(),
            change_my_mind: "no".into(),
            disqualifier: None,
            confidence: Confidence::default(),
            provenance: "stub".into(),
        })
    }
}

struct OkRunner;
impl StepRunner for OkRunner {
    fn run_unit(&self, i: &StepInput) -> StepOutput {
        StepOutput {
            run_id: i.run_id.clone(),
            unit_ix: i.unit_ix,
            attempt: i.attempt,
            output: "drafted the document".into(),
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
        governance_class: None,
        credential: None,
        free_tier: None,
        health: None,
    }
}

fn core(name: &str) -> (Core, std::path::PathBuf) {
    let dir =
        std::env::temp_dir().join(format!("wicked-core-hostpin-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("estate.db").to_str().unwrap().to_string();
    (
        Core::spawn_with_engine(db, Arc::new(StubDispatcher), Arc::new(OkRunner)),
        dir,
    )
}

fn def(id: &str, pin: &str) -> String {
    format!(
        r#"{{"id":"{id}","phases":[{{"id":"draft","kind":"build","gate":"auto","role":"creator",
            "verified_evidence":true,"validator_pin":"{pin}"}}]}}"#
    )
}

fn launch(core: &Core, id: &str, sid: &str) -> anyhow::Result<String> {
    core.launch_run(LaunchSpec {
        base_ref: None,
        project_id: None,
        problem: "draft the doc".into(),
        clis: vec![cli("fake-a"), cli("fake-b")],
        entity_mode: EntityMode::Shared,
        session_id: sid.into(),
        human_confirm: HumanConfirm::None,
        auto_deliver: false,
        repo_ref: None,
        workflow: Some(id.into()),
        extra_write_roots: Vec::new(),
        extra_read_roots: Vec::new(),
        project_graph: None,
        plan: None,
        deliver_step: None,
        exclude_seats: Vec::new(),
        evidence_root: None,
        primary: None,
    })
}

/// Wait for either the planned units of `sid` (Ok) or an engine error naming `sid` (Err).
fn planned_or_error(
    core: &Core,
    events: &std::sync::mpsc::Receiver<CoreEvent>,
    sid: &str,
) -> Result<usize, String> {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        while let Ok(ev) = events.try_recv() {
            let j = ev.to_json();
            if j["type"] == "error" && j.to_string().contains(sid) {
                return Err(j.to_string());
            }
            if j["type"] == "sessionFailed" && j["session"] == sid {
                return Err(j.to_string());
            }
        }
        if let Ok(views) = core.sessions_detail() {
            if let Some(v) = views.iter().find(|v| v.session.id == sid) {
                if !v.units.is_empty() {
                    return Ok(v.units.len());
                }
            }
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    Err("neither planned nor failed".into())
}

#[test]
fn a_host_provisioned_pin_plans_an_agent_phase_and_an_unresolvable_one_still_bails_677() {
    let (core, dir) = core("plan");
    let events = core.subscribe();
    // The host's own check — no live writer, no hand seeding.
    let unapproved = core
        .vault_validator(
            "the draft names its sources",
            "test -n \"$WICKED_RUN_ID\" || true",
        )
        .expect("vault the host's validator");
    // The unapproved pin is refused at plan time (approval is the audited step).
    core.register_workflow(def("host-unapproved", &unapproved))
        .expect("register");
    let refused = launch(&core, "host-unapproved", "s-unapproved")
        .map_err(|e| e.to_string())
        .and_then(|_| planned_or_error(&core, &events, "s-unapproved").map(|_| ()));
    assert!(
        refused
            .as_ref()
            .is_err_and(|e| e.contains("UNAPPROVED") || e.contains("unapproved")),
        "an unapproved pin bails: {refused:?}"
    );

    let approved = core
        .approve_validator(&unapproved)
        .expect("approve the vaulted validator");
    assert_ne!(approved, unapproved, "approval is a new content address");
    core.register_workflow(def("host-approved", &approved))
        .expect("register a def pinning the approved pin on an agent phase");
    launch(&core, "host-approved", "s-approved").expect("launch");
    assert_eq!(
        planned_or_error(&core, &events, "s-approved"),
        Ok(1),
        "the run plans on a machine nobody seeded by hand"
    );

    // An unknown pin still bails, and approving one is an error, not a silent no-op.
    core.register_workflow(def("host-missing", "deadbeefdeadbeef"))
        .expect("register");
    let missing = launch(&core, "host-missing", "s-missing")
        .map_err(|e| e.to_string())
        .and_then(|_| planned_or_error(&core, &events, "s-missing").map(|_| ()));
    assert!(
        missing
            .as_ref()
            .is_err_and(|e| e.contains("not in the vault")),
        "{missing:?}"
    );
    let err = core.approve_validator("deadbeefdeadbeef").unwrap_err();
    assert!(err.to_string().starts_with("not_found:"), "{err}");

    // Refused at provisioning: an empty script, and one the run-time backstop would refuse.
    assert!(core
        .vault_validator("c", "  ")
        .unwrap_err()
        .to_string()
        .starts_with("bad_request:"));
    assert!(core
        .vault_validator("c", "echo x > /etc/passwd")
        .unwrap_err()
        .to_string()
        .starts_with("bad_request:"));
    drop(core);
    let _ = std::fs::remove_dir_all(&dir);
}
