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
use std::time::Instant;

use crate::store::{Ledger, SeatFailureRecord, TaskRecord};
use crate::synthesis;
use crate::types::{
    AgenticCli, BallotContext, CouncilTask, DispatchOutcome, Dispatcher, EventSink, RankSignal,
    RankStore, TaskState, Verdict, SEATS,
};

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
    dispatcher: &dyn Dispatcher,
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
    // Per-CLI ranking signal accumulated ACROSS ballots: total latency summed over every
    // dispatch (a 3-ballot deliberation costs 3 dispatches and is reported as such), and
    // success from the seat's final ballot (the one the verdict is synthesized from).
    let mut signal_acc: std::collections::BTreeMap<String, (bool, u64)> =
        std::collections::BTreeMap::new();
    let (votes, verdict) = loop {
        // Dispatch each CLI in isolation, recording per-CLI latency for ranking.
        // (Sequential; isolation guarantees independence — parallelism is a follow-up.)
        let mut votes = Vec::new();
        let mut failures: Vec<SeatFailureRecord> = Vec::new();
        for (i, cli) in roster.iter().enumerate() {
            let ctx = BallotContext {
                seat: Some(SEATS[i % SEATS.len()].clone()),
                ballot,
                approval_threshold: APPROVAL_THRESHOLD,
                prior_tally: prior_tally.clone(),
                dissent_arguments: dissent_arguments.clone(),
            };
            let started = Instant::now();
            let outcome = dispatcher.dispatch_ballot_detailed(cli, task, &ctx);
            let latency_ms = started.elapsed().as_millis() as u64;
            let entry = signal_acc.entry(cli.key.clone()).or_insert((false, 0));
            entry.0 = outcome.is_voted();
            entry.1 += latency_ms;
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
                            "latency_ms": latency_ms,
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
    use crate::types::{Confidence, EventSink, NoopEventSink, RankStore, Ranking, Vote};
    use crate::EstateHandle;
    use std::sync::Mutex;

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
}
