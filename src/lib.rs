//! wicked-core — the in-process composition runtime for the wicked-estate core services.
//!
//! One thread (the [`actor`]) owns the writable estate store; everything else holds a clonable
//! [`Core`] handle and talks to it via commands + a live event stream. This separates the
//! system-of-record (SQLite, single writer) from the orchestration seam (a command API + events),
//! so consumers (agent, UI, MCP) stop re-opening and racing on the shared file. See `DESIGN.md`.
//!
//! Built: the actor + command/reply + event fan-out, the full plan → distribute → execute →
//! evidence pipeline ([`Core::launch`], stub execute path), and the read API
//! ([`Core::sessions_detail`], [`Core::work_output`]). Remaining (see `DESIGN.md`): the wrapped-CLI
//! execute backend (real subprocess + gate-hook), migrating the GUI onto `Core`, and deleting the
//! `wicked-agent` crate.

mod actor;
mod applications;
mod bus;
mod campaign;
mod cli_runner;
mod code_graph;
mod command;
mod distribute;
mod docs;
mod domain;
mod domain_extraction;
mod event;
mod execute;
mod execute_wrapped;
mod gate_hook;
mod graph_browser;
mod knowledge;
mod memory;
mod pipeline;
mod plan;
mod repo;
mod repo_intel;
mod scope;
mod session_runner;
mod sources;
mod terminal;
mod validator;
mod validator_vault;
mod workflow;

pub use actor::{RunBusy, RunExists};
pub use applications::{
    attach_doc, attach_repo, create_app, delete_app, get_app, list_apps, AppDoc, AppRepo,
    Application, SeedKind,
};
pub use bus::{
    deterministic_key, matches_filter, BusBridge, BusDb, BusEmit, BusEvent, CORE_DOMAIN,
    RUN_LAUNCHED, RUN_REQUESTED,
};
pub use campaign::{
    all_campaigns, blocked_by_failure, get_campaign, ready_set, satisfied,
    validate as validate_campaign, Campaign, CampaignDef, CampaignEdge, CampaignGateDecision,
    CampaignNode, CampaignStatus, EdgeCondition, FailurePolicy, NodeStatus, RunSpec,
};
pub use cli_runner::{TASK_COMPLETED, TASK_DISPATCHED};
pub use code_graph::{rank_symbols, recon_repo, RankedSymbol};
pub use docs::{list_docs, new_doc, read_doc, write_doc, DocMeta};
pub use domain::{
    all_sessions, get_session, get_work_output, put_node, session_units, AgentSession,
    HumanConfirm, RoutingInfo, SessionStatus, SessionView, StageKind, UnitStatus, WorkUnit,
};
pub use domain_extraction::{
    coverage_eq_one_validator, provision_and_approve_coverage_validator, COVERAGE_CRITERION,
    COVERAGE_SCRIPT, COVERAGE_VALIDATOR_PIN, DOMAIN_EXTRACTION_WORKFLOW_ID,
};
pub use event::CoreEvent;
pub use execute_wrapped::WrappedCliStepRunner;
pub use session_runner::PersistentStepRunner;
pub use gate_hook::{
    count_claims, decisions_path_for, gov_run_dir, run_gate_hook, run_output_gate_hook,
    HookDrainSummary, DECISIONS_PATH_ENV, ESTATE_DB_ENV, GATE_PHASE_ENV, GATE_SCOPE_ENV,
};
pub use graph_browser::{
    browse_nodes, graph_kinds, list_node_notes, node_detail, NeighborEdge, NodeDetail, NodeNote,
    NodeSummary, SymbolAnnotation,
};
pub use knowledge::RecalledKnowledge;
pub use memory::{now_secs, RecalledMemory};
pub use pipeline::SessionResult;
pub use plan::plan_from_def;
pub use repo::{RepoEntry, RepoSpec};
pub use repo_intel::{
    change_digest_since, commits_since, profile_repo, Commit, GraphStats, Hotspot, RepoProfile,
};
pub use scope::{resolve_scope, EntityMode};
pub use sources::{add_node_note, add_source, base_dir, enrich_source, index_docs, ReconDoc};
pub use validator::{
    agent_validate, author_deterministic_validator, combine_verdict, gate_phase, run_validator,
    run_validator_reporting, sandbox_availability, AgentVerdict, DeterministicValidator,
    GateVerdict, SandboxLevel, DETERMINISTIC_VALIDATOR_SEAT,
};
pub use validator_vault::{
    approve_and_store, load_validator, pin, provision_validator, store_validator, VALIDATOR_VAULT,
};
pub use wicked_council::AgenticCli;
pub use workflow::{
    bug_def, feature_def, migration_def, GateCond, GateSpec, GateType, HumanDecision, PhaseDef,
    PhaseRole, StepInput, StepOutput, StepRunner, StepStatus, StubStepRunner, Usage, WorkflowDef,
    WorkflowDefError, WorkflowRegistry,
};

/// What to run: the problem to decompose, the council roster (`AgenticCli` seats), the scope toggle,
/// and a stable session id. The roster is passed explicitly so callers (tests, UI) control it; the
/// UI resolves it from the council registry.
pub struct LaunchSpec {
    pub problem: String,
    pub clis: Vec<AgenticCli>,
    pub entity_mode: EntityMode,
    pub session_id: String,
    /// The human-confirm gate policy: pause before none / every / a specific unit. Defaults to
    /// `None` (run straight through) when built without it.
    pub human_confirm: HumanConfirm,
    /// The id of a registered repo to run within (P3). When set, COE creates an isolated git
    /// worktree for the run and executes there; `None` runs without a repo (no worktree).
    pub repo_ref: Option<String>,
    /// The registered `WorkflowDef` id to run (`feature`/`bug`/`migration` or a drop-in). When set,
    /// planning is DATA-DRIVEN: units come from the def's phases (stage from the phase's declared
    /// `kind`) via [`crate::plan_from_def`]. `None` ⇒ the legacy free-text planner (prose split +
    /// keyword classify), so existing callers are unchanged.
    pub workflow: Option<String>,
}

/// Resolve the council roster from the registry (built-ins merged with the user's
/// `~/.config/wicked-council/clis.toml`), keeping only council-enabled seats. This is what a
/// consumer passes as [`LaunchSpec::clis`] for a real run.
pub fn registry_roster() -> Vec<AgenticCli> {
    let user = std::env::var_os("HOME")
        .map(|h| std::path::PathBuf::from(h).join(".config/wicked-council/clis.toml"));
    wicked_council::registry::load(user.as_deref())
        .unwrap_or_default()
        .into_iter()
        .filter(|c| c.enabled_for_council)
        .collect()
}

use command::Command;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;

/// Sends `Shutdown` when the LAST `Core` handle drops. The actor holds its own `self_tx` (so workers
/// can post results back), which means the command channel never closes on its own — this guard is
/// the real termination signal: when every external `Core` clone is gone, the shared `Arc` drops,
/// this fires `Shutdown`, the actor breaks its loop, and the store handle + thread are released.
struct ShutdownGuard {
    tx: Sender<Command>,
}
impl Drop for ShutdownGuard {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Shutdown);
    }
}

/// A handle to the core runtime. Clone freely — every clone funnels into the single store-owning
/// actor thread, so callers compose the core services without contending on the SQLite file. When
/// the last clone drops, the actor shuts down (see [`ShutdownGuard`]).
#[derive(Clone)]
pub struct Core {
    tx: Sender<Command>,
    /// The off-actor PTY writer/master/child map (DES-TERMINAL-001 §4). `write_terminal` /
    /// `resize_terminal` act on this DIRECTLY — no store round-trip — so keystroke I/O never queues
    /// behind the single store-writer actor. Shared (cloned) with the actor, which owns open/close.
    pty: terminal::PtyMap,
    _shutdown: Arc<ShutdownGuard>,
}

impl Core {
    /// Spawn the store actor over the estate store at `path`, with the production engine seams: the
    /// real council dispatcher + the real wrapped-CLI step runner (runs actual agentic CLIs in the
    /// run's worktree). The actor lives until every `Core` handle is dropped. Tests use
    /// [`Core::spawn_with_engine`] to inject a stub runner instead.
    pub fn spawn(path: impl Into<String>) -> Core {
        Core::spawn_with_engine(
            path,
            distribute::real_dispatcher(),
            std::sync::Arc::new(execute_wrapped::WrappedCliStepRunner::default()),
        )
    }

    /// Spawn the store actor with INJECTED engine seams — the council `dispatcher` (vote collection)
    /// and the `runner` (per-unit slow work). Tests inject a stub dispatcher + a controllable step
    /// runner to exercise the interactive engine without real subprocesses; `spawn` wires the
    /// production defaults.
    pub fn spawn_with_engine(
        path: impl Into<String>,
        dispatcher: std::sync::Arc<dyn wicked_council::types::Dispatcher + Send + Sync>,
        runner: std::sync::Arc<dyn StepRunner>,
    ) -> Core {
        Core::spawn_inner(path, dispatcher, runner, None)
    }

    /// Spawn with the Law 1 EXECUTION-MEDIATION SEAM (DES-EXEC-001 §2.3) turned ON EXPLICITLY against the
    /// bus db at `bus_db_path` — the actor publishes `wicked.task.dispatched` for a `cli-runner`
    /// subscriber instead of dispatching units in-process, and consumes `wicked.task.completed` back. This
    /// is the env-free entry (no `WICKED_BUS_EXEC` global) so a test can prove the round-trip without
    /// racing other tests on process env. Production opts in via `WICKED_BUS_EXEC` + `WICKED_BUS_DB`
    /// (read by [`spawn_with_engine`]).
    pub fn spawn_with_engine_exec(
        path: impl Into<String>,
        dispatcher: std::sync::Arc<dyn wicked_council::types::Dispatcher + Send + Sync>,
        runner: std::sync::Arc<dyn StepRunner>,
        bus_db_path: impl Into<String>,
    ) -> Core {
        Core::spawn_inner(path, dispatcher, runner, Some(bus_db_path.into()))
    }

    /// Spawn the store actor with a [`PersistentStepRunner`] as the execution seam — units within
    /// the same run share a single live PTY session (no per-unit cold-start). Uses the real council
    /// dispatcher. The returned `Core` also exposes a [`PersistentStepRunner`] handle so the caller
    /// can call [`PersistentStepRunner::drop_session`] after each run completes.
    pub fn spawn_with_pty_sessions(path: impl Into<String>) -> (Core, std::sync::Arc<PersistentStepRunner>) {
        let path = path.into();
        let (tx, rx) = channel();
        let self_tx = tx.clone();
        let pty = terminal::new_map();
        let pty_actor = pty.clone();
        let runner = std::sync::Arc::new(session_runner::PersistentStepRunner::new(
            tx.clone(),
            pty.clone(),
        ));
        let runner_actor = runner.clone();
        std::thread::spawn(move || {
            actor::run(
                path,
                rx,
                self_tx,
                distribute::real_dispatcher(),
                runner_actor,
                pty_actor,
                None,
            )
        });
        let core = Core {
            tx: tx.clone(),
            pty,
            _shutdown: Arc::new(ShutdownGuard { tx }),
        };
        (core, runner)
    }

    fn spawn_inner(
        path: impl Into<String>,
        dispatcher: std::sync::Arc<dyn wicked_council::types::Dispatcher + Send + Sync>,
        runner: std::sync::Arc<dyn StepRunner>,
        exec_bus: Option<String>,
    ) -> Core {
        let (tx, rx) = channel();
        let path = path.into();
        let self_tx = tx.clone();
        // The off-actor PTY I/O map: one clone drives write/resize from `Core`, one is owned by the
        // actor for open/close/shutdown. Both reach the same sessions behind its mutex.
        let pty = terminal::new_map();
        let pty_actor = pty.clone();
        std::thread::spawn(move || {
            actor::run(path, rx, self_tx, dispatcher, runner, pty_actor, exec_bus)
        });
        Core {
            tx: tx.clone(),
            pty,
            _shutdown: Arc::new(ShutdownGuard { tx }),
        }
    }

    /// Subscribe to the live event stream. Returns a receiver that gets every [`CoreEvent`] emitted
    /// after this call (the UI watches work happen instead of polling).
    pub fn subscribe(&self) -> Receiver<CoreEvent> {
        let (s, r) = channel();
        let _ = self.tx.send(Command::Subscribe(s));
        r
    }

    /// Connect this Core to a wicked-bus event log (DES-EXEC-001 §2.5): spawn the launch bridge — a
    /// dedicated poller thread that turns each `wicked.run.requested {workflow, problem, args}` on the
    /// bus into a `LaunchRun` on this actor, and emits `wicked.run.launched` back onto the bus when a
    /// run starts. `roster` is the council seats a launched run runs with (a caller passes
    /// [`registry_roster`] in production). The returned [`BusBridge`] owns the thread — drop it (or
    /// call [`BusBridge::stop`]) to stop polling. The poller runs entirely off the actor thread with
    /// its own SQLite connection to the bus db, reaching the actor only via commands (actor-safe).
    pub fn connect_bus(
        &self,
        bus_db_path: impl Into<String>,
        roster: Vec<AgenticCli>,
    ) -> BusBridge {
        bus::connect(
            self.tx.clone(),
            bus_db_path,
            roster,
            EntityMode::Shared,
            std::time::Duration::from_millis(200),
        )
    }

    /// Liveness probe — emits a `Heartbeat` to subscribers and waits for the actor to ack.
    pub fn ping(&self) {
        let (reply, rx) = channel();
        if self.tx.send(Command::Ping(reply)).is_ok() {
            let _ = rx.recv();
        }
    }

    /// Launch a full governed session. Fire-and-forget: returns the session id immediately while the
    /// run proceeds on the actor thread, streaming progress (and any failure) as [`CoreEvent`]s.
    /// `subscribe()` BEFORE calling this to catch the whole sequence.
    pub fn launch(&self, mut spec: LaunchSpec) -> String {
        if spec.session_id.trim().is_empty() {
            spec.session_id = format!(
                "sess-{}",
                pipeline::deterministic_id(&[&spec.problem, &spec.clis.len().to_string()])
            );
        }
        let session_id = spec.session_id.clone();
        let _ = self.tx.send(Command::Launch(spec));
        session_id
    }

    /// Launch an INTERACTIVE, resumable run. Plans + distributes on the actor, then executes each
    /// unit off-thread (the actor stays responsive). Returns the run id, or a [`RunBusy`] error if a
    /// run with that id is already in flight. Progress arrives as [`CoreEvent`]s — `subscribe()`
    /// first to catch the whole sequence.
    pub fn launch_run(&self, spec: LaunchSpec) -> anyhow::Result<String> {
        let (reply, rx) = channel();
        self.tx
            .send(Command::LaunchRun { spec, reply })
            .map_err(|_| anyhow::anyhow!("core actor stopped"))?;
        rx.recv()
            .map_err(|_| anyhow::anyhow!("core actor dropped the reply"))?
    }

    /// Resume an interactive run from its persisted cursor (after a pause, crash, or a fresh
    /// process). Re-dispatches the next not-yet-done unit. Returns the resulting status, or a
    /// [`RunBusy`] error if the run is already in flight.
    pub fn resume_run(&self, run_id: &str) -> anyhow::Result<SessionStatus> {
        let (reply, rx) = channel();
        self.tx
            .send(Command::ResumeRun {
                run_id: run_id.to_string(),
                reply,
            })
            .map_err(|_| anyhow::anyhow!("core actor stopped"))?;
        rx.recv()
            .map_err(|_| anyhow::anyhow!("core actor dropped the reply"))?
    }

    /// Resolve a human-confirm gate on a PAUSED run: [`HumanDecision::Approve`] (optionally amending
    /// the next unit's instruction) resumes execution; [`HumanDecision::Reject`] cancels the run.
    /// Errors if the run is not currently paused at a gate.
    pub fn confirm_gate(
        &self,
        run_id: &str,
        decision: HumanDecision,
    ) -> anyhow::Result<SessionStatus> {
        let (reply, rx) = channel();
        self.tx
            .send(Command::ConfirmGate {
                run_id: run_id.to_string(),
                decision,
                reply,
            })
            .map_err(|_| anyhow::anyhow!("core actor stopped"))?;
        rx.recv()
            .map_err(|_| anyhow::anyhow!("core actor dropped the reply"))?
    }

    /// Cancel a run — mark it terminally `Cancelled` and stop advancing it. Safe to call whether the
    /// run is executing or paused.
    pub fn cancel_run(&self, run_id: &str) -> anyhow::Result<SessionStatus> {
        let (reply, rx) = channel();
        self.tx
            .send(Command::CancelRun {
                run_id: run_id.to_string(),
                reply,
            })
            .map_err(|_| anyhow::anyhow!("core actor stopped"))?;
        rx.recv()
            .map_err(|_| anyhow::anyhow!("core actor dropped the reply"))?
    }

    /// Register a git repository the orchestrator can run within. Validates it is a git repo with at
    /// least one commit; returns the persisted [`RepoEntry`] (with its resolved id + default branch).
    pub fn register_repo(&self, spec: RepoSpec) -> anyhow::Result<RepoEntry> {
        let (reply, rx) = channel();
        self.tx
            .send(Command::RegisterRepo { spec, reply })
            .map_err(|_| anyhow::anyhow!("core actor stopped"))?;
        rx.recv()
            .map_err(|_| anyhow::anyhow!("core actor dropped the reply"))?
    }

    /// List every registered repository.
    pub fn list_repos(&self) -> anyhow::Result<Vec<RepoEntry>> {
        let (reply, rx) = channel();
        self.tx
            .send(Command::ListRepos { reply })
            .map_err(|_| anyhow::anyhow!("core actor stopped"))?;
        rx.recv()
            .map_err(|_| anyhow::anyhow!("core actor dropped the reply"))?
    }

    // ── Campaign DAG scheduler (DES-CAMPAIGN-001) ────────────────────────────────

    /// Validate + launch a [`CampaignDef`] — a DAG of Runs. Independent nodes dispatch immediately;
    /// a dependent node dispatches the instant its deps reach their completion condition, bounded by
    /// `max_concurrency`. Fire-and-forget: returns the campaign id; progress arrives as `Campaign*`
    /// [`CoreEvent`]s (`subscribe()` first). Rejects a cycle / empty / duplicate-edge def at launch.
    pub fn launch_campaign(&self, def: CampaignDef) -> anyhow::Result<String> {
        let (reply, rx) = channel();
        self.tx
            .send(Command::LaunchCampaign { def, reply })
            .map_err(|_| anyhow::anyhow!("core actor stopped"))?;
        rx.recv()
            .map_err(|_| anyhow::anyhow!("core actor dropped the reply"))?
    }

    /// Resume a campaign from its persisted state (after a pause, crash, or a fresh process) — the
    /// scheduler re-derives the ready set from the persisted terminal statuses and re-attaches any
    /// mid-run node, never re-running a completed node or duplicating.
    pub fn resume_campaign(&self, id: &str) -> anyhow::Result<CampaignStatus> {
        let (reply, rx) = channel();
        self.tx
            .send(Command::ResumeCampaign {
                id: id.to_string(),
                reply,
            })
            .map_err(|_| anyhow::anyhow!("core actor stopped"))?;
        rx.recv()
            .map_err(|_| anyhow::anyhow!("core actor dropped the reply"))?
    }

    /// Cancel a campaign — cancel every in-flight node's Run and mark the rest `Cancelled`.
    pub fn cancel_campaign(&self, id: &str) -> anyhow::Result<CampaignStatus> {
        let (reply, rx) = channel();
        self.tx
            .send(Command::CancelCampaign {
                id: id.to_string(),
                reply,
            })
            .map_err(|_| anyhow::anyhow!("core actor stopped"))?;
        rx.recv()
            .map_err(|_| anyhow::anyhow!("core actor dropped the reply"))?
    }

    /// Pause a campaign — dispatch no new nodes; in-flight nodes continue cooperatively.
    /// `resume_campaign` re-enables dispatch.
    pub fn pause_campaign(&self, id: &str) -> anyhow::Result<CampaignStatus> {
        let (reply, rx) = channel();
        self.tx
            .send(Command::PauseCampaign {
                id: id.to_string(),
                reply,
            })
            .map_err(|_| anyhow::anyhow!("core actor stopped"))?;
        rx.recv()
            .map_err(|_| anyhow::anyhow!("core actor dropped the reply"))?
    }

    /// Resolve a campaign gate. A per-node HITL gate uses [`CampaignGateDecision::Approve`] /
    /// [`CampaignGateDecision::Reject`] (the node is `AwaitingHuman`); the `HumanGateOnFailure` policy
    /// gate uses `Retry` / `Skip` / `Abort` (the node `Failed` and is queued).
    pub fn confirm_campaign_gate(
        &self,
        id: &str,
        node_id: &str,
        decision: CampaignGateDecision,
    ) -> anyhow::Result<CampaignStatus> {
        let (reply, rx) = channel();
        self.tx
            .send(Command::ConfirmCampaignGate {
                id: id.to_string(),
                node_id: node_id.to_string(),
                decision,
                reply,
            })
            .map_err(|_| anyhow::anyhow!("core actor stopped"))?;
        rx.recv()
            .map_err(|_| anyhow::anyhow!("core actor dropped the reply"))?
    }

    /// A campaign's lifecycle status (`None` if the id is unknown).
    pub fn campaign_status(&self, id: &str) -> anyhow::Result<Option<CampaignStatus>> {
        let (reply, rx) = channel();
        self.tx
            .send(Command::CampaignStatusQuery {
                id: id.to_string(),
                reply,
            })
            .map_err(|_| anyhow::anyhow!("core actor stopped"))?;
        rx.recv()
            .map_err(|_| anyhow::anyhow!("core actor dropped the reply"))?
    }

    /// A campaign's full state (DAG + per-node statuses + run ids) — the read a DAG view builds from.
    pub fn campaign_detail(&self, id: &str) -> anyhow::Result<Option<Campaign>> {
        let (reply, rx) = channel();
        self.tx
            .send(Command::CampaignDetailQuery {
                id: id.to_string(),
                reply,
            })
            .map_err(|_| anyhow::anyhow!("core actor stopped"))?;
        rx.recv()
            .map_err(|_| anyhow::anyhow!("core actor dropped the reply"))?
    }

    /// Register a deny policy (real governance) through the actor — blocks any tool-call in `phase`
    /// whose context contains `trigger` (literal). Single-writer; persists on the shared store.
    pub fn register_deny_policy(&self, phase: &str, trigger: &str) -> anyhow::Result<()> {
        let (reply, rx) = channel();
        self.tx
            .send(Command::RegisterDenyPolicy {
                phase: phase.to_string(),
                trigger: trigger.to_string(),
                reply,
            })
            .map_err(|_| anyhow::anyhow!("core actor stopped"))?;
        rx.recv()
            .map_err(|_| anyhow::anyhow!("core actor dropped the reply"))?
    }

    /// Capture an episodic memory (a learned fact/decision) into the orchestrator's memory store.
    pub fn capture_memory(&self, content: &str, scope: &str) -> anyhow::Result<()> {
        let (reply, rx) = channel();
        self.tx
            .send(Command::CaptureMemory {
                content: content.to_string(),
                scope: scope.to_string(),
                reply,
            })
            .map_err(|_| anyhow::anyhow!("core actor stopped"))?;
        rx.recv()
            .map_err(|_| anyhow::anyhow!("core actor dropped the reply"))?
    }

    /// Recall up to `k` memories relevant to `query` (hybrid recall, salience-reranked). Returns an
    /// empty vec if the memory store is unavailable.
    pub fn recall_memories(&self, query: &str, k: usize) -> anyhow::Result<Vec<RecalledMemory>> {
        let (reply, rx) = channel();
        self.tx
            .send(Command::RecallMemory {
                query: query.to_string(),
                k,
                reply,
            })
            .map_err(|_| anyhow::anyhow!("core actor stopped"))?;
        rx.recv()
            .map_err(|_| anyhow::anyhow!("core actor dropped the reply"))?
    }

    /// LIST captured memories (newest first), up to `limit` — a direct listing, not a similarity
    /// search. The Memory surface uses this so stored memories always appear.
    pub fn list_memories(&self, scope: &str, limit: usize) -> anyhow::Result<Vec<RecalledMemory>> {
        let (reply, rx) = channel();
        self.tx
            .send(Command::ListMemories {
                scope: scope.to_string(),
                limit,
                reply,
            })
            .map_err(|_| anyhow::anyhow!("core actor stopped"))?;
        rx.recv()
            .map_err(|_| anyhow::anyhow!("core actor dropped the reply"))?
    }

    /// Dispatch an MCP JSON-RPC request to the in-process memory tool server (the 6 `memory.*` tools).
    /// Returns the JSON-RPC response (`None` for a notification). This is the MCP tool surface other
    /// agents / surfaces call to use the orchestrator's memory.
    pub fn mcp_call(
        &self,
        request: serde_json::Value,
    ) -> anyhow::Result<Option<serde_json::Value>> {
        let (reply, rx) = channel();
        self.tx
            .send(Command::McpCall { request, reply })
            .map_err(|_| anyhow::anyhow!("core actor stopped"))?;
        rx.recv()
            .map_err(|_| anyhow::anyhow!("core actor dropped the reply"))?
    }

    /// Ingest a document (title + chunks) into the orchestrator's knowledge base. Returns the chunk
    /// count.
    pub fn ingest_knowledge(&self, title: &str, chunks: Vec<String>) -> anyhow::Result<usize> {
        let (reply, rx) = channel();
        self.tx
            .send(Command::IngestKnowledge {
                title: title.to_string(),
                chunks,
                reply,
            })
            .map_err(|_| anyhow::anyhow!("core actor stopped"))?;
        rx.recv()
            .map_err(|_| anyhow::anyhow!("core actor dropped the reply"))?
    }

    /// Recall up to `k` knowledge chunks relevant to `query` (empty if the store is unavailable).
    pub fn recall_knowledge(
        &self,
        query: &str,
        k: usize,
    ) -> anyhow::Result<Vec<RecalledKnowledge>> {
        let (reply, rx) = channel();
        self.tx
            .send(Command::RecallKnowledge {
                query: query.to_string(),
                k,
                reply,
            })
            .map_err(|_| anyhow::anyhow!("core actor stopped"))?;
        rx.recv()
            .map_err(|_| anyhow::anyhow!("core actor dropped the reply"))?
    }

    /// The agent session ids currently on the store (lightweight; use [`sessions_detail`] for the
    /// full project list).
    pub fn sessions(&self) -> anyhow::Result<Vec<String>> {
        let (reply, rx) = channel();
        self.tx
            .send(Command::Sessions(reply))
            .map_err(|_| anyhow::anyhow!("core actor stopped"))?;
        rx.recv()
            .map_err(|_| anyhow::anyhow!("core actor dropped the reply"))?
    }

    /// Every session + its ordered units — the read the UI builds its project list from.
    pub fn sessions_detail(&self) -> anyhow::Result<Vec<SessionView>> {
        let (reply, rx) = channel();
        self.tx
            .send(Command::Projects(reply))
            .map_err(|_| anyhow::anyhow!("core actor stopped"))?;
        rx.recv()
            .map_err(|_| anyhow::anyhow!("core actor dropped the reply"))?
    }

    /// A unit's captured work output (the transcript), if any.
    pub fn work_output(&self, unit_id: &str) -> Option<String> {
        let (reply, rx) = channel();
        self.tx
            .send(Command::WorkOutput(unit_id.to_string(), reply))
            .ok()?;
        rx.recv().ok().flatten()
    }

    /// Drain a run's out-of-process gate-hook decisions (`decisions.ndjson`) into the store. The
    /// out-of-process hook only appended to the file; this is the single point where those claims
    /// are written to the store (single-writer). Idempotent — safe to call repeatedly. Returns a
    /// summary of what was applied this pass.
    pub fn apply_hook_decisions(
        &self,
        run_id: &str,
        ndjson_path: impl Into<std::path::PathBuf>,
    ) -> anyhow::Result<HookDrainSummary> {
        let (reply, rx) = channel();
        self.tx
            .send(Command::ApplyHookDecisions {
                run_id: run_id.to_string(),
                ndjson_path: ndjson_path.into(),
                reply,
            })
            .map_err(|_| anyhow::anyhow!("core actor stopped"))?;
        rx.recv()
            .map_err(|_| anyhow::anyhow!("core actor dropped the reply"))?
    }

    // ── PTY terminal sessions (DES-TERMINAL-001) ─────────────────────────────────────────────────

    /// Open a PTY terminal session running `cmd` (or the login shell if `None`) in `cwd`, sized
    /// `cols`x`rows`. Registry state is written on the actor (single writer); the byte-I/O runs
    /// off-actor. `governed=false` is a loud, opt-in ungoverned operator shell (bypasses the
    /// gate-hook — DES §7); default to `true`. Returns the new terminal id. Output arrives as
    /// [`CoreEvent::TerminalOutput`]; `subscribe()` BEFORE calling to catch `TerminalOpened` + bytes.
    pub fn open_terminal(
        &self,
        cwd: impl Into<std::path::PathBuf>,
        cmd: Option<Vec<String>>,
        cols: u16,
        rows: u16,
        governed: bool,
    ) -> anyhow::Result<String> {
        let (reply, rx) = channel();
        self.tx
            .send(Command::OpenTerminal {
                cwd: cwd.into(),
                cmd,
                cols,
                rows,
                governed,
                reply,
            })
            .map_err(|_| anyhow::anyhow!("core actor stopped"))?;
        rx.recv()
            .map_err(|_| anyhow::anyhow!("core actor dropped the reply"))?
    }

    /// Write raw input bytes (keystrokes) to a terminal. Acts on the off-actor PTY writer map
    /// DIRECTLY (no store round-trip, DES §4), so high-frequency input never queues behind the store
    /// writer. Fire-and-forget in spirit; errors only if the terminal id is unknown / the write fails.
    ///
    /// SIG-2: the shared map lock is held ONLY long enough to clone out this session's per-session
    /// writer `Arc`; the (possibly blocking) `write_all`+`flush` then runs under the PER-SESSION
    /// writer lock. So a stuck write on a child that isn't draining its stdin holds only THIS
    /// terminal's writer lock — it can NEVER stall close/open/resize or I/O on OTHER terminals.
    pub fn write_terminal(&self, id: &str, bytes: &[u8]) -> anyhow::Result<()> {
        use std::io::Write;
        let writer = {
            let map = terminal::lock(&self.pty);
            let s = map
                .get(id)
                .ok_or_else(|| anyhow::anyhow!("no such terminal: {id}"))?;
            s.writer.clone() // clone the Arc, then release the map lock below
        };
        let mut w = writer.lock().unwrap_or_else(|p| p.into_inner());
        w.write_all(bytes)?;
        w.flush()?;
        Ok(())
    }

    /// Resize a terminal's PTY to `cols`x`rows`. Acts on the off-actor master map DIRECTLY (no store
    /// round-trip, DES §4). Errors only if the terminal id is unknown / the resize fails.
    ///
    /// SIG-2: like `write_terminal`, holds the shared map lock only to clone out the per-session
    /// master `Arc`, then resizes under the per-session lock — never across the map lock.
    pub fn resize_terminal(&self, id: &str, cols: u16, rows: u16) -> anyhow::Result<()> {
        let master = {
            let map = terminal::lock(&self.pty);
            let s = map
                .get(id)
                .ok_or_else(|| anyhow::anyhow!("no such terminal: {id}"))?;
            s.master.clone() // clone the Arc, then release the map lock below
        };
        let m = master.lock().unwrap_or_else(|p| p.into_inner());
        m.resize(portable_pty::PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| anyhow::anyhow!("resize failed: {e}"))?;
        Ok(())
    }

    /// Close a terminal: the actor kills the child, joins the reader thread, and drops the registry +
    /// I/O entries (no orphaned process/thread — DES §5, R1). Blocks until teardown completes; a
    /// [`CoreEvent::TerminalExited`] is emitted.
    pub fn close_terminal(&self, id: &str) -> anyhow::Result<()> {
        let (reply, rx) = channel();
        self.tx
            .send(Command::CloseTerminal {
                id: id.to_string(),
                reply,
            })
            .map_err(|_| anyhow::anyhow!("core actor stopped"))?;
        rx.recv()
            .map_err(|_| anyhow::anyhow!("core actor dropped the reply"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    // Proves the P1 pattern end to end: one actor owns the store, serves a read, and fans events
    // out to subscribers — all in-process, no file polling, no second writer.
    #[test]
    fn actor_owns_store_serves_reads_and_emits_events() {
        let dir = std::env::temp_dir().join("wicked-core-test");
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("core-test.db");
        let _ = std::fs::remove_file(&db);

        let core = Core::spawn(db.display().to_string());
        let events = core.subscribe();

        // Read path: a fresh store has no agent sessions, and the read succeeds (actor owns it).
        let sessions = core.sessions().expect("sessions read should succeed");
        assert!(sessions.is_empty(), "a fresh store has no sessions");

        // Event path: ping emits a Heartbeat to the subscriber registered above (FIFO ordering on
        // the command channel guarantees Subscribe was processed before Ping).
        core.ping();
        let ev = events
            .recv_timeout(Duration::from_secs(2))
            .expect("a Heartbeat event should arrive");
        assert_eq!(ev, CoreEvent::Heartbeat);
    }

    // The whole point of COE: the pipeline composes plan → distribute (council synthesis) → execute
    // (governance + orchestration) → evidence, and STREAMS the progress as live events. Uses a STUB
    // dispatcher so the council runs its real synthesis over deterministic votes — NO subprocess, so
    // the test is reliable (the real-subprocess dispatch is wicked-council's own concern).
    #[test]
    fn pipeline_composes_and_streams_events_deterministically() {
        use std::sync::Arc;
        use wicked_council::types::{Category, Confidence, Dispatcher, InputMode, Vote};
        use wicked_council::CouncilTask;

        struct Stub;
        impl Dispatcher for Stub {
            fn dispatch(&self, cli: &AgenticCli, _task: &CouncilTask) -> Option<Vote> {
                Some(Vote {
                    cli: cli.key.clone(),
                    recommendation: "fake-a".into(),
                    top_risk: "none".into(),
                    change_my_mind: "no".into(),
                    disqualifier: None,
                    confidence: Confidence::default(),
                    provenance: "stub".into(),
                })
            }
        }
        let cli = |key: &str| AgenticCli {
            key: key.into(),
            display_name: key.into(),
            binary: "unused".into(),
            headless_invocation: "unused {PROMPT}".into(),
            category: Category::default(),
            input_mode: InputMode::default(),
            version_probe: vec![],
            trust_flags: vec![],
            alt_binaries: vec![],
            confidence: Confidence::default(),
            enabled_for_council: true,
        };

        let dir = std::env::temp_dir().join("wicked-core-pipeline-test");
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("pipeline.db");
        let _ = std::fs::remove_file(&db);
        let mut store = wicked_apps_core::open_store(Some(db.to_str().unwrap())).unwrap();

        let mut events: Vec<CoreEvent> = Vec::new();
        let result = crate::pipeline::run_session(
            &mut store,
            vec![cli("fake-a"), cli("fake-b")],
            "Do step one. Do step two",
            EntityMode::Shared,
            "test-pipeline",
            None, // free-text planner (legacy path)
            Arc::new(Stub),
            &mut |ev| events.push(ev),
        )
        .expect("run_session");

        // Composition result.
        assert_eq!(result.units.len(), 2);
        assert_eq!(result.approved, 2, "no deny policy ⇒ both approve");
        assert_eq!(result.rejected, 0);

        // Live event sequence — emitted in order, bookended by Started/Completed.
        let n = |pred: fn(&CoreEvent) -> bool| events.iter().filter(|e| pred(e)).count();
        assert_eq!(n(|e| matches!(e, CoreEvent::SessionStarted { .. })), 1);
        assert_eq!(n(|e| matches!(e, CoreEvent::UnitPlanned { .. })), 2);
        assert_eq!(n(|e| matches!(e, CoreEvent::UnitDistributed { .. })), 2);
        assert_eq!(n(|e| matches!(e, CoreEvent::GateDecided { .. })), 2);
        assert_eq!(n(|e| matches!(e, CoreEvent::UnitDone { .. })), 2);
        assert!(matches!(
            events.first(),
            Some(CoreEvent::SessionStarted { .. })
        ));
        assert!(matches!(
            events.last(),
            Some(CoreEvent::SessionCompleted { .. })
        ));

        // Persisted + readable through the same domain the read API serves.
        let units = session_units(&store, "test-pipeline").unwrap();
        assert_eq!(units.len(), 2);
        assert!(units.iter().all(|u| u.status == UnitStatus::Done));
        let out = get_work_output(&store, "test-pipeline:u1").expect("unit 1 output");
        assert!(out.contains("stub-output"), "transcript: {out}");

        // ── Law 2 AT RUNTIME: selecting a workflow makes the run a function of WorkflowDef DATA. ──
        // Same driver, same prose, but `Some("feature")` — the units now come from the feature def's
        // phases (ids + declared stage), NOT the sentence-splitter. This is the proof the slice-1
        // adversarial review's critical finding demanded: a runtime consumer of the registry.
        let feature = crate::feature_def();
        let mut ev2: Vec<CoreEvent> = Vec::new();
        crate::pipeline::run_session(
            &mut store,
            vec![cli("fake-a"), cli("fake-b")],
            "add SSO login", // under the legacy planner this prose is ONE unit; the def makes it 6
            EntityMode::Shared,
            "test-feature",
            Some("feature"),
            Arc::new(Stub),
            &mut |e| ev2.push(e),
        )
        .expect("def-driven run_session");
        let funits = session_units(&store, "test-feature").unwrap();
        assert_eq!(
            funits.len(),
            feature.phases.len(),
            "one unit per feature phase — the def drove planning, not the prose splitter"
        );
        for (u, p) in funits.iter().zip(feature.phases.iter()) {
            // The unit id encodes the backing phase (plan-time linkage the execute path can't clobber).
            assert_eq!(
                u.id,
                format!("test-feature:{}", p.id),
                "unit id backs its phase"
            );
            assert_eq!(
                u.stage, p.kind,
                "stage came from the phase's declared kind, not a keyword guess over the prose"
            );
        }
        assert!(
            funits.len() > 1,
            "the free-text planner would have made 1 unit from this prose; the def made {}",
            funits.len()
        );
    }
}
