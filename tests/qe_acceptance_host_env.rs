//! (QE acceptance) A host that sets `WICKED_QE_ACCEPTANCE_OFF` before spawning starts the engine
//! with QE acceptance already off for its own launches: the bus bridge is armed inside `spawn`, so
//! a setter call afterwards would leave a window. Its own test binary: the variable is
//! process-wide, and every other suite expects the default.

use std::sync::Arc;
use std::time::{Duration, Instant};

use wicked_core::assurance::QE_ACCEPTANCE_OFF_ENV;
use wicked_core::{BusDb, BusEmit, Core, CoreEvent, StepInput, StepOutput, StepRunner, StepStatus};
use wicked_council::types::{Category, Confidence, Dispatcher, InputMode, Vote};
use wicked_council::{AgenticCli, CouncilTask};

#[ctor::ctor(unsafe)]
fn arm_hermetic_emit_spool() {
    wicked_apps_core::emit::hermetic_test_spool();
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

struct NoBallots;
impl Dispatcher for NoBallots {
    fn dispatch(&self, _c: &AgenticCli, _: &CouncilTask) -> Option<Vote> {
        None
    }
}

struct OkRunner;
impl StepRunner for OkRunner {
    fn run_unit(&self, i: &StepInput) -> StepOutput {
        StepOutput {
            run_id: i.run_id.clone(),
            unit_ix: i.unit_ix,
            attempt: i.attempt,
            output: "done.\nVERDICT: PASS".into(),
            status: StepStatus::Ok,
            usage: None,
            files: vec![],
            tools: Vec::new(),
            governed: false,
        }
    }
}

const QE_ONE: &str = r#"{"id":"qe-one","required_instruments":["distinct_evaluator","judge","qe_acceptance"],"phases":[
  {"id":"build","kind":"build","gate":"auto"}]}"#;

#[test]
fn a_core_spawned_with_the_env_starts_with_qe_acceptance_off() {
    let dir = std::env::temp_dir().join(format!("wicked-core-qe-env-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("estate.db").to_str().unwrap().to_string();
    let bus_db = dir.join("bus.db").to_str().unwrap().to_string();

    // Blank reads as unset.
    std::env::set_var(QE_ACCEPTANCE_OFF_ENV, "  ");
    let plain = Core::spawn_with_engine(
        dir.join("plain.db").to_str().unwrap().to_string(),
        Arc::new(NoBallots),
        Arc::new(OkRunner),
    );
    assert_eq!(plain.qe_acceptance_off(), None);
    drop(plain);

    std::env::set_var(QE_ACCEPTANCE_OFF_ENV, "off at boot");
    let core = Core::spawn_with_engine(db, Arc::new(NoBallots), Arc::new(OkRunner));
    std::env::remove_var(QE_ACCEPTANCE_OFF_ENV);
    assert_eq!(core.qe_acceptance_off().as_deref(), Some("off at boot"));
    core.register_workflow(QE_ONE).expect("register qe-one");
    let ev = core.subscribe();
    let bridge = core.connect_bus(bus_db.clone(), vec![cli("codex"), cli("claude")]);
    BusDb::open(&bus_db)
        .unwrap()
        .emit(&BusEmit::new(
            wicked_core::RUN_REQUESTED,
            "wicked-cli",
            "cli.run",
            serde_json::json!({ "workflow": "qe-one", "problem": "Fix the thing.",
                "args": { "session_id": "env-boot" } }),
        ))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let started = loop {
        assert!(Instant::now() < deadline, "the bus request never started");
        if let Ok(e) = ev.recv_timeout(Duration::from_millis(200)) {
            if matches!(&e, CoreEvent::SessionStarted { session, .. } if session == "env-boot") {
                break e.to_json();
            }
        }
    };
    let qe = &started["assurance"]["qe"];
    assert_eq!(
        (qe["status"].as_str(), qe["reason"].as_str()),
        (
            Some("skipped"),
            Some("QE acceptance skipped by operator: off at boot")
        ),
        "{qe}"
    );
    // The setter takes over from there.
    core.set_qe_acceptance_off(None).unwrap();
    assert_eq!(core.qe_acceptance_off(), None);
    bridge.stop();
    let _ = std::fs::remove_dir_all(&dir);
}
