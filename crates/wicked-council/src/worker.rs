//! The detached queue/poll worker — the headline.
//!
//! `queue(task)` persists the task as `Queued` (a `COUNCIL_TASK` estate node), emits
//! `wicked.council.requested`, spawns a **background `std::thread`**, and **returns the
//! `task_id` immediately**. Nothing in the dispatch path requires the requesting agent to
//! stay resident — the thread owns the subprocess fan-out, synthesis, ranking, and event
//! emission.
//!
//! `poll(task_id)` is a cheap status read of the shared ledger:
//! `queued → running → {voted | timed_out | failed}` plus the verdict when ready.
//!
//! The worker's shared state is an in-memory `Arc<Mutex<..>>` ledger (so the spawned
//! thread and the caller see the same live state), and every mutation is mirrored durably
//! to the shared estate store as Nodes (+ a task→verdict edge). The background thread is
//! detached on the hot path; `queue_blocking` is provided for tests that want determinism
//! without polling, but the **non-blocking contract is what `queue` delivers** and what
//! the E2E test asserts via poll.

use std::sync::Arc;
use std::thread::JoinHandle;

use crate::store::{Ledger, SeatFailureRecord, TaskRecord};
use crate::synthesis;
use std::time::Instant;

use crate::types::{
    AgenticCli, BallotContext, CouncilTask, DispatchOutcome, Dispatcher, EventSink, RankSignal,
    RankStore, SeatFailure, SeatFailureKind, TaskState, TimedOutcome, Verdict, SEATS,
};

/// Recover a readable message from a caught panic payload.
fn panic_detail(payload: &Box<dyn std::any::Any + Send>) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "seat dispatch panicked".to_string())
}

/// The share of seats that must converge on one option before the council stops
/// deliberating — the approval bar every voter is told about in its ballot prompt.
pub const APPROVAL_THRESHOLD: f32 = 0.75;

/// Hard cap on ballots (first vote + runoffs) so deliberation always terminates.
/// Below-threshold after the final ballot degrades to plurality, exactly as before.
pub const MAX_BALLOTS: u32 = 3;

/// The detached worker. Holds the shared ledger plus the injected seams (dispatcher, rank
/// store, event sink) so the same engine wiring serves both the real CLI and the
/// deterministic E2E test (which injects fakes).
pub struct Worker {
    ledger: Ledger,
    dispatcher: Arc<dyn Dispatcher + Send + Sync>,
    rank_store: Arc<dyn RankStore + Send + Sync>,
    events: Arc<dyn EventSink + Send + Sync>,
    /// The CLIs convened (already probed-usable by the caller).
    roster: Arc<Vec<AgenticCli>>,
    /// The work-kind this council counts toward in ranking (criteria-derived).
    work_kind: String,
}

impl Worker {
    /// Build a worker over a shared ledger and the three seams.
    pub fn new(
        ledger: Ledger,
        dispatcher: Arc<dyn Dispatcher + Send + Sync>,
        rank_store: Arc<dyn RankStore + Send + Sync>,
        events: Arc<dyn EventSink + Send + Sync>,
        roster: Vec<AgenticCli>,
        work_kind: impl Into<String>,
    ) -> Self {
        Worker {
            ledger,
            dispatcher,
            rank_store,
            events,
            roster: Arc::new(roster),
            work_kind: work_kind.into(),
        }
    }

    /// **Non-blocking.** Persist the task as `Queued`, emit `wicked.council.requested`,
    /// spawn the detached fan-out thread, and return the `task_id` at once.
    ///
    /// Returns `(task_id, JoinHandle)`. The handle is the worker thread; callers on the hot
    /// path **drop it** (the thread is detached and writes its result to the ledger + estate
    /// store). Tests may `join()` it for determinism — see [`Worker::queue_blocking`].
    pub fn queue(&self, task: CouncilTask) -> (String, JoinHandle<()>) {
        let task_id = task.id.clone();
        let convened: Vec<String> = self.roster.iter().map(|c| c.key.clone()).collect();

        self.ledger.insert(TaskRecord {
            task: task.clone(),
            state: TaskState::Queued,
            convened: convened.clone(),
            votes: Vec::new(),
            verdict: None,
            seat_failures: Vec::new(),
        });

        self.events.emit(
            wicked_apps_core::EV_COUNCIL_REQUESTED,
            &serde_json::json!({
                "task_id": task_id,
                "topic": task.topic,
                "clis": convened,
                "session_id": task.session_id,
            }),
        );

        // Clone the shared handles into the thread. The ledger is an Arc<Mutex<…>> under
        // the hood (mirrored to the shared estate store), so the spawned thread and the
        // caller see the same live state and the same durable graph.
        let ledger = self.ledger.clone();
        let dispatcher = Arc::clone(&self.dispatcher);
        let rank_store = Arc::clone(&self.rank_store);
        let events = Arc::clone(&self.events);
        let roster = Arc::clone(&self.roster);
        let work_kind = self.work_kind.clone();
        let task_for_thread = task;

        let handle = std::thread::spawn(move || {
            run_council(
                &ledger,
                dispatcher.as_ref(),
                rank_store.as_ref(),
                events.as_ref(),
                &roster,
                &work_kind,
                &task_for_thread,
            );
        });

        (task_id, handle)
    }

    /// Test helper: queue then `join` the worker thread so the council has resolved when
    /// this returns. The production contract is [`Worker::queue`] (non-blocking); this only
    /// removes the poll loop from deterministic unit tests.
    pub fn queue_blocking(&self, task: CouncilTask) -> String {
        let (id, handle) = self.queue(task);
        handle.join().expect("worker thread panicked");
        id
    }

    /// Cheap status read: the current state and verdict (if any) of `task_id`.
    pub fn poll(&self, task_id: &str) -> Option<PollStatus> {
        self.ledger.get(task_id).map(|rec| PollStatus {
            task_id: task_id.to_string(),
            state: rec.state,
            returned: rec.votes.len() as u32,
            pending: rec.convened.len().saturating_sub(rec.votes.len()) as u32,
            verdict: rec.verdict,
            seat_failures: rec.seat_failures,
        })
    }
}

/// What `poll` returns (serialized for the CLI).
#[derive(Debug, Clone, serde::Serialize)]
pub struct PollStatus {
    /// The task polled.
    pub task_id: String,
    /// Current lifecycle state.
    pub state: TaskState,
    /// Votes collected so far.
    pub returned: u32,
    /// CLIs still outstanding.
    pub pending: u32,
    /// The verdict, once `state == Voted`.
    pub verdict: Option<Verdict>,
    /// Seats that were convened but did not vote on the latest ballot, and why.
    ///
    /// Do not read this off `pending`: `pending` is `convened - returned`, which lumps a seat
    /// that has not been dispatched yet together with one that was dispatched and failed. This
    /// list is only the second kind, and it is what the caller renders the degrade reason from
    /// instead of the old catch-all "council did not reach a vote".
    pub seat_failures: Vec<SeatFailureRecord>,
}

/// The body the detached thread runs: dispatch → collect → synthesize → rank → emit.
///
/// Free function (not a method) so it owns only the cloned handles, never `&self` —
/// reinforcing that no part of this needs the requesting agent.
#[allow(clippy::too_many_arguments)]
fn run_council(
    ledger: &Ledger,
    // `Sync` because seats are dispatched concurrently; the `Worker` already holds it as
    // `Arc<dyn Dispatcher + Send + Sync>`, so this only stops the bound being dropped here.
    dispatcher: &(dyn Dispatcher + Send + Sync),
    rank_store: &dyn RankStore,
    events: &dyn EventSink,
    roster: &[AgenticCli],
    work_kind: &str,
    task: &CouncilTask,
) {
    ledger.update(&task.id, |rec| rec.state = TaskState::Running);

    // No usable CLIs → fail honestly (quorum 0).
    if roster.is_empty() {
        ledger.update(&task.id, |rec| rec.state = TaskState::Failed);
        return;
    }

    // DELIBERATION LOOP — governance as a conversation, not a one-shot poll. Each ballot
    // dispatches every seat with its unique lens (SEATS rotation) and the approval bar;
    // if the ballot lands below APPROVAL_THRESHOLD, a runoff shares the tally + dissent
    // arguments with every seat so the council can converge like a real deliberating
    // body. MAX_BALLOTS caps the loop; the final ballot's plurality stands regardless.
    let mut ballot: u32 = 1;
    let mut prior_tally: Vec<(String, u32)> = Vec::new();
    let mut dissent_arguments: Vec<String> = Vec::new();
    // Per-CLI ranking signal accumulated ACROSS ballots: total RUN time summed over every
    // dispatch (a 3-ballot deliberation costs 3 dispatches and is reported as such), and
    // success from the seat's final ballot (the one the verdict is synthesized from).
    let mut signal_acc: std::collections::BTreeMap<String, (bool, u64)> =
        std::collections::BTreeMap::new();
    let (votes, verdict) = loop {
        // Dispatch every seat CONCURRENTLY, recording per-CLI latency for ranking.
        //
        // Isolation is what makes this safe, and it was already a hard requirement: each seat
        // runs in its own tempdir, its own subprocess and its own budget, and no seat reads
        // another's output. Nothing was shared to begin with.
        //
        // It is also what makes the budget affordable. A ballot used to cost the SUM of its
        // seats and now costs the SLOWEST — and since a below-bar ballot re-runs the whole
        // roster, the sequential version multiplied that sum by the ballot count. Measured on
        // the shipped 3-seat roster: 4m30s to route 3 units, against a budget claiming 30s.
        //
        // Results are collected by seat index and processed below in roster order, so the votes,
        // the failure list and the emitted events are identical to the sequential version. Only
        // the wall clock changes.
        let mut votes = Vec::new();
        let mut failures: Vec<SeatFailureRecord> = Vec::new();
        let dispatched: Vec<TimedOutcome> = std::thread::scope(|scope| {
            let handles: Vec<_> = roster
                .iter()
                .enumerate()
                .map(|(i, cli)| {
                    let ctx = BallotContext {
                        seat: Some(SEATS[i % SEATS.len()].clone()),
                        ballot,
                        approval_threshold: APPROVAL_THRESHOLD,
                        prior_tally: prior_tally.clone(),
                        dissent_arguments: dissent_arguments.clone(),
                    };
                    scope.spawn(move || {
                        let started = Instant::now();
                        // Catch the unwind HERE rather than at the join, so a seat that panicked
                        // after doing real work still reports the time it spent. Reporting 0
                        // would not merely lose information: the ranking signal sums this, and a
                        // failed seat that looks instantaneous is a lie in the flattering
                        // direction.
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            dispatcher.dispatch_ballot_timed(cli, task, &ctx)
                        }))
                        .unwrap_or_else(|e| TimedOutcome {
                            outcome: DispatchOutcome::Failed(SeatFailure::new(
                                SeatFailureKind::Panicked,
                                panic_detail(&e),
                            )),
                            queued_ms: 0,
                            ran_ms: started.elapsed().as_millis() as u64,
                        })
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| {
                    // A seat that unwinds is that seat's failure, not the council's. Letting the
                    // panic propagate would abort the whole distribution over one bad seat —
                    // exactly the blast radius a quorum exists to contain.
                    h.join().unwrap_or_else(|e| TimedOutcome {
                        // Unreachable while the closure catches its own unwind; kept so a future
                        // change there cannot turn one bad seat into an aborted distribution.
                        outcome: DispatchOutcome::Failed(SeatFailure::new(
                            SeatFailureKind::Panicked,
                            panic_detail(&e),
                        )),
                        queued_ms: 0,
                        ran_ms: 0,
                    })
                })
                .collect()
        });

        for (i, timed) in dispatched.into_iter().enumerate() {
            let TimedOutcome {
                outcome,
                queued_ms,
                ran_ms,
            } = timed;
            let cli = &roster[i];
            let entry = signal_acc.entry(cli.key.clone()).or_insert((false, 0));
            entry.0 = outcome.is_voted();
            // Rank on run time only. Queue time measures how busy the council was, not how fast
            // this CLI is, and charging it to the seat would rank whichever seat happened to wait.
            entry.1 += ran_ms;
            match outcome {
                DispatchOutcome::Voted(v) => votes.push(v),
                DispatchOutcome::Failed(failure) => {
                    // A seat that does not vote is a governance event, not a silent gap: it
                    // shrinks the quorum the verdict rests on. Surface it per seat, with the
                    // branch and the CLI's own stderr, so the degrade is diagnosable from the
                    // event stream alone.
                    events.emit(
                        wicked_apps_core::EV_COUNCIL_SEAT_FAILED,
                        &serde_json::json!({
                            "task_id": task.id,
                            "round": ballot,
                            "cli": cli.key,
                            "kind": failure.kind.as_str(),
                            "exit_code": failure.exit_code,
                            "stderr": failure.stderr,
                            "detail": failure.detail,
                            // The run, not the wall clock: this sits next to a message naming
                            // the dispatch budget, and the budget governs the run. Queue time is
                            // reported beside it rather than folded in.
                            "latency_ms": ran_ms,
                            "queued_ms": queued_ms,
                        }),
                    );
                    failures.push(SeatFailureRecord {
                        cli: cli.key.clone(),
                        failure,
                    });
                }
            }
        }

        // Persist this ballot's votes and failures (the record always reflects the latest
        // ballot). Both are written together so a reader never sees votes from this ballot
        // beside failures from the previous one.
        let collected = votes.clone();
        let collected_failures = failures.clone();
        ledger.update(&task.id, |rec| {
            rec.votes = collected;
            rec.seat_failures = collected_failures;
        });

        // No votes at all (every seat timed out / errored) → timed_out.
        if votes.is_empty() {
            ledger.update(&task.id, |rec| rec.state = TaskState::TimedOut);
            return;
        }

        // Synthesize the verdict (layer c) for this ballot.
        let verdict = synthesis::synthesize(&task.id, &votes);

        if verdict.agreement_ratio >= APPROVAL_THRESHOLD || ballot >= MAX_BALLOTS {
            break (votes, verdict);
        }

        // Below the bar with ballots remaining: surface the round, then deliberate again
        // with the tally + anonymized dissent arguments in every seat's next prompt.
        events.emit(
            wicked_apps_core::EV_COUNCIL_DELIBERATED,
            &serde_json::json!({
                "task_id": task.id,
                "round": ballot,
                "agreement_ratio": verdict.agreement_ratio,
                "votes": votes.len(),
                "threshold": APPROVAL_THRESHOLD,
            }),
        );

        let matrix = synthesis::build_matrix(&votes);
        prior_tally = matrix.recommendation_counts;
        let winning_norm = verdict
            .winning_recommendation
            .as_deref()
            .map(normalize)
            .unwrap_or_default();
        dissent_arguments = votes
            .iter()
            .filter(|v| normalize(&v.recommendation) != winning_norm)
            .filter(|v| !v.top_risk.trim().is_empty())
            .take(3)
            .map(|v| v.top_risk.clone())
            .collect();
        ballot += 1;
    };
    let winning = verdict.winning_recommendation.clone();

    // Record per-CLI ranking signals: did the seat succeed on the deciding ballot, did it
    // agree with the eventual consensus winner, and what did the whole deliberation cost?
    for (cli_key, (success, total_latency_ms)) in &signal_acc {
        let agreement = match (&winning, votes.iter().find(|v| &v.cli == cli_key)) {
            (Some(win), Some(v)) => normalize(&v.recommendation) == normalize(win),
            _ => false,
        };
        rank_store.record(
            cli_key,
            work_kind,
            &RankSignal {
                success: *success,
                agreement_with_consensus: agreement,
                latency_ms: *total_latency_ms,
            },
        );
    }

    // Persist verdict + voted state (mirrored to estate as a COUNCIL_VERDICT node + edge).
    let v_for_ledger = verdict.clone();
    ledger.update(&task.id, |rec| {
        rec.verdict = Some(v_for_ledger);
        rec.state = TaskState::Voted;
    });

    // Emit signals (fire-and-forget; payload is ids + counts + ratio, never raw text).
    events.emit(
        wicked_apps_core::EV_COUNCIL_VOTED,
        &serde_json::json!({
            "task_id": task.id,
            "verdict_kind": verdict.kind,
            "consensus": verdict.consensus,
            "agreement_ratio": verdict.agreement_ratio,
            "votes": votes.len(),
        }),
    );
    events.emit(
        wicked_apps_core::EV_CLI_RANKED,
        &serde_json::json!({
            "task_id": task.id,
            "work_kind": work_kind,
            "updated": signal_acc.keys().collect::<Vec<_>>(),
        }),
    );
}

/// Local copy of the synthesis normaliser so the worker doesn't reach into a private fn —
/// keeps the agreement check consistent with the matrix.
fn normalize(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Ledger;
    use crate::types::{Confidence, NoopEventSink, RankStore, Ranking, Vote};
    use crate::EstateHandle;
    use std::sync::Mutex;
    use std::time::Duration;

    fn cli(key: &str) -> AgenticCli {
        AgenticCli {
            key: key.to_string(),
            display_name: key.to_string(),
            binary: key.to_string(),
            headless_invocation: format!("{key} {{PROMPT}}"),
            category: crate::types::Category::AgenticCoder,
            input_mode: crate::types::InputMode::PromptArg,
            version_probe: vec![],
            trust_flags: vec![],
            alt_binaries: vec![],
            confidence: crate::types::Confidence::Verified,
            enabled_for_council: true,
            acp: None,
            capabilities: None,
        }
    }

    fn vote(cli_key: &str, rec: &str) -> Vote {
        Vote {
            cli: cli_key.to_string(),
            recommendation: rec.to_string(),
            top_risk: format!("{cli_key} risk"),
            change_my_mind: String::new(),
            disqualifier: None,
            confidence: Confidence::default(),
            provenance: String::new(),
        }
    }

    struct NoopRank;
    impl RankStore for NoopRank {
        fn record(&self, _cli: &str, _work_kind: &str, _signal: &RankSignal) {}
        fn best_for(&self, _work_kind: &str, _top: usize) -> Vec<Ranking> {
            Vec::new()
        }
    }

    /// Scripted deliberation: ballot 1 splits 2/1/1 (50% < 75%); on ballot 2 every seat
    /// sees the tally (ctx.ballot == 2, non-empty prior_tally) and converges on "1".
    struct ConvergingDispatcher {
        calls: Mutex<Vec<(String, u32, usize)>>, // (cli, ballot, tally_len)
    }
    impl Dispatcher for ConvergingDispatcher {
        fn dispatch(&self, _cli: &AgenticCli, _task: &CouncilTask) -> Option<Vote> {
            unreachable!("deliberation must flow through dispatch_ballot");
        }
        fn dispatch_ballot(
            &self,
            cli: &AgenticCli,
            _task: &CouncilTask,
            ctx: &BallotContext,
        ) -> Option<Vote> {
            self.calls
                .lock()
                .unwrap()
                .push((cli.key.clone(), ctx.ballot, ctx.prior_tally.len()));
            assert!(ctx.seat.is_some(), "every ballot dispatch carries a seat");
            assert!(
                (ctx.approval_threshold - APPROVAL_THRESHOLD).abs() < f32::EPSILON,
                "voters are told the approval bar"
            );
            let rec = if ctx.ballot == 1 {
                match cli.key.as_str() {
                    "a" | "b" => "1 — fits",
                    "c" => "2 — other",
                    _ => "3 — third",
                }
            } else {
                "1 — fits"
            };
            Some(vote(&cli.key, rec))
        }
    }

    fn worker_with(dispatcher: Arc<dyn Dispatcher + Send + Sync>, keys: &[&str]) -> Worker {
        let estate = EstateHandle::in_memory().expect("estate");
        Worker::new(
            Ledger::new(estate),
            dispatcher,
            Arc::new(NoopRank),
            Arc::new(NoopEventSink),
            keys.iter().map(|k| cli(k)).collect(),
            "general",
        )
    }

    fn task() -> CouncilTask {
        CouncilTask {
            id: "t-deliberate".into(),
            topic: "pick a profile".into(),
            options: vec!["one".into(), "two".into(), "three".into()],
            criteria: vec!["general".into()],
            session_id: "s".into(),
        }
    }

    #[test]
    fn fragmented_first_ballot_converges_on_runoff() {
        let dispatcher = Arc::new(ConvergingDispatcher {
            calls: Mutex::new(Vec::new()),
        });
        let worker = worker_with(dispatcher.clone(), &["a", "b", "c", "d"]);
        let id = worker.queue_blocking(task());
        let status = worker.poll(&id).expect("status");

        assert_eq!(status.state, TaskState::Voted);
        let verdict = status.verdict.expect("verdict");
        assert!(
            verdict.agreement_ratio >= APPROVAL_THRESHOLD,
            "runoff converged: {}",
            verdict.agreement_ratio
        );

        let calls = dispatcher.calls.lock().unwrap();
        // 4 seats × 2 ballots — the runoff re-polled everyone.
        assert_eq!(calls.len(), 8, "two full ballots dispatched: {calls:?}");
        // Ballot-1 calls carry no tally; ballot-2 calls carry the prior tally.
        assert!(calls
            .iter()
            .all(|(_, b, tally)| if *b == 1 { *tally == 0 } else { *tally > 0 }));
    }

    /// A unanimous (or above-bar) first ballot never triggers a runoff.
    struct UnanimousDispatcher;
    impl Dispatcher for UnanimousDispatcher {
        fn dispatch(&self, _cli: &AgenticCli, _task: &CouncilTask) -> Option<Vote> {
            unreachable!("deliberation must flow through dispatch_ballot");
        }
        fn dispatch_ballot(
            &self,
            cli: &AgenticCli,
            _task: &CouncilTask,
            ctx: &BallotContext,
        ) -> Option<Vote> {
            assert_eq!(ctx.ballot, 1, "no runoff should run above the bar");
            Some(vote(&cli.key, "2 — best"))
        }
    }

    #[test]
    fn above_bar_first_ballot_needs_no_runoff() {
        let worker = worker_with(Arc::new(UnanimousDispatcher), &["a", "b", "c"]);
        let id = worker.queue_blocking(task());
        let status = worker.poll(&id).expect("status");
        assert_eq!(status.state, TaskState::Voted);
        assert!(status.verdict.expect("verdict").agreement_ratio >= APPROVAL_THRESHOLD);
    }

    /// Deliberation terminates at MAX_BALLOTS even when the council never converges,
    /// and the final plurality verdict stands.
    struct NeverConvergesDispatcher {
        ballots_seen: Mutex<u32>,
    }
    impl Dispatcher for NeverConvergesDispatcher {
        fn dispatch(&self, _cli: &AgenticCli, _task: &CouncilTask) -> Option<Vote> {
            unreachable!("deliberation must flow through dispatch_ballot");
        }
        fn dispatch_ballot(
            &self,
            cli: &AgenticCli,
            _task: &CouncilTask,
            ctx: &BallotContext,
        ) -> Option<Vote> {
            let mut seen = self.ballots_seen.lock().unwrap();
            *seen = (*seen).max(ctx.ballot);
            // Everyone votes for themselves forever — permanent 25% fragmentation.
            let rec = match cli.key.as_str() {
                "a" => "1 — mine",
                "b" => "2 — mine",
                "c" => "3 — mine",
                _ => "4 — mine",
            };
            Some(vote(&cli.key, rec))
        }
    }

    #[test]
    fn never_converging_council_stops_at_max_ballots_with_plurality() {
        let dispatcher = Arc::new(NeverConvergesDispatcher {
            ballots_seen: Mutex::new(0),
        });
        let worker = worker_with(dispatcher.clone(), &["a", "b", "c", "d"]);
        let id = worker.queue_blocking(task());
        let status = worker.poll(&id).expect("status");

        assert_eq!(status.state, TaskState::Voted, "plurality still resolves");
        let verdict = status.verdict.expect("verdict");
        assert!(verdict.agreement_ratio < APPROVAL_THRESHOLD);
        assert!(verdict.winning_recommendation.is_some());
        assert_eq!(
            *dispatcher.ballots_seen.lock().unwrap(),
            MAX_BALLOTS,
            "deliberation capped at MAX_BALLOTS"
        );
    }

    /// Sleeps a fixed span per seat so wall clock distinguishes concurrent from sequential.
    struct SlowDispatcher {
        per_seat: std::time::Duration,
        peak_concurrent: Arc<Mutex<usize>>,
        in_flight: Arc<Mutex<usize>>,
    }
    impl Dispatcher for SlowDispatcher {
        fn dispatch(&self, cli: &AgenticCli, _task: &CouncilTask) -> Option<Vote> {
            {
                let mut n = self.in_flight.lock().unwrap();
                *n += 1;
                let mut peak = self.peak_concurrent.lock().unwrap();
                *peak = (*peak).max(*n);
            }
            std::thread::sleep(self.per_seat);
            *self.in_flight.lock().unwrap() -= 1;
            Some(vote(&cli.key, "2 — best"))
        }
    }

    #[test]
    fn a_ballot_costs_its_slowest_seat_not_the_sum_of_them() {
        // The council used to dispatch seats one after another, so a ballot cost the SUM of its
        // seats and a below-bar ballot multiplied that by the ballot count. On the shipped 3-seat
        // roster that was 4m30s to route 3 units, against a 30s budget.
        let per_seat = std::time::Duration::from_millis(300);
        let peak = Arc::new(Mutex::new(0usize));
        let dispatcher = Arc::new(SlowDispatcher {
            per_seat,
            peak_concurrent: peak.clone(),
            in_flight: Arc::new(Mutex::new(0)),
        });
        let worker = worker_with(dispatcher, &["a", "b", "c", "d"]);

        let started = Instant::now();
        let id = worker.queue_blocking(task());
        let elapsed = started.elapsed();

        assert_eq!(worker.poll(&id).expect("status").state, TaskState::Voted);
        assert_eq!(
            *peak.lock().unwrap(),
            4,
            "every seat on the ballot must be in flight at once"
        );
        // Sequential would be 4 x 300ms = 1.2s. Concurrent is one 300ms span plus overhead;
        // the ceiling sits between the two so neither a slow CI box nor a partial regression
        // (say, two-at-a-time) can pass.
        assert!(
            elapsed < per_seat * 3,
            "a ballot must cost its slowest seat, not the sum: {elapsed:?}"
        );
    }

    /// Panics on one named seat; every other seat votes.
    struct PanickingSeatDispatcher {
        victim: String,
    }
    impl Dispatcher for PanickingSeatDispatcher {
        fn dispatch(&self, cli: &AgenticCli, _task: &CouncilTask) -> Option<Vote> {
            assert!(cli.key != self.victim, "seat {} is a crash test", cli.key);
            Some(vote(&cli.key, "2 — best"))
        }
    }

    /// Records what the ranking store was actually told, so a test can assert on the signal
    /// rather than on the code path that produces it.
    #[derive(Default)]
    struct RecordingRank {
        seen: Mutex<Vec<(String, bool, u64)>>,
    }
    impl RankStore for RecordingRank {
        fn record(&self, cli: &str, _work_kind: &str, signal: &RankSignal) {
            self.seen
                .lock()
                .unwrap()
                .push((cli.to_string(), signal.success, signal.latency_ms));
        }
        fn best_for(&self, _work_kind: &str, _top: usize) -> Vec<Ranking> {
            Vec::new()
        }
    }

    /// Panics only AFTER burning measurable time, which is the case that matters: a seat that
    /// unwinds instantly is indistinguishable from one that was never charged for its work.
    struct SlowThenPanickingDispatcher {
        victim: String,
        work: Duration,
    }
    impl Dispatcher for SlowThenPanickingDispatcher {
        fn dispatch(&self, cli: &AgenticCli, _task: &CouncilTask) -> Option<Vote> {
            std::thread::sleep(self.work);
            assert!(cli.key != self.victim, "seat {} is a crash test", cli.key);
            Some(vote(&cli.key, "2 — best"))
        }
    }

    #[test]
    fn a_panicking_seat_is_still_charged_for_the_time_it_burned() {
        // A failed seat that reports zero latency is not merely missing data - the ranking signal
        // sums it, so the seat looks FASTER than every seat that succeeded. That is a lie in the
        // flattering direction, and it biases future routing toward the CLI that crashed.
        const WORK: Duration = Duration::from_millis(150);
        let rank = Arc::new(RecordingRank::default());
        let estate = EstateHandle::in_memory().expect("estate");
        let worker = Worker::new(
            Ledger::new(estate),
            Arc::new(SlowThenPanickingDispatcher {
                victim: "b".to_string(),
                work: WORK,
            }),
            rank.clone(),
            Arc::new(NoopEventSink),
            ["a", "b", "c"].iter().map(|k| cli(k)).collect(),
            "general",
        );
        let id = worker.queue_blocking(task());
        worker.poll(&id).expect("the council must still resolve");

        let seen = rank.seen.lock().unwrap().clone();
        let (_, success, latency_ms) = seen
            .iter()
            .find(|(k, _, _)| k == "b")
            .expect("the panicking seat must still be ranked: {seen:?}");
        assert!(!success, "the seat failed");
        assert!(
            *latency_ms >= WORK.as_millis() as u64,
            "a seat that worked for {WORK:?} before panicking must not be ranked as instant, \
             got {latency_ms}ms"
        );
    }

    #[test]
    fn a_panicking_seat_degrades_itself_not_the_council() {
        // Seats run on their own threads now. An unwinding seat that reached the council thread
        // would abort the whole distribution over one bad CLI - the opposite of what a quorum is
        // for - so it is caught and recorded as that seat's failure.
        let worker = worker_with(
            Arc::new(PanickingSeatDispatcher {
                victim: "b".to_string(),
            }),
            &["a", "b", "c"],
        );
        let id = worker.queue_blocking(task());
        let status = worker.poll(&id).expect("the council must still resolve");

        assert_eq!(status.state, TaskState::Voted);
        assert_eq!(status.returned, 2, "the surviving seats still voted");
        let failed: Vec<&str> = status
            .seat_failures
            .iter()
            .map(|f| f.cli.as_str())
            .collect();
        assert_eq!(failed, vec!["b"], "the panicking seat is named: {failed:?}");
        assert_eq!(
            status.seat_failures[0].failure.kind,
            SeatFailureKind::Panicked
        );
    }
}
