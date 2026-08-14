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

mod acp_permission;
mod acp_runner;
mod actor;
mod applications;
pub mod assumptions;
mod builtin_floors;
mod bus;
mod campaign;
mod cli_runner;
mod clock;
mod code_graph;
mod command;
mod diagnostic;
mod distribute;
mod docs;
mod domain;
mod domain_extraction;
mod event;
pub mod event_log;
mod execute;
mod execute_wrapped;
mod gate_hook;
mod graph_browser;
mod interaction;
mod knowledge;
#[cfg(test)]
mod lockstep;
mod memory;
mod outstanding_work;
pub mod path_policy;
mod pipeline;
mod plan;
mod project;
mod repo;
mod repo_intel;
mod scope;
mod session_runner;
mod sources;
mod spawn_audit;
mod terminal;
mod validator;
mod validator_vault;
mod workflow;

pub use acp_runner::AcpStepRunner;
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
pub use command::InjectTarget;
pub use docs::{list_docs, new_doc, read_doc, write_doc, DocMeta};
pub use domain::{
    all_sessions, get_session, get_work_output, put_node, put_nodes, session_units, AgentSession,
    HumanConfirm, RoutingInfo, SessionStatus, SessionView, StageKind, UnitStatus, WorkUnit,
};
pub use domain_extraction::{
    coverage_eq_one_validator, provision_and_approve_coverage_validator, COVERAGE_CRITERION,
    COVERAGE_SCRIPT, COVERAGE_VALIDATOR_PIN, DOMAIN_EXTRACTION_WORKFLOW_ID,
};
pub use event::{CoreEvent, InjectedContext, StepFailureKind};
pub use execute_wrapped::WrappedCliStepRunner;
pub use gate_hook::{
    count_claims, decisions_path_for, gov_run_dir, parse_protocol_version, protocol_version_line,
    run_gate_hook, run_output_gate_hook, HookDrainSummary, COVERAGE_DB_ENV, DECISIONS_PATH_ENV,
    ESTATE_DB_ENV, GATE_DB_ENV, GATE_PHASE_ENV, GATE_PHASE_ID_ENV, GATE_PROTOCOL_VERSION,
    GATE_SCOPE_ENV,
};
pub use graph_browser::{
    browse_nodes, graph_kinds, list_node_notes, node_detail, NeighborEdge, NodeDetail, NodeNote,
    NodeSummary, SymbolAnnotation,
};
pub use interaction::{
    list_interactions, now_millis, InteractionKind, InteractionRequest, InteractionStatus,
    INTERACTION_REQUEST,
};
pub use knowledge::RecalledKnowledge;
pub use memory::{now_secs, validate_scope_path, RecalledMemory};
pub use pipeline::SessionResult;
pub use plan::plan_from_def;
pub use project::{
    get_project, list_members, list_projects, member_projects, members_of_kind, MemberSpec,
    Project, ProjectMember, ProjectPatch, ProjectStatus, DEFAULT_PROJECT_ID, MEMBER_KIND_RUN,
    PROJECT, PROJECT_MEMBER,
};
pub use repo::{coverage_report_for_repo, get_repo, graph_kinds_for_repo, RepoEntry, RepoSpec};
pub use repo_intel::{
    change_digest_since, commits_since, profile_repo, Commit, GraphStats, Hotspot, RepoProfile,
};
pub use scope::{resolve_scope, EntityMode};
pub use session_runner::PersistentStepRunner;
pub use sources::{add_node_note, add_source, base_dir, enrich_source, index_docs, ReconDoc};
pub use validator::{
    agent_validate, author_deterministic_validator, combine_verdict, gate_phase, run_validator,
    run_validator_reporting, sandbox_availability, AgentVerdict, DeterministicValidator,
    GateVerdict, SandboxLevel, ValidatorOutcome, DETERMINISTIC_VALIDATOR_SEAT,
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
    /// The project this run is filed into (DES-PROJECT-001 §2.2). When set, the actor validates
    /// the project (must exist and be active) and attaches the `crew.run` membership IN THE SAME
    /// BATCH as the run's launch record — a crash cannot leave the run outside its project. An
    /// invalid or archived id fails the launch with no session persisted. `None` ⇒ unfiled (the
    /// synthesized `default` project).
    pub project_id: Option<String>,
    /// ADDITIONAL absolute write roots for this run's deliverables (core#259) — e.g. an inbox dir
    /// the workflow's contract names as the output destination. Widens the governed units'
    /// filesystem boundary (`WICKED_WRITE_ROOTS`) by exactly these roots, after the unit cwd.
    /// Validated at launch: each root must be absolute and outside the engine's config/pin tree
    /// ([`crate::path_policy::validate_extra_write_roots`]) — an invalid root fails the launch
    /// loudly with no session persisted. Empty for runs that deliver inside their own workdir.
    pub extra_write_roots: Vec<String>,
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

/// Start the background thread that reclaims idle chats.
///
/// Every warm chat seat pins an ACP bridge plus an agent child process — ~520 MB resident apiece —
/// and nothing else ever releases them: `chat_close` reaps correctly but is only called by a
/// client that is still alive to call it. A closed laptop lid, a crashed tab, or a page navigated
/// away leaves the seats warm for the daemon's whole lifetime (FINDING-027: 25 processes / 3.30 GB
/// after 7h34m, still climbing during pure observation). This thread is the only reclamation path
/// that does not depend on the client, which is exactly why it has to exist.
///
/// Holds a [`Weak`](std::sync::Weak), so it stops when the last `Core`/runner handle drops instead
/// of pinning the runner alive forever and inverting the leak it was written to fix.
///
/// `WICKED_CHAT_IDLE_SECS=0` disables it entirely, for a host that would rather pay the memory
/// than ever have a chat reclaimed underneath it.
fn spawn_chat_reaper(runner: &std::sync::Arc<AcpStepRunner>) {
    let ttl = AcpStepRunner::chat_idle_ttl();
    if ttl.is_zero() {
        return;
    }
    let weak = std::sync::Arc::downgrade(runner);
    // Sweep at a fraction of the TTL so a chat is reclaimed within ~10% of when it aged out rather
    // than up to a full TTL late; floored at 1s so a tiny test TTL cannot spin the CPU.
    let tick = std::cmp::max(ttl / 10, std::time::Duration::from_secs(1));
    std::thread::spawn(move || loop {
        std::thread::sleep(tick);
        // Upgrade, act, and DROP the strong ref before sleeping again — holding it across the
        // sleep would keep the runner (and its child processes) alive past the Core.
        match weak.upgrade() {
            Some(runner) => {
                runner.chat_reap_idle(ttl);
            }
            None => break,
        }
    });
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
    /// The concrete ACP runner handle for CHAT sessions (crew#165 / core#13). Chat acts on the
    /// session pool DIRECTLY (off-actor — warm-ups and turns are slow I/O that must not queue
    /// behind the single store-writer); its events still flow through the actor's emit point.
    /// `None` when the engine was spawned with an injected non-ACP runner — chat unsupported.
    chat: Option<std::sync::Arc<AcpStepRunner>>,
    /// Where this core's durable event logs live (`<store>.events`, see [`crate::event_log`]). Held on
    /// the handle rather than fetched from the actor because reading a run's history is a plain file
    /// read: routing it through the command channel would queue an evidence read behind whatever the
    /// single writer is doing.
    log_root: std::path::PathBuf,
    _shutdown: Arc<ShutdownGuard>,
}

impl Core {
    /// Spawn the store actor over the estate store at `path`, with the production engine seams: the
    /// real council dispatcher + the ACP multi-CLI session runner. ACP is the default — each CLI
    /// runs its wrapper binary in a persistent session so turns within a run share prompt-cache.
    /// Governed units (gate-hook injection required) always route to the single-shot wrapped runner;
    /// ACP is not used for those. When an ACP binary is absent, the runner emits a warning in the
    /// step output and falls back to single-shot invocation automatically. The actor lives until
    /// every `Core` handle is dropped. Tests use [`Core::spawn_with_engine`] to inject a stub
    /// runner instead.
    pub fn spawn(path: impl Into<String>) -> Core {
        let (core, _runner) = Core::spawn_with_acp_sessions(path);
        core
    }

    // ── Chat sessions (crew#165 / core#13): warm ACP seats + group fan-out ─────────

    fn chat_runner(&self) -> anyhow::Result<&std::sync::Arc<AcpStepRunner>> {
        self.chat.as_ref().ok_or_else(|| {
            anyhow::anyhow!("chat unsupported: engine spawned without the ACP runner")
        })
    }

    /// Eagerly warm one ACP session per seat for `chat_id`. Blocking (spawn + handshake per
    /// seat) — call off any latency-sensitive thread. Per-seat outcomes returned; the same
    /// outcomes stream to subscribers as `ChatSessionReady`/`ChatSessionFailed`.
    pub fn chat_open(
        &self,
        chat_id: &str,
        clis: &[String],
        cwd: Option<std::path::PathBuf>,
    ) -> anyhow::Result<Vec<(String, Result<(), String>)>> {
        let runner = self.chat_runner()?;
        let cwd = cwd.unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        Ok(runner.chat_open(chat_id, clis, &cwd))
    }

    /// Fan `text` out to the chat's warm seats (all, or the named subset) — one thread per
    /// seat so replies stream in parallel as `ChatDelta`/`ChatReply` events. Ack-fast:
    /// returns the seats targeted, not their replies.
    pub fn chat_send(
        &self,
        chat_id: &str,
        text: &str,
        targets: Option<Vec<String>>,
        cwd: Option<std::path::PathBuf>,
    ) -> anyhow::Result<Vec<String>> {
        let runner = self.chat_runner()?;
        let seats = match targets {
            Some(t) if !t.is_empty() => t,
            _ => runner.chat_seats(chat_id),
        };
        if seats.is_empty() {
            anyhow::bail!("chat '{chat_id}' has no warm seats — open it first");
        }
        let cwd = cwd.unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        for cli in &seats {
            let runner = runner.clone();
            let tx = self.tx.clone();
            let (chat_id, cli, text, cwd) = (
                chat_id.to_string(),
                cli.clone(),
                text.to_string(),
                cwd.clone(),
            );
            std::thread::spawn(move || {
                let outcome = runner.chat_turn(&chat_id, &cli, &text, &cwd);
                let (ok, body) = match outcome {
                    Ok(reply) => (true, reply),
                    Err(e) => (false, e),
                };
                let _ = tx.send(Command::EmitEvent(CoreEvent::ChatReply {
                    chat: chat_id,
                    cli_key: cli,
                    text: body,
                    ok,
                }));
            });
        }
        Ok(seats)
    }

    /// The seats currently warm for `chat_id`.
    pub fn chat_seats(&self, chat_id: &str) -> anyhow::Result<Vec<String>> {
        Ok(self.chat_runner()?.chat_seats(chat_id))
    }

    /// Every chat currently holding warm seats, with how long each has been idle.
    ///
    /// Each warm seat pins a bridge plus an agent child process (~520 MB resident), and clients
    /// mint chat ids freely — without an enumerate surface an accumulation is invisible until the
    /// host runs out of memory (FINDING-027).
    pub fn chat_list(&self) -> anyhow::Result<Vec<crate::acp_runner::ChatInfo>> {
        Ok(self.chat_runner()?.chat_list())
    }

    /// Close a chat's warm sessions (idempotent); emits `ChatClosed { reason: "requested" }`.
    ///
    /// Always `Requested`: this entry point is only ever reached from an operator surface. The
    /// daemon's own reclamations go through the runner directly with their own reason, so a client
    /// can always tell "I closed this" from "the daemon took it back".
    pub fn chat_close(&self, chat_id: &str) -> anyhow::Result<()> {
        self.chat_runner()?
            .chat_close(chat_id, crate::acp_runner::ChatCloseReason::Requested);
        Ok(())
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
    pub fn spawn_with_pty_sessions(
        path: impl Into<String>,
    ) -> (Core, std::sync::Arc<PersistentStepRunner>) {
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
        // Captured before `path` moves into the actor thread: the handle needs the same store path to
        // resolve the event-log root, and both sides must agree (see `actor::sidecar_base`).
        let log_path = path.clone();
        // Each actor gets its own lifecycle maps (epoch tracking + tombstone) and an empty
        // write registry (no ACP sessions for PTY path).
        let lifecycle_arc = std::sync::Arc::new(std::sync::Mutex::new(
            crate::acp_runner::ElicitationMaps::new(),
        ));
        let empty_write_reg: crate::acp_runner::WriteReg =
            std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        std::thread::spawn(move || {
            actor::run(
                path,
                rx,
                self_tx,
                distribute::real_dispatcher(),
                runner_actor,
                pty_actor,
                None,
                false, // is_acp
                Some(lifecycle_arc),
                empty_write_reg,
            )
        });
        let core = Core {
            tx: tx.clone(),
            pty,
            chat: None, // PTY runner — ACP chat sessions unavailable
            log_root: crate::event_log::log_root(&actor::sidecar_base(&log_path)),
            _shutdown: Arc::new(ShutdownGuard { tx }),
        };
        (core, runner)
    }

    /// Spawn the store actor with an [`AcpStepRunner`] as the execution seam — units within the
    /// same run share a persistent ACP session per CLI (no per-unit cold-start). Uses the real
    /// council dispatcher. The returned `Core` also exposes an [`AcpStepRunner`] handle so the
    /// caller can call [`AcpStepRunner::drop_session`] after each run completes to release the
    /// ACP child processes. See [`Core::spawn`] for the simpler version that manages the runner
    /// internally.
    ///
    /// Also starts the chat reaper — see [`spawn_chat_reaper`].
    pub fn spawn_with_acp_sessions(
        path: impl Into<String>,
    ) -> (Core, std::sync::Arc<AcpStepRunner>) {
        let (tx, rx) = channel();
        let path = path.into();
        let self_tx = tx.clone();
        let pty = terminal::new_map();
        let pty_actor = pty.clone();
        let runner = std::sync::Arc::new(AcpStepRunner::new(tx.clone()));
        // Share the maps and write registry already inside the runner so the actor and the
        // ACP execution layer use a single consistent lock.
        let actor_maps = runner.elicitation_maps().clone();
        let actor_write_reg = runner.write_reg.clone();
        let runner_actor = runner.clone();
        // Captured before `path` moves into the actor thread: the handle needs the same store path to
        // resolve the event-log root, and both sides must agree (see `actor::sidecar_base`).
        let log_path = path.clone();
        std::thread::spawn(move || {
            actor::run(
                path,
                rx,
                self_tx,
                distribute::real_dispatcher(),
                runner_actor,
                pty_actor,
                None,
                true, // is_acp
                Some(actor_maps),
                actor_write_reg,
            )
        });
        spawn_chat_reaper(&runner);
        let core = Core {
            tx: tx.clone(),
            pty,
            chat: Some(runner.clone()),
            log_root: crate::event_log::log_root(&actor::sidecar_base(&log_path)),
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
        // Captured before `path` moves into the actor thread (see `spawn_with_pty_sessions`).
        let log_path = path.clone();
        let spawn_lifecycle_arc = std::sync::Arc::new(std::sync::Mutex::new(
            crate::acp_runner::ElicitationMaps::new(),
        ));
        let spawn_write_reg: crate::acp_runner::WriteReg =
            std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        std::thread::spawn(move || {
            actor::run(
                path,
                rx,
                self_tx,
                dispatcher,
                runner,
                pty_actor,
                exec_bus,
                false, // is_acp
                Some(spawn_lifecycle_arc),
                spawn_write_reg,
            )
        });
        Core {
            tx: tx.clone(),
            pty,
            chat: None, // injected runner (tests / bus seam) — ACP chat sessions unavailable
            log_root: crate::event_log::log_root(&actor::sidecar_base(&log_path)),
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

    /// Resolve a pending ACP elicitation for `run_id`. The `action` must be one of `"accept"`,
    /// `"decline"`, or `"cancel"`; `response` carries the human's typed/selected value when
    /// `action == "accept"`, and is `None` otherwise.
    ///
    /// Returns `Ok(())` when the elicitation was found and the resolution was delivered to the
    /// waiting turn, or an error when no matching elicitation exists (already resolved, unknown
    /// run, or elicitation not supported for this runner).
    pub fn resolve_elicitation(
        &self,
        run_id: &str,
        elicitation_id: &str,
        action: String,
        response: Option<serde_json::Value>,
    ) -> anyhow::Result<()> {
        let (reply, rx) = std::sync::mpsc::sync_channel(1);
        self.tx
            .send(Command::ResolveElicitation {
                run_id: run_id.to_string(),
                elicitation_id: elicitation_id.to_string(),
                action,
                response,
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

    // ── Projects (DES-PROJECT-001) ───────────────────────────────────────────────
    // Writes go through the actor (the single writer); reads are open_store_ro at the
    // binding layer, exactly like governance reads.

    /// Create a project: validate the name (1–120 chars, unique among active projects), mint the
    /// `proj_<sortable>` id + `project:<id>` scope, persist. Returns the created [`Project`].
    pub fn project_create(
        &self,
        name: &str,
        description: Option<String>,
    ) -> anyhow::Result<Project> {
        let (reply, rx) = channel();
        self.tx
            .send(Command::ProjectCreate {
                name: name.to_string(),
                description,
                reply,
            })
            .map_err(|_| anyhow::anyhow!("core actor stopped"))?;
        rx.recv()
            .map_err(|_| anyhow::anyhow!("core actor dropped the reply"))?
    }

    /// Rename / describe / archive / restore a project (`active ⇄ archived`; no hard delete).
    pub fn project_update(&self, id: &str, patch: ProjectPatch) -> anyhow::Result<Project> {
        let (reply, rx) = channel();
        self.tx
            .send(Command::ProjectUpdate {
                id: id.to_string(),
                patch,
                reply,
            })
            .map_err(|_| anyhow::anyhow!("core actor stopped"))?;
        rx.recv()
            .map_err(|_| anyhow::anyhow!("core actor dropped the reply"))?
    }

    /// Attach a member to a project. Idempotent on `(project, kind, ref)` — the bool is `true`
    /// only when a NEW membership was written (the caller emits its bus event on that).
    pub fn project_attach_member(&self, spec: MemberSpec) -> anyhow::Result<(ProjectMember, bool)> {
        let (reply, rx) = channel();
        self.tx
            .send(Command::ProjectMemberAttach { spec, reply })
            .map_err(|_| anyhow::anyhow!("core actor stopped"))?;
        rx.recv()
            .map_err(|_| anyhow::anyhow!("core actor dropped the reply"))?
    }

    /// Detach a member (tombstone — the member's own data is never touched). `false` = not found.
    pub fn project_detach_member(&self, project_id: &str, member_id: &str) -> anyhow::Result<bool> {
        let (reply, rx) = channel();
        self.tx
            .send(Command::ProjectMemberDetach {
                project_id: project_id.to_string(),
                member_id: member_id.to_string(),
                reply,
            })
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

    /// Upsert a governance policy. `policy_json` is a JSON-serialized `wicked_governance::Policy`
    /// (fields: id, kind, applies_to, effect, trigger, severity, criteria, rule, obligations).
    /// Validates, then stores via the single-writer actor. Fails closed on validation errors.
    pub fn upsert_policy(&self, policy_json: impl Into<String>) -> anyhow::Result<()> {
        let (reply, rx) = channel();
        self.tx
            .send(Command::UpsertPolicy {
                policy_json: policy_json.into(),
                reply,
            })
            .map_err(|_| anyhow::anyhow!("core actor stopped"))?;
        rx.recv()
            .map_err(|_| anyhow::anyhow!("core actor dropped the reply"))?
    }

    /// Upsert a conformance rule. `rule_json` is a JSON-serialized `wicked_governance::ConformanceRule`
    /// (fields: id, rule_type, statement, severity, confidence, targets, provenance).
    /// Validates (INV-C1/C2/C4), then stores via the single-writer actor. Fails closed on validation errors.
    pub fn upsert_conformance_rule(&self, rule_json: impl Into<String>) -> anyhow::Result<()> {
        let (reply, rx) = channel();
        self.tx
            .send(Command::UpsertConformanceRule {
                rule_json: rule_json.into(),
                reply,
            })
            .map_err(|_| anyhow::anyhow!("core actor stopped"))?;
        rx.recv()
            .map_err(|_| anyhow::anyhow!("core actor dropped the reply"))?
    }

    /// Withdraw a governance policy from enforcement. Returns `false` if no policy has that id.
    ///
    /// Retire, not delete: a governance system whose rules cannot be withdrawn cannot correct a
    /// mistake, but hard-deleting would strand every past decision that cites the id. The node
    /// stays readable and stops being selected.
    pub fn retire_policy(&self, id: &str) -> anyhow::Result<bool> {
        let (reply, rx) = channel();
        self.tx
            .send(Command::RetirePolicy {
                id: id.to_string(),
                reply,
            })
            .map_err(|_| anyhow::anyhow!("core actor stopped"))?;
        rx.recv()
            .map_err(|_| anyhow::anyhow!("core actor dropped the reply"))?
    }

    /// Withdraw a conformance rule from recall. Returns `false` if no rule has that id. Same
    /// retire-not-delete contract as [`Self::retire_policy`].
    pub fn retire_conformance_rule(&self, id: &str) -> anyhow::Result<bool> {
        let (reply, rx) = channel();
        self.tx
            .send(Command::RetireConformanceRule {
                id: id.to_string(),
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

    /// A run's recorded event history, oldest first — each entry the event's own tagged JSON
    /// ([`CoreEvent::to_json`], the same object `/ws` carries) plus a capture-time `ts` and an
    /// ordering `seq`.
    ///
    /// This is the read half of FINDING-014. Consumers that need a run's event trail after the fact
    /// (evidence bundles, above all) read it HERE rather than re-deriving pseudo-events from unit
    /// records — a re-derivation cannot recover what it never saw, and invents its own type names
    /// doing it. Reads the log directly rather than going through the actor: the log is
    /// independently owned and append-only, so a read needs no store handle and cannot block on the
    /// actor's queue while a run is mid-flight.
    ///
    /// Empty for a run that emitted nothing, one that predates the log, or an unknown id — an absent
    /// history is not an error. Streaming chunk events are excluded by design; see
    /// [`crate::event_log`].
    pub fn run_events(&self, run_id: &str) -> Vec<serde_json::Value> {
        crate::event_log::read_run(&self.log_root, run_id)
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

    /// Register (or replace) a workflow definition in the actor's runtime registry. `json` is a
    /// JSON-serialised [`WorkflowDef`]; validates before inserting and rejects invalid JSON or a
    /// def that fails its own structural validation. Idempotent on id — calling twice with the same
    /// id replaces the first registration. Returns the registered workflow id.
    ///
    /// Registered defs are visible immediately: the next `launch_run` call with a matching `workflow`
    /// id will plan from this def without a process restart.
    pub fn register_workflow(&self, json: impl Into<String>) -> anyhow::Result<String> {
        let (reply, rx) = channel();
        self.tx
            .send(Command::RegisterWorkflow {
                json: json.into(),
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

    /// Inject an operator message into active PTY worker(s) for `run_id`.
    ///
    /// The message is written verbatim to stdin (with a trailing `\n`) of every PTY session whose
    /// CLI key matches `target`. ACP-backed sessions have no PTY and are skipped silently (with a
    /// warning logged to stderr). A [`CoreEvent::WorkerMessageInjected`] is emitted for each
    /// successful write.
    ///
    /// Fire-and-forget in spirit — the actor returns `Ok(())` even when no sessions were found;
    /// failures reach the caller only if the actor channel is dead.
    pub fn inject_worker_message(
        &self,
        run_id: &str,
        message: &str,
        target: InjectTarget,
    ) -> anyhow::Result<()> {
        let (reply, rx) = channel();
        self.tx
            .send(Command::InjectWorkerMessage {
                run_id: run_id.to_string(),
                message: message.to_string(),
                target,
                reply,
            })
            .map_err(|_| anyhow::anyhow!("core actor stopped"))?;
        rx.recv()
            .map_err(|_| anyhow::anyhow!("core actor dropped the reply"))?
    }

    /// Stop the current worker for `ord` inside run `run_id` and re-dispatch it.
    ///
    /// * `new_cli = Some(key)` — re-dispatch immediately to that CLI (no council re-run).
    /// * `new_cli = None` — re-convene the council and let it pick; the re-dispatch happens once
    ///   the council vote returns (the method returns `Ok(())` before that happens — the result
    ///   appears as a [`CoreEvent::UnitReassigned`] followed by the normal unit-lifecycle events).
    ///
    /// Returns an error if the run is not currently `Executing`, or if `ord` is not the cursor unit.
    pub fn reassign_unit(
        &self,
        run_id: &str,
        ord: u32,
        new_cli: Option<String>,
    ) -> anyhow::Result<()> {
        let (reply, rx) = channel();
        self.tx
            .send(Command::ReassignUnit {
                run_id: run_id.to_string(),
                ord,
                new_cli,
                reply,
            })
            .map_err(|_| anyhow::anyhow!("core actor stopped"))?;
        rx.recv()
            .map_err(|_| anyhow::anyhow!("core actor dropped the reply"))?
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
            acp: None,
            capabilities: None,
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
