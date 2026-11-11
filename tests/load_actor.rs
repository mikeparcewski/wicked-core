//! (core#1390) LOAD HARNESS for the engine actor. On demand only (`#[ignore]`): it measures this
//! host, so it asserts nothing about time.
//!
//! N runs at once on one registered repo of M files (each run gets its own worktree, as crew's
//! runs share one checkout). Every unit is a fake seat that sleeps and answers, leaving its tree
//! as it found it (the phases are read-only), so the actor does all its own work (worktrees,
//! snapshots, folds, store writes) while no model runs. When a run finishes, another is launched in its place until the window
//! closes. Meanwhile it measures:
//!
//! * `list`: `sessions_detail()`, the read behind crew's GET /runs and the watchdog's
//!   `listExecuting`;
//! * `team` (read model) and `transcript` (still through the actor): the per-run reads the run
//!   page polls;
//! * `ping`: an actor round trip, which is how long a write such as a gate answer waits for its
//!   turn;
//! * `gate_hook`: the real `wicked-core gate-hook` subprocess judging a `Read` and a `Bash` call
//!   of a read-only unit in one of the worktrees (the write-root witness walks the tree);
//! * the actor's own timing (`Core::actor_stats`): backlog and the slowest commands.
//!
//! Run:
//! `WICKED_LOAD_RUNS=10 WICKED_LOAD_FILES=1300 WICKED_LOAD_SECS=120 \
//!  cargo test --test load_actor -- --ignored --nocapture`
//! (core#1429) `WICKED_LOAD_GATES=1` pauses every unit at a gate (`human_confirm: all`) and a gate
//! thread answers them: Approve, and Stop (a cancel) at the second gate of every third run. Each
//! run's first unit leaves `WICKED_LOAD_IGNORED` (3000) ignored files under `node_modules/`, so a
//! Stop reaps a tree with a dependency directory, as a studio run's is. The run then ASSERTS that
//! no actor command took a second or more (`gate` samples time each answer).
//!
//! Knobs: `WICKED_LOAD_RUNS` (3), `WICKED_LOAD_FILES` (1300), `WICKED_LOAD_UNITS` (4),
//! `WICKED_LOAD_SECS` (60), `WICKED_LOAD_STEP_MS` (500-3000, as `lo-hi`), and
//! `WICKED_LOAD_REPORT` (a path the JSON report is also written to). `WICKED_READ_MODEL=off`
//! measures the baseline: every read through the actor, as before core#1390.
#![cfg(unix)]

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use wicked_core::{
    Core, EntityMode, HumanConfirm, LaunchSpec, RepoSpec, SessionStatus, StepInput, StepOutput,
    StepRunner, StepStatus,
};
use wicked_council::types::{Category, Confidence, Dispatcher, InputMode, Vote};
use wicked_council::{AgenticCli, CouncilTask};

/// Pre-main: the hermetic emit spool (core#311) and a scratch HOME, so the engine reads no
/// operator config. SAFETY (`ctor(unsafe)`): runs before `main` on one thread and only sets
/// process env vars via the std API.
#[ctor::ctor(unsafe)]
fn hermetic_process() {
    wicked_apps_core::emit::hermetic_test_spool();
    let home = std::env::temp_dir().join(format!("wicked-core-load-home-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&home);
    unsafe {
        std::env::set_var("HOME", &home);
        std::env::set_var("USERPROFILE", &home);
        std::env::set_var("CLAUDE_CONFIG_DIR", home.join(".claude"));
        std::env::set_var("WICKED_MEMORY_EMBEDDER", "hash");
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

fn knob(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

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

/// The fake seat: sleeps a pseudo-random step time and answers; it writes nothing (a read-only
/// phase that changed its tree would be denied, and the run would park instead of finishing).
struct LoadSeat {
    lo_ms: u64,
    hi_ms: u64,
    seq: AtomicU64,
    /// (core#1429) Ignored files the first unit of a run leaves under `node_modules/`.
    ignored: u64,
}
impl StepRunner for LoadSeat {
    fn run_unit(&self, input: &StepInput) -> StepOutput {
        let n = self.seq.fetch_add(1, Ordering::Relaxed);
        // A cheap deterministic spread over [lo, hi].
        let span = self.hi_ms.saturating_sub(self.lo_ms).max(1);
        let ms = self.lo_ms + (n.wrapping_mul(2_654_435_761) % span);
        std::thread::sleep(Duration::from_millis(ms));
        // An installed dependency directory: ignored, so the tree stays clean, and a reap deletes it.
        if self.ignored > 0 && input.unit.ord == 1 {
            if let Some(wd) = input.workdir.as_deref() {
                for i in 0..self.ignored {
                    let dir = wd.join(format!("node_modules/p{:03}", i / 100));
                    std::fs::create_dir_all(&dir).expect("the fixture's dependency directory");
                    std::fs::write(dir.join(format!("i{i:05}.js")), "module.exports = 1;\n")
                        .expect("a fixture dependency file");
                }
            }
        }
        StepOutput {
            run_id: input.run_id.clone(),
            unit_ix: input.unit_ix,
            attempt: input.attempt,
            output: format!(
                "Unit {} did its step and changed nothing. {}",
                input.unit.ord,
                "This is the load harness's fixed answer, long enough to count as substance. "
                    .repeat(4)
            ),
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
        binary: "/bin/true".into(),
        headless_invocation: "/bin/true {PROMPT}".into(),
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

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git runs");
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

/// A repo of `files` tracked files (~1 KB each, 50 per directory), one commit.
fn make_repo(root: &Path, files: u64) {
    std::fs::create_dir_all(root).unwrap();
    git(root, &["init", "-q"]);
    git(root, &["config", "user.email", "t@example.invalid"]);
    git(root, &["config", "user.name", "t"]);
    git(root, &["config", "commit.gpgsign", "false"]);
    let body = "export const value = 1; // load harness fixture line\n".repeat(20);
    for i in 0..files {
        let dir = root.join(format!("src/m{:03}", i / 50));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("f{i:05}.ts")), &body).unwrap();
    }
    std::fs::write(root.join("README.md"), "load\n").unwrap();
    std::fs::write(root.join(".gitignore"), "node_modules/\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "init"]);
}

fn workflow(units: u64) -> String {
    let mut phases = Vec::new();
    for i in 1..=units {
        let dep = if i == 1 {
            String::new()
        } else {
            format!(r#","depends_on":["s{}"]"#, i - 1)
        };
        phases.push(format!(
            r#"{{"id":"s{i}","kind":"build","gate":"auto","role":"neutral"{dep}}}"#
        ));
    }
    format!(r#"{{"id":"load-chain","phases":[{}]}}"#, phases.join(","))
}

fn spec(run: &str, repo_ref: &str) -> LaunchSpec {
    let gated = knob("WICKED_LOAD_GATES", 0) == 1;
    LaunchSpec {
        base_ref: None,
        project_id: None,
        problem: "Load harness run: do each step".into(),
        clis: vec![cli("a"), cli("b")],
        entity_mode: EntityMode::Shared,
        session_id: run.into(),
        human_confirm: if gated {
            HumanConfirm::All
        } else {
            HumanConfirm::None
        },
        auto_deliver: false,
        repo_ref: Some(repo_ref.to_string()),
        workflow: Some("load-chain".into()),
        extra_write_roots: Vec::new(),
        extra_read_roots: Vec::new(),
        project_graph: None,
        plan: None,
        deliver_step: None,
        exclude_seats: Vec::new(),
        evidence_root: None,
        primary: None,
        reduced_assurance: true,
        power: false,
        voice: None,
        deliverables: Vec::new(),
        qe_acceptance: Default::default(),
    }
}

#[derive(Default)]
struct Samples(Mutex<BTreeMap<&'static str, Vec<f64>>>);
impl Samples {
    fn add(&self, kind: &'static str, took: Duration) {
        self.0
            .lock()
            .unwrap()
            .entry(kind)
            .or_default()
            .push(took.as_secs_f64() * 1000.0);
    }
    fn summary(&self) -> serde_json::Value {
        let m = self.0.lock().unwrap();
        let mut out = serde_json::Map::new();
        for (k, v) in m.iter() {
            let mut s = v.clone();
            s.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let p = |q: f64| s[((s.len() as f64 * q).ceil() as usize).clamp(1, s.len()) - 1];
            out.insert(
                k.to_string(),
                serde_json::json!({
                    "n": s.len(),
                    "p50_ms": p(0.50).round(),
                    "p99_ms": p(0.99).round(),
                    "max_ms": s.last().copied().unwrap_or(0.0).round(),
                }),
            );
        }
        serde_json::Value::Object(out)
    }
}

/// One real `wicked-core gate-hook` judgement of a read-only unit's `tool` call in `worktree`: how
/// long it took and its exit code (0 allow, 2 deny).
fn gate_hook(
    db: &str,
    worktree: &Path,
    decisions: &Path,
    notes: &Path,
    tool: &str,
) -> (Duration, i32) {
    let payload = match tool {
        "Bash" => serde_json::json!({"tool_name":"Bash","tool_input":{"command":"ls src"}}),
        _ => {
            serde_json::json!({"tool_name":"Read","tool_input":{"file_path": worktree.join("README.md")}})
        }
    };
    let t = Instant::now();
    let mut child = Command::new(env!("CARGO_BIN_EXE_wicked-core"))
        .arg("gate-hook")
        .current_dir(worktree)
        .env("WICKED_GATE_DB", db)
        .env("WICKED_GATE_SCOPE", "wicked-agent/load/shared")
        .env("WICKED_GATE_PHASE", "unit-1")
        .env("WICKED_DECISIONS_PATH", decisions)
        .env("WICKED_NO_CODE_SCOPE", "read-only")
        .env("WICKED_WRITE_ROOTS", worktree)
        .env("WICKED_READ_ROOTS", worktree)
        .env("WICKED_DELIVERABLE_ROOTS", notes)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("gate-hook spawns");
    {
        use std::io::Write as _;
        let mut stdin = child.stdin.take().unwrap();
        let _ = stdin.write_all(payload.to_string().as_bytes());
    }
    let code = child.wait().ok().and_then(|s| s.code()).unwrap_or(-1);
    (t.elapsed(), code)
}

#[test]
#[ignore = "load harness: run on demand, it measures this host"]
fn actor_latency_under_concurrent_runs() {
    let runs = knob("WICKED_LOAD_RUNS", 3);
    let files = knob("WICKED_LOAD_FILES", 1300);
    let units = knob("WICKED_LOAD_UNITS", 4);
    let secs = knob("WICKED_LOAD_SECS", 60);
    let gated = knob("WICKED_LOAD_GATES", 0) == 1;
    // (review on #1433) The bound is asserted over every command of the window: each one over a
    // second is kept on the slow list, which no faster run evicts as the per-variant ring would.
    if gated && std::env::var_os("WICKED_ACTOR_SLOW_MS").is_none() {
        std::env::set_var("WICKED_ACTOR_SLOW_MS", "1000");
    }
    let ignored = if gated {
        knob("WICKED_LOAD_IGNORED", 3000)
    } else {
        0
    };
    let (lo_ms, hi_ms) = std::env::var("WICKED_LOAD_STEP_MS")
        .ok()
        .and_then(|v| {
            let (a, b) = v.split_once('-')?;
            Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
        })
        .unwrap_or((500u64, 3000u64));

    let base = std::env::temp_dir().join(format!("wicked-core-load-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let repo = base.join("repo");
    let t_repo = Instant::now();
    make_repo(&repo, files);
    eprintln!("load: repo of {files} files in {:?}", t_repo.elapsed());
    let db = base.join("estate.db").to_string_lossy().into_owned();

    let core = Core::spawn_with_engine(
        db.clone(),
        Arc::new(StubDispatcher),
        Arc::new(LoadSeat {
            lo_ms,
            hi_ms,
            seq: AtomicU64::new(0),
            ignored,
        }),
    );
    core.register_workflow(workflow(units)).expect("workflow");
    let entry = core
        .register_repo(RepoSpec {
            name: "load".into(),
            root_path: repo.to_string_lossy().into_owned(),
            registered_at: 1,
        })
        .expect("register repo");

    let samples = Arc::new(Samples::default());
    let stop = Arc::new(AtomicBool::new(false));
    let live: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let launched = Arc::new(AtomicU64::new(0));
    let finished = Arc::new(AtomicU64::new(0));
    let failed = Arc::new(AtomicU64::new(0));

    let launch = |n: u64| -> String {
        let id = format!("load-{n:04}");
        core.launch_run(spec(&id, &entry.id)).expect("launch");
        id
    };
    for _ in 0..runs {
        let n = launched.fetch_add(1, Ordering::Relaxed);
        let id = launch(n);
        live.lock().unwrap().push(id);
    }

    let mut threads = Vec::new();
    // Readers: the list read (x2, as crew's watchdog + GET /runs poll it), and the per-run reads.
    for r in 0..2 {
        let (core, samples, stop) = (core.clone(), samples.clone(), stop.clone());
        threads.push(std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let t = Instant::now();
                let ok = core.sessions_detail().is_ok();
                samples.add(if ok { "list" } else { "list_error" }, t.elapsed());
                std::thread::sleep(Duration::from_millis(150 + 50 * r));
            }
        }));
    }
    {
        let (core, samples, stop, live) =
            (core.clone(), samples.clone(), stop.clone(), live.clone());
        threads.push(std::thread::spawn(move || {
            let mut i = 0usize;
            while !stop.load(Ordering::Relaxed) {
                let id = {
                    let l = live.lock().unwrap();
                    if l.is_empty() {
                        None
                    } else {
                        Some(l[i % l.len()].clone())
                    }
                };
                i += 1;
                if let Some(id) = id {
                    let t = Instant::now();
                    let ok = core.run_team(&id).is_ok();
                    samples.add(if ok { "team" } else { "team_error" }, t.elapsed());
                    let t = Instant::now();
                    let _ = core.unit_transcript_at(&format!("{id}:s1"), None);
                    samples.add("transcript", t.elapsed());
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        }));
    }
    {
        let (core, samples, stop) = (core.clone(), samples.clone(), stop.clone());
        threads.push(std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let t = Instant::now();
                core.ping();
                samples.add("ping", t.elapsed());
                std::thread::sleep(Duration::from_millis(250));
            }
        }));
    }
    // The gate hook, on a worktree of the same repo that no run uses (a run's worktree is reaped
    // when it ends, and a tree that changed under the witness would turn every call into a deny).
    // A call that is not allowed is counted apart: its timing is not an allowed call's.
    {
        let (samples, stop, db) = (samples.clone(), stop.clone(), db.clone());
        let scratch = base.join("hook");
        let worktree = base.join("hook-worktree");
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                &worktree.to_string_lossy(),
                "HEAD",
            ],
        );
        threads.push(std::thread::spawn(move || {
            std::fs::create_dir_all(scratch.join("notes")).unwrap();
            let decisions = scratch.join("decisions.ndjson");
            let mut n = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let tool = if n.is_multiple_of(2) { "Bash" } else { "Read" };
                let (took, code) =
                    gate_hook(&db, &worktree, &decisions, &scratch.join("notes"), tool);
                samples.add(
                    match (tool, code) {
                        ("Bash", 0) => "gate_hook_bash",
                        (_, 0) => "gate_hook_read",
                        _ => "gate_hook_not_allowed",
                    },
                    took,
                );
                n += 1;
                std::thread::sleep(Duration::from_millis(300));
            }
        }));
    }

    // (core#1429) The gate answers: every open gate is approved, except the second gate of every
    // third run, which is a Stop (a cancel through the gate, the path that held the actor 175 s).
    let gates_seen: Arc<Mutex<BTreeMap<String, u32>>> = Arc::new(Mutex::new(BTreeMap::new()));
    let stopped = Arc::new(AtomicU64::new(0));
    if gated {
        let (core, samples, stop, live, gates_seen, stopped) = (
            core.clone(),
            samples.clone(),
            stop.clone(),
            live.clone(),
            gates_seen.clone(),
            stopped.clone(),
        );
        threads.push(std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let views = core.sessions_detail().unwrap_or_default();
                let ids: Vec<String> = live.lock().unwrap().clone();
                for v in views.iter().filter(|v| {
                    ids.contains(&v.session.id) && v.session.status == SessionStatus::AwaitingHuman
                }) {
                    let id = v.session.id.clone();
                    let n: u64 = id
                        .rsplit('-')
                        .next()
                        .and_then(|x| x.parse().ok())
                        .unwrap_or(0);
                    let k = {
                        let mut g = gates_seen.lock().unwrap();
                        let e = g.entry(id.clone()).or_insert(0);
                        *e += 1;
                        *e
                    };
                    // A Stop only over a populated tree: the reap it measures deletes one.
                    let populated = v
                        .session
                        .workdir
                        .as_deref()
                        .is_some_and(|w| std::path::Path::new(w).join("node_modules").is_dir());
                    let stop_here = n.is_multiple_of(3) && k >= 2 && populated;
                    let decision = if stop_here {
                        wicked_core::HumanDecision::Reject
                    } else {
                        wicked_core::HumanDecision::Approve {
                            amend: None,
                            amend_scope: Default::default(),
                        }
                    };
                    let t = Instant::now();
                    let res = core.confirm_gate(&id, decision);
                    samples.add(
                        if res.is_ok() { "gate" } else { "gate_refused" },
                        t.elapsed(),
                    );
                    if stop_here && matches!(res, Ok(SessionStatus::Cancelled)) {
                        stopped.fetch_add(1, Ordering::Relaxed);
                    }
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }));
    }

    // Keep N runs live for the window: replace each run that ends.
    let window = Instant::now();
    while window.elapsed() < Duration::from_secs(secs) {
        std::thread::sleep(Duration::from_millis(500));
        let views = match core.sessions_detail() {
            Ok(v) => v,
            Err(e) => {
                samples.add("replace_loop_error", Duration::ZERO);
                eprintln!("load: the run list failed: {e}");
                continue;
            }
        };
        let mut l = live.lock().unwrap();
        let ended: Vec<String> = l
            .iter()
            .filter(|id| {
                views.iter().any(|v| {
                    &v.session.id == *id
                        && (matches!(
                            v.session.status,
                            SessionStatus::Completed
                                | SessionStatus::Failed
                                | SessionStatus::Cancelled
                        ) || (!gated && v.session.status == SessionStatus::AwaitingHuman))
                })
            })
            .cloned()
            .collect();
        for id in ended {
            let v = views.iter().find(|v| v.session.id == id).unwrap();
            match v.session.status {
                SessionStatus::Completed => finished.fetch_add(1, Ordering::Relaxed),
                // (core#1429) A Stop the gate thread gave.
                SessionStatus::Cancelled if gated => stopped.load(Ordering::Relaxed),
                _ => {
                    let why = core
                        .run_events(&id)
                        .into_iter()
                        .rev()
                        .find(|e| e.get("type").and_then(|t| t.as_str()) == Some("awaitingHuman"))
                        .map(|e| {
                            let s = e.to_string();
                            s.chars().take(600).collect::<String>()
                        })
                        .unwrap_or_default();
                    eprintln!("load: run {id} ended {:?} {why}", v.session.status);
                    let _ = core.cancel_run(&id);
                    failed.fetch_add(1, Ordering::Relaxed)
                }
            };
            l.retain(|x| x != &id);
            let n = launched.fetch_add(1, Ordering::Relaxed);
            l.push(launch(n));
        }
    }
    stop.store(true, Ordering::Relaxed);
    for t in threads {
        t.join().expect("a measuring thread panicked");
    }
    // A run of good numbers from an engine that had stopped would be no measurement at all.
    core.list_repos()
        .expect("the actor still answers commands at the end of the window");
    let stats = core.actor_stats();
    let load1 = Command::new("sysctl")
        .args(["-n", "vm.loadavg"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    let report = serde_json::json!({
        "read_model": std::env::var("WICKED_READ_MODEL").unwrap_or_else(|_| "on".into()),
        "runs": runs, "files": files, "units": units, "secs": secs,
        "step_ms": [lo_ms, hi_ms],
        "launched": launched.load(Ordering::Relaxed),
        "completed": finished.load(Ordering::Relaxed),
        "not_completed": failed.load(Ordering::Relaxed),
        "gates": gated,
        "stops": stopped.load(Ordering::Relaxed),
        "loadavg": load1,
        "latency": samples.summary(),
        "actor": {
            "max_backlog": stats.max_backlog,
            "slow_threshold_ms": stats.slow_threshold_ms,
            "slowest": stats.commands.iter().take(8).collect::<Vec<_>>(),
            "slow_count": stats.slow.len(),
        },
    });
    let text = serde_json::to_string_pretty(&report).unwrap();
    println!("{text}");
    if let Ok(path) = std::env::var("WICKED_LOAD_REPORT") {
        let _ = std::fs::write(path, &text);
    }
    drop(core);
    let _ = std::fs::remove_dir_all(&base);
    // (core#1429) With gates answered (Stops included), no actor command takes a second.
    if gated {
        assert!(
            stopped.load(Ordering::Relaxed) > 0,
            "the window Stopped no run over a populated tree: {text}"
        );
        assert!(
            stats.slow_threshold_ms <= 1000 && stats.slow.is_empty(),
            "an actor command took a second or more: {:?}",
            stats.slow
        );
    }
}
