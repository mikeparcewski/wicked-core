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

use crate::store::{Ledger, TaskRecord};
use crate::synthesis;
use crate::types::{
    AgenticCli, CouncilTask, Dispatcher, EventSink, RankSignal, RankStore, TaskState, Verdict,
};

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

    // Dispatch each CLI in isolation, recording per-CLI latency for ranking. (Sequential
    // here; the isolation guarantees independence and the contract is "the agent isn't
    // resident", not "maximally parallel" — parallelism is a straightforward follow-up
    // since each dispatch is independent.)
    let mut votes = Vec::new();
    let mut latencies = Vec::new();
    for cli in roster {
        let started = Instant::now();
        let vote = dispatcher.dispatch(cli, task);
        let latency_ms = started.elapsed().as_millis() as u64;
        latencies.push((cli.key.clone(), vote.is_some(), latency_ms));
        if let Some(v) = vote {
            votes.push(v);
        }
    }

    // Persist collected votes.
    let collected = votes.clone();
    ledger.update(&task.id, |rec| rec.votes = collected);

    // No votes at all (every seat timed out / errored) → timed_out.
    if votes.is_empty() {
        ledger.update(&task.id, |rec| rec.state = TaskState::TimedOut);
        return;
    }

    // Synthesize the verdict (layer c).
    let verdict = synthesis::synthesize(&task.id, &votes);
    let winning = verdict.winning_recommendation.clone();

    // Record per-CLI ranking signals: did the seat succeed, and did it agree with the
    // eventual consensus winner?
    for (cli_key, success, latency_ms) in &latencies {
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
                latency_ms: *latency_ms,
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
            "updated": latencies.iter().map(|(k, _, _)| k).collect::<Vec<_>>(),
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
