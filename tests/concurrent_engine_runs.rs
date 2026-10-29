//! (core#957) Several unattended runs executing at once on the PRODUCTION engine (`Core::spawn`:
//! the real council dispatcher and the real step runner), all on the same two stub seats — a
//! headless CLI that prints `VERDICT: PASS` and exits. Every run must reach a terminal state, and
//! the actor must still answer afterwards — also when the process's stderr refuses every write,
//! as a Node host's non-blocking stderr pipe does while its reader lags (the cause of #957: std's
//! `eprintln!` panicked there and the panic message was lost, killing workers and the actor).
#![cfg(unix)]

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use wicked_core::{
    CampaignDef, CampaignNode, Core, CoreEvent, DenialGatePolicy, EntityMode, FailurePolicy,
    HumanConfirm, LaunchSpec, RunSpec,
};
use wicked_council::types::{Category, Confidence, InputMode};
use wicked_council::AgenticCli;

/// Pre-main: arm the hermetic emit spool (core#311) and give the process a scratch HOME, so the
/// production engine reads no operator config, skills or seat homes. SAFETY (`ctor(unsafe)`): runs
/// before `main` on one thread and only sets process env vars via the std API (the scratch HOME and
/// `CLAUDE_CONFIG_DIR`, so no installed plugin or seat config is read).
#[ctor::ctor(unsafe)]
fn hermetic_process() {
    wicked_apps_core::emit::hermetic_test_spool();
    let home = std::env::temp_dir().join(format!("wicked-core-c957-home-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&home);
    unsafe {
        std::env::set_var("HOME", &home);
        std::env::set_var("USERPROFILE", &home);
        std::env::set_var("CLAUDE_CONFIG_DIR", home.join(".claude"));
        std::env::set_var("WICKED_MEMORY_EMBEDDER", "hash");
        // A configured host's live bus, exec mediation, skills and telemetry stay out of it.
        for var in [
            "WICKED_BUS_DB",
            "WICKED_BUS_EXEC",
            "WICKED_SKILLS_SNAPSHOT",
            "WICKED_BASE_SKILL_REF",
            "WICKED_OTEL_ENDPOINT",
            "WICKED_ESTATE_DB",
        ] {
            std::env::remove_var(var);
        }
    }
}

const WF: &str = r#"{"id":"c957-build-review","phases":[
  {"id":"build","kind":"build","gate":"auto","role":"neutral"},
  {"id":"review","kind":"review","gate":"auto","role":"evaluator","depends_on":["build"]}]}"#;

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("wicked-core-c957-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn seat(key: &str, script: &std::path::Path) -> AgenticCli {
    AgenticCli {
        key: key.into(),
        display_name: key.into(),
        binary: "/bin/sh".into(),
        // Quoted: the invocation tokenizer splits on unquoted whitespace (a TMPDIR may hold spaces).
        headless_invocation: format!("/bin/sh \"{}\" {{PROMPT}}", script.display()),
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

fn spec(sid: &str, clis: Vec<AgenticCli>) -> LaunchSpec {
    LaunchSpec {
        base_ref: None,
        project_id: None,
        problem: "Build the thing, then review it.".into(),
        clis,
        entity_mode: EntityMode::Shared,
        session_id: sid.into(),
        human_confirm: HumanConfirm::None,
        auto_deliver: false,
        repo_ref: None,
        workflow: Some("c957-build-review".into()),
        extra_write_roots: Vec::new(),
        extra_read_roots: Vec::new(),
        project_graph: None,
        plan: None,
        deliver_step: None,
        exclude_seats: Vec::new(),
        evidence_root: None,
        primary: None,
        reduced_assurance: false,
        power: false,
        qe_acceptance: Default::default(),
        deliverables: Vec::new(),
    }
}

/// Every event until each of `sids` ended or 180 s passed; panics naming the runs that never ended
/// with their last events. Returns each run's end (`completed` / `failed …`).
fn wait_all(
    core: &Core,
    ev: &std::sync::mpsc::Receiver<CoreEvent>,
    sids: &[String],
) -> BTreeMap<String, String> {
    let mut ended: BTreeMap<String, String> = BTreeMap::new();
    let mut last: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let deadline = Instant::now() + Duration::from_secs(180);
    while sids.iter().any(|s| !ended.contains_key(s)) && Instant::now() < deadline {
        match ev.recv_timeout(Duration::from_millis(250)) {
            Ok(e) => {
                let j = e.to_json();
                if let Some(s) = j["session"].as_str() {
                    let tail = last.entry(s.to_string()).or_default();
                    tail.push(format!("{}:{}", j["type"], j["ord"]));
                    if tail.len() > 12 {
                        tail.remove(0);
                    }
                }
                match &e {
                    CoreEvent::SessionCompleted { session } => {
                        ended.insert(session.clone(), "completed".into());
                    }
                    CoreEvent::SessionFailed { session, .. } => {
                        ended.insert(session.clone(), format!("failed {j}"));
                    }
                    _ => {}
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    let stuck: Vec<_> = sids
        .iter()
        .filter(|s| !ended.contains_key(*s))
        .map(|s| (s.clone(), last.get(s).cloned().unwrap_or_default()))
        .collect();
    assert!(
        stuck.is_empty(),
        "runs that never ended: {stuck:#?}\nended: {ended:#?}"
    );
    // The actor still answers.
    core.sessions().expect("the actor is alive after the runs");
    ended
}

/// How many runs execute at once: the crew e2e that found #957 had five.
fn runs() -> usize {
    5
}

fn engine(name: &str) -> (Core, Vec<AgenticCli>) {
    let dir = scratch(name);
    let script = dir.join("seat.sh");
    std::fs::write(
        &script,
        "printf 'did the work\\nVERDICT: PASS\\n'\nexit 0\n",
    )
    .unwrap();
    let clis = vec![seat("alpha", &script), seat("beta", &script)];
    let core = Core::spawn(dir.join("estate.db").to_str().unwrap().to_string());
    core.register_workflow(WF).expect("register the workflow");
    (core, clis)
}

#[test]
fn concurrent_unattended_runs_on_two_seats_all_finish() {
    let (core, clis) = engine("runs");
    let ev = core.subscribe();
    let sids: Vec<String> = (0..runs()).map(|i| format!("c957-{i}")).collect();
    for sid in &sids {
        core.launch_run(spec(sid, clis.clone())).expect("launch");
    }
    wait_all(&core, &ev, &sids);
}

/// The crew e2e's exact mix: bus `wicked.crew.run.requested` launches (the engine's own bridge)
/// beside campaign nodes, all at once on the same two seats.
#[test]
fn bus_requests_beside_campaign_nodes_all_finish() {
    let (core, clis) = engine("bus");
    let bus_db = scratch("bus-db")
        .join("bus.db")
        .to_str()
        .unwrap()
        .to_string();
    let ev = core.subscribe();
    let bridge = core.connect_bus(bus_db.clone(), clis.clone());
    let n = runs();
    let db = wicked_core::BusDb::open(&bus_db).unwrap();
    let mut sids: Vec<String> = Vec::new();
    for i in 0..n.div_ceil(2) {
        let sid = format!("c957-bus-{i}");
        db.emit(&wicked_core::BusEmit::new(
            wicked_core::RUN_REQUESTED,
            "wicked-cli",
            "cli.run",
            serde_json::json!({
                "workflow": "c957-build-review",
                "problem": "Build the thing, then review it.",
                "args": { "session_id": sid }
            }),
        ))
        .unwrap();
        sids.push(sid);
    }
    let nodes: Vec<CampaignNode> = (0..n / 2)
        .map(|i| CampaignNode {
            node_id: format!("n{i}"),
            run_spec: RunSpec {
                problem: "Build the thing, then review it.".into(),
                clis: clis.clone(),
                entity_mode: EntityMode::Shared,
                human_confirm: HumanConfirm::None,
                repo_ref: None,
                workflow_id: Some("c957-build-review".into()),
                skip_qe_acceptance_reason: None,
                force_qe_acceptance: false,
            },
        })
        .collect();
    sids.extend(nodes.iter().map(|n| format!("cb:{}:a0", n.node_id)));
    core.launch_campaign(CampaignDef {
        id: "cb".into(),
        name: "cb".into(),
        nodes,
        edges: vec![],
        policy: FailurePolicy::ContinueIndependent,
        max_concurrency: 8,
        denial_gate: DenialGatePolicy::default(),
    })
    .expect("launch the campaign");
    wait_all(&core, &ev, &sids);
    bridge.stop();
}

/// The ends of `sids` within `within`, without panicking (a panic message may have nowhere to go).
fn ends_within(
    ev: &std::sync::mpsc::Receiver<CoreEvent>,
    sids: &[String],
    within: Duration,
) -> BTreeMap<String, String> {
    let mut ended: BTreeMap<String, String> = BTreeMap::new();
    let deadline = Instant::now() + within;
    while sids.iter().any(|s| !ended.contains_key(s)) && Instant::now() < deadline {
        match ev.recv_timeout(Duration::from_millis(250)) {
            Ok(CoreEvent::SessionCompleted { session }) => {
                ended.insert(session, "completed".into());
            }
            Ok(CoreEvent::SessionFailed { session, .. }) => {
                ended.insert(session, "failed".into());
            }
            Ok(_) | Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    ended
}

/// (core#957) The engine's host may hand it a stderr that refuses writes: a Node host's stderr is
/// a NON-BLOCKING pipe, and when its reader falls behind a write fails with EAGAIN. `eprintln!`
/// panics on a failed write, and the panic message is lost the same way — so a worker or the
/// actor died with no trace, and its run hung. This test runs itself as a child whose stderr is a
/// full non-blocking pipe, launches concurrent runs there, and requires every one to finish.
#[test]
fn runs_finish_when_stderr_refuses_every_write() {
    const CHILD: &str = "WICKED_C957_STDERR_CHILD";
    if let Some(result) = std::env::var_os(CHILD) {
        let (core, clis) = engine("stderr-child");
        let ev = core.subscribe();
        let sids: Vec<String> = (0..runs()).map(|i| format!("c957-stderr-{i}")).collect();
        for sid in &sids {
            core.launch_run(spec(sid, clis.clone())).expect("launch");
        }
        let ended = ends_within(&ev, &sids, Duration::from_secs(90));
        let alive = core.sessions().is_ok();
        std::fs::write(
            result,
            serde_json::json!({ "ended": ended, "runs": sids.len(), "actor_alive": alive })
                .to_string(),
        )
        .unwrap();
        return;
    }
    use std::os::fd::FromRawFd;
    let mut fds = [0i32; 2];
    // SAFETY: a plain pipe(2); both fds are owned below.
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe");
    let (read_end, write_end) = (fds[0], fds[1]);
    // SAFETY: fcntl on the fd we just created.
    unsafe {
        let fl = libc::fcntl(write_end, libc::F_GETFL);
        assert_eq!(
            libc::fcntl(write_end, libc::F_SETFL, fl | libc::O_NONBLOCK),
            0
        );
    }
    // Fill it: the reader (this test) never reads, so every later write by the child is EAGAIN.
    let chunk = [b'x'; 4096];
    loop {
        // SAFETY: writing a stack buffer to our own fd.
        let n = unsafe { libc::write(write_end, chunk.as_ptr().cast(), chunk.len()) };
        if n < 0 {
            break;
        }
    }
    let result = scratch("stderr-parent").join("result.json");
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "runs_finish_when_stderr_refuses_every_write",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD, &result)
        .stdout(std::process::Stdio::null())
        // SAFETY: `write_end` is ours and moves into the child's stderr (closed here on drop).
        .stderr(unsafe { std::process::Stdio::from_raw_fd(write_end) })
        .spawn()
        .expect("spawn the child");
    let deadline = Instant::now() + Duration::from_secs(150);
    let status = loop {
        if let Some(st) = child.try_wait().unwrap() {
            break Some(st);
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    // SAFETY: closing the read end we own.
    unsafe { libc::close(read_end) };
    let got: serde_json::Value = std::fs::read_to_string(&result)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(serde_json::Value::Null);
    let n = got["runs"].as_u64().unwrap_or(0) as usize;
    let completed = got["ended"]
        .as_object()
        .map_or(0, |m| m.values().filter(|v| *v == "completed").count());
    assert!(
        status.is_some() && n > 0 && completed == n && got["actor_alive"] == true,
        "with a stderr that refuses writes, every run must still complete and the actor survive: \
         child {status:?}, result {got}"
    );
}
