//! S2 (#601) tests: checkpoints, batching, confirmation, dedup, admission and the final pass.
//! The carrier half runs against a scripted bridge in `acp_runner::team_s2_tests`.

use super::*;
use serde_json::json;
use std::sync::atomic::AtomicUsize;

/// A real git repository with one committed file, and its content snapshot through the same
/// pinned git dir production uses.
struct Fixture {
    dir: PathBuf,
    repo: Repo,
    baseline: String,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "wicked-team-{name}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let git = |args: &[&str]| {
            crate::worktree_guard::git(&dir, args, &[]).unwrap();
        };
        git(&["init", "-q"]);
        std::fs::write(
            dir.join("src/lib.rs"),
            "fn a() {}\nfn b() {\n    let x = 1;\n}\n",
        )
        .unwrap();
        git(&["add", "-A"]);
        git(&[
            "-c",
            "user.email=t@example.invalid",
            "-c",
            "user.name=t",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "-m",
            "base",
        ]);
        let repo = Repo {
            workdir: dir.clone(),
            git_dir: dir.join(".git"),
        };
        let baseline = repo.snapshot().unwrap();
        Self {
            dir,
            repo,
            baseline,
        }
    }

    fn write(&self, rel: &str, text: &str) {
        std::fs::write(self.dir.join(rel), text).unwrap();
    }

    fn ctx(&self, monitors: u8, candidates: &[&str]) -> AttachCtx {
        AttachCtx {
            run_id: "run-1".into(),
            ord: 3,
            attempt: 1,
            creator: "claude".into(),
            plan: TeamPlan {
                monitors,
                candidates: candidates.iter().map(|s| s.to_string()).collect(),
            },
            repo: Some(self.repo.clone()),
            baseline_tree: Some(self.baseline.clone()),
            criterion: "the handler cancels stale fetches".into(),
            phase: "build".into(),
            code_graph_db: None,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A monitor host that counts what it is asked and answers from a script keyed by seat.
#[derive(Default)]
struct FakeHost {
    opens: Mutex<Vec<String>>,
    turns: AtomicUsize,
    replies: Mutex<HashMap<String, String>>,
    unadmitted: Vec<String>,
}

impl MonitorHost for FakeHost {
    fn admitted(&self, seat: &str) -> Result<(), String> {
        if self.unadmitted.iter().any(|s| s == seat) {
            Err(format!("seat '{seat}' is not admitted to input governance"))
        } else {
            Ok(())
        }
    }
    fn open(&self, _pool_key: &str, seat: &str, _scope: &MonitorScope) -> Result<(), String> {
        self.opens.lock().unwrap().push(seat.to_string());
        Ok(())
    }
    fn turn(&self, pool_key: &str, _prompt: &str, _budget: Duration) -> Result<String, String> {
        self.turns.fetch_add(1, Ordering::Relaxed);
        let replies = self.replies.lock().unwrap();
        Ok(replies
            .iter()
            .find(|(k, _)| pool_key.ends_with(k.as_str()))
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| "DONE".to_string()))
    }
    fn close(&self, _pool_key: &str) {}
}

fn recorder() -> (Emit, Arc<Mutex<Vec<CoreEvent>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s = Arc::clone(&seen);
    (Arc::new(move |ev| s.lock().unwrap().push(ev)), seen)
}

fn checkpoint(seq: u64, kind: &str) -> CoreEvent {
    CoreEvent::UnitCheckpoint {
        session: "run-1".into(),
        ord: 3,
        attempt: 1,
        seq,
        tool_call_id: format!("toolu_{seq}"),
        kind: kind.into(),
        title: format!("call {seq}"),
        status: "completed".into(),
        paths: vec![],
    }
}

/// Run every due job synchronously and fold it back in.
fn pump(core: &mut TeamCore, host: &Arc<FakeHost>, emit: &Emit, now: Instant) -> usize {
    let jobs = core.due_jobs(now);
    let n = jobs.len();
    for job in jobs {
        let done = run_job(&job, &**host, emit);
        core.apply(done);
    }
    n
}

fn findings(seen: &Arc<Mutex<Vec<CoreEvent>>>) -> Vec<CoreEvent> {
    seen.lock()
        .unwrap()
        .iter()
        .filter(|e| matches!(e, CoreEvent::MonitorFinding { .. }))
        .cloned()
        .collect()
}

// ── Checkpoints batch at tool boundaries, not per delta ──────────────────────────────────

/// A `tool_call` (kind `edit`) then its terminal `tool_call_update` yields exactly ONE
/// checkpoint carrying the kind, title and paths the EARLIER frame named (the terminal frame
/// repeats none of them). Message chunks and a non-terminal update yield nothing.
#[test]
fn a_checkpoint_fires_once_per_terminal_tool_call_and_never_per_token() {
    let turn = TeamTurn::new(
        ("run-1".into(), 3, 1),
        SteerMailbox::default(),
        None,
        Some(PathBuf::from("/wt")),
        true,
    );
    let frame = |update: Value| json!({"method": "session/update", "params": {"update": update}});
    let mut out = Vec::new();
    for f in [
        frame(json!({"sessionUpdate": "agent_message_chunk", "content": {"text": "Let me"}})),
        frame(
            json!({"sessionUpdate": "tool_call", "toolCallId": "toolu_1", "kind": "edit",
            "title": "Edit src/retire.ts", "locations": [{"path": "/wt/src/retire.ts"}]}),
        ),
        frame(
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "toolu_1",
            "status": "in_progress"}),
        ),
        frame(json!({"sessionUpdate": "agent_message_chunk", "content": {"text": " edit"}})),
        frame(
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "toolu_1",
            "status": "completed", "_meta": {"claudeCode": {"toolName": "Edit"}}}),
        ),
        frame(
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "toolu_2",
            "status": "failed"}),
        ),
    ] {
        out.extend(turn.observe(&f));
    }
    assert_eq!(
        out,
        vec![
            CoreEvent::UnitCheckpoint {
                session: "run-1".into(),
                ord: 3,
                attempt: 1,
                seq: 1,
                tool_call_id: "toolu_1".into(),
                kind: "edit".into(),
                title: "Edit src/retire.ts".into(),
                status: "completed".into(),
                paths: vec!["src/retire.ts".into()],
            },
            CoreEvent::UnitCheckpoint {
                session: "run-1".into(),
                ord: 3,
                attempt: 1,
                seq: 2,
                tool_call_id: "toolu_2".into(),
                kind: "other".into(),
                title: String::new(),
                status: "failed".into(),
                paths: vec![],
            },
        ]
    );
}

/// DES §12-2: 30 tree-changing checkpoints inside 60 s, with ONE tree change, make exactly
/// ONE monitor turn — and the batch the interval allows at 60 s finds the same tree and is
/// skipped without a turn. 200 output deltas in between start nothing.
#[test]
fn thirty_checkpoints_in_a_minute_with_one_tree_change_make_one_monitor_turn() {
    let fx = Fixture::new("batch");
    let host = Arc::new(FakeHost::default());
    let (emit, _seen) = recorder();
    let mut core = TeamCore::new(host.clone(), emit.clone(), TeamLimits::default());
    core.attach(fx.ctx(1, &["claude#2"]));
    fx.write("src/lib.rs", "fn a() {}\nfn b() {\n    let x = 2;\n}\n");
    let t0 = Instant::now();
    for i in 0..30u64 {
        for _ in 0..200 / 30 {
            core.on_event(&CoreEvent::UnitOutputDelta {
                session: "run-1".into(),
                ord: 3,
                attempt: 1,
                text: "tok".into(),
            });
        }
        core.on_event(&checkpoint(i + 1, "edit"));
        pump(&mut core, &host, &emit, t0 + Duration::from_secs(i * 2));
    }
    assert_eq!(host.turns.load(Ordering::Relaxed), 1, "one batch, one turn");
    assert_eq!(
        pump(&mut core, &host, &emit, t0 + Duration::from_secs(61)),
        1
    );
    assert_eq!(
        host.turns.load(Ordering::Relaxed),
        1,
        "the next batch saw the same tree id: skipped, no turn"
    );
}

/// DES §12-2: a checkpoint burst over an UNCHANGED tree makes zero monitor turns, and a
/// read-kind checkpoint never even starts a batch.
#[test]
fn a_burst_on_an_unchanged_tree_makes_no_turn_and_reads_start_nothing() {
    let fx = Fixture::new("unchanged");
    let host = Arc::new(FakeHost::default());
    let (emit, _seen) = recorder();
    let mut core = TeamCore::new(host.clone(), emit.clone(), TeamLimits::default());
    core.attach(fx.ctx(1, &["claude#2"]));
    let t0 = Instant::now();
    let mut started = 0;
    for i in 0..5u64 {
        core.on_event(&checkpoint(i + 1, "read"));
        started += pump(&mut core, &host, &emit, t0 + Duration::from_secs(i * 100));
    }
    assert_eq!(started, 0, "read/search/fetch/think never set the flag");
    // A burst: 30 `execute` calls inside one interval, then the next interval's batch.
    let t1 = t0 + Duration::from_secs(1000);
    for i in 0..30u64 {
        core.on_event(&checkpoint(10 + i, "execute"));
        pump(&mut core, &host, &emit, t1 + Duration::from_secs(i * 2));
    }
    pump(&mut core, &host, &emit, t1 + Duration::from_secs(61));
    assert_eq!(host.turns.load(Ordering::Relaxed), 0);
}

// ── A finding without a file:line in the settled tree is not emitted ─────────────────────

/// DES §12-3: a FINDING whose `evidence` is not line `line` of `path` in the snapshot tree is
/// dropped (`rejected.unconfirmed`); the one that matches is emitted with that tree id. A
/// finding on a path that does not exist, and a malformed line, are dropped too.
#[test]
fn only_a_finding_confirmed_at_its_file_line_in_the_snapshot_tree_is_emitted() {
    let fx = Fixture::new("confirm");
    let host = Arc::new(FakeHost::default());
    host.replies.lock().unwrap().insert(
        "m1".into(),
        [
            r#"FINDING {"severity":"high","path":"src/lib.rs","line":3,"evidence":"let x = 1;","claim":"stale","suggestion":null}"#,
            r#"FINDING {"severity":"high","path":"src/gone.rs","line":1,"evidence":"x","claim":"no such file"}"#,
            r#"FINDING {"severity":"high","path":"src/lib.rs","line":3,"evidence":"let   x = 2;","claim":"x is never read","suggestion":"drop it"}"#,
            r#"FINDING {"severity":"high","path":"../etc/passwd","line":1,"evidence":"root","claim":"escape"}"#,
            "DONE",
        ]
        .join("\n"),
    );
    let (emit, seen) = recorder();
    let mut core = TeamCore::new(host.clone(), emit.clone(), TeamLimits::default());
    core.attach(fx.ctx(1, &["claude#2"]));
    fx.write("src/lib.rs", "fn a() {}\nfn b() {\n    let x = 2;\n}\n");
    core.on_event(&checkpoint(7, "edit"));
    pump(&mut core, &host, &emit, Instant::now());
    let tree = fx.repo.snapshot().unwrap();
    assert_eq!(
        findings(&seen),
        vec![CoreEvent::MonitorFinding {
            session: "run-1".into(),
            ord: 3,
            attempt: 1,
            finding_id: "f-70684a885c6e3572".into(),
            monitor_id: "m1".into(),
            seat: "claude#2".into(),
            severity: "high".into(),
            path: "src/lib.rs".into(),
            line: 3,
            evidence: "let   x = 2;".into(),
            claim: "x is never read".into(),
            suggestion: Some("drop it".into()),
            tree,
            in_diff: true,
            checkpoint_seq: 7,
        }]
    );
    let unit = core.take(fx.ctx(1, &["claude#2"]));
    let ledger = unit.lock().unwrap().ledger("completed");
    assert_eq!(
        ledger.rejected,
        Rejected {
            malformed: 1,
            below_bar: 0,
            unconfirmed: 2,
            duplicate: 0
        }
    );
}

// ── Dedup ────────────────────────────────────────────────────────────────────────────────

/// DES §12-4: two monitors citing the same line TEXT (at different line numbers) emit ONE
/// finding and the ledger names the second seat in `corroboratedBy`; the same monitor
/// repeating it in a later batch counts as `duplicate`; a `low` finding emits nothing and
/// counts `belowBar`.
#[test]
fn dedup_is_on_line_text_across_monitors_and_low_is_below_the_bar() {
    let fx = Fixture::new("dedup");
    let host = Arc::new(FakeHost::default());
    let hit = r#"FINDING {"severity":"medium","path":"src/lib.rs","line":2,"evidence":"fn b() {","claim":"b shadows"}"#;
    let low = r#"FINDING {"severity":"low","path":"src/lib.rs","line":1,"evidence":"fn a() {}","claim":"nit"}"#;
    host.replies
        .lock()
        .unwrap()
        .insert("m1".into(), format!("{hit}\n{low}\nDONE"));
    host.replies
        .lock()
        .unwrap()
        .insert("m2".into(), format!("{hit}\nDONE"));
    let (emit, seen) = recorder();
    let limits = TeamLimits {
        batch_min_interval: Duration::ZERO,
        ..TeamLimits::default()
    };
    let mut core = TeamCore::new(host.clone(), emit.clone(), limits);
    core.attach(fx.ctx(2, &["claude#2", "claude#3"]));
    fx.write("src/lib.rs", "fn a() {}\nfn b() {\n    let x = 3;\n}\n");
    core.on_event(&checkpoint(1, "edit"));
    pump(&mut core, &host, &emit, Instant::now());
    // A second tree change: m1 repeats its finding in the next batch.
    fx.write("src/lib.rs", "fn a() {}\nfn b() {\n    let x = 4;\n}\n");
    core.on_event(&checkpoint(2, "edit"));
    pump(&mut core, &host, &emit, Instant::now());
    let emitted = findings(&seen);
    assert_eq!(emitted.len(), 1, "{emitted:?}");
    let CoreEvent::MonitorFinding { finding_id, .. } = &emitted[0] else {
        unreachable!()
    };
    assert_eq!(finding_id, "f-6a4ac002d79dfc6b");
    let ledger = core
        .take(fx.ctx(2, &["claude#2", "claude#3"]))
        .lock()
        .unwrap()
        .ledger("completed");
    assert_eq!(ledger.findings.len(), 1);
    assert_eq!(
        ledger.findings[0].corroborated_by,
        vec!["claude#3".to_string()]
    );
    assert_eq!(
        ledger.rejected.below_bar, 2,
        "m1's low line, in both batches"
    );
    assert_eq!(
        ledger.rejected.duplicate, 2,
        "m1 and m2 each repeated it once"
    );
}

/// The id is the line's TEXT: a shifted line number, or different whitespace, is the same
/// finding; another path is another finding.
#[test]
fn the_finding_id_is_keyed_on_path_and_normalized_line_text() {
    assert_eq!(finding_id("src/lib.rs", "fn b() {"), "f-6a4ac002d79dfc6b");
    assert_eq!(
        finding_id("src/lib.rs", "  fn   b() {  "),
        "f-6a4ac002d79dfc6b"
    );
    assert_ne!(finding_id("src/other.rs", "fn b() {"), "f-6a4ac002d79dfc6b");
}

// ── Admission: a monitor is distinct and admitted, or it never starts ────────────────────

/// DES §12-5 (admission half): a candidate equal to the creator's instance, or one whose
/// adapter is not admitted to input governance, yields `monitorAttached{failed}` and starts
/// NO process; the next admitted candidate is summoned in its place.
#[test]
fn the_creator_and_an_unadmitted_seat_are_refused_without_a_process() {
    let fx = Fixture::new("admit");
    let host = Arc::new(FakeHost {
        unadmitted: vec!["codex#2".into()],
        ..FakeHost::default()
    });
    let (emit, seen) = recorder();
    let mut core = TeamCore::new(host.clone(), emit.clone(), TeamLimits::default());
    core.attach(fx.ctx(1, &["claude", "codex#2", "claude#2"]));
    fx.write("src/lib.rs", "changed\n");
    core.on_event(&checkpoint(1, "edit"));
    pump(&mut core, &host, &emit, Instant::now());
    let attached: Vec<(String, String, String)> = seen
        .lock()
        .unwrap()
        .iter()
        .filter_map(|e| match e {
            CoreEvent::MonitorAttached {
                monitor_id,
                seat,
                status,
                ..
            } => Some((monitor_id.clone(), seat.clone(), status.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        attached,
        vec![
            ("m1".into(), "claude".into(), "failed".into()),
            ("m2".into(), "codex#2".into(), "failed".into()),
            ("m3".into(), "claude#2".into(), "attached".into()),
        ]
    );
    assert_eq!(*host.opens.lock().unwrap(), vec!["claude#2".to_string()]);
}

/// DES §12-6: a plan of zero monitors spawns nothing and emits nothing.
#[test]
fn zero_monitors_spawn_nothing_and_say_nothing() {
    let fx = Fixture::new("zero");
    let host = Arc::new(FakeHost::default());
    let (emit, seen) = recorder();
    let mut core = TeamCore::new(host.clone(), emit.clone(), TeamLimits::default());
    core.attach(fx.ctx(0, &["claude#2"]));
    fx.write("src/lib.rs", "changed\n");
    core.on_event(&checkpoint(1, "edit"));
    pump(&mut core, &host, &emit, Instant::now());
    assert!(host.opens.lock().unwrap().is_empty());
    assert!(seen.lock().unwrap().is_empty());
}

/// Issue #601 acceptance 7: exhausting `MAX_BATCHES` is recorded as
/// `monitors[].status == "budget_exhausted"`, and no further turn is taken.
#[test]
fn max_batches_exhaustion_is_disclosed_in_the_ledger() {
    let fx = Fixture::new("budget");
    let host = Arc::new(FakeHost::default());
    let (emit, _seen) = recorder();
    let limits = TeamLimits {
        batch_min_interval: Duration::ZERO,
        max_batches: 2,
        ..TeamLimits::default()
    };
    let mut core = TeamCore::new(host.clone(), emit.clone(), limits);
    core.attach(fx.ctx(1, &["claude#2"]));
    for i in 0..4u64 {
        fx.write("src/lib.rs", &format!("version {i}\n"));
        core.on_event(&checkpoint(i + 1, "edit"));
        pump(&mut core, &host, &emit, Instant::now());
    }
    assert_eq!(host.turns.load(Ordering::Relaxed), 2);
    let ledger = core
        .take(fx.ctx(1, &["claude#2"]))
        .lock()
        .unwrap()
        .ledger("completed");
    assert_eq!(ledger.monitors[0].status, "budget_exhausted");
    assert_eq!(ledger.monitors[0].batches, 2);
}

/// DES §4.7 step 3: at the final pass a finding whose line moved gets `finalLine`, and one
/// whose text is gone becomes `superseded`. A wrapped unit (never attached) is summoned and
/// reviewed once on the final diff.
#[test]
fn the_final_pass_reviews_the_settled_tree_and_reconfirms_every_finding() {
    let fx = Fixture::new("final");
    let host = Arc::new(FakeHost::default());
    host.replies.lock().unwrap().insert(
        "m1".into(),
        [
            r#"FINDING {"severity":"high","path":"src/lib.rs","line":1,"evidence":"fn a() {}","claim":"a is dead"}"#,
            r#"FINDING {"severity":"high","path":"src/lib.rs","line":3,"evidence":"let x = 5;","claim":"magic"}"#,
            "DONE",
        ]
        .join("\n"),
    );
    let (emit, seen) = recorder();
    let mut core = TeamCore::new(host.clone(), emit.clone(), TeamLimits::default());
    // Wrapped carrier: no attach, no checkpoint — only the final pass.
    fx.write("src/lib.rs", "fn a() {}\nfn b() {\n    let x = 5;\n}\n");
    let unit = core.take(fx.ctx(1, &["claude#2"]));
    let ledger = final_pass(
        &unit,
        &*host,
        &emit,
        TeamLimits::default(),
        true,
        Instant::now() + Duration::from_secs(60),
    );
    assert_eq!(findings(&seen).len(), 2);
    assert_eq!(ledger.final_pass, "completed");
    assert_eq!(ledger.monitors[0].batches, 1);
    // Now the worker keeps editing: a line lands above `fn a`, and `let x = 5;` is removed.
    fx.write("src/lib.rs", "// header\nfn a() {}\nfn b() {\n}\n");
    let again = final_pass(
        &unit,
        &*host,
        &emit,
        TeamLimits::default(),
        true,
        Instant::now() + Duration::from_secs(60),
    );
    let by_line: Vec<(u32, Option<u32>, String)> = again
        .findings
        .iter()
        .map(|f| (f.finding.line, f.final_line, f.status.clone()))
        .collect();
    assert_eq!(
        by_line,
        vec![
            (1, Some(2), "unanswered".to_string()),
            (3, None, "superseded".to_string()),
        ]
    );
}

/// A unit that did not end Ok skips the final pass and says so.
#[test]
fn a_failed_unit_skips_the_final_pass() {
    let fx = Fixture::new("skipped");
    let host = Arc::new(FakeHost::default());
    let (emit, _seen) = recorder();
    let mut core = TeamCore::new(host.clone(), emit.clone(), TeamLimits::default());
    let unit = core.take(fx.ctx(1, &["claude#2"]));
    let ledger = final_pass(
        &unit,
        &*host,
        &emit,
        TeamLimits::default(),
        false,
        Instant::now() + Duration::from_secs(60),
    );
    assert_eq!(ledger.final_pass, "skipped");
    assert_eq!(host.turns.load(Ordering::Relaxed), 0);
}
