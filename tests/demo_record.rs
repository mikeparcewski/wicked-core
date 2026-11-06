//! (DES-DEMO-RECORD-001, core#1249) The `demo_record` Tool through the REAL engine: a plan naming it,
//! launched with a demo root (`evidence_root` + crew's `launch.json`), dispatches the catalog's fixed
//! command (a stub `wicked-garden` on `PATH`) inside the one-port jail with the declared variables, and
//! the demo floor decides the unit from the engine's receipt of the take.
//!
//! - A PASS take of the approved storyline, with every file it implies, completes the run.
//! - A take that ran and failed (`left_app`) escalates as a verdict (`demo_verdict`).
//! - A take the host could not make (`app_unreachable`) escalates as a refusal (`demo_refused`).
//! - A PASS that left out a chapter's segment is a verdict, never a pass.
//! - A file planted in `demo-video/` before the take is moved aside: it can never count.
//! - A host with no port-scoped jail records nothing and says why.
//!
//! POSIX only: the stub is a shell script (the suite's idiom).

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;
use wicked_core::{
    Core, CoreEvent, EntityMode, HumanConfirm, LaunchSpec, PlanSteps, StepInput, StepOutput,
    StepRunner, TeamConfig, TEAM_OUTBOX_FILE,
};
use wicked_council::types::{Confidence, Dispatcher, Vote};
use wicked_council::{AgenticCli, CouncilTask};

fn fixture_root() -> PathBuf {
    std::env::temp_dir().join(format!("wicked-core-demo-{}", std::process::id()))
}

/// Pre-main (single-threaded): the hermetic emit spool and a stub `wicked-garden` FIRST on `PATH`.
/// The stub records its env and argv, then writes the take `<root>/mode` names:
/// `PASS` (every file, the approved storyline's digest), `SHORT` (PASS but one segment missing),
/// `FAIL` (`left_app`), `REFUSE` (`app_unreachable`), `NONE` (writes nothing).
///
/// SAFETY (`ctor(unsafe)`): runs before `main` on one thread and only touches the filesystem and
/// process env vars via the std API.
#[ctor::ctor(unsafe)]
fn arm() {
    wicked_apps_core::emit::hermetic_test_spool();
    let root = fixture_root();
    let _ = std::fs::remove_dir_all(&root);
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).expect("fixture bin dir");
    let stub = bin.join("wicked-garden");
    let script = "#!/bin/sh\n\
         R=\"$WICKED_DEMO_ROOT\"\n\
         env > \"$R/demo-video/env.txt\"\n\
         printf '%s\\n' \"$*\" > \"$R/demo-video/argv.txt\"\n\
         mode=$(cat \"$R/mode\" 2>/dev/null || echo PASS)\n\
         sha=$( (sha256sum 2>/dev/null || shasum -a 256) < \"$WICKED_DEMO_STORYLINE/storyline.json\" | cut -d' ' -f1)\n\
         if [ \"$mode\" = NONE ]; then exit 0; fi\n\
         mkdir -p \"$R/demo-video/segments/01-a\" \"$R/demo-video/segments/02-b\" \"$R/review\"\n\
         for f in demo.mp4 chapters.md timings.json segments/01-a/segment.mp4 segments/02-b/segment.mp4; do echo x > \"$R/demo-video/$f\"; done\n\
         for f in chapters.png joins.png end.png ffprobe.json; do echo x > \"$R/review/$f\"; done\n\
         case \"$mode\" in\n\
           SHORT) rm \"$R/demo-video/segments/02-b/segment.mp4\"; v=PASS; c=null;;\n\
           FAIL) v=FAIL; c='\"left_app\"';;\n\
           REFUSE) v=INCONCLUSIVE; c='\"app_unreachable\"';;\n\
           *) v=PASS; c=null;;\n\
         esac\n\
         printf '{\"overall\":\"%s\",\"cause\":%s,\"storyline_sha\":\"%s\"}' \"$v\" \"$c\" \"$sha\" > \"$R/demo-video/record.json\"\n\
         exit 0\n";
    std::fs::write(&stub, script).expect("write the stub");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let config = root.join("claude-config");
    std::fs::create_dir_all(&config).expect("fixture claude config dir");
    std::env::remove_var("WICKED_SKILLS_SNAPSHOT");
    std::env::set_var("CLAUDE_CONFIG_DIR", &config);
    let mut paths = vec![bin];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    std::env::set_var("PATH", std::env::join_paths(paths).expect("PATH joins"));
}

struct NoBallots;
impl Dispatcher for NoBallots {
    fn dispatch(&self, c: &AgenticCli, _: &CouncilTask) -> Option<Vote> {
        Some(Vote {
            cli: c.key.clone(),
            recommendation: "1".into(),
            top_risk: "none".into(),
            change_my_mind: "no".into(),
            disqualifier: None,
            confidence: Confidence::default(),
            provenance: "test".into(),
        })
    }
}

struct NoTurns;
impl StepRunner for NoTurns {
    fn run_unit(&self, i: &StepInput) -> StepOutput {
        panic!("a tool-only plan ran a worker turn: {}", i.unit.id);
    }
}

fn sha256(bytes: &[u8]) -> String {
    use sha2::Digest;
    format!("{:x}", sha2::Sha256::digest(bytes))
}

/// A demo root as crew's route leaves it, plus an approved snapshot (the engine writes it when the
/// plan step passes; a record-only plan has none, so the test lays it down).
fn demo_root(base: &Path, mode: &str, run: &str) -> PathBuf {
    let root = base.join("demo");
    std::fs::create_dir_all(root.join("plan")).unwrap();
    std::fs::write(
        root.join("launch.json"),
        json!({"app_origin": "http://127.0.0.1:7721", "launch_path": "/apps/esc",
               "target": {"kind": "crew_app", "id": "esc"}, "live_data_to_models": true})
        .to_string(),
    )
    .unwrap();
    let story = br#"{"title":"t","segments":[{"key":"01-a"},{"key":"02-b"}]}"#;
    let gen = root.join("approved").join("1");
    std::fs::create_dir_all(&gen).unwrap();
    std::fs::write(gen.join("storyline.json"), story).unwrap();
    std::fs::write(
        gen.join("chapters.json"),
        br#"[{"key":"01-a"},{"key":"02-b"}]"#,
    )
    .unwrap();
    std::fs::write(
        root.join("approved").join("current.json"),
        json!({"gen": 1, "run": run, "storyline_sha": sha256(story), "chapter_keys": ["01-a", "02-b"], "intro": false})
            .to_string(),
    )
    .unwrap();
    std::fs::write(root.join("mode"), mode).unwrap();
    root
}

struct Run {
    events: Vec<CoreEvent>,
    root: PathBuf,
}

fn run(name: &str, mode: &str, plant: bool) -> Run {
    let base = fixture_root().join(name);
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let sid = format!("demo-{name}");
    let root = demo_root(&base, mode, &sid);
    if plant {
        std::fs::create_dir_all(root.join("demo-video")).unwrap();
        std::fs::write(root.join("demo-video/demo.mp4"), b"planted").unwrap();
    }
    let core = Core::spawn_with_engine_team(
        base.join("estate.db").to_string_lossy().into_owned(),
        Arc::new(NoBallots),
        Arc::new(NoTurns),
        TeamConfig::new(None, Some(base.join(TEAM_OUTBOX_FILE)))
            .with_final_pass_budget(Duration::from_millis(300)),
    );
    let rx = core.subscribe();
    let plan: PlanSteps = serde_json::from_value(json!({
        "steps": [{"catalog": "demo_record", "id": "record"}]
    }))
    .unwrap();
    core.launch_run(LaunchSpec {
        problem: "make a demo".into(),
        clis: Vec::new(),
        entity_mode: EntityMode::Shared,
        session_id: sid.clone(),
        human_confirm: HumanConfirm::None,
        auto_deliver: false,
        repo_ref: None,
        base_ref: None,
        workflow: None,
        project_id: None,
        extra_write_roots: vec![root.join("plan").to_string_lossy().into_owned()],
        extra_read_roots: Vec::new(),
        project_graph: None,
        plan: Some(plan),
        deliver_step: None,
        exclude_seats: Vec::new(),
        evidence_root: Some(root.to_string_lossy().into_owned()),
        primary: None,
        reduced_assurance: false,
        power: false,
        voice: None,
        deliverables: Vec::new(),
        qe_acceptance: Default::default(),
    })
    .expect("the demo plan launches");
    let mut events = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(ev) => {
                let done = match &ev {
                    CoreEvent::SessionCompleted { session } => *session == sid,
                    CoreEvent::SessionFailed { session, .. } => *session == sid,
                    CoreEvent::AwaitingHuman { session, .. } => *session == sid,
                    _ => false,
                };
                events.push(ev);
                if done {
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(_) => break,
        }
    }
    Run { events, root }
}

/// macOS is the one host with a port-scoped jail (sandbox-exec); elsewhere nothing records.
fn port_jailed() -> bool {
    cfg!(target_os = "macos")
        && wicked_core::sandbox_availability().0 == wicked_core::SandboxLevel::Sandboxed
}

fn types(events: &[CoreEvent]) -> Vec<String> {
    events
        .iter()
        .map(|e| e.to_json()["type"].as_str().unwrap_or("?").to_string())
        .collect()
}

fn escalation(events: &[CoreEvent]) -> Option<(String, String)> {
    events.iter().find_map(|e| match e {
        CoreEvent::GateEscalated {
            verdict_summary,
            denial_source,
            ..
        } => Some((verdict_summary.clone(), denial_source.clone())),
        _ => None,
    })
}

fn completed(events: &[CoreEvent]) -> bool {
    events
        .iter()
        .any(|e| matches!(e, CoreEvent::SessionCompleted { .. }))
}

#[test]
fn a_passing_take_completes_and_the_recorder_got_exactly_the_declared_env() {
    if !port_jailed() {
        eprintln!("SKIP: no port-scoped jail on this host (the unjailed test covers it)");
        return;
    }
    let r = run("pass", "PASS", false);
    assert!(completed(&r.events), "{:?}", types(&r.events));
    assert!(escalation(&r.events).is_none());
    let argv = std::fs::read_to_string(r.root.join("demo-video/argv.txt")).unwrap();
    assert_eq!(argv.trim(), "run scripts/demo/demo-record.mjs record");
    let env = std::fs::read_to_string(r.root.join("demo-video/env.txt")).unwrap();
    let get = |k: &str| {
        env.lines()
            .find_map(|l| l.strip_prefix(&format!("{k}=")).map(str::to_string))
    };
    assert_eq!(
        get("WICKED_DEMO_APP_ORIGIN").as_deref(),
        Some("http://127.0.0.1:7721")
    );
    assert_eq!(get("WICKED_DEMO_LAUNCH_PATH").as_deref(), Some("/apps/esc"));
    assert_eq!(get("WICKED_DEMO_JAIL").as_deref(), Some("sandbox-exec"));
    assert_eq!(get("WICKED_DEMO_LIVE_DATA_TO_MODELS").as_deref(), Some("1"));
    assert_eq!(
        get("WICKED_DEMO_STORYLINE").map(PathBuf::from),
        Some(r.root.join("approved").join("1"))
    );
    let allowed = [
        "WICKED_RUN_ID",
        "WICKED_RUN_UNIT",
        "WICKED_DEMO_ROOT",
        "WICKED_DEMO_APP_ORIGIN",
        "WICKED_DEMO_LAUNCH_PATH",
        "WICKED_DEMO_TARGET",
        "WICKED_DEMO_LIVE_DATA_TO_MODELS",
        "WICKED_DEMO_STORYLINE",
        "WICKED_DEMO_JAIL",
        "WICKED_DEMO_DEPS",
        "WICKED_CREW_URL",
        "WICKED_GARDEN_ROOT",
        "WICKED_GARDEN_CACHE_DIR",
    ];
    for l in env.lines() {
        let k = l.split_once('=').map_or(l, |(k, _)| k);
        assert!(
            !k.starts_with("WICKED_") || allowed.contains(&k),
            "undeclared {k} reached the recorder"
        );
    }
}

#[test]
fn a_failed_take_is_a_verdict_and_a_refusal_is_a_retry() {
    if !port_jailed() {
        eprintln!("SKIP: no port-scoped jail on this host");
        return;
    }
    for (mode, source) in [
        ("FAIL", "demo_verdict"),
        ("REFUSE", "demo_refused"),
        ("SHORT", "demo_verdict"),
    ] {
        let r = run(&format!("m-{}", mode.to_lowercase()), mode, false);
        let (summary, got) = escalation(&r.events)
            .unwrap_or_else(|| panic!("{mode}: must escalate: {:?}", types(&r.events)));
        assert_eq!(got, source, "{mode}: {summary}");
        assert!(!completed(&r.events), "{mode} never completes the run");
        if mode == "SHORT" {
            assert!(summary.contains("02-b"), "{summary}");
        }
    }
}

#[test]
fn a_planted_video_is_set_aside_and_never_counts() {
    if !port_jailed() {
        eprintln!("SKIP: no port-scoped jail on this host");
        return;
    }
    let r = run("planted", "NONE", true);
    let (_, source) = escalation(&r.events).expect("no verdict file: refused");
    assert_eq!(source, "demo_refused");
    assert!(
        !r.root.join("demo-video/demo.mp4").exists(),
        "the planted file is gone from the take"
    );
    assert_eq!(
        std::fs::read(r.root.join("takes/1/demo-video/demo.mp4")).unwrap(),
        b"planted"
    );
}

#[test]
fn a_host_with_no_port_scoped_jail_records_nothing() {
    if port_jailed() {
        eprintln!("SKIP: this host has a port-scoped jail");
        return;
    }
    let r = run("unjailed", "PASS", false);
    let (summary, source) = escalation(&r.events).expect("escalates");
    assert_eq!(source, "demo_refused", "{summary}");
    assert!(
        !r.root.join("demo-video/env.txt").exists(),
        "the recorder never ran"
    );
    let record = std::fs::read_to_string(r.root.join("demo-video/record.json")).unwrap();
    assert!(
        record.contains("no_port_scoped_jail") || record.contains("unjailed_host"),
        "{record}"
    );
}

/// A record step cannot consume another run's approval: the snapshot names the run it belongs to.
#[test]
fn another_runs_approval_is_never_recorded() {
    if !port_jailed() {
        eprintln!("SKIP: no port-scoped jail on this host");
        return;
    }
    let base = fixture_root().join("foreign");
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    // The approval below names another run; this launch reuses the root.
    let root = demo_root(&base, "PASS", "some-other-run");
    let core = Core::spawn_with_engine_team(
        base.join("estate.db").to_string_lossy().into_owned(),
        Arc::new(NoBallots),
        Arc::new(NoTurns),
        TeamConfig::new(None, Some(base.join(TEAM_OUTBOX_FILE)))
            .with_final_pass_budget(Duration::from_millis(300)),
    );
    let rx = core.subscribe();
    let plan: PlanSteps =
        serde_json::from_value(json!({"steps": [{"catalog": "demo_record", "id": "record"}]}))
            .unwrap();
    let sid = "demo-foreign".to_string();
    core.launch_run(LaunchSpec {
        problem: "make a demo".into(),
        clis: Vec::new(),
        entity_mode: EntityMode::Shared,
        session_id: sid.clone(),
        human_confirm: HumanConfirm::None,
        auto_deliver: false,
        repo_ref: None,
        base_ref: None,
        workflow: None,
        project_id: None,
        extra_write_roots: vec![root.join("plan").to_string_lossy().into_owned()],
        extra_read_roots: Vec::new(),
        project_graph: None,
        plan: Some(plan),
        deliver_step: None,
        exclude_seats: Vec::new(),
        evidence_root: Some(root.to_string_lossy().into_owned()),
        primary: None,
        reduced_assurance: false,
        power: false,
        voice: None,
        deliverables: Vec::new(),
        qe_acceptance: Default::default(),
    })
    .unwrap();
    let mut events = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if let Ok(ev) = rx.recv_timeout(Duration::from_millis(500)) {
            let stop = matches!(&ev, CoreEvent::AwaitingHuman { session, .. } | CoreEvent::SessionCompleted { session } if *session == sid);
            events.push(ev);
            if stop {
                break;
            }
        }
    }
    let (summary, source) = escalation(&events).expect("refused");
    assert_eq!(source, "demo_refused", "{summary}");
    assert!(summary.contains("not this run's"), "{summary}");
    assert!(
        !root.join("demo-video/env.txt").exists(),
        "the recorder never ran"
    );
}
