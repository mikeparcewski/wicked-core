//! The command channel into the store actor. Every command carries its own reply sender (a
//! oneshot, modeled with a `std::sync::mpsc` channel) so callers get a typed result back while all
//! store access stays serialized on the single actor thread.

use crate::domain::SessionStatus;
use crate::event::CoreEvent;
use crate::gate_hook::HookDrainSummary;
use crate::repo::{RepoEntry, RepoSpec};
use crate::workflow::{HumanDecision, StepOutput};
use std::path::PathBuf;
use std::sync::mpsc::Sender;

/// A request processed by the [`crate::actor`] store-owning thread. Internal — callers use the
/// typed methods on [`crate::Core`].
pub(crate) enum Command {
    /// Liveness probe: emit a `Heartbeat` to subscribers and ack.
    Ping(Sender<()>),
    /// Enumerate the agent session ids currently on the store (the read the UI needs first).
    Sessions(Sender<anyhow::Result<Vec<String>>>),
    /// Every session + its ordered units — what the UI builds its project list from.
    Projects(Sender<anyhow::Result<Vec<crate::SessionView>>>),
    /// A unit's captured work output (transcript), if any.
    WorkOutput(String, Sender<Option<String>>),
    /// Register a live event subscriber.
    Subscribe(Sender<CoreEvent>),
    /// Run a full governed session (fire-and-forget — progress + outcome arrive as `CoreEvent`s,
    /// including `CoreEvent::Error` on failure). Runs on the actor thread (the single writer).
    Launch(crate::LaunchSpec),
    /// Drain a run's out-of-process gate-hook decisions (`decisions.ndjson`) into the store. The
    /// actor is the ONLY writer of those claims (the hook itself never writes the store) — this is
    /// the single-writer reconciliation of the wrapped-CLI governance path.
    ApplyHookDecisions {
        run_id: String,
        ndjson_path: PathBuf,
        reply: Sender<anyhow::Result<HookDrainSummary>>,
    },
    /// Launch an INTERACTIVE, resumable run: plan + distribute on the actor (fast store writes), then
    /// execute each unit OFF-THREAD on the worker pool (so the actor stays responsive). Reply carries
    /// the run id, or a busy error if a run with that id is already in flight. (Contrast `Launch`,
    /// the legacy straight-through path that blocks the actor.)
    LaunchRun {
        spec: crate::LaunchSpec,
        reply: Sender<anyhow::Result<String>>,
    },
    /// Resume an interactive run from its persisted cursor (after a pause, crash, or fresh process).
    /// Re-dispatches the next not-yet-done unit. Busy error if the run is already in flight.
    ResumeRun {
        run_id: String,
        reply: Sender<anyhow::Result<SessionStatus>>,
    },
    /// Resolve a human-confirm gate on a paused run: `Approve` (optionally amending the next unit's
    /// instruction) resumes execution; `Reject` cancels the run. Errors if the run isn't paused.
    ConfirmGate {
        run_id: String,
        decision: HumanDecision,
        reply: Sender<anyhow::Result<SessionStatus>>,
    },
    /// Cancel a run — mark it terminally `Cancelled` and stop dispatching. (Real subprocess kill of an
    /// in-flight worker lands with the wrapped-CLI backend in P4a; for now the cursor stops advancing
    /// and any late worker result is ignored by the idempotency guard.)
    CancelRun {
        run_id: String,
        reply: Sender<anyhow::Result<SessionStatus>>,
    },
    /// Register a git repository the orchestrator can run within (validates it's a git repo with
    /// ≥1 commit). The single writer persists the `RepoEntry`.
    RegisterRepo {
        spec: RepoSpec,
        reply: Sender<anyhow::Result<RepoEntry>>,
    },
    /// List every registered repository.
    ListRepos {
        reply: Sender<anyhow::Result<Vec<RepoEntry>>>,
    },
    /// Register a deny policy (real governance) on the shared store — single-writer, through the
    /// actor (not a shelled binary). Blocks any tool-call in `phase` whose context contains `trigger`.
    RegisterDenyPolicy {
        phase: String,
        trigger: String,
        reply: Sender<anyhow::Result<()>>,
    },
    /// Capture an episodic memory (a learned fact/decision) at `scope` (e.g. `app:<id>`; "" = root).
    CaptureMemory {
        content: String,
        scope: String,
        reply: Sender<anyhow::Result<()>>,
    },
    /// Recall up to `k` memories relevant to `query` (hybrid recall, salience-reranked).
    RecallMemory {
        query: String,
        k: usize,
        reply: Sender<anyhow::Result<Vec<crate::memory::RecalledMemory>>>,
    },
    /// LIST memories within `scope`'s subtree (e.g. `app:<id>`; "" = all), newest first, up to `limit`.
    ListMemories {
        scope: String,
        limit: usize,
        reply: Sender<anyhow::Result<Vec<crate::memory::RecalledMemory>>>,
    },
    /// Dispatch an MCP JSON-RPC request to the in-process memory tool server (6 memory tools).
    McpCall {
        request: serde_json::Value,
        reply: Sender<anyhow::Result<Option<serde_json::Value>>>,
    },
    /// Ingest a document (title + chunks) into the knowledge base.
    IngestKnowledge {
        title: String,
        chunks: Vec<String>,
        reply: Sender<anyhow::Result<usize>>,
    },
    /// Recall up to `k` knowledge chunks relevant to `query`.
    RecallKnowledge {
        query: String,
        k: usize,
        reply: Sender<anyhow::Result<Vec<crate::knowledge::RecalledKnowledge>>>,
    },
    /// Internal: a worker posts the output of one unit step back to the actor (the single writer),
    /// which applies the governance gate + advances the cursor. Not called by external consumers.
    ApplyStepResult { output: StepOutput },
    /// Internal: a worker streams a live output chunk; the actor (the single emit point) fans it out
    /// as a [`CoreEvent::CliOutputDelta`]. Keeps the single-emit-point invariant while streaming.
    CliOutputDelta {
        run_id: String,
        ord: u32,
        chunk: String,
    },
    /// Stop the actor loop and release the store. Sent automatically when the LAST external `Core`
    /// handle drops (the actor holds its own `self_tx` for worker write-back, so channel-close alone
    /// can never terminate it — this is the real exit). In-flight workers' results are abandoned but
    /// the cursor is persisted, so a later `ResumeRun` continues the run.
    Shutdown,
}
