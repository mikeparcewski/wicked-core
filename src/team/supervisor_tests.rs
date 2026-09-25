//! DES-TEAMING-002 T6 — the supervisor on the bus, over a real temp bus db and a real git
//! worktree: DES-001 S2 acceptance #1–#6 and #8 re-expressed on rows (T6 (a)), the restart replay
//! (T6 (b)), the hold round (T6 (c)), the councils (T6 (d), DES-001 #15/#16 on rows) and help
//! (T6 (e)). The supervisor core is pumped synchronously (every job runs inline), so no test
//! depends on thread timing; every bound is injected and no test sleeps a production budget.
//!
//! Nothing here touches a real outbox or state home: P1's `rig` puts the bus and the team outbox
//! under the OS temp dir, and the git fixture is a temp repository.

use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

use serde_json::json;

use super::*;
use crate::team::events::{self as tev, TeamBody, TeamEvent};
use crate::team::publish::tests::{fixture_with, rig, Rig};

// ── fixtures ─────────────────────────────────────────────────────────────────────────────────────

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A real git repository with one committed file, and its content snapshot through the same pinned
/// git dir production uses.
pub(crate) struct Fixture {
    pub dir: PathBuf,
    pub repo: Repo,
    pub baseline: String,
}

impl Fixture {
    pub fn new(name: &str, file: &str, text: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "wicked-t6-{name}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, AtomicOrdering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let git = |args: &[&str]| {
            crate::worktree_guard::git(&dir, args, &[]).unwrap();
        };
        git(&["init", "-q"]);
        std::fs::write(dir.join(file), text).unwrap();
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

    pub fn write(&self, rel: &str, text: &str) {
        std::fs::write(self.dir.join(rel), text).unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

type Reply = Box<dyn Fn(&str, &str) -> Result<String, String> + Send + Sync>;

/// A member host that records what it is asked and answers from a script `(pool_key, prompt) →
/// reply`. It keeps the real host's session rule: a turn runs only on an OPEN key, and a failed
/// turn EVICTS the session.
pub(crate) struct FakeHost {
    pub opens: Mutex<Vec<(String, String)>>,
    open_keys: Mutex<HashSet<String>>,
    pub turns: Mutex<Vec<(String, String)>>,
    reply: Mutex<Reply>,
    pub unadmitted: Mutex<Vec<String>>,
}

impl FakeHost {
    pub fn new(
        reply: impl Fn(&str, &str) -> Result<String, String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            opens: Mutex::default(),
            open_keys: Mutex::default(),
            turns: Mutex::default(),
            reply: Mutex::new(Box::new(reply)),
            unadmitted: Mutex::default(),
        }
    }

    pub fn set_reply(
        &self,
        reply: impl Fn(&str, &str) -> Result<String, String> + Send + Sync + 'static,
    ) {
        *self.reply.lock().unwrap() = Box::new(reply);
    }

    pub fn turn_count(&self) -> usize {
        self.turns.lock().unwrap().len()
    }

    pub fn prompts_matching(&self, needle: &str) -> Vec<String> {
        self.turns
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, p)| p.contains(needle))
            .map(|(_, p)| p.clone())
            .collect()
    }
}

impl MonitorHost for FakeHost {
    fn admitted(&self, seat: &str) -> Result<(), String> {
        if self.unadmitted.lock().unwrap().iter().any(|s| s == seat) {
            Err(format!("seat '{seat}' is not admitted to input governance"))
        } else {
            Ok(())
        }
    }
    fn open(&self, pool_key: &str, seat: &str, _scope: &MonitorScope) -> Result<(), String> {
        self.opens
            .lock()
            .unwrap()
            .push((pool_key.to_string(), seat.to_string()));
        self.open_keys.lock().unwrap().insert(pool_key.to_string());
        Ok(())
    }
    fn turn(&self, pool_key: &str, prompt: &str, _budget: Duration) -> Result<String, String> {
        self.turns
            .lock()
            .unwrap()
            .push((pool_key.to_string(), prompt.to_string()));
        if !self.open_keys.lock().unwrap().contains(pool_key) {
            return Err(format!("member '{pool_key}' is not open"));
        }
        let r = (self.reply.lock().unwrap())(pool_key, prompt);
        if r.is_err() {
            self.open_keys.lock().unwrap().remove(pool_key);
        }
        r
    }
    fn close(&self, pool_key: &str) {
        self.open_keys.lock().unwrap().remove(pool_key);
    }
}

type Verdicts = Box<dyn Fn(&DecisionRequest) -> CouncilOutcome + Send + Sync>;

/// A council stub: answers from a script and records every request and its excluded parties. With
/// a `delay`, it honours the budget it is handed the way the real call does (`recv_timeout`): a
/// delay past the budget is a `TimedOut` after the budget.
pub(crate) struct FakeCouncil {
    verdict: Mutex<Verdicts>,
    pub calls: Mutex<Vec<(DecisionRequest, Vec<String>)>>,
    pub delay: Option<Duration>,
}

impl FakeCouncil {
    pub fn new(v: impl Fn(&DecisionRequest) -> CouncilOutcome + Send + Sync + 'static) -> Self {
        Self {
            verdict: Mutex::new(Box::new(v)),
            calls: Mutex::default(),
            delay: None,
        }
    }
    pub fn slow(delay: Duration) -> Self {
        Self {
            delay: Some(delay),
            ..Self::yes()
        }
    }
    pub fn ruling(winner: Option<usize>) -> CouncilOutcome {
        CouncilOutcome::Ruled(DecisionVerdict {
            task_id: "task-1".into(),
            winner,
            consensus: winner.is_some(),
            agreement_pct: if winner.is_some() { 67 } else { 0 },
            returned: 3,
            seated: 3,
            dissent: vec!["one seat disagreed".into()],
            no_ruling_reason: winner.is_none().then(|| "no quorum".into()),
        })
    }
    pub fn yes() -> Self {
        Self::new(|_| Self::ruling(Some(0)))
    }
}

impl Council for FakeCouncil {
    fn convene(
        &self,
        req: DecisionRequest,
        excluded: &[String],
        _budget: Duration,
    ) -> CouncilOutcome {
        self.calls
            .lock()
            .unwrap()
            .push((req.clone(), excluded.to_vec()));
        if let Some(d) = self.delay {
            if d > _budget {
                std::thread::sleep(_budget);
                return CouncilOutcome::TimedOut;
            }
            std::thread::sleep(d);
        }
        (self.verdict.lock().unwrap())(&req)
    }
}

pub(crate) fn sup_cfg(rig: &Rig) -> SupervisorConfig {
    SupervisorConfig {
        bus_db: rig.bus.clone(),
        outbox: rig.outbox.clone(),
        attempt_wait: Duration::from_millis(30),
        limits: TeamLimits {
            batch_min_interval: Duration::ZERO,
            max_batches: MAX_BATCHES_FOR_TESTS,
            diff_cap: super::super::DIFF_CAP,
            monitor_turn_budget: Duration::from_secs(20),
            final_pass_budget: Duration::from_secs(120),
        },
        // Generous: a pass returns as soon as it is done; only a stuck one waits this long
        // (slow CI hosts spawn git slowly). Tests of the deadline itself set their own.
        final_pass_budget: Duration::from_secs(120),
        poll: Duration::from_millis(20),
        max_disputes: MAX_DISPUTES,
        boot_ms: 0,
        tail: None,
        publish_bound: Duration::from_millis(500),
    }
}

const MAX_BATCHES_FOR_TESTS: u32 = 10;

const RUN: &str = "r-t6";
const LIB: &str = "fn a() {}\nfn b() {\n    let x = 1;\n}\n";

/// A synchronous harness: the supervisor core, pumped by hand over the rig's bus.
struct Harness {
    rig: Rig,
    fx: Fixture,
    core: SupervisorCore,
    host: Arc<FakeHost>,
    council: Arc<FakeCouncil>,
    cursor: i64,
}

impl Harness {
    fn new(name: &str) -> Self {
        Self::with(name, sup_cfg, FakeCouncil::yes())
    }

    fn with(name: &str, cfg: impl Fn(&Rig) -> SupervisorConfig, council: FakeCouncil) -> Self {
        let rig = rig(name);
        let fx = Fixture::new(name, "src/lib.rs", LIB);
        let host = Arc::new(FakeHost::new(|_, _| Ok("DONE".into())));
        let council = Arc::new(council);
        let core = SupervisorCore::new(cfg(&rig), host.clone(), council.clone());
        Self {
            rig,
            fx,
            core,
            host,
            council,
            cursor: 0,
        }
    }

    fn publish(&self, ev: &TeamEvent) -> i64 {
        match self.rig.team_bus().publish(ev).unwrap() {
            PublishOutcome::Published(id) => id,
            o => panic!("{} not published: {o:?}", ev.event_type()),
        }
    }

    /// `path.started` (PA `pa`, roster `roster`) and `plan.accepted` in `band`.
    fn start(&self, pa: &str, roster: &[&str], band: &str) -> i64 {
        let floor = self.publish(&fixture_with(tev::PATH_STARTED, 0, RUN, |p| {
            p["cli"] = json!(pa);
            p["roster"] = json!(roster);
        }));
        self.publish(&fixture_with(tev::PLAN_ACCEPTED, 0, RUN, |p| {
            p["band"] = json!(band);
        }));
        floor
    }

    fn claim(&self, ord: u32, attempt: u32, by: &str) -> i64 {
        self.claim_step(ord, attempt, by, "build", true)
    }

    fn claim_step(&self, ord: u32, attempt: u32, by: &str, step: &str, repo: bool) -> i64 {
        let (wd, gd, base) = (
            self.fx.dir.to_string_lossy().into_owned(),
            self.fx.repo.git_dir.to_string_lossy().into_owned(),
            self.fx.baseline.clone(),
        );
        self.publish(&fixture_with(tev::STEP_CLAIMED, 0, RUN, |p| {
            p["ord"] = json!(ord);
            p["attempt"] = json!(attempt);
            p["by"] = json!(by);
            p["at"] = json!(crate::interaction::now_millis());
            p["step_id"] = json!(step);
            p["criterion"] = json!("the handler cancels stale fetches");
            if repo {
                p["baseline_tree"] = json!(base);
                p["repo"] = json!({"workdir": wd, "git_dir": gd});
            } else {
                p["baseline_tree"] = Value::Null;
                p["repo"] = Value::Null;
            }
        }))
    }

    fn checkpoint(&self, ord: u32, attempt: u32, seq: u64, kind: &str) -> i64 {
        self.publish(&fixture_with(tev::CHECKPOINT_REACHED, 0, RUN, |p| {
            p["ord"] = json!(ord);
            p["attempt"] = json!(attempt);
            p["seq"] = json!(seq);
            p["kind"] = json!(kind);
            p["tool_call_id"] = json!(format!("toolu_{seq}"));
        }))
    }

    fn answer(&self, ord: u32, attempt: u32, f: &Value, disposition: &str, reason: &str) -> i64 {
        self.publish(&fixture_with(tev::ADVICE_ANSWERED, 0, RUN, |p| {
            p["ord"] = json!(ord);
            p["attempt"] = json!(attempt);
            p["raise_seq"] = f["raise_seq"].clone();
            p["finding_id"] = f["finding_id"].clone();
            p["disposition"] = json!(disposition);
            p["reason"] = json!(reason);
        }))
    }

    fn delivered(&self, ord: u32, attempt: u32, f: &Value, outcome: &str) -> i64 {
        self.publish(&fixture_with(tev::ADVICE_DELIVERED, 0, RUN, |p| {
            p["ord"] = json!(ord);
            p["attempt"] = json!(attempt);
            p["raise_seq"] = f["raise_seq"].clone();
            p["finding_id"] = f["finding_id"].clone();
            p["channel"] = json!("boundary");
            p["steer_id"] = Value::Null;
            p["outcome"] = json!(outcome);
            p["delivery_id"] = json!(format!("boundary:build:{attempt}"));
        }))
    }

    fn complete(&self, ord: u32, attempt: u32, by: &str, status: &str) -> i64 {
        self.complete_step(ord, attempt, by, "build", status)
    }

    fn complete_step(&self, ord: u32, attempt: u32, by: &str, step: &str, status: &str) -> i64 {
        self.publish(&fixture_with(tev::STEP_COMPLETED, 0, RUN, |p| {
            p["ord"] = json!(ord);
            p["attempt"] = json!(attempt);
            p["by"] = json!(by);
            p["at"] = json!(crate::interaction::now_millis());
            p["step_id"] = json!(step);
            p["status"] = json!(status);
        }))
    }

    /// Read every new row, apply it, and run every job it makes due, inline, until quiet.
    fn pump(&mut self) {
        loop {
            let db = BusDb::shared(&self.rig.bus).unwrap();
            let batch = db.poll(TEAM_FILTER, self.cursor, 1000).unwrap();
            let mut jobs = Vec::new();
            for ev in &batch {
                self.cursor = self.cursor.max(ev.event_id);
                let Ok(event) = TeamEvent::from_payload(&ev.event_type, &ev.payload) else {
                    continue;
                };
                jobs.extend(self.core.on_row(
                    &TeamRow {
                        event_id: ev.event_id,
                        event,
                    },
                    false,
                ));
            }
            jobs.extend(self.core.due_batches(Instant::now()));
            if batch.is_empty() && jobs.is_empty() {
                return;
            }
            for job in jobs {
                self.run(job);
            }
        }
    }

    fn run(&mut self, job: Job) {
        match job {
            Job::Batch(b) => {
                let done = run_job(&b, &*self.host);
                self.core.apply_batch(done);
            }
            Job::FinalPass(fp) => run_final_pass(*fp, &*self.host, &*self.council),
            Job::Help(h) => run_help(&h, &*self.host, &self.core.publisher_bus()),
        }
    }

    /// The payloads of every `event_type` row of the run, in bus order.
    fn rows(&self, event_type: &str) -> Vec<Value> {
        BusDb::shared(&self.rig.bus)
            .unwrap()
            .poll(event_type, 0, 10_000)
            .unwrap()
            .into_iter()
            .filter(|e| e.payload["run_id"] == RUN)
            .map(|e| e.payload)
            .collect()
    }

    fn ids(&self, event_type: &str) -> Vec<(i64, Value)> {
        BusDb::shared(&self.rig.bus)
            .unwrap()
            .poll(event_type, 0, 10_000)
            .unwrap()
            .into_iter()
            .filter(|e| e.payload["run_id"] == RUN)
            .map(|e| (e.event_id, e.payload))
            .collect()
    }

    /// The attempt's folded ledger (S's `ledger.folded`), parsed.
    fn folded(&self, ord: u32, attempt: u32) -> TeamLedger {
        let p = self
            .rows(tev::LEDGER_FOLDED)
            .into_iter()
            .find(|p| p["ord"] == ord && p["attempt"] == attempt)
            .unwrap_or_else(|| panic!("no ledger.folded for {ord}:{attempt}"));
        match TeamEvent::from_payload(tev::LEDGER_FOLDED, &p)
            .unwrap()
            .body
        {
            TeamBody::LedgerFolded(b) => b.ledger,
            _ => unreachable!(),
        }
    }
}

fn finding_line(severity: &str, path: &str, line: u32, evidence: &str, claim: &str) -> String {
    format!(
        "FINDING {}",
        json!({"severity": severity, "path": path, "line": line, "evidence": evidence,
               "claim": claim})
    )
}

// ── (a) DES-001 S2 #1: checkpoints at terminal tool calls, never per token ──────────────────────

/// DES-001 #1 on rows: a CLAIMED attempt's `tool_call` (kind `edit`) then its terminal
/// `tool_call_update` yields exactly ONE `checkpoint.reached` carrying the kind, title and paths
/// the earlier frame named; an unclaimed attempt (no `step.claimed` on the bus) yields none.
#[test]
fn t6_a1_a_claimed_turn_checkpoints_once_per_terminal_tool_call() {
    let rig = rig("t6a1");
    let runner = super::super::runner::TeamRunner::from_config(&TeamConfig::new(
        Some(rig.bus.clone()),
        Some(rig.outbox.clone()),
    ))
    .unwrap();
    let claim = Some(super::super::TurnClaim {
        runner,
        claimed_id: 7,
        by: "claude#1".into(),
    });
    let frame = |u: Value| json!({"method": "session/update", "params": {"update": u}});
    let frames = [
        frame(json!({"sessionUpdate": "agent_message_chunk", "content": {"text": "Let me"}})),
        frame(
            json!({"sessionUpdate": "tool_call", "toolCallId": "toolu_1", "kind": "edit",
            "title": "Edit src/retire.ts", "locations": [{"path": "/wt/src/retire.ts"}]}),
        ),
        frame(
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "toolu_1",
            "status": "in_progress"}),
        ),
        frame(
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "toolu_1",
            "status": "completed"}),
        ),
    ];
    let teamed =
        super::super::TeamTurn::new((RUN.into(), 3, 1), claim, None, Some(PathBuf::from("/wt")));
    let out: Vec<TeamEvent> = frames.iter().filter_map(|f| teamed.observe(f)).collect();
    assert_eq!(out.len(), 1, "{out:?}");
    let TeamBody::CheckpointReached(b) = &out[0].body else {
        panic!("{out:?}")
    };
    assert_eq!(
        (b.seq, b.kind.as_str(), b.title.as_str(), b.paths.clone()),
        (
            1,
            "edit",
            "Edit src/retire.ts",
            vec!["src/retire.ts".to_string()]
        )
    );
    assert_eq!(b.status, tev::CheckpointStatus::Completed);
    assert_eq!((out[0].env.ord, out[0].env.attempt), (Some(3), Some(1)));
    let unclaimed = super::super::TeamTurn::new((RUN.into(), 3, 1), None, None, None);
    assert!(frames.iter().all(|f| unclaimed.observe(f).is_none()));
}

// ── (a) DES-001 S2 #2: batching ──────────────────────────────────────────────────────────────────

/// DES-001 #2 on rows: 30 tree-changing checkpoints inside the interval with one tree change make
/// exactly ONE member turn.
#[test]
fn t6_a2_thirty_checkpoints_with_one_tree_change_make_one_member_turn() {
    let mut h = Harness::with(
        "t6a2",
        |rig| {
            let mut c = sup_cfg(rig);
            c.limits.batch_min_interval = Duration::from_secs(60);
            c
        },
        FakeCouncil::yes(),
    );
    h.start("claude#1", &["claude#1", "claude#2"], "20-39");
    h.claim(3, 1, "claude#1");
    h.pump();
    h.fx.write("src/lib.rs", "fn a() {}\nfn b() {\n    let x = 2;\n}\n");
    for seq in 1..=30 {
        h.checkpoint(3, 1, seq, "edit");
        h.pump();
    }
    assert_eq!(h.host.turn_count(), 1, "one member turn in the interval");
    assert_eq!(h.rows(tev::MEMBER_JOINED).len(), 1);
}

/// DES-001 #2's second clause on rows: a checkpoint burst on an unchanged tree makes no turn,
/// opens no session and publishes nothing (read-only kinds do not even mark it pending).
#[test]
fn t6_a2_a_burst_on_an_unchanged_tree_makes_no_turn_and_reads_start_nothing() {
    let mut h = Harness::new("t6a2u");
    h.start("claude#1", &["claude#1", "claude#2"], "20-39");
    h.claim(3, 1, "claude#1");
    for seq in 1..=5 {
        h.checkpoint(3, 1, seq, "execute");
    }
    for seq in 6..=10 {
        h.checkpoint(3, 1, seq, "read");
    }
    h.pump();
    assert_eq!(h.host.turn_count(), 0, "an unchanged tree makes no turn");
    assert!(h.rows(tev::FINDING_RAISED).is_empty());
    assert!(
        h.rows(tev::MEMBER_JOINED).is_empty(),
        "no session opened for an unchanged tree"
    );
}

// ── (a) DES-001 S2 #3: confirmation ──────────────────────────────────────────────────────────────

/// DES-001 #3 on rows: a reply citing a `path:line` whose text is not the snapshot's raises
/// nothing and counts `unconfirmed` in the folded ledger; a matching one raises one
/// `finding.raised` with the snapshot tree id.
#[test]
fn t6_a3_only_a_finding_confirmed_at_its_line_is_raised() {
    let mut h = Harness::new("t6a3");
    let good = finding_line("high", "src/lib.rs", 3, "    let x = 2;", "x is stale");
    let bad = finding_line("high", "src/lib.rs", 3, "    let x = 99;", "invented");
    h.host
        .set_reply(move |_, _| Ok(format!("{good}\n{bad}\nDONE")));
    h.start("claude#1", &["claude#1", "claude#2"], "20-39");
    h.claim(3, 1, "claude#1");
    h.fx.write("src/lib.rs", "fn a() {}\nfn b() {\n    let x = 2;\n}\n");
    h.checkpoint(3, 1, 1, "edit");
    h.pump();
    let raised = h.rows(tev::FINDING_RAISED);
    assert_eq!(raised.len(), 1, "{raised:?}");
    let tree = h.fx.repo.snapshot().unwrap();
    assert_eq!(raised[0]["tree"], json!(tree));
    assert_eq!(raised[0]["line"], 3);
    assert_eq!(raised[0]["raise_seq"], 1);
    assert_eq!(raised[0]["by"], "claude#2");
    h.complete(3, 1, "claude#1", "ok");
    h.pump();
    let l = h.folded(3, 1);
    assert_eq!(l.rejected.unconfirmed, 1, "{l:?}");
    assert_eq!(l.findings.len(), 1);
}

// ── (a) DES-001 S2 #4: dedup, anchors, the bar ───────────────────────────────────────────────────

const RETIRE: &str =
    "fn retire(scope: &str) {\n    let a = 1;\n    store.erase_scope(scope)?;\n}\n\n\
fn purge(scope: &str) {\n    let b = 2;\n    store.erase_scope(scope)?;\n}\n";

/// DES-001 #4 on rows: two members citing the same line text in the same anchor raise ONE finding
/// and the ledger lists the second in `corroboratedBy`; the same hazardous line in `retire()` and
/// `purge()` raises TWO findings with different ids, the same `line_key` and different anchors; a
/// `low` raises nothing and counts `belowBar`.
#[test]
fn t6_a4_dedup_is_by_anchor_and_line_text_and_low_is_below_the_bar() {
    let mut h = Harness::new("t6a4");
    h.fx.write("src/lib.rs", RETIRE);
    let l3 = finding_line(
        "high",
        "src/lib.rs",
        3,
        "    store.erase_scope(scope)?;",
        "no rollback",
    );
    let l8 = finding_line(
        "high",
        "src/lib.rs",
        8,
        "    store.erase_scope(scope)?;",
        "no rollback",
    );
    let low = finding_line("low", "src/lib.rs", 1, "fn retire(scope: &str) {", "nit");
    let (a, b) = (l3.clone(), l3.clone());
    h.host.set_reply(move |key, _| {
        if key.ends_with(":m1") {
            Ok(format!("{a}\n{l8}\n{low}\nDONE"))
        } else {
            Ok(format!("{b}\nDONE"))
        }
    });
    h.start("claude#1", &["claude#1", "claude#2", "claude#3"], "40-69");
    h.claim(3, 1, "claude#1");
    h.checkpoint(3, 1, 1, "edit");
    h.pump();
    let raised = h.rows(tev::FINDING_RAISED);
    assert_eq!(raised.len(), 2, "{raised:#?}");
    assert_ne!(raised[0]["finding_id"], raised[1]["finding_id"]);
    assert_eq!(raised[0]["line_key"], raised[1]["line_key"]);
    let anchors: Vec<&str> = raised
        .iter()
        .map(|r| r["anchor"].as_str().unwrap())
        .collect();
    assert_eq!(
        anchors,
        ["fn retire(scope: &str) {", "fn purge(scope: &str) {"]
    );
    assert!(raised.iter().all(|r| r["anchor_source"] == "hunk"));
    h.complete(3, 1, "claude#1", "ok");
    h.pump();
    let l = h.folded(3, 1);
    let retire = l
        .findings
        .iter()
        .find(|f| f.finding.line == 3)
        .expect("the retire finding");
    assert_eq!(retire.corroborated_by, vec!["claude#3".to_string()]);
    assert_eq!(l.rejected.below_bar, 1);
}

/// DES-001 #4's moved-line clause: a finding whose line moves keeps its id and its ledger entry
/// gets its `finalLine` by line key at the final pass.
#[test]
fn t6_a4_a_moved_line_keeps_its_id_and_gets_its_final_line() {
    let mut h = Harness::new("t6a4m");
    let hit = finding_line("medium", "src/lib.rs", 3, "    let x = 2;", "x is stale");
    h.host.set_reply(move |_, p| {
        if p.contains("batch —") {
            Ok(format!("{hit}\nDONE"))
        } else {
            Ok("DONE".into())
        }
    });
    h.start("claude#1", &["claude#1", "claude#2"], "20-39");
    h.claim(3, 1, "claude#1");
    h.fx.write("src/lib.rs", "fn a() {}\nfn b() {\n    let x = 2;\n}\n");
    h.checkpoint(3, 1, 1, "edit");
    h.pump();
    h.fx.write(
        "src/lib.rs",
        "// header\n// more\nfn a() {}\nfn b() {\n    let x = 2;\n}\n",
    );
    h.complete(3, 1, "claude#1", "ok");
    h.pump();
    let l = h.folded(3, 1);
    assert_eq!(l.findings.len(), 1);
    assert_eq!(l.findings[0].final_line, Some(5), "{l:?}");
}

// ── (a) DES-001 S2 #5 / #6: admission, zero monitors ─────────────────────────────────────────────

/// DES-001 #5 on rows: a candidate that is the creator's instance, or on an unadmitted adapter,
/// yields `member.joined{status:"failed"}` and opens no session.
#[test]
fn t6_a5_the_creator_and_an_unadmitted_seat_join_failed_without_a_process() {
    let mut h = Harness::new("t6a5");
    h.host.unadmitted.lock().unwrap().push("codex".into());
    // The PA is claude#1; this step's creator is the member claude#2 (a member's step).
    h.start(
        "claude#1",
        &["claude#1", "claude#2", "codex", "claude#3"],
        "40-69",
    );
    h.claim(3, 1, "claude#2");
    h.fx.write("src/lib.rs", "fn a() {}\nfn b() {\n    let x = 2;\n}\n");
    h.checkpoint(3, 1, 1, "edit");
    h.pump();
    let joined = h.rows(tev::MEMBER_JOINED);
    let failed: Vec<(&str, &str)> = joined
        .iter()
        .filter(|j| j["status"] == "failed")
        .map(|j| (j["seat"].as_str().unwrap(), j["error"].as_str().unwrap()))
        .collect();
    assert_eq!(failed.len(), 2, "{joined:#?}");
    assert!(
        failed[0].0 == "claude#2" && failed[0].1.contains("creator"),
        "{failed:?}"
    );
    assert!(
        failed[1].0 == "codex" && failed[1].1.contains("admitted"),
        "{failed:?}"
    );
    let opened: Vec<String> = h
        .host
        .opens
        .lock()
        .unwrap()
        .iter()
        .map(|(_, s)| s.clone())
        .collect();
    assert_eq!(
        opened,
        vec!["claude#3".to_string()],
        "only the admitted distinct seat opens"
    );
}

/// DES-001 #6 on rows: a docs-only band (S4 `monitors: 0`) summons nothing and publishes no
/// `member.joined`.
#[test]
fn t6_a6_a_zero_monitor_band_summons_nothing_and_says_nothing() {
    let mut h = Harness::new("t6a6");
    h.start("claude#1", &["claude#1", "claude#2"], "0-19");
    h.claim(3, 1, "claude#1");
    h.fx.write("docs.md", "# doc\n");
    h.checkpoint(3, 1, 1, "edit");
    h.complete(3, 1, "claude#1", "ok");
    h.pump();
    assert!(h.rows(tev::MEMBER_JOINED).is_empty());
    assert!(h.host.opens.lock().unwrap().is_empty());
    assert!(h.folded(3, 1).monitors.is_empty());
}

/// The band the supervisor reads decides the target; a band the stream does not state is the top
/// band (a missing score never watches less).
#[test]
fn monitor_target_reads_the_band_and_fails_closed_to_the_top_band() {
    assert_eq!(monitor_target(Some("0-19"), 0), 0);
    assert_eq!(monitor_target(Some("20-39"), 0), 1);
    assert_eq!(monitor_target(Some("40-69"), 0), 2);
    assert_eq!(monitor_target(Some("70-100"), 0), 3);
    assert_eq!(monitor_target(Some("0-19"), 2), 2, "the PA's ask raises it");
    assert_eq!(
        monitor_target(Some("20-39"), 9),
        3,
        "never past the ceiling"
    );
    assert_eq!(monitor_target(None, 0), 3, "no band = the top band");
    assert_eq!(monitor_target(Some("garbage"), 0), 3);
}

// ── (a) DES-001 S2 #8: a wrapped unit gets the final-pass ledger ────────────────────────────────

/// DES-001 #8 on rows: a unit with NO checkpoints (a wrapped carrier) is monitored at its final
/// pass: `member.joined{attached}`, one `finding.raised`, and a `ledger.folded` with one batch,
/// `completed`, the finding `not_delivered` and unanswered; its member gets ONE hold-round turn
/// listing it as "not delivered — …", answers WITHDRAW, and the ledger records it withdrawn, no
/// council, `teamPause:false`.
#[test]
fn t6_a8_a_wrapped_unit_gets_the_final_pass_ledger_and_the_hold_round() {
    let mut h = Harness::new("t6a8");
    let hit = finding_line("high", "src/lib.rs", 3, "    let x = 5;", "x is wrong");
    h.host.set_reply(move |_, p| {
        if p.contains("hold round") {
            let id = p
                .split_whitespace()
                .find(|w| w.starts_with("f-"))
                .unwrap()
                .to_string();
            Ok(format!("WITHDRAW {id} — the worker is right\nDONE"))
        } else {
            Ok(format!("{hit}\nDONE"))
        }
    });
    h.start("claude#1", &["claude#1", "claude#2"], "20-39");
    h.claim(3, 1, "claude#1");
    h.fx.write("src/lib.rs", "fn a() {}\nfn b() {\n    let x = 5;\n}\n");
    h.complete(3, 1, "claude#1", "ok");
    h.pump();
    assert!(h.rows(tev::CHECKPOINT_REACHED).is_empty());
    let joined = h.rows(tev::MEMBER_JOINED);
    assert_eq!(joined.len(), 1);
    assert_eq!(joined[0]["status"], "attached");
    assert_eq!(h.rows(tev::FINDING_RAISED).len(), 1);
    let holds = h.host.prompts_matching("hold round");
    assert_eq!(holds.len(), 1, "exactly one hold-round turn");
    assert!(holds[0].contains("not delivered —"), "{}", holds[0]);
    let l = h.folded(3, 1);
    assert_eq!(l.final_pass, FinalPass::Completed);
    assert_eq!(l.monitors.len(), 1);
    assert_eq!(l.monitors[0].batches, 1);
    let f = &l.findings[0];
    assert_eq!(f.delivery, super::super::LedgerDelivery::NotDelivered);
    assert_eq!(f.status, FindingStatus::Withdrawn);
    assert_eq!(f.monitor_reply.as_ref().unwrap().kind, ReplyKind::Withdraw);
    assert!(f.dispute.is_none());
    assert!(!l.team_pause);
    assert!(h.rows(tev::COUNCIL_CALLED).is_empty());
    // The members were closed: one member.left per opening.
    assert_eq!(h.rows(tev::MEMBER_LEFT).len(), 1);
}

// ── (b) a restart between two batches loses no finding ──────────────────────────────────────────

/// T6 (b): supervisor A raises F1 in attempt 1's first batch; the daemon restarts (A is gone, and
/// attempt 1 with it). Supervisor B replays the run from its floor, and attempt 2's claim arrives
/// live: B carries F1 into attempt 2 (`carried_from_attempt: 1`), a second batch raises F2, and
/// attempt 2's fold contains both.
#[test]
fn t6_b_a_restart_between_two_batches_loses_no_finding() {
    let mut h = Harness::new("t6b");
    let f1 = finding_line("high", "src/lib.rs", 3, "    let x = 2;", "x is stale");
    h.host.set_reply(move |_, _| Ok(format!("{f1}\nDONE")));
    let floor = h.start("claude#1", &["claude#1", "claude#2"], "20-39");
    h.claim(3, 1, "claude#1");
    h.fx.write("src/lib.rs", "fn a() {}\nfn b() {\n    let x = 2;\n}\n");
    h.checkpoint(3, 1, 1, "edit");
    h.pump();
    assert_eq!(h.rows(tev::FINDING_RAISED).len(), 1);

    // The restart: a new supervisor whose boot is after attempt 1's claim.
    let mut cfg = sup_cfg(&h.rig);
    cfg.boot_ms = crate::interaction::now_millis() + 1;
    std::thread::sleep(Duration::from_millis(5));
    let host_b = Arc::new(FakeHost::new(|_, _| {
        Ok(format!(
            "{}\nDONE",
            finding_line("medium", "src/lib.rs", 1, "fn a() {}", "a is dead")
        ))
    }));
    let mut b = SupervisorCore::new(cfg, host_b.clone(), h.council.clone());
    b.arm(&LiveTeamRun {
        run_id: RUN.into(),
        status: crate::domain::SessionStatus::Executing,
        team: crate::domain::RunTeamState {
            transport: Some(Transport::Bus),
            stream_floor: Some(floor),
            ..Default::default()
        },
        roster: vec!["claude#1".into(), "claude#2".into()],
    });
    let tail = BusDb::shared(&h.rig.bus).unwrap().tail_event_id().unwrap();
    let (cursor, jobs) = replay(&mut b, &h.rig.bus, tail, None);
    assert!(jobs.is_empty(), "history starts nothing");
    h.core = b;
    h.host = host_b;
    h.cursor = cursor;
    h.claim(3, 2, "claude#1");
    h.pump();
    let carried: Vec<Value> = h
        .rows(tev::FINDING_RAISED)
        .into_iter()
        .filter(|r| r["attempt"] == 2)
        .collect();
    assert_eq!(carried.len(), 1, "{carried:#?}");
    assert_eq!(carried[0]["carried_from_attempt"], 1);
    h.fx.write("src/lib.rs", "fn a() {}\nfn b() {\n    let x = 3;\n}\n");
    h.checkpoint(3, 2, 1, "edit");
    h.pump();
    h.complete(3, 2, "claude#1", "ok");
    h.pump();
    let l = h.folded(3, 2);
    let claims: BTreeSet<&str> = l
        .findings
        .iter()
        .map(|f| f.finding.claim.as_str())
        .collect();
    assert!(
        claims.contains("x is stale") && claims.contains("a is dead"),
        "{l:#?}"
    );
    let f1 = l
        .findings
        .iter()
        .find(|f| f.finding.claim == "x is stale")
        .unwrap();
    assert_eq!(f1.finding.carried_from_attempt, Some(1));
}

/// §4.7: a row seen both in replay and live changes nothing the second time.
#[test]
fn t6_k_a_row_replayed_and_delivered_live_changes_nothing() {
    let mut h = Harness::new("t6k-dup");
    h.start("claude#1", &["claude#1", "claude#2"], "20-39");
    h.claim(3, 1, "claude#1");
    h.pump();
    let before = h.core.units.len();
    // Re-apply every row once more, as a replay would.
    let db = BusDb::shared(&h.rig.bus).unwrap();
    for ev in db.poll(TEAM_FILTER, 0, 1000).unwrap() {
        let event = TeamEvent::from_payload(&ev.event_type, &ev.payload).unwrap();
        let jobs = h.core.on_row(
            &TeamRow {
                event_id: ev.event_id,
                event,
            },
            true,
        );
        assert!(jobs.is_empty());
    }
    assert_eq!(h.core.units.len(), before);
    assert_eq!(h.rows(tev::MEMBER_JOINED).len(), 0);
}

// ── (c) the hold round ───────────────────────────────────────────────────────────────────────────

/// T6 (c): the hold round publishes exactly ONE `finding.settled` per unaccepted finding — a
/// declined one, an injected-but-unanswered one and a never-delivered one — and none for the
/// accepted one; each member is asked once, about its own findings, each listed with the worker's
/// state; silence is `held` ("no reply (counted as hold)").
#[test]
fn t6_c_the_hold_round_settles_each_unaccepted_finding_once_and_silence_holds() {
    let mut h = Harness::with(
        "t6c",
        sup_cfg,
        FakeCouncil::new(|_| FakeCouncil::ruling(Some(0))),
    );
    let text = "fn a() {\n    one();\n    two();\n    three();\n    four();\n}\n";
    h.fx.write("src/lib.rs", text);
    let lines = [
        finding_line("high", "src/lib.rs", 2, "    one();", "one is wrong"),
        finding_line("high", "src/lib.rs", 3, "    two();", "two is wrong"),
        finding_line("high", "src/lib.rs", 4, "    three();", "three is wrong"),
        finding_line("medium", "src/lib.rs", 5, "    four();", "four is wrong"),
    ];
    let reply = lines.join("\n");
    h.host.set_reply(move |_, p| {
        if p.contains("hold round") {
            // HOLD the first listed, say nothing about the rest.
            let first = p
                .lines()
                .find(|l| l.starts_with("- f-"))
                .and_then(|l| l.split_whitespace().nth(1))
                .unwrap()
                .to_string();
            Ok(format!("HOLD {first} — it is a real bug\nDONE"))
        } else {
            Ok(format!("{reply}\nDONE"))
        }
    });
    h.start("claude#1", &["claude#1", "claude#2"], "20-39");
    h.claim(3, 1, "claude#1");
    h.checkpoint(3, 1, 1, "edit");
    h.pump();
    let raised = h.rows(tev::FINDING_RAISED);
    assert_eq!(raised.len(), 4);
    h.answer(3, 1, &raised[0], "declined", "one is fine: spec §2");
    h.delivered(3, 1, &raised[1], "injected");
    h.delivered(3, 1, &raised[2], "turn_ended");
    h.answer(3, 1, &raised[3], "accepted", "fixed");
    h.complete(3, 1, "claude#1", "ok");
    h.pump();
    let holds = h.host.prompts_matching("hold round");
    assert_eq!(holds.len(), 1, "one member, one turn");
    assert!(
        holds[0].contains("one is fine: spec §2"),
        "the decline reason"
    );
    assert!(holds[0].contains("no answer"), "injected, unanswered");
    assert!(
        holds[0].contains("not delivered — turn_ended"),
        "{}",
        holds[0]
    );
    assert!(
        !holds[0].contains("four is wrong"),
        "the accepted finding is not listed"
    );
    let settled = h.rows(tev::FINDING_SETTLED);
    assert_eq!(settled.len(), 3, "{settled:#?}");
    let mut by_seq: Vec<(u64, String, String)> = settled
        .iter()
        .map(|s| {
            (
                s["raise_seq"].as_u64().unwrap(),
                s["status"].as_str().unwrap().to_string(),
                s["reason"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    by_seq.sort();
    assert_eq!(by_seq[0].1, "held");
    assert_eq!(by_seq[0].2, "it is a real bug");
    for (_, status, reason) in &by_seq[1..] {
        assert_eq!(status, "held");
        assert_eq!(reason, "no reply (counted as hold)");
    }
}

// ── (d) councils: DES-001 #15 / #16 on rows ──────────────────────────────────────────────────────

/// Set up one unresolved HIGH on attempt 1 (declined by the worker, held by the member) and fold.
fn one_unresolved_high(name: &str, council: FakeCouncil, disposition: Option<&str>) -> Harness {
    let mut h = Harness::with(name, sup_cfg, council);
    let hit = finding_line("high", "src/lib.rs", 3, "    let x = 2;", "x is stale");
    h.host.set_reply(move |_, p| {
        if p.contains("hold round") {
            Ok("DONE".into())
        } else {
            Ok(format!("{hit}\nDONE"))
        }
    });
    h.start("claude#1", &["claude#1", "claude#2", "codex"], "20-39");
    h.claim(3, 1, "claude#1");
    h.fx.write("src/lib.rs", "fn a() {}\nfn b() {\n    let x = 2;\n}\n");
    h.checkpoint(3, 1, 1, "edit");
    h.pump();
    let raised = h.rows(tev::FINDING_RAISED);
    match disposition {
        Some("declined") => {
            h.delivered(3, 1, &raised[0], "injected");
            h.answer(3, 1, &raised[0], "declined", "x is intentional");
        }
        Some("unanswered") => {
            h.delivered(3, 1, &raised[0], "injected");
        }
        _ => {}
    }
    h.complete(3, 1, "claude#1", "ok");
    h.pump();
    h
}

/// DES-001 #15 on rows: a declined-and-held HIGH convenes exactly ONE council. `council.called`
/// carries the finding's question, the worker's and the member's positions, the evidence, the
/// parties it excludes (the creator and the author), and a transcript whose ids are exactly that
/// finding's rows (raised, delivered, answered, settled).
#[test]
fn t6_d15_a_declined_held_high_convenes_one_council_with_its_transcript() {
    let h = one_unresolved_high("t6d15", FakeCouncil::yes(), Some("declined"));
    let called = h.ids(tev::COUNCIL_CALLED);
    assert_eq!(called.len(), 1, "{called:#?}");
    let c = &called[0].1;
    assert_eq!(c["trigger"], "unresolved_high");
    assert_eq!(c["subject"], "finding:1");
    let excluded: Vec<&str> = c["excluded_seats"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(excluded, ["claude#1", "claude#2"]);
    assert_eq!(c["positions"][0]["reason"], "x is intentional");
    assert!(c["evidence"].as_str().unwrap().contains("x is stale"));
    // The transcript names exactly this finding's rows.
    let mut want: Vec<i64> = Vec::new();
    for t in [
        tev::FINDING_RAISED,
        tev::ADVICE_DELIVERED,
        tev::ADVICE_ANSWERED,
        tev::FINDING_SETTLED,
    ] {
        want.extend(h.ids(t).into_iter().map(|(id, _)| id));
    }
    want.sort();
    let got: Vec<i64> = c["transcript"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_i64().unwrap())
        .collect();
    assert_eq!(got, want);
    let calls = h.council.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].1,
        vec!["claude#1".to_string(), "claude#2".to_string()]
    );
    assert_eq!(h.rows(tev::COUNCIL_RULED).len(), 1);
}

/// DES-001 #15: an accepted HIGH, a MEDIUM, or a member WITHDRAW convenes none.
#[test]
fn t6_d15_accepted_medium_or_withdrawn_convenes_no_council() {
    let mut h = Harness::new("t6d15-none");
    let hi = finding_line("high", "src/lib.rs", 3, "    let x = 2;", "x is stale");
    let med = finding_line("medium", "src/lib.rs", 1, "fn a() {}", "a is dead");
    h.host.set_reply(move |_, p| {
        if p.contains("hold round") {
            let id = p
                .split_whitespace()
                .find(|w| w.starts_with("f-"))
                .unwrap()
                .to_string();
            Ok(format!("WITHDRAW {id} — fine\nDONE"))
        } else {
            Ok(format!("{hi}\n{med}\nDONE"))
        }
    });
    h.start("claude#1", &["claude#1", "claude#2"], "20-39");
    h.claim(3, 1, "claude#1");
    h.fx.write("src/lib.rs", "fn a() {}\nfn b() {\n    let x = 2;\n}\n");
    h.checkpoint(3, 1, 1, "edit");
    h.pump();
    let raised = h.rows(tev::FINDING_RAISED);
    let high = raised
        .iter()
        .find(|r| r["severity"] == "high")
        .unwrap()
        .clone();
    h.answer(3, 1, &high, "accepted", "fixed");
    h.complete(3, 1, "claude#1", "ok");
    h.pump();
    assert!(h.rows(tev::COUNCIL_CALLED).is_empty());
    assert!(!h.folded(3, 1).team_pause);
}

/// DES-001 #16 (a)/(b) on rows: council YES → `dispute.verdict:"yes"`, no pause; NO → the ledger
/// pauses.
#[test]
fn t6_d16_ab_council_yes_continues_and_no_pauses() {
    let yes = one_unresolved_high("t6d16a", FakeCouncil::yes(), Some("declined"));
    let l = yes.folded(3, 1);
    assert_eq!(
        l.findings[0].dispute.as_ref().unwrap().verdict,
        Verdict::Yes
    );
    assert!(!l.team_pause);
    assert_eq!(yes.rows(tev::COUNCIL_RULED)[0]["verdict"], "yes");
    let no = one_unresolved_high(
        "t6d16b",
        FakeCouncil::new(|_| FakeCouncil::ruling(Some(1))),
        Some("declined"),
    );
    let l = no.folded(3, 1);
    assert_eq!(l.findings[0].dispute.as_ref().unwrap().verdict, Verdict::No);
    assert!(l.team_pause);
}

/// DES-001 #16 (c) on rows: every no-verdict reason pauses, with the matching reason on
/// `council.ruled` and in the ledger.
#[test]
fn t6_d16_c_every_no_verdict_reason_pauses() {
    type Case = (&'static str, fn() -> CouncilOutcome, &'static str);
    let cases: Vec<Case> = vec![
        ("nq", || FakeCouncil::ruling(None), "no_quorum"),
        (
            "sb",
            || CouncilOutcome::NoSeats("none".into()),
            "seats_benched",
        ),
        ("er", || CouncilOutcome::Failed("boom".into()), "error"),
        ("to", || CouncilOutcome::TimedOut, "timeout"),
    ];
    for (name, out, reason) in cases {
        let h = one_unresolved_high(
            &format!("t6d16c-{name}"),
            FakeCouncil::new(move |_| out()),
            Some("declined"),
        );
        let ruled = h.rows(tev::COUNCIL_RULED);
        assert_eq!(ruled[0]["verdict"], "no_verdict", "{name}");
        assert_eq!(ruled[0]["reason"], reason, "{name}");
        let l = h.folded(3, 1);
        assert!(l.team_pause, "{name}: {l:?}");
    }
}

/// DES-001 #16 (c) `cap`: past `MAX_DISPUTES` unresolved HIGHs in one attempt, the next dispute is
/// called and ruled `no_verdict{cap}` without convening; the ledger pauses.
#[test]
fn t6_d16_c_disputes_past_the_cap_rule_no_verdict_cap() {
    let mut h = Harness::new("t6d16cap");
    let text = "fn a() {\n    one();\n    two();\n    three();\n    four();\n}\n";
    h.fx.write("src/lib.rs", text);
    let reply: Vec<String> = ["one", "two", "three", "four"]
        .iter()
        .enumerate()
        .map(|(i, w)| {
            finding_line(
                "high",
                "src/lib.rs",
                i as u32 + 2,
                &format!("    {w}();"),
                w,
            )
        })
        .collect();
    let reply = reply.join("\n");
    h.host.set_reply(move |_, p| {
        if p.contains("hold round") {
            Ok("DONE".into())
        } else {
            Ok(format!("{reply}\nDONE"))
        }
    });
    h.start("claude#1", &["claude#1", "claude#2"], "20-39");
    h.claim(3, 1, "claude#1");
    h.checkpoint(3, 1, 1, "edit");
    h.complete(3, 1, "claude#1", "ok");
    h.pump();
    assert_eq!(h.rows(tev::COUNCIL_CALLED).len(), 4);
    assert_eq!(h.council.calls.lock().unwrap().len(), 3, "three convened");
    let ruled = h.rows(tev::COUNCIL_RULED);
    assert_eq!(
        ruled.iter().filter(|r| r["reason"] == "cap").count(),
        1,
        "{ruled:#?}"
    );
    assert!(h.folded(3, 1).team_pause);
}

/// DES-001 #16 (h)/(i) on rows: an unanswered (injected, no ADVICE line) and a never-delivered
/// HIGH each convene one council; the worker's position reads "no answer" / "not delivered — …".
#[test]
fn t6_d16_hi_unanswered_and_undelivered_highs_take_the_council_path() {
    let h = one_unresolved_high("t6d16h", FakeCouncil::yes(), Some("unanswered"));
    let c = &h.rows(tev::COUNCIL_CALLED)[0];
    assert_eq!(c["positions"][0]["reason"], "no answer");
    let h = one_unresolved_high("t6d16i", FakeCouncil::yes(), None);
    let c = &h.rows(tev::COUNCIL_CALLED)[0];
    assert!(
        c["positions"][0]["reason"]
            .as_str()
            .unwrap()
            .starts_with("not delivered —"),
        "{c}"
    );
}

/// DES-001 #16 (j) on rows: a council that never returns within the final-pass budget → the
/// ledger is `timed_out`, the HIGH is held with `no_verdict{timeout}`, and it pauses — and S's
/// fold is published before the worker's deadline.
#[test]
fn t6_d16_j_a_council_past_the_budget_times_out_and_pauses() {
    let mut h = Harness::with(
        "t6d16j",
        |rig| {
            let mut c = sup_cfg(rig);
            c.final_pass_budget = Duration::from_secs(8);
            c
        },
        FakeCouncil::slow(Duration::from_secs(60)),
    );
    let hit = finding_line("high", "src/lib.rs", 3, "    let x = 2;", "x is stale");
    h.host.set_reply(move |_, p| {
        if p.contains("hold round") {
            Ok("DONE".into())
        } else {
            Ok(format!("{hit}\nDONE"))
        }
    });
    h.start("claude#1", &["claude#1", "claude#2"], "20-39");
    h.claim(3, 1, "claude#1");
    h.fx.write("src/lib.rs", "fn a() {}\nfn b() {\n    let x = 2;\n}\n");
    h.checkpoint(3, 1, 1, "edit");
    h.pump();
    h.complete(3, 1, "claude#1", "ok");
    h.pump();
    let ruled = h.rows(tev::COUNCIL_RULED);
    assert_eq!(ruled[0]["verdict"], "no_verdict");
    assert_eq!(ruled[0]["reason"], "timeout");
    let l = h.folded(3, 1);
    assert_eq!(l.final_pass, FinalPass::TimedOut, "{l:?}");
    assert!(l.team_pause, "{l:?}");
    let f = &l.findings[0];
    assert_eq!(f.monitor_reply.as_ref().unwrap().kind, ReplyKind::Hold);
    assert_eq!(
        f.dispute.as_ref().unwrap().reason,
        Some(super::super::NoVerdictReason::Timeout)
    );
}

/// §4.1 / §8.11: a finding whose `finding.raised` the bus refused is spooled, and the final pass
/// drains its own lane before it reads the stream: the hold round sees it and the fold holds it.
#[test]
fn t6_a_spooled_s_fact_is_drained_before_the_final_pass_reads() {
    let mut h = Harness::new("t6-spool");
    let hit = finding_line("medium", "src/lib.rs", 3, "    let x = 2;", "x is stale");
    h.host.set_reply(move |_, _| Ok(format!("{hit}\nDONE")));
    h.start("claude#1", &["claude#1", "claude#2"], "20-39");
    h.claim(3, 1, "claude#1");
    h.fx.write("src/lib.rs", "fn a() {}\nfn b() {\n    let x = 2;\n}\n");
    h.rig.refuse(&[tev::FINDING_RAISED]);
    h.checkpoint(3, 1, 1, "edit");
    h.pump();
    assert!(h.rows(tev::FINDING_RAISED).is_empty(), "refused: spooled");
    h.rig.allow();
    h.complete(3, 1, "claude#1", "ok");
    h.pump();
    let l = h.folded(3, 1);
    assert_eq!(l.final_pass, FinalPass::Completed, "{l:?}");
    assert_eq!(l.findings.len(), 1);
    assert_eq!(
        h.rows(tev::FINDING_SETTLED).len(),
        1,
        "the hold round saw it"
    );
}

/// §4.1 / §8.11: an S fact still refused when the attempt folds leaves the record incomplete: the
/// fold (queued behind it) is `stream_gap`, and the worker's timeout synthesis — which sees the
/// run's spooled line — is `stream_gap` too. Neither is a ledger missing a finding.
#[test]
fn t6_a_fold_with_an_s_fact_still_spooled_is_stream_gap() {
    let mut h = Harness::with(
        "t6-unpub",
        |rig| {
            let mut c = sup_cfg(rig);
            c.final_pass_budget = Duration::from_secs(15);
            c
        },
        FakeCouncil::yes(),
    );
    let hit = finding_line("medium", "src/lib.rs", 3, "    let x = 2;", "x is stale");
    h.host.set_reply(move |_, _| Ok(format!("{hit}\nDONE")));
    h.start("claude#1", &["claude#1", "claude#2"], "20-39");
    h.claim(3, 1, "claude#1");
    h.fx.write("src/lib.rs", "fn a() {}\nfn b() {\n    let x = 2;\n}\n");
    h.rig.refuse(&[tev::FINDING_RAISED]);
    h.checkpoint(3, 1, 1, "edit");
    h.pump();
    h.complete(3, 1, "claude#1", "ok");
    h.pump();
    assert!(
        h.rows(tev::LEDGER_FOLDED).is_empty(),
        "queued behind the refused raise"
    );
    let folds: Vec<Value> = h
        .rig
        .outbox_lines()
        .into_iter()
        .filter(|l| l.get("type").and_then(Value::as_str) == Some(tev::LEDGER_FOLDED))
        .collect();
    assert_eq!(folds.len(), 1, "{folds:#?}");
    assert_eq!(folds[0]["payload"]["final_pass"], "stream_gap");
    assert!(h.rig.team_bus().run_has_pending(RUN));
    // Past its deadline the cursor loop tombstones it: the bus returning never publishes it.
    let fold_key = folds[0]["idempotency_key"].as_str().unwrap().to_string();
    let deadline = Instant::now() + Duration::from_secs(40);
    while h.rig.team_bus().is_pending(&fold_key) && Instant::now() < deadline {
        h.core.retry_spooled();
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        !h.rig.team_bus().is_pending(&fold_key),
        "tombstoned at its deadline"
    );
    h.rig.allow();
    h.rig.team_bus().drain_all();
    assert!(
        h.rows(tev::LEDGER_FOLDED).is_empty(),
        "never published late"
    );
    assert!(
        !h.rows(tev::FINDING_RAISED).is_empty(),
        "the raise itself still lands"
    );
}

/// §8.11: a fold past its deadline is not published, and its spooled line (if any) is
/// tombstoned.
#[test]
fn t6_a_fold_past_its_deadline_is_tombstoned_never_published() {
    let mut h = Harness::with(
        "t6-late",
        |rig| {
            let mut c = sup_cfg(rig);
            c.final_pass_budget = Duration::from_millis(300);
            c
        },
        FakeCouncil::yes(),
    );
    h.host.set_reply(|_, _| {
        std::thread::sleep(Duration::from_millis(600));
        Ok("DONE".into())
    });
    h.start("claude#1", &["claude#1", "claude#2"], "20-39");
    h.claim(3, 1, "claude#1");
    h.fx.write("src/lib.rs", "fn a() {}\nfn b() {\n    let x = 2;\n}\n");
    h.rig.refuse(&[tev::LEDGER_FOLDED]);
    h.complete(3, 1, "claude#1", "ok");
    h.pump();
    h.rig.allow();
    h.rig.team_bus().drain_all();
    assert!(
        h.rows(tev::LEDGER_FOLDED).is_empty(),
        "never published late"
    );
}

// ── (e) HELP ─────────────────────────────────────────────────────────────────────────────────────

/// T6 (e): a `HELP:` line in the PA's output yields one `help.requested` (R, before its
/// `step.completed`); a member answers it (`help.answered`, S); and the next step boundary renders
/// the answer to the PA — once: a later step of the same seat does not render it again.
#[test]
fn t6_e_a_help_line_is_asked_answered_and_rendered_at_the_next_boundary() {
    let mut h = Harness::new("t6e");
    h.host.set_reply(|_, p| {
        if p.contains("asks the team for help") {
            Ok("ANSWER: use the retry helper\nEVIDENCE: src/lib.rs:2\nDONE".into())
        } else {
            Ok("DONE".into())
        }
    });
    let floor = h.start("claude#1", &["claude#1", "claude#2"], "0-19");
    let runner = super::super::runner::TeamRunner::from_config(
        &TeamConfig::new(Some(h.rig.bus.clone()), Some(h.rig.outbox.clone()))
            .with_schedule(vec![Duration::from_millis(20); 3])
            .with_attempt_wait(Duration::from_millis(30))
            .with_final_pass_budget(Duration::from_millis(50))
            .with_gate_poll(Duration::from_millis(10)),
    )
    .unwrap();
    let claimed_id = h.claim(3, 1, "claude#1");
    let claimed = super::super::runner::Claimed {
        runner: runner.clone(),
        run_id: RUN.into(),
        ord: 3,
        attempt: 1,
        by: "claude#1".into(),
        step_id: "build".into(),
        stream_floor: floor,
        claimed_id,
        reviewing: None,
        criterion: "the handler cancels stale fetches".into(),
    };
    let out = crate::workflow::StepOutput {
        run_id: RUN.into(),
        unit_ix: 3,
        attempt: 1,
        output: "work done\nHELP: which helper retries a fetch?\n".into(),
        status: crate::workflow::StepStatus::Ok,
        usage: None,
        files: vec![],
        tools: vec![],
        governed: false,
    };
    let _ = super::super::runner::complete(&claimed, &out);
    let asked = h.rows(tev::HELP_REQUESTED);
    assert_eq!(asked.len(), 1, "{asked:#?}");
    assert_eq!(asked[0]["question"], "which helper retries a fetch?");
    h.pump();
    let answered = h.rows(tev::HELP_ANSWERED);
    assert_eq!(answered.len(), 1);
    assert_eq!(answered[0]["help_id"], asked[0]["help_id"]);
    assert_eq!(answered[0]["answer"], "use the retry helper");
    assert_eq!(answered[0]["by"], "claude#2");
    // The PA's next step renders it.
    let next_id = h.claim_step(4, 0, "claude#1", "test", false);
    let next = super::super::runner::Claimed {
        ord: 4,
        attempt: 0,
        step_id: "test".into(),
        claimed_id: next_id,
        ..claimed.clone()
    };
    let b = super::super::runner::boundary(&next);
    let text = b.block.expect("a block").output;
    assert!(text.contains("use the retry helper"), "{text}");
    assert!(text.contains("which helper retries a fetch?"), "{text}");
    // A later step of the same seat was already shown it.
    let later_id = h.claim_step(5, 0, "claude#1", "review", false);
    let later = super::super::runner::Claimed {
        ord: 5,
        claimed_id: later_id,
        step_id: "review".into(),
        ..next
    };
    assert!(super::super::runner::boundary(&later)
        .block
        .is_none_or(|b| !b.output.contains("use the retry helper")));
}

// ── The supervisor thread end to end: replay, then tail ──────────────────────────────────────────

/// §4.7 end to end through the THREAD: a supervisor spawned over a bus that already holds a live
/// run's history arms it from the engine's live set, replays it, and then folds a live attempt
/// whose claim arrives after spawn — no gate timeout.
#[test]
fn t6_the_thread_replays_a_live_run_then_folds_a_live_attempt() {
    let rig = rig("t6-thread");
    let fx = Fixture::new("t6-thread", "src/lib.rs", LIB);
    let publish = |ev: &TeamEvent| match rig.team_bus().publish(ev).unwrap() {
        PublishOutcome::Published(id) => id,
        o => panic!("{o:?}"),
    };
    let floor = publish(&fixture_with(tev::PATH_STARTED, 0, RUN, |p| {
        p["cli"] = json!("claude#1");
        p["roster"] = json!(["claude#1", "claude#2"]);
    }));
    let host = Arc::new(FakeHost::new(|_, _| Ok("DONE".into())));
    let floor_c = floor;
    let live: LiveRuns = Arc::new(move || {
        Ok(vec![LiveTeamRun {
            run_id: RUN.into(),
            status: crate::domain::SessionStatus::Executing,
            team: crate::domain::RunTeamState {
                transport: Some(Transport::Bus),
                stream_floor: Some(floor_c),
                ..Default::default()
            },
            roster: vec!["claude#1".into(), "claude#2".into()],
        }])
    });
    let mut cfg = sup_cfg(&rig);
    cfg.boot_ms = crate::interaction::now_millis();
    let handle = spawn(cfg, host.clone(), Arc::new(FakeCouncil::yes()), live);
    let (wd, gd, base) = (
        fx.dir.to_string_lossy().into_owned(),
        fx.repo.git_dir.to_string_lossy().into_owned(),
        fx.baseline.clone(),
    );
    publish(&fixture_with(tev::STEP_CLAIMED, 0, RUN, |p| {
        p["ord"] = json!(0);
        p["attempt"] = json!(0);
        p["by"] = json!("claude#1");
        p["at"] = json!(crate::interaction::now_millis());
        p["baseline_tree"] = json!(base);
        p["repo"] = json!({"workdir": wd, "git_dir": gd});
    }));
    publish(&fixture_with(tev::STEP_COMPLETED, 0, RUN, |p| {
        p["ord"] = json!(0);
        p["attempt"] = json!(0);
        p["by"] = json!("claude#1");
        p["at"] = json!(crate::interaction::now_millis());
    }));
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let folded = BusDb::shared(&rig.bus)
            .unwrap()
            .poll(tev::LEDGER_FOLDED, 0, 100)
            .unwrap()
            .into_iter()
            .any(|e| e.payload["run_id"] == RUN && e.payload["ord"] == 0);
        if folded {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the live attempt was never folded"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(handle);
}

// ── Absence branches (the PR's absence table) ────────────────────────────────────────────────────

/// A claim from before this boot is dead however it is read — here LIVE (as when the spawn's tail
/// snapshot could not be taken): it is not monitored, and its unaccepted finding is carried into
/// the unit's next attempt, even though the dead attempt folded (its fold may never have reached
/// its gate).
#[test]
fn liveness_is_the_claims_time_against_boot_never_the_read_mode() {
    let mut h = Harness::with(
        "t6-live",
        |rig| {
            let mut c = sup_cfg(rig);
            c.boot_ms = crate::interaction::now_millis() + 60_000;
            c
        },
        FakeCouncil::yes(),
    );
    h.start("claude#1", &["claude#1", "claude#2"], "20-39");
    h.claim(3, 1, "claude#1");
    h.publish(&fixture_with(tev::FINDING_RAISED, 0, RUN, |p| {
        p["ord"] = json!(3);
        p["attempt"] = json!(1);
        p["raise_seq"] = json!(1);
    }));
    h.publish(&fixture_with(tev::LEDGER_FOLDED, 0, RUN, |p| {
        p["ord"] = json!(3);
        p["attempt"] = json!(1);
    }));
    h.fx.write("src/lib.rs", "fn a() {}\nfn b() {\n    let x = 2;\n}\n");
    h.checkpoint(3, 1, 1, "edit");
    h.pump();
    assert!(h.core.units.is_empty(), "a dead attempt is not watched");
    assert_eq!(h.host.turn_count(), 0);
    // The next attempt, claimed by this process.
    h.core.cfg.boot_ms = 0;
    h.claim(3, 2, "claude#1");
    h.pump();
    let carried: Vec<Value> = h
        .rows(tev::FINDING_RAISED)
        .into_iter()
        .filter(|r| r["attempt"] == 2)
        .collect();
    assert_eq!(carried.len(), 1, "{carried:#?}");
    assert_eq!(carried[0]["carried_from_attempt"], 1);
}

/// A completion of this process's attempt whose claim the supervisor never read still folds —
/// `stream_gap`, which pauses — instead of leaving its runner to the timeout.
#[test]
fn a_completion_without_a_seen_claim_folds_stream_gap() {
    let mut h = Harness::new("t6-unseen");
    h.start("claude#1", &["claude#1", "claude#2"], "20-39");
    h.complete(3, 1, "claude#1", "ok");
    h.pump();
    let l = h.folded(3, 1);
    assert_eq!(l.final_pass, FinalPass::StreamGap, "{l:?}");
    assert!(l.team_pause);
}
