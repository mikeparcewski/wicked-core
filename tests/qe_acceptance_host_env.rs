//! (QE acceptance) A host that sets `WICKED_QE_ACCEPTANCE_OFF` before spawning starts the engine
//! with QE acceptance already off for its own launches: the bus bridge is armed inside `spawn`, so
//! a setter call afterwards would leave a window. Its own test binary: the variable is
//! process-wide, and every other suite expects the default.

use std::sync::Arc;
use std::time::{Duration, Instant};

use wicked_core::assurance::QE_ACCEPTANCE_OFF_ENV;
use wicked_core::{
    BusBridgeState, BusDb, BusEmit, Core, CoreEvent, StepInput, StepOutput, StepRunner, StepStatus,
};
use wicked_council::types::{Dispatcher, Vote};
use wicked_council::{AgenticCli, CouncilTask};

#[ctor::ctor(unsafe)]
fn arm_hermetic_emit_spool() {
    wicked_apps_core::emit::hermetic_test_spool();
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

    // codex r4: the bridge `spawn` arms itself (`WICKED_BUS_DB`) is the one a request can reach
    // before any setter call, so it must already carry the boot value — no `connect_bus` here. Its
    // roster is the registry's, read from a scratch HOME seating two fake CLIs and nothing else.
    let home = dir.join("home");
    std::fs::create_dir_all(home.join(".config/wicked-council")).unwrap();
    std::env::set_var("HOME", &home);
    let mut toml = String::new();
    for c in wicked_core::registry_roster() {
        let seated = c.key == "codex" || c.key == "claude";
        toml.push_str(&format!(
            "[[cli]]\nkey = {key:?}\ndisplay_name = {key:?}\nbinary = \"unused\"\n\
             headless_invocation = \"unused {{PROMPT}}\"\n{off}\n",
            key = c.key,
            off = if seated {
                ""
            } else {
                "enabled_for_council = false\nseat_eligible_for_work = false\n"
            }
        ));
    }
    std::fs::write(home.join(".config/wicked-council/clis.toml"), toml).unwrap();
    std::env::set_var("WICKED_BUS_DB", &bus_db);
    std::env::set_var(QE_ACCEPTANCE_OFF_ENV, "off at boot");
    let core = Core::spawn_with_engine(db, Arc::new(NoBallots), Arc::new(OkRunner));
    std::env::remove_var(QE_ACCEPTANCE_OFF_ENV);
    std::env::remove_var("WICKED_BUS_DB");
    assert!(
        matches!(core.bus_bridge_state(), BusBridgeState::Armed { .. }),
        "spawn armed the bridge: {:?}",
        core.bus_bridge_state()
    );
    assert_eq!(core.qe_acceptance_off().as_deref(), Some("off at boot"));
    core.register_workflow(QE_ONE).expect("register qe-one");
    let ev = core.subscribe();
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
    drop(core);
    let _ = std::fs::remove_dir_all(&dir);
}
