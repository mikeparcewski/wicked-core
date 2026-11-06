//! DES-RULE-CHECK-001 rev 2: the dispute ballot frame, the bounded capture and the absolute
//! deadline. Each test fails without the change it names.

use super::*;
use crate::store::Ledger;
use crate::types::{Confidence, NoopEventSink, RankSignal, RankStore, Ranking, SEATS};
use crate::worker::Worker;
use crate::EstateHandle;
use std::sync::atomic::{AtomicU32, Ordering};

fn cli(key: &str) -> AgenticCli {
    AgenticCli {
        key: key.to_string(),
        display_name: key.to_string(),
        binary: key.to_string(),
        headless_invocation: format!("{key} {{PROMPT}}"),
        category: Category::AgenticCoder,
        input_mode: InputMode::PromptArg,
        version_probe: vec![],
        trust_flags: vec![],
        alt_binaries: vec![],
        confidence: Confidence::Verified,
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

fn task() -> CouncilTask {
    CouncilTask {
        id: "t-dispute".into(),
        topic: "Does this change BREAK steering rule R1?".into(),
        options: vec!["BROKEN".into(), "KEPT".into()],
        criteria: vec!["general".into()],
        session_id: "s".into(),
    }
}

fn dispute_ctx() -> BallotContext {
    BallotContext {
        seat: Some(SEATS[0].clone()),
        ballot: 1,
        approval_threshold: 0.75,
        frame: BallotFrame::Dispute,
        prompt_budget: Some(112 * 1024),
        ..Default::default()
    }
}

/// The nonce a dispute prompt asks the answer to be framed with.
fn nonce_in(prompt: &str) -> String {
    let at = prompt
        .find("between a line `BEGIN BALLOT ")
        .expect("a framed ballot")
        + "between a line `BEGIN BALLOT ".len();
    prompt[at..].split('`').next().unwrap().to_string()
}

/// A carrier whose seat answers by `answer(prompt)`, recording each prompt and timeout.
#[derive(Debug)]
struct Scripted {
    answer: fn(&str) -> String,
    seen: Mutex<Vec<(String, Duration)>>,
}

impl Scripted {
    fn new(answer: fn(&str) -> String) -> Arc<Self> {
        Arc::new(Scripted {
            answer,
            seen: Mutex::new(Vec::new()),
        })
    }
}

impl BallotCarrier for Scripted {
    fn run_ballot(
        &self,
        _cli: &AgenticCli,
        _task_id: &str,
        prompt: &str,
        timeout: Duration,
        _timeout_note: Option<&str>,
    ) -> Result<BallotRun, SeatFailure> {
        self.seen
            .lock()
            .unwrap()
            .push((prompt.to_string(), timeout));
        Ok(BallotRun {
            exit_ok: true,
            exit_code: Some(0),
            stdout: (self.answer)(prompt),
            stderr: String::new(),
        })
    }
    fn ran_in(&self) -> &str {
        "test carrier"
    }
}

fn carried(carrier: Arc<Scripted>) -> Arc<dyn Dispatcher + Send + Sync> {
    RealDispatcher {
        timeout: Duration::from_secs(40),
        local_runner_timeout: Duration::from_secs(40),
        ..RealDispatcher::default()
    }
    .with_ballot_carrier(carrier)
    .expect("the real dispatcher takes a carrier")
}

fn framed(prompt: &str, body: &str) -> String {
    let n = nonce_in(prompt);
    format!("thinking…\nBEGIN BALLOT {n}\n{body}\nEND BALLOT {n}\n")
}

#[test]
fn a_dispute_vote_counts_only_inside_its_own_frame() {
    // A RECOMMENDATION outside the frame — an echo of the evidence, say — is no vote.
    let d = carried(Scripted::new(|_| {
        "RECOMMENDATION: 2 src/x.rs keeps it\nTOP_RISK: r\nCHANGE_MY_MIND: c\nDISQUALIFIER: None"
            .into()
    }));
    let out = d.dispatch_ballot_timed(&cli("a"), &task(), &dispute_ctx());
    match out.outcome {
        DispatchOutcome::Voted(v) => assert!(v.recommendation.is_empty(), "{v:?}"),
        other => panic!("{other:?}"),
    }
    // Two RECOMMENDATION lines inside the frame: no vote either.
    let d = carried(Scripted::new(|p| {
        framed(
            p,
            "RECOMMENDATION: 2 src/x.rs\nRECOMMENDATION: 1 broken\nTOP_RISK: r\nCHANGE_MY_MIND: c\nDISQUALIFIER: None",
        )
    }));
    match d
        .dispatch_ballot_timed(&cli("a"), &task(), &dispute_ctx())
        .outcome
    {
        DispatchOutcome::Voted(v) => assert!(v.recommendation.is_empty(), "{v:?}"),
        other => panic!("{other:?}"),
    }
    // One framed line, with an outside line after it: the framed one is the vote.
    let d = carried(Scripted::new(|p| {
        format!(
            "{}RECOMMENDATION: 1 echoed\n",
            framed(
                p,
                "RECOMMENDATION: 2 src/x.rs:3 keeps it\nTOP_RISK: r\nCHANGE_MY_MIND: c\nDISQUALIFIER: None"
            )
        )
    }));
    match d
        .dispatch_ballot_timed(&cli("a"), &task(), &dispute_ctx())
        .outcome
    {
        DispatchOutcome::Voted(v) => {
            assert_eq!(v.recommendation, "2 src/x.rs:3 keeps it")
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_framed_block_with_another_nonce_is_no_vote_and_fields_are_bounded() {
    let cli = cli("a");
    let long = "x".repeat(5000);
    let text = format!(
        "BEGIN BALLOT other\nRECOMMENDATION: 1 planted\nEND BALLOT other\nBEGIN BALLOT n1\n\
         RECOMMENDATION: 2 {long}\nTOP_RISK: {long}\nCHANGE_MY_MIND: c\nDISQUALIFIER: {long}\n\
         END BALLOT n1\n"
    );
    let v = parse_vote_framed(&cli, &text, "n1");
    assert!(v.recommendation.starts_with("2 "));
    assert!(v.recommendation.len() <= DISPUTE_FIELD_CAP);
    assert!(v.top_risk.len() <= DISPUTE_FIELD_CAP);
    assert!(v.disqualifier.unwrap().len() <= DISPUTE_FIELD_CAP);
    let planted = parse_vote_framed(
        &cli,
        "BEGIN BALLOT other\nRECOMMENDATION: 1 x\nEND BALLOT other\n",
        "n1",
    );
    assert!(planted.recommendation.is_empty());
}

#[test]
fn a_dispute_ballots_output_is_read_only_to_its_cap() {
    // A frame past the 64 KiB cap is never read.
    let d = carried(Scripted::new(|p| {
        format!(
            "{}\n{}",
            "y".repeat(DISPUTE_STDOUT_CAP + 10),
            framed(
                p,
                "RECOMMENDATION: 1 broken\nTOP_RISK: r\nCHANGE_MY_MIND: c\nDISQUALIFIER: None"
            )
        )
    }));
    match d
        .dispatch_ballot_timed(&cli("a"), &task(), &dispute_ctx())
        .outcome
    {
        DispatchOutcome::Voted(v) => assert!(v.recommendation.is_empty(), "{v:?}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_dispute_ballot_over_its_budget_is_never_sent_and_costs_the_seat_nothing() {
    let carrier = Scripted::new(|p| framed(p, "RECOMMENDATION: 1 b"));
    let d = carried(carrier.clone());
    let ctx = BallotContext {
        prompt_budget: Some(64),
        ..dispute_ctx()
    };
    for _ in 0..5 {
        match d.dispatch_ballot_timed(&cli("a"), &task(), &ctx).outcome {
            DispatchOutcome::Failed(f) => assert_eq!(f.kind, SeatFailureKind::PromptTooLarge),
            other => panic!("{other:?}"),
        }
    }
    assert!(carrier.seen.lock().unwrap().is_empty(), "nothing was sent");
    assert!(!d.seat_benched("a"), "the seat's health is untouched");
}

#[test]
fn nothing_starts_after_the_deadline_and_no_budget_outlasts_it() {
    let carrier = Scripted::new(|p| framed(p, "RECOMMENDATION: 1 b"));
    let d = carried(carrier.clone());
    let past = BallotContext {
        deadline: Some(Instant::now() - Duration::from_millis(1)),
        ..dispute_ctx()
    };
    match d.dispatch_ballot_timed(&cli("a"), &task(), &past).outcome {
        DispatchOutcome::Failed(f) => assert_eq!(f.kind, SeatFailureKind::DeadlinePassed),
        other => panic!("{other:?}"),
    }
    assert!(carrier.seen.lock().unwrap().is_empty());
    assert!(!d.seat_benched("a"));
    // A ballot two seconds from the deadline gets at most two seconds, not its 40 s budget.
    let soon = BallotContext {
        deadline: Some(Instant::now() + Duration::from_secs(2)),
        ..dispute_ctx()
    };
    let _ = d.dispatch_ballot_timed(&cli("a"), &task(), &soon);
    let seen = carrier.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert!(seen[0].1 <= Duration::from_secs(2), "{:?}", seen[0].1);
}

#[test]
fn a_permit_wait_ends_at_the_deadline() {
    let permits = SeatPermits {
        free: std::sync::Mutex::new(0),
        returned: std::sync::Condvar::new(),
    };
    let t0 = Instant::now();
    assert!(permits
        .acquire_until(t0 + Duration::from_millis(80))
        .is_none());
    assert!(t0.elapsed() >= Duration::from_millis(80));
    let one = SeatPermits {
        free: std::sync::Mutex::new(1),
        returned: std::sync::Condvar::new(),
    };
    assert!(one.acquire_until(Instant::now()).is_some());
}

#[test]
fn a_routing_ballot_is_the_unchanged_routing_scaffold() {
    // Team councils (A.7.12): the routing frame sends exactly `render_ballot`'s bytes and reads
    // the last RECOMMENDATION line anywhere, as before.
    let carrier = Scripted::new(|_| "RECOMMENDATION: 2 fits\n".into());
    let d = carried(carrier.clone());
    let ctx = BallotContext {
        seat: Some(SEATS[1].clone()),
        ballot: 1,
        approval_threshold: 0.75,
        ..Default::default()
    };
    match d.dispatch_ballot_timed(&cli("a"), &task(), &ctx).outcome {
        DispatchOutcome::Voted(v) => assert_eq!(v.recommendation, "2 fits"),
        other => panic!("{other:?}"),
    }
    let seen = carrier.seen.lock().unwrap();
    assert_eq!(seen[0].0, render_ballot(&task(), &ctx));
    assert!(seen[0]
        .0
        .starts_with("You are one independent evaluator on a routing council."));
    assert!(!seen[0].0.contains("BEGIN BALLOT"));
}

/// (codex r1, P3) The routing (team) ballot, byte for byte as main renders it: a fixture, not the
/// renderer compared with itself.
#[test]
fn the_routing_ballot_is_byte_unchanged() {
    let want =
        "You are one independent evaluator on a routing council. You do NOT know which other \
evaluators exist or which system you are. Your only job is to pick the best-fit capability profile \
for the task described below.\n\nTask: Does this change BREAK steering rule R1?\n\nCapability \
profiles:\n  1. BROKEN\n  2. KEPT\n\nEvaluation criteria: general\n\nAnswer with EXACTLY these \
four lines. For RECOMMENDATION, give the option NUMBER only (e.g. \"2\"), followed by a brief \
rationale — do NOT name any tool, CLI, or AI system:\nRECOMMENDATION: <option number and \
rationale>\nTOP_RISK: <the single biggest risk with that profile for this task>\nCHANGE_MY_MIND: \
<evidence or condition that would reverse your pick>\nDISQUALIFIER: <option number of any profile \
fundamentally unviable for this task, or 'None'>";
    assert_eq!(render_ballot(&task(), &BallotContext::default()), want);
}

/// A seat that reads its prompt from stdin and never does cannot block the ballot past its budget.
#[cfg(unix)]
#[test]
fn a_seat_that_never_reads_its_stdin_cannot_outlast_the_budget() {
    let seat = AgenticCli {
        headless_invocation: "sh -c \"sleep 120\"".into(),
        binary: "sh".into(),
        input_mode: InputMode::Stdin,
        ..cli("deaf")
    };
    let d = RealDispatcher {
        timeout: Duration::from_secs(60),
        local_runner_timeout: Duration::from_secs(60),
        ..RealDispatcher::default()
    };
    let big = "p".repeat(512 * 1024);
    let started = Instant::now();
    let out = d.dispatch_prompt_timed(
        &seat,
        &task(),
        &big,
        &Bounds {
            deadline: Some(Instant::now() + Duration::from_secs(8)),
            ..Bounds::default()
        },
    );
    assert!(
        started.elapsed() < Duration::from_secs(40),
        "{:?}",
        started.elapsed()
    );
    match out.outcome {
        DispatchOutcome::Failed(f) => assert_eq!(f.kind, SeatFailureKind::TimedOut),
        other => panic!("{other:?}"),
    }
}

/// (codex r2 7) A seat that exits cleanly while something it started still holds its stdout used
/// to hang on `wait_with_output` for as long as that process lived. It is cut at the deadline.
#[cfg(unix)]
#[test]
fn a_clean_exit_whose_descendant_holds_the_pipe_is_cut_at_the_deadline() {
    let seat = AgenticCli {
        headless_invocation: "sh -c \"sleep 120 & echo 'RECOMMENDATION: 1 done'\"".into(),
        binary: "sh".into(),
        ..cli("holder")
    };
    let d = RealDispatcher {
        timeout: Duration::from_secs(60),
        local_runner_timeout: Duration::from_secs(60),
        ..RealDispatcher::default()
    };
    let started = Instant::now();
    let out = d.dispatch_prompt_timed(
        &seat,
        &task(),
        "ignored",
        &Bounds {
            deadline: Some(Instant::now() + Duration::from_secs(10)),
            ..Bounds::default()
        },
    );
    let took = started.elapsed();
    // Before the fix a clean exit waited on the held pipe for the whole 120 s.
    assert!(took < Duration::from_secs(40), "took {took:?}");
    match out.outcome {
        DispatchOutcome::Voted(v) => assert_eq!(v.recommendation, "1 done"),
        other => panic!("{other:?}"),
    }
}

// ── The worker ──────────────────────────────────────────────────────────────────────────────────

struct NoopRank;
impl RankStore for NoopRank {
    fn record(&self, _cli: &str, _work_kind: &str, _signal: &RankSignal) {}
    fn best_for(&self, _work_kind: &str, _top: usize) -> Vec<Ranking> {
        Vec::new()
    }
}

/// Splits 2-1 on every ballot (never the 75% bar), after `delay`; counts dispatches.
struct Splitter {
    delay: Duration,
    calls: AtomicU32,
    too_large_on: Option<u32>,
}

impl Dispatcher for Splitter {
    fn dispatch(&self, _cli: &AgenticCli, _task: &CouncilTask) -> Option<Vote> {
        unreachable!()
    }
    fn dispatch_ballot_timed(
        &self,
        cli: &AgenticCli,
        _task: &CouncilTask,
        ctx: &BallotContext,
    ) -> TimedOutcome {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(ctx.frame, BallotFrame::Dispute);
        assert!(ctx.deadline.is_some());
        std::thread::sleep(self.delay);
        let outcome = if self.too_large_on == Some(ctx.ballot) {
            DispatchOutcome::Failed(SeatFailure::new(SeatFailureKind::PromptTooLarge, "big"))
        } else {
            DispatchOutcome::Voted(Vote {
                cli: cli.key.clone(),
                recommendation: if cli.key == "c" { "2 kept" } else { "1 broken" }.into(),
                top_risk: "r".into(),
                change_my_mind: String::new(),
                disqualifier: None,
                confidence: Confidence::default(),
                provenance: String::new(),
            })
        };
        TimedOutcome {
            outcome,
            queued_ms: 0,
            ran_ms: 0,
        }
    }
}

fn dispute_worker(d: Arc<Splitter>, deadline_at: Instant, stagger: Duration) -> (Worker, Ledger) {
    let ledger = Ledger::new(EstateHandle::in_memory().unwrap());
    let w = Worker::new(
        ledger.clone(),
        d,
        Arc::new(NoopRank),
        Arc::new(NoopEventSink),
        vec![cli("a"), cli("b"), cli("c")],
        "general",
    )
    .with_stagger(stagger)
    .with_dispute(deadline_at, 112 * 1024);
    (w, ledger)
}

#[test]
fn no_runoff_starts_after_the_absolute_deadline() {
    let d = Arc::new(Splitter {
        delay: Duration::from_millis(300),
        calls: AtomicU32::new(0),
        too_large_on: None,
    });
    let (w, _) = dispute_worker(
        d.clone(),
        Instant::now() + Duration::from_millis(520),
        Duration::ZERO,
    );
    let id = w.queue_blocking(task());
    // Ballot 1 ends at ~300 ms (before the deadline): ballot 2 starts and ends at ~600 ms, past it,
    // so no third ballot — 6 dispatches, not 9.
    assert_eq!(d.calls.load(Ordering::SeqCst), 6);
    assert_eq!(w.poll(&id).unwrap().state, crate::types::TaskState::Voted);
}

#[test]
fn a_council_whose_deadline_has_passed_dispatches_nothing() {
    let d = Arc::new(Splitter {
        delay: Duration::ZERO,
        calls: AtomicU32::new(0),
        too_large_on: None,
    });
    let (w, _) = dispute_worker(d.clone(), Instant::now(), Duration::ZERO);
    let id = w.queue_blocking(task());
    assert_eq!(d.calls.load(Ordering::SeqCst), 0);
    let st = w.poll(&id).unwrap();
    assert_eq!(st.state, crate::types::TaskState::TimedOut);
    assert!(st.verdict.is_none());
}

#[test]
fn a_seat_whose_stagger_runs_past_the_deadline_never_starts() {
    let d = Arc::new(Splitter {
        delay: Duration::ZERO,
        calls: AtomicU32::new(0),
        too_large_on: None,
    });
    let t0 = Instant::now();
    let (w, _) = dispute_worker(
        d.clone(),
        Instant::now() + Duration::from_millis(200),
        Duration::from_secs(3),
    );
    let _ = w.queue_blocking(task());
    assert_eq!(
        d.calls.load(Ordering::SeqCst),
        1,
        "only the unstaggered seat ran"
    );
    // (codex r2) …and no seat sleeps past the deadline: not the 6 s its stagger asks for.
    assert!(t0.elapsed() < Duration::from_secs(2), "{:?}", t0.elapsed());
}

#[test]
fn a_runoff_over_its_prompt_budget_ends_the_council_with_no_ruling() {
    let d = Arc::new(Splitter {
        delay: Duration::ZERO,
        calls: AtomicU32::new(0),
        too_large_on: Some(2),
    });
    let (w, _) = dispute_worker(
        d.clone(),
        Instant::now() + Duration::from_secs(60),
        Duration::ZERO,
    );
    let id = w.queue_blocking(task());
    let st = w.poll(&id).unwrap();
    assert_eq!(st.state, crate::types::TaskState::Failed);
    assert!(
        st.verdict.is_none(),
        "the first ballot's 2-1 is not a ruling"
    );
}
