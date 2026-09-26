//! DES-TEAMING-002 T5 — the worker-thread seam over a real bus db: `step.claimed` before the turn,
//! the step-boundary injector and its `outcome:"injected"` dedup on any channel, the bounded gate
//! wait and its fail-closed synthesis, the snapshot merge, and the judge's view of the ledger
//! (DES-001 acceptance #14). Every test uses its own temp bus and temp outbox (P1's `rig`), and
//! every bound is injected (`TeamConfig::with_*`): no test sleeps a production budget.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use serde_json::json;

use super::*;
use crate::cli_runner::{run_unit_and_judge_with_team, MONITOR_EXCLUSION_DENY};
use crate::team::events::{GateOpenedKind, LedgerFolded, Owner};
use crate::team::publish::tests::{fixture_with, rig, Rig};
use crate::team::{FindingStatus, LedgerDelivery, LedgerFinding, Severity};
use crate::workflow::{DeltaSink, StepRunner};

// ── fixtures ─────────────────────────────────────────────────────────────────────────────────────

fn cfg(rig: &Rig) -> TeamConfig {
    TeamConfig::new(Some(rig.bus.clone()), Some(rig.outbox.clone()))
        .with_schedule(vec![Duration::from_millis(20); 3])
        .with_attempt_wait(Duration::from_millis(30))
        .with_final_pass_budget(Duration::from_millis(400))
        .with_gate_poll(Duration::from_millis(20))
}

fn runner(rig: &Rig) -> TeamRunner {
    TeamRunner::from_config(&cfg(rig)).expect("bus and outbox")
}

/// Publish `path.started` for `run` and return its event id (the stream floor).
fn start(rig: &Rig, run: &str) -> i64 {
    let ev = crate::team::publish::tests::fixture(tev::PATH_STARTED, 0, run);
    match rig.team_bus().publish(&ev).unwrap() {
        PublishOutcome::Published(id) => id,
        o => panic!("path.started not published: {o:?}"),
    }
}

fn bus_stamp(floor: i64) -> UnitTeamSnapshot {
    let mut s = UnitTeamSnapshot::stamped(Transport::Bus, None, None);
    s.stream_floor = Some(floor);
    s
}

fn input(run: &str, ord: u32, attempt: u32, stamp: Option<UnitTeamSnapshot>) -> StepInput {
    let mut unit =
        crate::domain::WorkUnit::pending(format!("{run}:u{ord}"), run, ord, format!("step {ord}"));
    unit.assigned_cli = Some("claude#1".into());
    unit.team_run = stamp.is_some();
    unit.team = stamp;
    StepInput {
        run_id: run.into(),
        unit_ix: ord as usize,
        attempt,
        unit,
        workflow_id: format!("{run}:plan-1"),
        entity_mode: crate::scope::EntityMode::Isolated,
        workdir: None,
        governance: None,
        prior_outputs: vec![],
        elicitation_epoch: 0,
        process_gen: None,
        launch_seq: 0,
        required_skills: Vec::new(),
    }
}

/// A `finding.raised` row (S) for `(ord, attempt, raise_seq)` authored by `by`.
fn finding(
    run: &str,
    ord: u32,
    attempt: u32,
    raise_seq: u32,
    severity: &str,
    path: &str,
    by: &str,
) -> TeamEvent {
    fixture_with(tev::FINDING_RAISED, 0, run, |p| {
        p["ord"] = json!(ord);
        p["attempt"] = json!(attempt);
        p["raise_seq"] = json!(raise_seq);
        p["severity"] = json!(severity);
        p["path"] = json!(path);
        p["by"] = json!(by);
        p["evidence"] = json!(format!("evidence of {path}"));
    })
}

fn finding_id_of(ev: &TeamEvent) -> String {
    match &ev.body {
        TeamBody::FindingRaised(b) => b.finding_id.clone(),
        _ => panic!("not a finding"),
    }
}

fn delivered(f: &TeamEvent, channel: &str, outcome: &str, delivery_id: &str) -> TeamEvent {
    let TeamBody::FindingRaised(b) = &f.body else {
        panic!("not a finding")
    };
    let (run, ord, attempt) = (f.env.run_id.clone(), f.env.ord, f.env.attempt);
    let (seq, id) = (b.raise_seq, b.finding_id.clone());
    fixture_with(tev::ADVICE_DELIVERED, 0, &run, |p| {
        p["ord"] = json!(ord);
        p["attempt"] = json!(attempt);
        p["raise_seq"] = json!(seq);
        p["finding_id"] = json!(id);
        p["channel"] = json!(channel);
        p["outcome"] = json!(outcome);
        p["delivery_id"] = json!(delivery_id);
        p["steer_id"] = if channel == "acp_steering" {
            json!(delivery_id)
        } else {
            Value::Null
        };
    })
}

fn publish(rig: &Rig, ev: &TeamEvent) -> i64 {
    match rig.team_bus().publish(ev).unwrap() {
        PublishOutcome::Published(id) => id,
        o => panic!("{} not published: {o:?}", ev.event_type()),
    }
}

/// `(event_id, payload)` of every row of `run` of `event_type`.
fn rows_of(rig: &Rig, run: &str, event_type: &str) -> Vec<(i64, Value)> {
    let db = BusDb::shared(&rig.bus).unwrap();
    db.poll(event_type, 0, 10_000)
        .unwrap()
        .into_iter()
        .filter(|e| e.payload["run_id"] == run)
        .map(|e| (e.event_id, e.payload))
        .collect()
}

fn key_of(rig: &Rig, event_id: i64) -> String {
    rig.conn()
        .query_row(
            "SELECT idempotency_key FROM events WHERE event_id = ?1",
            [event_id],
            |r| r.get(0),
        )
        .unwrap()
}

fn claimed(a: Attempt) -> Box<Claimed> {
    match a {
        Attempt::Claimed(c) => c,
        other => panic!("not claimed: {other:?}"),
    }
}

fn ok_output(i: &StepInput) -> StepOutput {
    StepOutput {
        run_id: i.run_id.clone(),
        unit_ix: i.unit_ix,
        attempt: i.attempt,
        output: "done".into(),
        status: StepStatus::Ok,
        usage: None,
        files: Vec::new(),
        tools: Vec::new(),
        governed: false,
    }
}

/// A ledger whose one finding was raised by `claude#2` and corroborated by `claude#3`.
fn authored_ledger() -> TeamLedger {
    let f = finding("x", 1, 0, 1, "high", "src/a.rs", "claude#2");
    let TeamBody::FindingRaised(b) = f.body else {
        unreachable!()
    };
    TeamLedger::new(
        FinalPass::Completed,
        Vec::new(),
        vec![LedgerFinding {
            finding: Finding {
                finding_id: b.finding_id,
                monitor_id: "m1".into(),
                seat: "claude#2".into(),
                severity: Severity::High,
                path: b.path,
                line: b.line,
                evidence: b.evidence,
                claim: "the fetch is never cancelled".into(),
                suggestion: None,
                tree: b.tree,
                in_diff: true,
                checkpoint_seq: 0,
                anchor: String::new(),
                carried_from_attempt: None,
            },
            final_line: None,
            corroborated_by: vec!["claude#3".into()],
            delivery: LedgerDelivery::Injected,
            status: FindingStatus::Accepted,
            worker_reason: Some("added AbortController".into()),
            monitor_reply: None,
            dispute: None,
        }],
        Default::default(),
    )
}

/// A stand-in supervisor (S): on every `step.completed` of `run` it publishes `ledger.folded`
/// for that attempt, carrying `ledger`. Stops when dropped.
struct Supervisor {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn supervise(rig: &Rig, run: &str, ledger: TeamLedger) -> Supervisor {
    let stop = Arc::new(AtomicBool::new(false));
    let (bus, outbox, run, stop2) = (
        rig.bus.clone(),
        rig.outbox.clone(),
        run.to_string(),
        stop.clone(),
    );
    let handle = std::thread::spawn(move || {
        let tb = TeamBus::new(bus.clone(), outbox, Duration::from_millis(30));
        let db = BusDb::shared(&bus).unwrap();
        let mut floor = 0;
        while !stop2.load(Ordering::SeqCst) {
            for ev in db.poll(tev::STEP_COMPLETED, floor, 50).unwrap_or_default() {
                floor = floor.max(ev.event_id);
                if ev.payload["run_id"] != run.as_str() {
                    continue;
                }
                let ord = ev.payload["ord"].as_u64().unwrap() as u32;
                let attempt = ev.payload["attempt"].as_u64().unwrap() as u32;
                let folded = TeamEvent {
                    env: Envelope {
                        run_id: run.clone(),
                        ord: Some(ord),
                        attempt: Some(attempt),
                        by: "engine".into(),
                        at: 0,
                        re: None,
                    },
                    body: TeamBody::LedgerFolded(LedgerFolded {
                        final_pass: ledger.final_pass,
                        ledger: ledger.clone(),
                        transport: Transport::Bus,
                        transcript: Transcript {
                            from_event_id: ev.event_id,
                            to_event_id: ev.event_id,
                            count: 1,
                            truncated: false,
                            events: vec![TranscriptRow {
                                event_id: ev.event_id,
                                event_type: tev::STEP_COMPLETED.into(),
                                payload: ev.payload.clone(),
                            }],
                        },
                    }),
                };
                let _ = tb.publish(&folded);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    });
    Supervisor {
        stop,
        handle: Some(handle),
    }
}

/// A worker seat for the judge tests: records every input; `during` runs inside the work turn
/// (never the judge's), e.g. to publish what the carrier would.
struct Seat {
    seen: Mutex<Vec<StepInput>>,
    during: Box<dyn Fn(&StepInput) + Send + Sync>,
}

impl Seat {
    fn new(during: impl Fn(&StepInput) + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            seen: Mutex::new(Vec::new()),
            during: Box::new(during),
        })
    }

    fn judges(&self) -> Vec<StepInput> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|i| is_judge(i))
            .cloned()
            .collect()
    }

    fn work_turns(&self) -> Vec<StepInput> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|i| !is_judge(i))
            .cloned()
            .collect()
    }
}

fn is_judge(i: &StepInput) -> bool {
    i.unit.description.contains("CRITERION:")
}

impl StepRunner for Seat {
    fn run_unit(&self, i: &StepInput) -> StepOutput {
        self.seen.lock().unwrap().push(i.clone());
        if is_judge(i) {
            return StepOutput {
                output: "PASS\njudged\nPASS".into(),
                ..ok_output(i)
            };
        }
        (self.during)(i);
        ok_output(i)
    }
}

fn seat(key: &str) -> crate::AgenticCli {
    crate::AgenticCli {
        key: key.into(),
        display_name: key.into(),
        binary: "unused".into(),
        headless_invocation: format!("{} -p {{PROMPT}}", seat_key(key)),
        category: Default::default(),
        input_mode: Default::default(),
        version_probe: vec![],
        trust_flags: vec![],
        alt_binaries: vec![],
        confidence: Default::default(),
        enabled_for_council: true,
        acp: None,
        capabilities: None,
        login_invocation: None,
        health: None,
    }
}

/// Distinct invocation identities for instance seats (`claude#2` runs `claude2`), so the roster
/// can hold two instances and a distinct seat without the identity rule folding them together.
fn instance(key: &str, bin: &str) -> crate::AgenticCli {
    crate::AgenticCli {
        headless_invocation: format!("{bin} -p {{PROMPT}}"),
        ..seat(key)
    }
}

fn workdir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("wicked-core-t5-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn judged_input(run: &str, floor: i64, wd: &std::path::Path) -> StepInput {
    let mut i = input(run, 1, 0, Some(bus_stamp(floor)));
    i.unit.assigned_cli = Some("claude".into());
    i.unit.validator = Some(crate::validator::DeterministicValidator {
        criterion: "the work is correct".into(),
        script: "true".into(),
        approved: true,
    });
    i.workdir = Some(wd.to_path_buf());
    i
}

const NOOP: &DeltaSink = &|_: &str| {};

// ── T5 (a) — step.claimed precedes every checkpoint of the attempt ───────────────────────────────

/// T5 (a), in-process path: `step.claimed` is published before the turn starts, so its
/// `event_id` is lower than every `checkpoint.reached` the carrier publishes during the turn.
#[test]
fn t5_a_step_claimed_precedes_every_checkpoint_in_process() {
    let rig = rig("t5a");
    let run = "t5a";
    let floor = start(&rig, run);
    let _s = supervise(&rig, run, empty_ledger(FinalPass::Completed));
    let bus = rig.team_bus();
    let worker = Seat::new(move |i| {
        for seq in 1..=3u64 {
            let cp = fixture_with(tev::CHECKPOINT_REACHED, 0, &i.run_id, |p| {
                p["ord"] = json!(i.unit.ord);
                p["attempt"] = json!(i.attempt);
                p["seq"] = json!(seq);
            });
            bus.publish(&cp).unwrap();
        }
    });
    let r: Arc<dyn StepRunner> = worker.clone();
    let tr = runner(&rig);
    let (out, _, evidence) = run_unit_and_judge_with_team(
        &r,
        &input(run, 1, 0, Some(bus_stamp(floor))),
        NOOP,
        &[],
        None,
        Some(&tr),
    );
    assert_eq!(out.status, StepStatus::Ok);
    let claimed = rows_of(&rig, run, tev::STEP_CLAIMED);
    let checkpoints = rows_of(&rig, run, tev::CHECKPOINT_REACHED);
    assert_eq!(claimed.len(), 1, "one step.claimed");
    assert_eq!(checkpoints.len(), 3);
    assert!(
        checkpoints.iter().all(|(id, _)| *id > claimed[0].0),
        "step.claimed {} precedes every checkpoint {checkpoints:?}",
        claimed[0].0
    );
    let completed = rows_of(&rig, run, tev::STEP_COMPLETED);
    assert_eq!(completed.len(), 1, "one step.completed");
    assert!(completed[0].0 > checkpoints.last().unwrap().0);
    let team = evidence.team.expect("a teamed unit's snapshot");
    assert_eq!(team.claimed_event_id, Some(claimed[0].0));
    assert_eq!(team.ledger_source, Some(LedgerSource::Folded));
}

// ── T5 (b) — the step-boundary injector ──────────────────────────────────────────────────────────

/// T5 (b): a unit with undelivered HIGHs on the stream receives ONE `[team advice]` prior-context
/// block on its next step and one `advice.delivered{channel:"boundary", outcome:"injected"}` row
/// PER rendered finding (three findings → three rows, three distinct keys); the next step does
/// not render them again, answered or not.
#[test]
fn t5_b_boundary_renders_each_undelivered_finding_once_with_one_row_each() {
    let rig = rig("t5b");
    let run = "t5b";
    let floor = start(&rig, run);
    let fs: Vec<TeamEvent> = (1..=3)
        .map(|n| finding(run, 1, 0, n, "high", &format!("src/f{n}.rs"), "claude#2"))
        .collect();
    for f in &fs {
        publish(&rig, f);
    }
    let _s = supervise(&rig, run, empty_ledger(FinalPass::Completed));
    let worker = Seat::new(|_| {});
    let r: Arc<dyn StepRunner> = worker.clone();
    let tr = runner(&rig);
    let (out, _, _) = run_unit_and_judge_with_team(
        &r,
        &input(run, 2, 0, Some(bus_stamp(floor))),
        NOOP,
        &[],
        None,
        Some(&tr),
    );
    assert_eq!(out.status, StepStatus::Ok);
    let turn = &worker.work_turns()[0];
    let blocks: Vec<_> = turn
        .prior_outputs
        .iter()
        .filter(|p| p.label == ADVICE_LABEL)
        .collect();
    assert_eq!(blocks.len(), 1, "one [team advice] block");
    for f in &fs {
        assert!(
            blocks[0].output.contains(&finding_id_of(f)),
            "the block renders {}",
            finding_id_of(f)
        );
    }
    let rows = rows_of(&rig, run, tev::ADVICE_DELIVERED);
    assert_eq!(rows.len(), 3, "one advice.delivered per rendered finding");
    let keys: BTreeSet<String> = rows.iter().map(|(id, _)| key_of(&rig, *id)).collect();
    assert_eq!(keys.len(), 3, "three distinct keys");
    for (_, p) in &rows {
        assert_eq!(p["channel"], "boundary");
        assert_eq!(p["outcome"], "injected");
        assert_eq!(p["delivery_id"], "boundary:u2:0");
        assert_eq!(p["ord"], 1, "the row is about the finding's own attempt");
    }
    // The next step: an `injected` row exists for each, so nothing is rendered again.
    let (_, _, _) = run_unit_and_judge_with_team(
        &r,
        &input(run, 3, 0, Some(bus_stamp(floor))),
        NOOP,
        &[],
        None,
        Some(&tr),
    );
    let second = &worker.work_turns()[1];
    assert!(
        second.prior_outputs.iter().all(|p| p.label != ADVICE_LABEL),
        "an injected finding is never rendered again: {:?}",
        second.prior_outputs
    );
    assert_eq!(rows_of(&rig, run, tev::ADVICE_DELIVERED).len(), 3);
}

/// T5 (b): the dedup is `outcome:"injected"` on ANY channel. A finding already injected over
/// `acp_steering` is not rendered; one whose only rows are `turn_ended`, `refused` or
/// `not_delivered` IS rendered at the next boundary.
#[test]
fn t5_b_injected_on_any_channel_dedups_and_other_outcomes_do_not() {
    let rig = rig("t5b2");
    let run = "t5b2";
    let floor = start(&rig, run);
    let steered = finding(run, 1, 0, 1, "high", "src/steered.rs", "claude#2");
    let ended = finding(run, 1, 0, 2, "high", "src/ended.rs", "claude#2");
    let refused = finding(run, 1, 0, 3, "high", "src/refused.rs", "claude#2");
    let missed = finding(run, 1, 0, 4, "medium", "src/missed.rs", "claude#2");
    for f in [&steered, &ended, &refused, &missed] {
        publish(&rig, f);
    }
    publish(
        &rig,
        &delivered(&steered, "acp_steering", "injected", "s-1"),
    );
    publish(
        &rig,
        &delivered(&ended, "acp_steering", "turn_ended", "s-2"),
    );
    publish(&rig, &delivered(&refused, "acp_steering", "refused", "s-3"));
    publish(&rig, &delivered(&missed, "none", "not_delivered", "end:0"));
    let tr = runner(&rig);
    let c = claimed(claim(Some(&tr), &input(run, 2, 0, Some(bus_stamp(floor)))).unwrap());
    let b = boundary(&c);
    let text = b.block.expect("a block").output;
    assert!(!text.contains(&finding_id_of(&steered)), "{text}");
    for f in [&ended, &refused, &missed] {
        assert!(text.contains(&finding_id_of(f)), "{text}");
    }
    let rendered: Vec<u32> = b.rendered.iter().map(|(_, s)| *s).collect();
    assert_eq!(rendered, vec![2, 3, 4]);
}

// ── T5 (c) — the gate wait's fail-closed timeout ─────────────────────────────────────────────────

/// T5 (c), worker half: with no `ledger.folded` within the (shortened) budget, the worker
/// synthesizes the `final_pass:"timed_out"` snapshot from the attempt's own rows WITHOUT
/// publishing it; the unanswered HIGH makes it pause.
#[test]
fn t5_c_no_fold_within_the_budget_synthesizes_timed_out_and_publishes_nothing() {
    let rig = rig("t5c");
    let run = "t5c";
    let floor = start(&rig, run);
    let tr = runner(&rig);
    let i = input(run, 1, 0, Some(bus_stamp(floor)));
    let c = claimed(claim(Some(&tr), &i).unwrap());
    // A HIGH raised on THIS attempt during its turn, never answered.
    publish(&rig, &finding(run, 1, 0, 1, "high", "src/a.rs", "claude#2"));
    let started = Instant::now();
    let snap = complete(&c, &ok_output(&i));
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(snap.transport, Transport::Bus);
    assert_eq!(snap.ledger_source, Some(LedgerSource::Synthesized));
    assert_eq!(snap.ledger_ref, None);
    let ledger = snap.ledger.expect("the synthesized ledger");
    assert_eq!(ledger.final_pass, FinalPass::TimedOut);
    assert_eq!(ledger.findings.len(), 1);
    assert!(ledger.team_pause, "an unresolved HIGH on timeout pauses");
    assert!(
        rows_of(&rig, run, tev::LEDGER_FOLDED).is_empty(),
        "ledger.folded has one owner (S): the worker never publishes it"
    );
    assert_eq!(rows_of(&rig, run, tev::STEP_COMPLETED).len(), 1);
    let t = snap.transcript.expect("the attempt's transcript");
    assert!(t.events.iter().any(|r| r.event_type == tev::FINDING_RAISED));
}

/// Fail closed: a `step.completed` the bus refused is tombstoned at the timeout (it can never
/// land after the gate decided), and with no terminal row the synthesized ledger is `stream_gap`,
/// which pauses — never an unpaused `timed_out` over an incomplete record.
#[test]
fn a_spooled_step_completed_is_superseded_at_the_timeout_and_the_ledger_is_stream_gap() {
    let rig = rig("t5c2");
    let run = "t5c2";
    let floor = start(&rig, run);
    let tr = runner(&rig);
    let i = input(run, 1, 0, Some(bus_stamp(floor)));
    let c = claimed(claim(Some(&tr), &i).unwrap());
    rig.refuse(&[tev::STEP_COMPLETED]);
    let snap = complete(&c, &ok_output(&i));
    let ledger = snap.ledger.unwrap();
    assert_eq!(ledger.final_pass, FinalPass::StreamGap);
    assert!(ledger.team_pause);
    rig.allow();
    rig.team_bus().drain_all();
    assert!(
        rows_of(&rig, run, tev::STEP_COMPLETED).is_empty(),
        "the late step.completed is never published"
    );
}

/// Fail closed: a stream the worker cannot read at the timeout is `stream_gap` (pauses).
#[test]
fn an_unreadable_stream_at_the_timeout_is_stream_gap() {
    let rig = rig("t5c3");
    let dir = rig.dir.join("not-a-bus");
    std::fs::create_dir_all(&dir).unwrap();
    let broken = TeamRunner::from_config(
        &TeamConfig::new(
            Some(dir.to_string_lossy().into_owned()),
            Some(rig.outbox.clone()),
        )
        .with_schedule(vec![Duration::from_millis(5)])
        .with_final_pass_budget(Duration::from_millis(100))
        .with_gate_poll(Duration::from_millis(20)),
    )
    .unwrap();
    let c = Claimed {
        runner: broken,
        run_id: "t5c3".into(),
        ord: 1,
        attempt: 0,
        by: "claude#1".into(),
        step_id: "unit-1".into(),
        stream_floor: 1,
        claimed_id: 2,
        reviewing: None,
        criterion: String::new(),
    };
    let i = input("t5c3", 1, 0, Some(bus_stamp(1)));
    let snap = complete(&c, &ok_output(&i));
    let ledger = snap.ledger.unwrap();
    assert_eq!(ledger.final_pass, FinalPass::StreamGap);
    assert!(ledger.team_pause);
}

// ── T5 (d) — the judge sees the ledger and the transcript; DES-001 #14 ───────────────────────────

fn work_fence(prompt: &str) -> &str {
    let start = prompt.find("WORK:\n```").expect("a WORK fence") + "WORK:\n```".len();
    let end = prompt.rfind("```").expect("the fence closes");
    &prompt[start..end]
}

/// T5 (d) + DES-001 #14 (a): inline pinned judge. The ledger authored by `claude#2`
/// (corroborated by `claude#3`) excludes both from judging; the distinct seat judges; the judge
/// prompt carries the rendered ledger AND the transcript inside the WORK fence.
#[test]
fn t5_d_inline_pinned_judge_excludes_ledger_authors_and_reads_ledger_and_transcript() {
    let rig = rig("t5d");
    let run = "t5d";
    let floor = start(&rig, run);
    let _s = supervise(&rig, run, authored_ledger());
    let wd = workdir("t5d");
    let worker = Seat::new(|_| {});
    let r: Arc<dyn StepRunner> = worker.clone();
    let roster = [
        instance("claude#2", "claude2"),
        instance("claude#3", "claude3"),
        seat("codex"),
    ];
    let tr = runner(&rig);
    let (_, verdict, evidence) = run_unit_and_judge_with_team(
        &r,
        &judged_input(run, floor, &wd),
        NOOP,
        &roster,
        None,
        Some(&tr),
    );
    let verdict = verdict.expect("the pinned judge ran");
    assert!(verdict.pass, "{verdict:?}");
    let judges = worker.judges();
    let seats: Vec<_> = judges
        .iter()
        .map(|j| j.unit.assigned_cli.clone().unwrap_or_default())
        .collect();
    assert_eq!(
        seats,
        vec!["codex".to_string()],
        "only the distinct seat judges"
    );
    let fence = work_fence(&judges[0].unit.description);
    assert!(fence.contains("[team ledger"), "{fence}");
    assert!(
        fence.contains("raised by claude#2 (corroborated by claude#3)"),
        "{fence}"
    );
    assert!(fence.contains("[team transcript"), "{fence}");
    assert!(fence.contains(tev::STEP_COMPLETED), "{fence}");
    assert!(
        evidence.team.unwrap().ledger.unwrap().rendered_to_judge,
        "the ledger records that it was rendered to the judge"
    );
    let _ = std::fs::remove_dir_all(&wd);
}

/// DES-001 #14 (b): the inline DEFAULT judge (no pinned validator, the tree changed) excludes the
/// ledger authors too, and reads the ledger in its WORK fence.
#[test]
fn t5_d_inline_default_judge_excludes_ledger_authors() {
    let rig = rig("t5d2");
    let run = "t5d2";
    let floor = start(&rig, run);
    let _s = supervise(&rig, run, authored_ledger());
    let wd = workdir("t5d2");
    let worker = Seat::new(|_| {});
    let r: Arc<dyn StepRunner> = worker.clone();
    let roster = [
        instance("claude#2", "claude2"),
        instance("claude#3", "claude3"),
        seat("codex"),
    ];
    let mut i = judged_input(run, floor, &wd);
    i.unit.validator = None;
    i.unit.default_floor = true;
    let tr = runner(&rig);
    let (_, verdict, _) = run_unit_and_judge_with_team(&r, &i, NOOP, &roster, None, Some(&tr));
    assert!(verdict.is_some(), "the default judge ran");
    let judges = worker.judges();
    let seats: Vec<_> = judges
        .iter()
        .map(|j| j.unit.assigned_cli.clone().unwrap_or_default())
        .collect();
    assert_eq!(seats, vec!["codex".to_string()]);
    assert!(work_fence(&judges[0].unit.description).contains("[team ledger"));
    let _ = std::fs::remove_dir_all(&wd);
}

/// DES-001 #14 (e): excluding the monitors leaves no eligible seat → `judge_skipped` names them.
#[test]
fn t5_d_no_seat_left_after_excluding_the_monitors_names_them() {
    let rig = rig("t5d3");
    let run = "t5d3";
    let floor = start(&rig, run);
    let _s = supervise(&rig, run, authored_ledger());
    let wd = workdir("t5d3");
    let worker = Seat::new(|_| {});
    let r: Arc<dyn StepRunner> = worker.clone();
    let roster = [
        instance("claude#2", "claude2"),
        instance("claude#3", "claude3"),
    ];
    let tr = runner(&rig);
    let (_, verdict, evidence) = run_unit_and_judge_with_team(
        &r,
        &judged_input(run, floor, &wd),
        NOOP,
        &roster,
        None,
        Some(&tr),
    );
    assert!(verdict.is_none());
    assert!(
        worker.judges().is_empty(),
        "no monitor grades its own finding"
    );
    let why = evidence.judge_skipped.expect("the skip is disclosed");
    assert!(
        why.contains("claude#2") && why.contains("claude#3"),
        "{why}"
    );
    let _ = std::fs::remove_dir_all(&wd);
}

/// A stand-in gate-eval daemon: answers every request with `judge_cli`, recording each request.
fn eval_daemon(
    bus: &str,
    judge_cli: Value,
) -> (Arc<Mutex<Vec<Value>>>, Arc<AtomicBool>, JoinHandle<()>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let (seen2, stop2, bus) = (seen.clone(), stop.clone(), bus.to_string());
    let h = std::thread::spawn(move || {
        let db = BusDb::shared(&bus).unwrap();
        let mut floor = 0;
        while !stop2.load(Ordering::SeqCst) {
            for ev in db
                .poll("wicked.gate.eval.requested", floor, 20)
                .unwrap_or_default()
            {
                floor = ev.event_id;
                seen2.lock().unwrap().push(ev.payload.clone());
                let _ = db.emit(&crate::bus::BusEmit::new(
                    "wicked.gate.eval.responded",
                    "test",
                    "test.gate",
                    json!({"eval_id": ev.payload["eval_id"], "pass": true,
                           "reasoning": "stand-in", "judge_cli": judge_cli}),
                ));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    });
    (seen, stop, h)
}

fn bus_judge(rig: &Rig, run: &str, ledger: TeamLedger, judge_cli: Value) -> (Value, Value) {
    let (req, verdict) = bus_judge_by(rig, run, ledger, judge_cli, Some("claude"));
    (req.expect("a request"), verdict)
}

/// [`bus_judge`] for a unit authored by `author` (`None` = no `assigned_cli`); the request is
/// `None` when none reached the daemon.
fn bus_judge_by(
    rig: &Rig,
    run: &str,
    ledger: TeamLedger,
    judge_cli: Value,
    author: Option<&str>,
) -> (Option<Value>, Value) {
    let floor = start(rig, run);
    let _s = supervise(rig, run, ledger);
    let wd = workdir(run);
    let (seen, stop, h) = eval_daemon(&rig.bus, judge_cli);
    let worker = Seat::new(|_| {});
    let r: Arc<dyn StepRunner> = worker.clone();
    let tr = runner(rig);
    let mut i = judged_input(run, floor, &wd);
    i.unit.assigned_cli = author.map(str::to_string);
    let (_, verdict, _) =
        run_unit_and_judge_with_team(&r, &i, NOOP, &[], Some(&rig.bus), Some(&tr));
    stop.store(true, Ordering::SeqCst);
    let _ = h.join();
    let _ = std::fs::remove_dir_all(&wd);
    let req = seen.lock().unwrap().first().cloned();
    let v = verdict.expect("a verdict");
    (req, json!({"pass": v.pass, "reasoning": v.reasoning}))
}

/// DES-001 #14 (c): the bus request carries `excluded_seats` from the ledger; a response whose
/// `judge_cli` is excluded, or missing, is a DENY with the fail-closed reason; a distinct
/// `judge_cli` is honoured. The work in the request carries the rendered ledger.
#[test]
fn t5_d_bus_judge_must_prove_monitor_exclusion() {
    let rig = rig("t5d4");
    for (run, judge, pass) in [
        ("t5d4-a", json!("claude#2"), false),
        ("t5d4-b", json!("claude"), false),
        ("t5d4-c", Value::Null, false),
        ("t5d4-d", json!("codex"), true),
    ] {
        let (req, verdict) = bus_judge(&rig, run, authored_ledger(), judge.clone());
        assert_eq!(
            req["excluded_seats"],
            json!(["claude#2", "claude#3"]),
            "{run}"
        );
        assert!(
            req["work"].as_str().unwrap().contains("[team ledger"),
            "{run}"
        );
        assert_eq!(verdict["pass"], pass, "{run}: judge {judge} → {verdict}");
        if !pass {
            assert_eq!(verdict["reasoning"], MONITOR_EXCLUSION_DENY, "{run}");
        }
    }
}

/// DES-001 #14 (d), tightened (core#625): an empty ledger sends `excluded_seats: []`, but the
/// WORK AUTHOR is always excluded: a response whose `judge_cli` is the author (by instance or cli
/// key), or that names no judge at all, is a DENY; a distinct judge is honoured. An empty
/// `excluded_seats` never bypasses the evaluator≠creator check.
#[test]
fn t5_d_bus_judge_with_an_empty_ledger_still_excludes_the_work_author() {
    let rig = rig("t5d5");
    for (run, judge, pass) in [
        ("t5d5-a", json!("claude"), false),
        ("t5d5-b", json!("claude#4"), false),
        ("t5d5-c", Value::Null, false),
        ("t5d5-d", json!("codex"), true),
    ] {
        let (req, verdict) =
            bus_judge(&rig, run, empty_ledger(FinalPass::Completed), judge.clone());
        assert_eq!(req["excluded_seats"], json!([]), "{run}");
        assert_eq!(req["work_author"], "claude", "{run}");
        assert_eq!(verdict["pass"], pass, "{run}: judge {judge} → {verdict}");
        if !pass {
            assert!(
                verdict["reasoning"]
                    .as_str()
                    .unwrap()
                    .starts_with("gate eval bus-path DENY (fail-closed)"),
                "{run}: {verdict}"
            );
        }
    }
}

/// core#625: a pinned unit with NO work author cannot prove evaluator≠creator on the bus path:
/// it is a DENY, and no request is published for a daemon to judge.
#[test]
fn t5_d_bus_judge_denies_a_unit_with_no_work_author() {
    let rig = rig("t5d6");
    let (req, verdict) = bus_judge_by(
        &rig,
        "t5d6",
        empty_ledger(FinalPass::Completed),
        json!("codex"),
        None,
    );
    assert!(req.is_none(), "no request is published: {req:?}");
    assert_eq!(verdict["pass"], false, "{verdict}");
    assert!(
        verdict["reasoning"]
            .as_str()
            .unwrap()
            .contains("work author"),
        "{verdict}"
    );
}

/// The gate wait's cursor advances past every row it inspected: a unit whose fold sits behind
/// more than one poll batch of OTHER attempts' folds still finds it (review of #624 round 2).
#[test]
fn the_gate_wait_finds_its_fold_behind_more_than_a_batch_of_unrelated_folds() {
    let rig = rig("t5batch");
    let run = "t5batch";
    let floor = start(&rig, run);
    let tr = TeamRunner::from_config(&cfg(&rig).with_final_pass_budget(Duration::from_secs(20)))
        .unwrap();
    let i = input(run, 1, 0, Some(bus_stamp(floor)));
    let c = claimed(claim(Some(&tr), &i).unwrap());
    // S, for this attempt: 60 folds of other runs land after its step.completed, then ours.
    let stop = Arc::new(AtomicBool::new(false));
    let h = {
        let (bus, outbox, stop) = (rig.bus.clone(), rig.outbox.clone(), stop.clone());
        std::thread::spawn(move || {
            let tb = TeamBus::new(bus.clone(), outbox, Duration::from_millis(30));
            let db = BusDb::shared(&bus).unwrap();
            while !stop.load(Ordering::SeqCst) {
                if !db
                    .poll(tev::STEP_COMPLETED, 0, 10)
                    .unwrap_or_default()
                    .is_empty()
                {
                    let fold = |run: &str| {
                        fixture_with(tev::LEDGER_FOLDED, 0, run, |p| {
                            p["ord"] = json!(1);
                            p["attempt"] = json!(0);
                        })
                    };
                    for n in 0..60 {
                        tb.publish(&fold(&format!("other-{n}"))).unwrap();
                    }
                    tb.publish(&fold("t5batch")).unwrap();
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        })
    };
    let snap = complete(&c, &ok_output(&i));
    stop.store(true, Ordering::SeqCst);
    let _ = h.join();
    assert_eq!(
        snap.ledger_source,
        Some(LedgerSource::Folded),
        "S's fold was published in time: {snap:?}"
    );
    assert_eq!(snap.ledger_ref.as_deref(), Some("ledger.folded#1:0"));
}

// ── T5 (e) and the absence axes ──────────────────────────────────────────────────────────────────

/// T5 (e), worker half: an un-teamed team unit (the no-bus stamp) produces no team rows and a
/// local snapshot: `transport: none`, `no_bus`, an empty ledger and an empty transcript.
#[test]
fn t5_e_an_unteamed_stamp_yields_the_local_snapshot_and_no_rows() {
    let stamp = UnitTeamSnapshot::stamped(
        Transport::None,
        Some("no bus".into()),
        Some(LedgerSource::NoBus),
    );
    let worker = Seat::new(|_| {});
    let r: Arc<dyn StepRunner> = worker.clone();
    let (_, _, evidence) =
        run_unit_and_judge_with_team(&r, &input("t5e", 1, 0, Some(stamp)), NOOP, &[], None, None);
    let snap = evidence.team.expect("the local snapshot");
    assert_eq!(snap.transport, Transport::None);
    assert_eq!(snap.ledger_source, Some(LedgerSource::NoBus));
    let ledger = snap.ledger.expect("an empty ledger");
    assert!(ledger.findings.is_empty() && ledger.monitors.is_empty());
    assert!(!ledger.team_pause);
    assert_eq!(snap.transcript.unwrap().count, 0);
}

/// Absence axis "bus present at launch, absent now": the dispatch stamped `bus` but this worker
/// has no team runner. The attempt runs un-teamed and SAYS so; nothing is published.
#[test]
fn a_bus_stamp_with_no_runner_is_an_unteamed_attempt_with_its_reason() {
    let a = claim(None, &input("t5n", 1, 0, Some(bus_stamp(7)))).unwrap();
    let Attempt::Local(s) = a else {
        panic!("not local: {a:?}")
    };
    assert_eq!(s.transport, Transport::None);
    assert!(s.reason.unwrap().contains("absent now"));
}

/// A non-team unit is never touched.
#[test]
fn a_non_team_unit_is_not_teamed() {
    assert!(matches!(
        claim(None, &input("t5x", 1, 0, None)).unwrap(),
        Attempt::NotTeam
    ));
}

/// §4.8 row 5: `step.claimed` fails past the bound → the attempt's runner lane is tombstoned,
/// the attempt is un-teamed with the reason, and nothing of the attempt is ever published —
/// not after the bus returns, not after a replay.
#[test]
fn row5_a_failed_step_claimed_unteams_the_attempt_and_it_publishes_nothing_ever() {
    let rig = rig("t5r5");
    let run = "t5r5";
    let floor = start(&rig, run);
    rig.refuse(&[tev::STEP_CLAIMED]);
    let tr = runner(&rig);
    let a = claim(Some(&tr), &input(run, 1, 0, Some(bus_stamp(floor)))).unwrap();
    let Attempt::Local(s) = a else {
        panic!("not local: {a:?}")
    };
    assert_eq!(s.transport, Transport::None);
    assert!(s.reason.unwrap().starts_with("un-teamed attempt"));
    assert!(rig.outbox_lines().iter().any(|l| l["superseded_run"] == run
        && l["owner"] == "runner"
        && l["ord"] == 1
        && l["attempt"] == 0));
    rig.allow();
    rig.team_bus().drain_all();
    assert!(rows_of(&rig, run, tev::STEP_CLAIMED).is_empty());
    let _ = Owner::Runner;
}

/// Fail closed: `step.claimed` failed AND its tombstone cannot be written → the step FAILS before
/// its turn (the worker never runs), so a spooled claim can never land for an un-teamed attempt.
#[test]
fn an_unwritable_tombstone_fails_the_step_before_its_turn() {
    let rig = rig("t5tomb");
    let run = "t5tomb";
    let floor = start(&rig, run);
    rig.refuse(&[tev::STEP_CLAIMED]);
    // The outbox path is a directory: neither the spool nor the tombstone can be written.
    let outbox = rig.dir.join("outbox-dir");
    std::fs::create_dir_all(&outbox).unwrap();
    let tr = TeamRunner::from_config(
        &TeamConfig::new(Some(rig.bus.clone()), Some(outbox))
            .with_schedule(vec![Duration::from_millis(5)])
            .with_attempt_wait(Duration::from_millis(30)),
    )
    .unwrap();
    let worker = Seat::new(|_| {});
    let r: Arc<dyn StepRunner> = worker.clone();
    let (out, _, evidence) = run_unit_and_judge_with_team(
        &r,
        &input(run, 1, 0, Some(bus_stamp(floor))),
        NOOP,
        &[],
        None,
        Some(&tr),
    );
    assert_eq!(out.status, StepStatus::Failed);
    assert!(worker.work_turns().is_empty(), "the turn never started");
    assert_eq!(evidence.team.unwrap().transport, Transport::None);
}

/// The actor's merge: the worker may downgrade, never upgrade; a teamed stamp with no worker
/// snapshot (or one without a ledger) is `stream_gap`, which pauses.
#[test]
fn merge_snapshot_fails_closed_on_every_absence() {
    let bus = bus_stamp(3);
    let none = UnitTeamSnapshot::stamped(Transport::None, Some("r".into()), None);
    // No stamp: not a team unit.
    assert!(merge_snapshot(None, Some(bus.clone())).is_none());
    // A teamed stamp, no worker snapshot → stream_gap, pauses.
    let m = merge_snapshot(Some(&bus), None).unwrap();
    assert_eq!(m.transport, Transport::Bus);
    assert!(m.ledger.as_ref().unwrap().team_pause);
    assert_eq!(m.ledger.unwrap().final_pass, FinalPass::StreamGap);
    // A teamed snapshot without a ledger → the same.
    let m = merge_snapshot(Some(&bus), Some(bus.clone())).unwrap();
    assert!(m.ledger.unwrap().team_pause);
    // The worker cannot upgrade an un-teamed stamp.
    let mut teamed = bus.clone();
    teamed.ledger = Some(empty_ledger(FinalPass::Completed));
    let m = merge_snapshot(Some(&none), Some(teamed)).unwrap();
    assert_eq!(m.transport, Transport::None);
    // The worker may downgrade (a failed step.claimed).
    let local = local_snapshot(&bus, Some("un-teamed attempt: x".into()));
    let m = merge_snapshot(Some(&bus), Some(local)).unwrap();
    assert_eq!(m.transport, Transport::None);
    assert_eq!(m.reason.as_deref(), Some("un-teamed attempt: x"));
}

/// The judge's exclusion is computed from the ledger: every author and corroborator, any
/// status, once each; `is_excluded` matches an instance or its cli key.
#[test]
fn ledger_authors_are_computed_from_the_ledger() {
    let l = authored_ledger();
    assert_eq!(ledger_authors(&l), vec!["claude#2", "claude#3"]);
    let ex = ledger_authors(&l);
    assert!(is_excluded("claude#2", &ex));
    assert!(
        is_excluded("claude", &ex),
        "the cli key of an excluded instance"
    );
    assert!(!is_excluded("codex", &ex));
    let _ = GateOpenedKind::TeamDispute {
        finding_ids: vec![],
    };
}

/// A supplied budget never lets a unit skip its gate wait: `WICKED_TEAM_FINAL_PASS_SECS=0` is
/// floored to [`MIN_GATE_WAIT`] in the production config.
#[test]
fn a_supplied_zero_budget_never_skips_the_gate_wait() {
    let _env = crate::test_env::ENV_LOCK
        .write()
        .unwrap_or_else(|p| p.into_inner());
    let prior = std::env::var("WICKED_TEAM_FINAL_PASS_SECS").ok();
    std::env::set_var("WICKED_TEAM_FINAL_PASS_SECS", "0");
    let rig = rig("t5floor");
    let cfg = TeamConfig::for_store(rig.dir.join("core.db").to_str().unwrap());
    match prior {
        Some(v) => std::env::set_var("WICKED_TEAM_FINAL_PASS_SECS", v),
        None => std::env::remove_var("WICKED_TEAM_FINAL_PASS_SECS"),
    }
    assert!(cfg.final_pass_budget >= MIN_GATE_WAIT, "{cfg:?}");
}

// ── DES-TEAMING-002 T6 ───────────────────────────────────────────────────────────────────────────

/// T6 (i): evaluator ≠ creator for a MEMBER's step. The step runs on the member seat (`codex`, its
/// `assigned_cli`), so the member is its work author; the ledger's authors (`claude#2`, and its
/// corroborator `claude#3`) are excluded too. The judge is none of them.
#[test]
fn t6_i_the_judge_of_a_member_step_is_neither_the_member_nor_a_ledger_author() {
    let rig = rig("t6i");
    let run = "t6i";
    let floor = start(&rig, run);
    let _s = supervise(&rig, run, authored_ledger());
    let wd = workdir("t6i");
    let worker = Seat::new(|_| {});
    let r: Arc<dyn StepRunner> = worker.clone();
    let roster = [
        instance("claude#2", "claude2"),
        instance("claude#3", "claude3"),
        seat("codex"),
        seat("gemini"),
    ];
    let mut i = judged_input(run, floor, &wd);
    i.unit.owner = crate::workflow::StepOwner::Team;
    i.unit.assigned_cli = Some("codex".into());
    i.unit.member_step = Some(crate::domain::MemberStepState {
        member: "codex".into(),
        ..Default::default()
    });
    i.unit.validator = None;
    i.unit.default_floor = true;
    let tr = runner(&rig);
    let (_, verdict, _) = run_unit_and_judge_with_team(&r, &i, NOOP, &roster, None, Some(&tr));
    assert!(verdict.is_some(), "a judge ran");
    let seats: Vec<String> = worker
        .judges()
        .iter()
        .map(|j| j.unit.assigned_cli.clone().unwrap_or_default())
        .collect();
    assert_eq!(
        seats,
        vec!["gemini".to_string()],
        "never the member nor an author"
    );
    let _ = std::fs::remove_dir_all(&wd);
}

/// T6 (f), the worker half: the PA's review attempt of a member's step runs no judge and no floor
/// (team evidence, never the gate), and its `STEP` line becomes one `step.reviewed` naming the
/// member attempt under review, before its `step.completed`.
#[test]
fn t6_f_a_review_attempt_is_not_judged_and_publishes_its_step_verdict() {
    let rig = rig("t6f-review");
    let run = "t6fr";
    let floor = start(&rig, run);
    let _s = supervise(
        &rig,
        run,
        TeamLedger::new(FinalPass::Completed, vec![], vec![], Default::default()),
    );
    let wd = workdir("t6fr");
    let worker = Seat::new(|_| {});
    let r: Arc<dyn StepRunner> = Arc::new(Scripted {
        inner: worker.clone(),
        out: "reviewed\nSTEP u1: REJECT to:pa — the migration drops a column\n".into(),
    });
    let mut i = judged_input(run, floor, &wd);
    i.unit.owner = crate::workflow::StepOwner::Team;
    i.unit.member_step = Some(crate::domain::MemberStepState {
        member: "codex".into(),
        reviewing: Some(0),
        ..Default::default()
    });
    i.attempt = 1;
    i.unit.default_floor = true;
    let tr = runner(&rig);
    let roster = [seat("claude"), seat("codex"), seat("gemini")];
    let (_, verdict, evidence) =
        run_unit_and_judge_with_team(&r, &i, NOOP, &roster, None, Some(&tr));
    assert!(verdict.is_none(), "no judge on a review attempt");
    assert!(worker.judges().is_empty());
    assert!(
        evidence.team.is_some(),
        "the attempt's team snapshot rides back"
    );
    let reviewed = rows_of(&rig, run, tev::STEP_REVIEWED);
    assert_eq!(reviewed.len(), 1, "{reviewed:?}");
    let p = &reviewed[0].1;
    assert_eq!(p["step_id"], step_id_of(&i));
    assert_eq!(p["verdict"], "rejected");
    assert_eq!(p["to"], "pa");
    assert_eq!(p["reviewed_attempt"], 0);
    assert_eq!(p["attempt"], 1);
    let completed = rows_of(&rig, run, tev::STEP_COMPLETED);
    assert!(
        reviewed[0].0 < completed[0].0,
        "step.reviewed precedes step.completed"
    );
    let _ = std::fs::remove_dir_all(&wd);
}

/// A runner that answers every worker turn with `out` and delegates judges to `inner`.
struct Scripted {
    inner: Arc<Seat>,
    out: String,
}

impl StepRunner for Scripted {
    fn run_unit(&self, input: &StepInput) -> StepOutput {
        let mut o = self.inner.run_unit(input);
        if !is_judge(input) {
            o.output = self.out.clone();
        }
        o
    }
}

/// T6 §8.9: a worker's `ADVICE` lines become `advice.answered` rows (R) — on the raise of THIS
/// attempt when there is one, else the latest raise of the unit — before its `step.completed`.
#[test]
fn t6_advice_lines_are_answered_on_the_bus_before_step_completed() {
    let rig = rig("t6-advice");
    let run = "t6adv";
    let floor = start(&rig, run);
    let old = finding(run, 1, 0, 1, "high", "src/a.rs", "claude#2");
    let cur = finding(run, 1, 1, 1, "high", "src/b.rs", "claude#2");
    publish(&rig, &old);
    publish(&rig, &cur);
    let (old_id, cur_id) = (finding_id_of(&old), finding_id_of(&cur));
    let i = input(run, 1, 1, Some(bus_stamp(floor)));
    let tr = runner(&rig);
    let Attempt::Claimed(c) = claim(Some(&tr), &i).unwrap() else {
        panic!("claimed")
    };
    let out = StepOutput {
        run_id: run.into(),
        unit_ix: 1,
        attempt: 1,
        output: format!(
            "done\nADVICE {cur_id}: ACCEPT — fixed\nADVICE {old_id}: DECLINE — not a bug\n\
             ADVICE f-0000000000000000: ACCEPT — never raised\n"
        ),
        status: StepStatus::Ok,
        usage: None,
        files: vec![],
        tools: vec![],
        governed: false,
    };
    let _ = complete(&c, &out);
    let answered = rows_of(&rig, run, tev::ADVICE_ANSWERED);
    assert_eq!(answered.len(), 2, "{answered:#?}");
    let by_id = |id: &str| {
        answered
            .iter()
            .find(|(_, p)| p["finding_id"] == id)
            .map(|(_, p)| p.clone())
            .unwrap()
    };
    assert_eq!(by_id(&cur_id)["attempt"], 1);
    assert_eq!(by_id(&cur_id)["disposition"], "accepted");
    assert_eq!(by_id(&old_id)["attempt"], 0, "the latest raise of the unit");
    assert_eq!(by_id(&old_id)["reason"], "not a bug");
    assert_eq!(
        by_id(&cur_id)["answered_in"],
        format!("{}:1", step_id_of(&i))
    );
    let completed = rows_of(&rig, run, tev::STEP_COMPLETED);
    assert!(answered.iter().all(|(id, _)| *id < completed[0].0));
}

/// T6 (k)'s boundary half: a finding an EARLIER attempt of this unit raised is labelled
/// `carried_from_attempt:<n>` in the advice block; the same finding re-raised by the supervisor on
/// this attempt is rendered once (its latest raise), not twice.
#[test]
fn t6_k_the_boundary_labels_a_finding_carried_from_an_earlier_attempt() {
    let rig = rig("t6k-bnd");
    let run = "t6kb";
    let floor = start(&rig, run);
    let old = finding(run, 1, 0, 1, "high", "src/a.rs", "claude#2");
    publish(&rig, &old);
    let i = input(run, 1, 1, Some(bus_stamp(floor)));
    let tr = runner(&rig);
    let Attempt::Claimed(c) = claim(Some(&tr), &i).unwrap() else {
        panic!("claimed")
    };
    let b = boundary(&c);
    let text = b.block.expect("rendered").output;
    assert!(text.contains("carried_from_attempt:0"), "{text}");
    // The supervisor re-raises it on this attempt: the next boundary renders the one raise.
    let carried = fixture_with(tev::FINDING_RAISED, 0, run, |p| {
        p["ord"] = json!(1);
        p["attempt"] = json!(1);
        p["raise_seq"] = json!(1);
        p["path"] = json!("src/a.rs");
        p["by"] = json!("claude#2");
        p["evidence"] = json!("evidence of src/a.rs");
        p["carried_from_attempt"] = json!(0);
    });
    publish(&rig, &carried);
    let b = boundary(&Claimed {
        attempt: 2,
        ..(*c).clone()
    });
    let text = b.block.expect("rendered").output;
    assert_eq!(text.matches(&finding_id_of(&old)).count(), 1, "{text}");
    assert!(text.contains("carried_from_attempt:0"), "{text}");
}

/// T6 (j): nothing in `src` names the deleted direct channels. The tokens are assembled here so
/// this file does not contain them.
#[test]
fn t6_j_no_direct_team_channel_is_left_in_src() {
    let banned: Vec<String> = [
        ["Team", "Cmd"],
        ["Team", "Handle"],
        ["team", "_finish"],
        ["Steer", "Mailbox"],
    ]
    .iter()
    .map(|p| p.concat())
    .collect();
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut hits = Vec::new();
    let mut stack = vec![root];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                stack.push(p);
            } else if let Ok(text) = std::fs::read_to_string(&p) {
                for (n, line) in text.lines().enumerate() {
                    if banned.iter().any(|b| line.contains(b.as_str())) {
                        hits.push(format!("{}:{}: {line}", p.display(), n + 1));
                    }
                }
            }
        }
    }
    assert!(hits.is_empty(), "{hits:#?}");
}
