//! The store actor: the ONE thread that owns the writable `SqliteStore`. Every command is handled
//! here, serially, so multiple in-process callers (agent, UI, MCP) never contend for the SQLite
//! writer lock or race a reader against a mid-batch write. This is the single-writer guarantee.
//!
//! Two execution shapes share this thread:
//!  * `Launch` — the legacy straight-through driver (`run_session`) runs to completion inline (fine
//!    for the fast stub path).
//!  * `LaunchRun`/`ResumeRun` — the INTERACTIVE engine: the actor does the fast store writes
//!    (plan/distribute, gate, cursor advance) and dispatches each unit's slow work to a worker
//!    thread that holds NO store handle. The worker posts `ApplyStepResult` back over a
//!    `Sender<Command>` clone the actor owns, so the actor stays responsive (serves reads) while a
//!    unit runs, yet remains the only writer. An `in_flight` guard rejects a second mutating command
//!    for a run already executing (`RunBusy`) so a run is never double-dispatched.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;

use base64::Engine as _;
use wicked_apps_core::{
    open_store_any, AnyStore, ConformanceClaim, Decision, GraphRead, GraphStore, NodeKind, ToNode,
    AGENT_SESSION,
};
use wicked_council::types::Dispatcher;
use wicked_estate_core::SymbolQuery;
use wicked_governance::{conform, decide, select_any};

use crate::acp_runner::{ElicitationMaps, KillHandle, WriteReg};
use crate::command::Command;
use crate::domain::{put_node, AgentSession, SessionStatus};
use crate::event::CoreEvent;
use crate::terminal::{self, PtyMap};
use crate::workflow::{PriorUnitOutput, StepInput, StepRunner};
use crate::{pipeline, resolve_scope, EntityMode, LaunchSpec};
use wicked_apps_core::HardenedCommand;

/// The actor-owned terminal registry entry (DES §4 "id → status"). Presence in the registry map IS
/// the "open" status; removal (on exit/close) is the terminal state — this is the single-emit guard
/// that keeps `TerminalExited` firing exactly once. `next_seq` is the per-terminal output sequence,
/// assigned here on the one actor thread so the stream stays ordered.
struct TermReg {
    next_seq: u64,
    /// In-flight (sent-but-not-yet-emitted) output bytes for this terminal — the reader reads this
    /// gauge to pace itself (SIG-1 backpressure); the actor decrements it here as each chunk is
    /// emitted. Shared `Arc` with the reader thread.
    in_flight: Arc<AtomicUsize>,
    /// Cumulative output bytes the reader has DROPPED (drop-oldest overflow). Compared against
    /// `reported_dropped` so the actor emits a degraded marker only when NEW output was shed.
    dropped_total: Arc<AtomicU64>,
    /// The dropped-byte total we've already reported to the consumer (via a degraded marker).
    reported_dropped: u64,
}

/// A run id already executing may not be mutated again — surfaced to the caller as this error.
#[derive(Debug)]
pub struct RunBusy(pub String);
impl std::fmt::Display for RunBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "run {} is busy (a step is in flight)", self.0)
    }
}
impl std::error::Error for RunBusy {}

/// A NON-TERMINAL run with this id already exists — re-planning over it would reset its cursor, so the
/// clobber guard refuses. A TYPED error (not a bare string) so callers — notably the bus bridge — can
/// recognize this as an idempotent redelivery via `downcast_ref` instead of substring-matching the
/// message. `.0` is the run id, `.1` its current status rendered for the operator-facing message.
#[derive(Debug)]
pub struct RunExists(pub String, pub String);
impl std::fmt::Display for RunExists {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "run {} already exists (status {}); resume or cancel it, or use a new id",
            self.0, self.1
        )
    }
}
impl std::error::Error for RunExists {}

thread_local! {
    /// The estate store path for GOVERNED in-process dispatch (DES-OUTGOV-003 §4). Armed at actor
    /// startup; read by [`in_process_governance`] when building a governed unit's `StepInput`. A
    /// thread-local (mirroring [`crate::cli_runner`]'s `EXEC_PUBLISHER`) makes the store path reachable
    /// deep in `dispatch_unit` WITHOUT threading a parameter through
    /// redrive/advance_or_pause/confirm_gate; it is per-actor-thread, so tests spawning multiple actors
    /// (each with its own store) never cross-talk.
    static GOV_DB_PATH: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

thread_local! {
    /// Warn ONCE per actor thread that input governance is off for a non-file store (avoids per-unit spam).
    static GOV_OFF_WARNED: std::cell::RefCell<bool> = const { std::cell::RefCell::new(false) };
}

/// The governance context for an IN-PROCESS governed unit (DES-OUTGOV-003 §4), or `None` when input
/// governance cannot apply: no store armed, or a store the SQLite gate-hook subprocess cannot open
/// independently — `:memory:` (cannot cross processes) or `postgres://` (SQLite-only hook; the
/// spec-dispatch read-only opener is deferred to core#30). The path is made ABSOLUTE because the hook
/// runs with cwd = the worktree, so a relative `.wicked-estate/graph.db` would open the wrong/empty
/// store (finding #6). A non-file store surfaces a LOUD one-time operator notice (council [3]/[13]) so a
/// silently-ungoverned run is not mistaken for a governed one.
pub(crate) fn in_process_governance() -> Option<crate::workflow::GovernanceContext> {
    let path = GOV_DB_PATH.with(|c| c.borrow().clone())?;
    if path == ":memory:" || path.contains("://") {
        GOV_OFF_WARNED.with(|w| {
            if !*w.borrow() {
                *w.borrow_mut() = true;
                eprintln!(
                    "wicked-core: INPUT governance NOT active for this run — store `{path}` is not a \
                     file-backed SQLite db the gate-hook can open (postgres/:memory: → core#30). \
                     Wrapped-CLI tool-calls run UNGOVERNED."
                );
            }
        });
        return None;
    }
    let abs = std::fs::canonicalize(&path)
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| {
            let p = std::path::Path::new(&path);
            if p.is_absolute() {
                path.clone()
            } else {
                std::env::current_dir()
                    .map(|d| d.join(p).to_string_lossy().into_owned())
                    .unwrap_or_else(|_| path.clone())
            }
        });
    Some(crate::workflow::GovernanceContext {
        db_path: abs,
        // Deliberately not resolved here: this function only knows the process-wide store path, not
        // which repo a run targets. `dispatch_unit` fills it in from the session's registered repo
        // (`repo_code_graph_db`). Leaving it `None` is the safe default — no repo, no estate MCP.
        code_graph_db: None,
        // Per-RUN, not process-wide: `dispatch_unit` fills this from the session (core#259).
        // Empty here means an ungoverned/standalone context widens nothing.
        extra_write_roots: Vec::new(),
    })
}

/// The repo-local code graph a governed worker's estate MCP may open, for the run's registered repo.
/// `None` when the run targets no repo, the repo id is not registered, the store read fails, or the
/// repo has never been indexed.
///
/// That last arm used to be unreachable. The resolver created the graph's parent directory and handed
/// back the path whether or not anything had ever written it, so a worker on an un-indexed repo got a
/// live MCP over an empty database — every graph query answering "nothing here" about a repo full of
/// code, with no error anywhere (FINDING-069). [`existing_code_graph`] creates nothing, so the file
/// either exists and the worker gets the real graph, or it does not and the worker gets no estate
/// tools at all.
///
/// Every `None` arm is a decision to ship the worker NO estate tools. That is the point: the only other
/// store in reach is the operational one, and a worker with a writable handle to it can delete the
/// platform's entire state (FINDING-067). Fewer tools is a degraded run; a wiped store is a dead one,
/// and a silently empty one is worse than both because it looks like an answer.
fn repo_code_graph_db(store: &dyn GraphStore, repo_ref: Option<&str>) -> Option<String> {
    let repo = crate::repo::get_repo(store, repo_ref?).ok().flatten()?;
    let path = crate::code_graph::existing_code_graph(std::path::Path::new(&repo.root_path))?;
    Some(path.to_string_lossy().into_owned())
}

thread_local! {
    /// Project-graph refusals already reported on this actor thread. A refused binding is
    /// re-evaluated on EVERY unit dispatch, so without this an operator whose graph was never
    /// built reads the same paragraph once a step. Keyed on the whole message, so a binding that
    /// later fails for a DIFFERENT reason still gets its own line.
    static GRAPH_BIND_WARNED: std::cell::RefCell<HashSet<String>> =
        std::cell::RefCell::new(HashSet::new());
}

/// Say — once — that a run is NOT getting the project graph its launcher bound, and why.
///
/// Silence is the wrong default here for the same reason it was wrong for ungoverned runs
/// (council [3]/[13]): the fallback is a working run with NARROWER tools, which is indistinguishable
/// from success right up until a worker concludes that a sibling repo does not exist. The remedy
/// differs per cause, so the cause is in the message rather than a generic "graph unavailable".
fn warn_bind_refused(run_id: &str, db: &str, why: &str) {
    let msg = format!(
        "wicked-core: run {run_id} is NOT bound to the project code graph `{db}` — {why}. Its \
         governed workers fall back to the run repo's OWN graph, or to no estate tools at all if \
         that repo has never been indexed."
    );
    GRAPH_BIND_WARNED.with(|w| {
        if w.borrow_mut().insert(msg.clone()) {
            eprintln!("{msg}");
        }
    });
}

/// Resolve `p` to a comparable absolute path: canonical when it exists, lexically absolute when it
/// does not. A sidecar store that has not been created yet still has to be refusable by name.
fn comparable_path(p: &str) -> String {
    let path = std::path::Path::new(p);
    if let Ok(c) = std::fs::canonicalize(path) {
        return c.to_string_lossy().into_owned();
    }
    if path.is_absolute() {
        return p.to_string();
    }
    std::env::current_dir()
        .map(|d| d.join(path).to_string_lossy().into_owned())
        .unwrap_or_else(|_| p.to_string())
}

/// Is `candidate` the engine's OWN operational store, or one of its sidecars?
///
/// `operational` is the actor's store spec. [`sidecar_base`] turns it into the filesystem base the
/// `.mem` / `.knowledge` / `.events` sidecars hang off — including for a `postgres://` store, whose
/// graph is remote but whose sidecars are still local files a worker could delete.
///
/// The trailing `.` prefix test is deliberately WIDER than the three sidecars named today: a
/// sidecar added later would otherwise be handed out until someone remembered to extend this list,
/// and the cost of over-refusing (`core.db.backup` say) is a run with narrower tools, while the
/// cost of under-refusing is FINDING-067 — a worker with a writable handle to the platform's whole
/// state, which deleted 833 nodes including the run that triggered it. Wrong in the safe direction
/// on purpose.
fn is_operational_store(candidate: &str, operational: &str) -> bool {
    let base = comparable_path(&sidecar_base(operational));
    if candidate == base || candidate == comparable_path(operational) {
        return true;
    }
    candidate.starts_with(&format!("{base}."))
}

/// The PROJECT's co-located code graph for this run, when — and only when — the engine can VOUCH
/// for it. `None` means "fall back to the per-repo graph", never "the launcher was wrong to ask".
///
/// # Why this verifies instead of trusting
///
/// [`crate::project::ProjectGraphBinding`] explains why the LAUNCHER supplies the path: it owns the
/// project graph's location, membership and labelling, and an engine that re-derived them would
/// drift from it. But a supplied path is an ASSERTION, and this is the value that becomes the
/// governed worker's estate MCP `--db` — the store its SearchEntity / BlastRadius / TraverseGraph
/// answer from, with a handle that can write. Both of this seam's standing findings are reachable
/// through a bad one: hand over the operational store and a worker can wipe the platform
/// (FINDING-067); hand over a database that does not describe this repo and every query answers
/// "nothing here" about code sitting in the worktree (FINDING-069). So every arm below is a
/// refusal backed by something read off the disk, not off the launch options.
///
/// # The arms
///
/// 1. **Not absolute** — the worker's `cwd` is its worktree, so a relative path opens a different
///    store, or a freshly-created empty one. (Finding #6, the same rule `in_process_governance`
///    applies to the governance db.)
/// 2. **Not an existing file** — the project graph was never built, or was deleted. Creates
///    nothing, exactly like [`crate::code_graph::existing_code_graph`]: "no graph" and "graph right
///    here" must never be the same value again.
/// 3. **The operational store** — see [`is_operational_store`].
/// 4. **Holds no indexed files** — an empty database is worse than no tools, because it looks like
///    an answer. This is the FINDING-069 check generalised: existence is necessary, not sufficient,
///    since a crashed or interrupted index leaves a real file behind with a schema and no rows.
/// 5. **Does not hold THIS RUN'S repo** — the sharpest case, and the reason `repo_label` is on the
///    wire at all. A project graph that holds `wicked-crew` but not `wicked-core` is a perfectly
///    healthy database, and binding it to a run working in `wicked-core` would give that worker
///    tools that confidently deny the existence of the function under its cursor. Narrower and
///    true beats broader and lying, so the run falls back to its own repo's graph.
///
/// Note what is NOT an arm: a graph missing some OTHER member repo, or one indexed at an older
/// commit. Both are bound. A graph holding this run's repo is a strict superset of the per-repo
/// graph on the axis that matters — the worker's own code is described truthfully — and refusing on
/// partial membership would hand back a NARROWER store on the grounds that a wider one was not
/// wide enough. Staleness is not an arm either: every code graph is stale the moment it is written,
/// the per-repo graph this falls back to has exactly the same drift with no check at all, and
/// gating on it would disqualify the project graph after a single commit while buying no safety.
///
/// # Cost
///
/// One read-only SQLite open plus `SELECT path FROM files` per unit dispatch, re-done rather than
/// cached because the file can be deleted or rebuilt mid-run and a cached "vouched" would outlive
/// the evidence for it. That is milliseconds against a dispatch that spawns an agentic CLI, and it
/// keeps this function's answer as fresh as `repo_code_graph_db`'s `is_file()`.
fn project_code_graph_db(
    binding: Option<&crate::project::ProjectGraphBinding>,
    repo_ref: Option<&str>,
    operational_db: Option<&str>,
    run_id: &str,
) -> Option<String> {
    let binding = binding?;
    let db = binding.db_path.trim();
    if db.is_empty() {
        return None;
    }

    if !std::path::Path::new(db).is_absolute() {
        warn_bind_refused(
            run_id,
            db,
            "the path is not absolute, and a governed worker runs with cwd = its own worktree, so \
             it would resolve to a different store than the launcher meant",
        );
        return None;
    }

    let resolved = match std::fs::canonicalize(db) {
        Ok(p) if p.is_file() => p.to_string_lossy().into_owned(),
        _ => {
            warn_bind_refused(
                run_id,
                db,
                "there is no file at that path — the project graph has not been built yet, or was \
                 deleted. Build it with POST /api/v1/projects/<id>/graph/refresh; launching a run \
                 never indexes on its own",
            );
            return None;
        }
    };

    // No known operational store ⇒ no way to prove this is not it ⇒ refuse. The actor arms
    // `GOV_DB_PATH` before it serves a single command, so on the dispatch path this is always
    // known; failing closed costs nothing there and keeps the FINDING-067 guard from being
    // quietly skippable by any future caller that forgets to pass it.
    let Some(operational) = operational_db else {
        warn_bind_refused(
            run_id,
            db,
            "the engine's own store path is unknown here, so it cannot prove this binding is not \
             that store — a writable handle to it lets a worker delete the platform's state \
             (FINDING-067)",
        );
        return None;
    };
    if is_operational_store(&resolved, operational) {
        warn_bind_refused(
            run_id,
            db,
            "it IS the engine's operational store (or one of its sidecars) — a worker holding a \
             writable handle to that store can delete the platform's entire state (FINDING-067)",
        );
        return None;
    }

    let store = match wicked_apps_core::open_store_ro(Some(&resolved)) {
        Ok(s) => s,
        Err(e) => {
            warn_bind_refused(
                run_id,
                db,
                &format!("it could not be opened read-only ({e})"),
            );
            return None;
        }
    };
    let files = match store.indexed_files() {
        Ok(f) => f,
        Err(e) => {
            warn_bind_refused(
                run_id,
                db,
                &format!("its indexed-file list could not be read ({e})"),
            );
            return None;
        }
    };
    if files.is_empty() {
        warn_bind_refused(
            run_id,
            db,
            "it holds no indexed files. An EMPTY graph is worse than no graph: every query answers \
             \"nothing here\" about code that plainly exists, which is how an agent concludes the \
             code is not there (FINDING-069)",
        );
        return None;
    }

    // A repo-less run has no own-repo to be wrong about, so a non-empty graph is all there is to
    // check. A run that TARGETS a repo must find that repo in there.
    if repo_ref.is_some() {
        let label = binding.repo_label.as_deref().unwrap_or_default().trim();
        if label.is_empty() {
            warn_bind_refused(
                run_id,
                db,
                "the binding carries no repo label for a run that targets a repo, so the engine \
                 cannot check that the graph describes the code the worker will edit",
            );
            return None;
        }
        // wicked-estate namespaces a co-located repo's paths as `<label>/…`, so this is the whole
        // membership test. A project graph built WITHOUT `--repo` labels has unprefixed paths and
        // fails here too — correct, because an unlabelled graph cannot be attributed to a repo.
        let prefix = format!("{label}/");
        if !files.iter().any(|f| f.starts_with(&prefix)) {
            warn_bind_refused(
                run_id,
                db,
                &format!(
                    "it does not hold this run's own repo (no files under the label `{label}`) — \
                     most likely the repo was attached to the project after the last refresh. \
                     Binding it would give the worker tools that deny the existence of the code in \
                     its own worktree, so the narrower per-repo graph is used instead"
                ),
            );
            return None;
        }
    }

    Some(resolved)
}

/// The code graph a governed WORKER's estate MCP opens for this run: the project's co-located graph
/// when the engine can vouch for it, else the run repo's own.
///
/// The fallback direction is the doctrine of this whole seam, restated: NARROWER AND TRUE beats
/// WIDER AND WRONG, and both beat handing over a store the engine cannot vouch for.
///
/// Deliberately NOT used for the coverage validator (`repo_code_graph_db` at the
/// `apply_and_finish_unit` call site keeps that job). Coverage is a GATE: it counts behaviour-bearing
/// nodes to decide whether a phase did its work. Measuring that over a graph containing sibling
/// repos would let repo B's annotations satisfy a criterion pinned to repo A — a gate that gets
/// easier to pass the more repos a project has. The worker's READ tools want the widest truthful
/// view; the evaluator's MEASUREMENT wants exactly the repo under test. Same value, two jobs, and
/// only one of them should widen.
fn run_code_graph_db(
    store: &dyn GraphStore,
    session: &AgentSession,
    operational_db: Option<&str>,
) -> Option<String> {
    project_code_graph_db(
        session.project_graph.as_ref(),
        session.repo_ref.as_deref(),
        operational_db,
        &session.id,
    )
    .or_else(|| repo_code_graph_db(store, session.repo_ref.as_deref()))
}

/// The FILESYSTEM base the per-core sidecars hang off: `<base>.mem`, `<base>.knowledge` and
/// `<base>.events`.
///
/// When the graph store is a URL backend (e.g. `postgres://…`), `path` is not a filesystem path and
/// appending `.mem` would yield a bogus `postgres://….mem`, so the sidecars anchor at the local estate
/// default instead.
///
/// Shared rather than inlined because [`crate::Core`] derives the event-log root from it too, off the
/// actor thread — two copies of this rule would mean a reader that looks in the wrong directory for a
/// postgres-backed core, and the symptom would be an empty evidence trail rather than an error.
pub(crate) fn sidecar_base(path: &str) -> String {
    if path.contains("://") {
        ".wicked-estate/graph.db".to_string()
    } else {
        path.to_string()
    }
}

/// Run the actor loop until `Command::Shutdown` arrives (sent automatically when the last
/// [`crate::Core`] handle drops — see `ShutdownGuard`). NOTE: channel-close alone can never stop this
/// loop, because the actor itself holds `self_tx` (a live sender) so workers can post results back;
/// `Shutdown` is therefore the real exit. On exit, `store` drops and the writable connection is
/// released. `dispatcher`/`runner` are the injectable council + step-execution seams (real in prod,
/// stubbed in tests).
#[allow(clippy::too_many_arguments)]
/// Lock ordering: `write_reg` must always be acquired BEFORE `elicitation_maps`.
/// Never hold both simultaneously; acquire `write_reg`, read/snapshot state, release,
/// then acquire `elicitation_maps` if needed. Violating this ordering risks ABBA deadlock
/// between `shared_run_terminal` (holds `write_reg`, then acquires `maps`) and
/// session-start helpers (acquire `maps` alone).
pub(crate) fn run(
    path: String,
    rx: Receiver<Command>,
    self_tx: Sender<Command>,
    dispatcher: Arc<dyn Dispatcher + Send + Sync>,
    runner: Arc<dyn StepRunner>,
    pty_map: PtyMap,
    exec_bus: Option<String>,
    is_acp: bool,
    elicitation_maps: Option<Arc<std::sync::Mutex<ElicitationMaps>>>,
    write_reg: WriteReg,
) {
    // Backend-agnostic: `path` may be a filesystem path (SQLite, the default) OR a `postgres://`
    // spec (selects estate's Postgres backend under the `postgres` feature). `AnyStore` is one
    // concrete type, so the engine below borrows it as `&dyn GraphRead` / `&mut dyn GraphStore`
    // without ever learning which backend it holds.
    let mut store: AnyStore = match open_store_any(Some(&path)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("wicked-core: could not open store at {path}: {e}");
            return;
        }
    };

    // Arm the governance store path for in-process governed dispatch (DES-OUTGOV-003 §4). The store now
    // exists on disk, so the gate-hook subprocess can open it read-only to evaluate tool-calls.
    GOV_DB_PATH.with(|c| *c.borrow_mut() = Some(path.clone()));

    // Seed the deterministic floor the built-in Evaluator phases pin (FINDING-025 item 1).
    //
    // This copy is an EARLY WARNING, not the guarantee. The guarantee lives on the plan path
    // (`pipeline::plan_and_distribute` seeds immediately before `attach_pinned_validators`), because
    // that is the one choke point every caller crosses — including `run_session`, which is public and
    // never constructs an actor. Seeding only here is exactly the bug that shipped in the first cut:
    // correct for the daemon, a hard bail for everyone else.
    //
    // What this call buys, at boot rather than at first run: the floor is visible to an operator
    // listing the vault, and a store that cannot hold it says so once, here, in terms naming the
    // cause — instead of surfacing later as an unresolvable-pin error they have to decode. So it is
    // best-effort in the same sense as the sidecars below, and for the same reason: a failure must
    // not stop the engine coming up, and the plan path will try again and fail loudly if it must.
    if let Err(e) = crate::builtin_floors::seed_builtin_floors(&mut store) {
        eprintln!(
            "wicked-core: could not seed the built-in evidence floor ({e}); runs of the built-in \
             workflows will fail closed at plan time on an unresolvable validator pin"
        );
    }

    let sidecar_base: String = sidecar_base(&path);

    // The orchestrator's episodic memory (a SEPARATE single-writer store, sibling of the estate db).
    // Best-effort: a memory-open failure must never stop the engine, so it's an `Option`.
    let mut memory = match crate::memory::RunMemory::open(&sidecar_base) {
        Ok(m) => Some(m),
        Err(e) => {
            eprintln!("wicked-core: memory store unavailable ({e}); continuing without recall");
            None
        }
    };
    // The orchestrator's knowledge base (documents) — also a separate single-writer store, best-effort.
    let mut knowledge = match crate::knowledge::RunKnowledge::open(&sidecar_base) {
        Ok(k) => Some(k),
        Err(e) => {
            eprintln!("wicked-core: knowledge store unavailable ({e}); continuing without it");
            None
        }
    };

    // Fans out to live subscribers AND records to the durable per-run event log (FINDING-014), so a
    // run's evidence no longer depends on someone holding a socket while it happened. Rooted at
    // `<store>.events` — the same sidecar anchor as memory/knowledge above, so a core opened against a
    // scratch db keeps its logs there instead of in a shared global tree.
    let mut subscribers =
        crate::event_log::EventSink::persistent(crate::event_log::log_root(&sidecar_base));
    // Runs with a worker step in flight — guards against double-dispatch (non-idempotent side effects).
    let mut in_flight: HashSet<String> = HashSet::new();
    // The actor-owned PTY terminal registry (id → status + seq). Byte-I/O lives off-actor in
    // `pty_map`; this small map is the single-writer state the actor owns (DES §4).
    let mut terminals: HashMap<String, TermReg> = HashMap::new();
    // Actor-maintained PTY session index: run_id → [(cli_key, terminal_id)].
    // Populated on WorkerSessionStarted, pruned on WorkerSessionClosed / TerminalExited.
    // Used by InjectWorkerMessage + ReassignUnit without touching the runner or store.
    let mut run_sessions: HashMap<String, Vec<(String, String)>> = HashMap::new();
    // Live-output coalescer: per-(run, ord) pending text + last-flush stamp, turning the raw
    // CliOutputDelta firehose into the throttled UnitOutputDelta stream (≤1 flush / 500ms / unit,
    // or 2KB, each capped at 2KB). Owned HERE — the single-writer thread — so workers only ever
    // send raw chunks. Drained on ApplyStepResult (the tail must not be lost) and CancelRun.
    let mut output_throttle = crate::output_throttle::UnitOutputThrottle::new();
    // Actor-owned workflow registry: built-ins seeded at startup, runtime defs added via
    // `Command::RegisterWorkflow`. The file overlay dir (`$WICKED_WORKFLOWS_DIR` /
    // `~/.config/wicked-core/workflows`) is loaded ONCE here so every subsequent `LaunchRun` on
    // this actor instance sees both the file-overlay and any runtime-registered defs without
    // re-reading the directory per call.
    let mut registry = crate::workflow::WorkflowRegistry::with_defaults();
    if let Some(dir) = pipeline::workflow_overlay_dir() {
        if let Err(e) = registry.load_dir(&dir) {
            eprintln!(
                "wicked-core: workflow overlay {} failed to load ({e}); using built-ins only",
                dir.display()
            );
        }
        // The def that DISPATCHES is the installed one, and it drifts from this binary the moment a
        // pin changes without a re-install — worse, crew REWRITES it from a hardcoded copy, so the
        // stale pin restores itself (FINDING-080/084). A stale pin means the run is gated by a
        // validator this engine no longer stands behind, and it reports success either way.
        //
        // REPAIR rather than remove. Removing was the first attempt and it is wrong: `register`
        // overwrites by id, so there is no shadowed built-in left to fall back to — and
        // `domain-extraction` ships only as a drop-in, so removal makes the id UNKNOWN and dispatch
        // fails with "no such workflow", trading a wrong gate for a confusing one. The binary owns
        // this pin (it is the value the vault was seeded with), so writing it into the loaded def is
        // the correction, and the workflow stays available and correctly gated.
        for m in crate::domain_extraction::installed_pin_mismatches(&registry) {
            eprintln!("wicked-core: {m}");
            match registry.repin(&m.workflow, &m.phase, m.expected) {
                true => eprintln!(
                    "wicked-core: repaired installed `{}` phase `{}` to {} for this process; the \
                     file on disk is still stale and will be read again on the next start",
                    m.workflow, m.phase, m.expected
                ),
                false => eprintln!(
                    "wicked-core: could NOT repair `{}` phase `{}`; refusing to serve it",
                    m.workflow, m.phase
                ),
            }
        }
    }
    // ── DES-002 T6: ACP elicitation lifecycle ──────────────────────────────────
    //
    // `lifecycle_maps` — unconditional arc for begin_launch / tombstone_bus_run / cleanup_run.
    //   Always `Some` for real spawn paths (spawn_with_acp_sessions, spawn_with_pty_sessions,
    //   spawn_inner). `None` only in unit tests that do not use the elicitation machinery.
    //
    // `actor_maps` — ACP-delivery arc (deliver, cancel_epoch, EmitEvent suppression, epoch ops).
    //   `Some` for ACP runners (is_acp=true, shares the same Arc as lifecycle_maps).
    //   `None` for PTY and injected runners.
    //
    // Lock ordering: `write_reg` BEFORE `elicitation_maps` (see module-level doc on `run`).
    let lifecycle_maps: Option<Arc<std::sync::Mutex<ElicitationMaps>>> = elicitation_maps.clone();
    let actor_maps: Option<Arc<std::sync::Mutex<ElicitationMaps>>> = if is_acp {
        elicitation_maps.clone()
    } else {
        None
    };
    // `process_gen` — unique UUID minted once per actor lifetime. Threads into `StepInput`
    // so bus consumers can discard completions from a prior daemon restart (stale-result guard).
    // NOT a global singleton: each actor lifetime gets a fresh token.
    let process_gen: uuid::Uuid = uuid::Uuid::new_v4();

    // Panic-safe reaper (Minor): guarantees every PTY child + reader thread is killed/reaped when
    // this function returns — on a clean `Shutdown` (map already drained ⇒ no-op) OR a handler PANIC
    // (which unwinds past the loop; the old end-of-`run` drain ran only on a NORMAL exit, so a panic
    // leaked them — the exact failure DES R1 forbids). Holds its own `pty_map` clone.
    let _pty_reaper = terminal::PtyReaper::new(pty_map.clone());

    // Startup orphan reaper (FINDING-003): worktrees of runs in a TERMINAL status are reaped when
    // clean (the same rule the terminal-status reap applies — so a crash between a run finishing
    // and its reap, or a run predating the reap, converges here instead of surviving restarts);
    // worktrees of LIVE runs are kept (resume); worktrees whose run id the store has never heard
    // of are force-removed. A sessions read failure SKIPS the reap: with liveness unknown, any
    // removal could take a resumable run's checkout, and leaking for one boot is the cheaper error.
    if let Ok(repos) = crate::repo::list_repos(&store) {
        if !repos.is_empty() {
            match crate::domain::all_sessions(&store) {
                Ok(sessions) => {
                    let (live, terminal) = partition_sessions_for_reap(&sessions);
                    crate::repo::reap_orphan_worktrees(&repos, &live, &terminal);
                }
                Err(e) => eprintln!(
                    "wicked-core: skipping the startup worktree reap — cannot read sessions ({e})"
                ),
            }
        }
    }

    // Rust↔wicked-bus bridge (DES-EXEC-001 §2.5): if a bus db is configured via `WICKED_BUS_DB`, spawn
    // the launch poller. It runs on its OWN thread with its OWN SQLite connection to the bus db (a
    // different file from the estate store this actor owns), and reaches this actor ONLY by sending
    // `Command::LaunchRun` over `self_tx` — the exact self_tx write-back pattern the unit workers use.
    // So a blocking bus poll can never stall the single writer. Opt-in via env so existing embeddings
    // /tests (env unset) are unaffected. The `bus_stop` flag + join on loop-exit below guarantee the
    // poller thread is not leaked when the last `Core` drops.
    let bus_stop = Arc::new(AtomicBool::new(false));
    let bus_bridge: Option<std::thread::JoinHandle<()>> = std::env::var("WICKED_BUS_DB")
        .ok()
        .filter(|p| !p.is_empty())
        .map(|bus_db| {
            crate::bus::spawn_run_requested_poller(
                bus_db,
                self_tx.clone(),
                crate::registry_roster(),
                crate::scope::EntityMode::Shared,
                std::time::Duration::from_millis(500),
                bus_stop.clone(),
            )
        });

    // Law 1 EXECUTION-MEDIATION SEAM (DES-EXEC-001 §2.3) — OPT-IN. Resolve the bus db to mediate
    // execution over: the explicit `spawn_with_engine_exec` override wins; otherwise the env gate
    // `WICKED_BUS_EXEC` (any non-empty value) turns it on against `WICKED_BUS_DB`. When OFF (the default),
    // `dispatch_unit` spawns the in-process worker exactly as before and NONE of the exec threads run.
    let exec_bus_db: Option<String> = exec_bus.or_else(|| {
        let on = std::env::var("WICKED_BUS_EXEC")
            .ok()
            .filter(|v| !v.is_empty())
            .is_some();
        on.then(|| {
            std::env::var("WICKED_BUS_DB")
                .ok()
                .filter(|p| !p.is_empty())
        })
        .flatten()
    });
    // ARM ATOMICALLY (seam finding #4): the publisher must NOT arm independently of the consumers. A
    // partial arm — publisher on, but a consumer self-disabled (e.g. its bus-db open failed) — would
    // publish `task.dispatched` with NO runner AND bypass the in-process fallback → a permanent wedge.
    // So we first CONFIRM both consumers can initialize (bus-db open ok + durable cursor resolved) via
    // `init_exec_consumers`; only then do we arm the publisher and spawn the consumer threads. If either
    // step fails, exec mode stays OFF and the default in-process path stands.
    let exec_stop = Arc::new(AtomicBool::new(false));
    let exec_handles: Vec<std::thread::JoinHandle<()>> = match &exec_bus_db {
        Some(bus_db) => match crate::cli_runner::init_exec_consumers(bus_db, bus_db, process_gen) {
            Some(consumers) if crate::cli_runner::arm_exec_publisher(bus_db) => {
                let interval = std::time::Duration::from_millis(100);
                let handles = crate::cli_runner::spawn_exec_consumers(
                    consumers,
                    runner.clone(),
                    self_tx.clone(),
                    lifecycle_maps.clone(),
                    process_gen,
                    interval,
                    exec_stop.clone(),
                );
                // RESTART RECOVERY (seam finding #1): re-drive any session persisted `Executing` — a
                // dispatch lost across a crash/restart (task.dispatched never completed, or its result
                // never applied) recovers by re-dispatching the cursor unit under a BUMPED attempt so a
                // genuinely NEW `task.dispatched` is emitted (a same-keyed re-emit would dedup to the
                // terminal row the cli-runner's cursor is already past → no re-run). Armed-mode ONLY, so
                // the default in-process path — which has no cross-restart durability — is untouched.
                redrive_executing_sessions(
                    &mut store,
                    &mut subscribers,
                    &runner,
                    &self_tx,
                    &mut in_flight,
                    &lifecycle_maps,
                    &actor_maps,
                    process_gen,
                    is_acp,
                );
                handles
            }
            _ => Vec::new(),
        },
        None => Vec::new(),
    };
    // Whatever the mode, anything still persisted `Executing` at this point has no worker in THIS
    // process. Armed mode has already redriven what it could; everything left is a zombie and must
    // say so rather than sitting silently (core#124).
    // `in_flight` is what armed mode actually restored. A redriven session KEEPS status
    // `Executing` (only its attempt is bumped), so without this it would be reported as an orphan
    // on every restart that recovered work — the reporter would be loudest exactly when recovery
    // worked.
    report_orphaned_executing_sessions(&store, &mut subscribers, &in_flight);

    while let Ok(cmd) = rx.recv() {
        match cmd {
            Command::Ping(reply) => {
                emit(&mut subscribers, CoreEvent::Heartbeat);
                let _ = reply.send(());
            }
            Command::Sessions(reply) => {
                let _ = reply.send(list_sessions(&store));
            }
            Command::Projects(reply) => {
                let _ = reply.send(list_projects(&store));
            }
            Command::WorkOutput(unit_id, reply) => {
                let _ = reply.send(crate::domain::get_work_output(&store, &unit_id));
            }
            Command::Subscribe(sub) => subscribers.push(sub),
            Command::Launch(spec) => {
                let LaunchSpec {
                    problem,
                    clis,
                    entity_mode,
                    session_id,
                    human_confirm: _, // legacy straight-through path ignores gates
                    repo_ref: _,      // legacy path has no worktree
                    workflow,
                    project_id: _, // legacy path predates projects; filing rides LaunchRun only
                    extra_write_roots: _, // legacy sync path widens nothing (core#259)
                    // Legacy path has no repo and therefore no project graph to bind; it also
                    // never reaches the governed dispatch that would read one.
                    project_graph: _,
                } = spec;
                // Legacy straight-through path: runs to completion on this thread (stub = fast).
                let res = pipeline::run_session(
                    &mut store,
                    clis,
                    &problem,
                    entity_mode,
                    &session_id,
                    workflow.as_deref(),
                    dispatcher.clone(),
                    &mut |ev| emit(&mut subscribers, ev),
                );
                if let Err(e) = res {
                    emit(
                        &mut subscribers,
                        CoreEvent::Error {
                            session: Some(session_id),
                            message: e.to_string(),
                        },
                    );
                }
            }
            Command::ApplyHookDecisions {
                run_id,
                ndjson_path,
                reply,
            } => {
                // The single-writer ingest of out-of-process gate-hook decisions. The hook only
                // appended to the ndjson; here (and ONLY here) do those claims hit the store.
                let _ = reply.send(crate::gate_hook::apply_hook_decisions(
                    &mut store,
                    &run_id,
                    &ndjson_path,
                ));
            }
            Command::LaunchRun { spec, reply } => {
                // Fast path: validate + create Planning stub + emit SessionStarted + reply < 1 ms.
                // The slow planning + council distribution is deferred to ContinueLaunch.
                let run_id = spec.session_id.clone();
                let res: anyhow::Result<String> = (|| {
                    validate_session_id(&run_id)?;
                    if in_flight.contains(&run_id) {
                        return Err(RunBusy(run_id.clone()).into());
                    }
                    if let Ok(Some(existing)) = crate::domain::get_session(&store, &run_id) {
                        if !matches!(
                            existing.status,
                            SessionStatus::Completed
                                | SessionStatus::Cancelled
                                | SessionStatus::Failed
                        ) {
                            return Err(RunExists(
                                run_id.clone(),
                                format!("{:?}", existing.status),
                            )
                            .into());
                        }
                    }
                    let _ = std::fs::remove_dir_all(crate::gate_hook::gov_run_dir(&run_id));
                    // Extra write roots (core#259) are judged in the sync fast path like the
                    // project/preflight checks above: an invalid root is a synchronous Err with
                    // NO session persisted — never a session whose boundary silently reopens
                    // the FINDING-098 pin-rewrite escape.
                    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
                    crate::path_policy::validate_extra_write_roots(
                        &spec.extra_write_roots,
                        home.as_deref(),
                    )
                    .map_err(|e| anyhow::anyhow!(e))?;
                    // Read the repo from the store (fast, no git subprocess) so the worker thread
                    // can create the worktree with its root_path without holding a store handle.
                    // `create_worktree` (git worktree add) is moved off the actor thread below.
                    let (repo_ref, repo_root) = if let Some(ref repo_id) = spec.repo_ref {
                        let repo = crate::repo::get_repo(&store, repo_id)?
                            .ok_or_else(|| anyhow::anyhow!("repo not registered: {repo_id}"))?;
                        (Some(repo_id.clone()), Some(repo.root_path.clone()))
                    } else {
                        (None, None)
                    };
                    // Project filing (DES-PROJECT-001 §2.2): validate BEFORE the stub write so an
                    // invalid/archived project is a synchronous Err with NO session persisted —
                    // never a silent unfiled run the caller believed was filed. The membership row
                    // is built here and committed IN THE SAME BATCH as the stub below.
                    let project_member = match spec.project_id {
                        Some(ref pid) => {
                            let mspec = crate::project::MemberSpec {
                                project_id: pid.clone(),
                                member_kind: crate::project::MEMBER_KIND_RUN.to_string(),
                                member_ref: run_id.clone(),
                                meta: None,
                                attached_by: "api".to_string(),
                            };
                            match crate::project::validate_attach(&store, &mspec)? {
                                // A re-launch of a terminal run that is already filed: keep the row.
                                Some(existing) => Some(existing),
                                None => Some(crate::project::member_from_spec(
                                    &mspec,
                                    crate::interaction::now_millis(),
                                )),
                            }
                        }
                        None => None,
                    };
                    // Governed unit limit check (fast, no LLM): reject over-limit runs BEFORE
                    // creating the stub so callers receive a synchronous Err, preserving the
                    // contract that an error at launch means no session was persisted.
                    let selected_def =
                        pipeline::resolve_workflow_def(spec.workflow.as_deref(), Some(&registry))?;
                    // Tool-dependency preflight (core#120) belongs HERE, in the sync fast path:
                    // pre_distribute's check fires during deferred ContinueLaunch, AFTER the caller
                    // already got a run id — a refused run must instead be a synchronous Err with
                    // no session persisted ("the process never started").
                    if let Some(def) = &selected_def {
                        crate::workflow::preflight_tool_phases(def)?;
                    }
                    let n_units = match &selected_def {
                        Some(def) => crate::plan::plan_from_def(def, &spec.problem, &run_id).len(),
                        None => crate::plan::plan_units(&spec.problem, &run_id).len(),
                    };
                    if n_units as u32 > DENY_PHASE_SPAN {
                        anyhow::bail!(
                            "run has {n_units} units, exceeding the {DENY_PHASE_SPAN}-unit governed limit; \
                             split the problem into smaller runs"
                        );
                    }
                    // Write a Planning stub so GET /runs shows the session immediately.
                    // workdir is not yet known (worktree creation happens off-thread below);
                    // it will be set to the resolved path in the WorktreeReady handler.
                    let cli_keys: Vec<String> = spec.clis.iter().map(|c| c.key.clone()).collect();
                    let collection_scope = match spec.entity_mode {
                        EntityMode::Shared => {
                            Some(resolve_scope(spec.entity_mode, &run_id, "shared"))
                        }
                        EntityMode::Isolated => None,
                    };
                    let stub = AgentSession {
                        id: run_id.clone(),
                        workflow_id: format!("wf-{run_id}"),
                        problem: spec.problem.clone(),
                        entity_mode: spec.entity_mode,
                        collection_scope,
                        clis: cli_keys,
                        status: SessionStatus::Planning,
                        human_confirm: spec.human_confirm,
                        unit_ix: 0,
                        attempt: 0,
                        workdir: None, // resolved off-thread; updated in WorktreeReady
                        repo_ref: repo_ref.clone(),
                        extra_write_roots: spec.extra_write_roots.clone(),
                        project_graph: spec.project_graph.clone(),
                        archived_at: None,
                        archive_note: None,
                    };
                    // ONE batch: the launch record and (when filed) its membership commit together
                    // — a crash between "run exists" and "run is in the project" cannot happen.
                    let mut launch_nodes = vec![stub.to_node()];
                    if let Some(ref member) = project_member {
                        launch_nodes.push(member.to_node());
                    }
                    crate::domain::put_nodes(&mut store, &launch_nodes)?;
                    emit(
                        &mut subscribers,
                        CoreEvent::SessionStarted {
                            session: run_id.clone(),
                            problem: spec.problem.clone(),
                            workflow_id: selected_def.as_ref().map(|d| d.id.clone()),
                            cli_count: spec.clis.len() as u32,
                            governed: in_process_governance().is_some(),
                            entity_mode: match spec.entity_mode {
                                EntityMode::Shared => "shared".to_string(),
                                EntityMode::Isolated => "isolated".to_string(),
                            },
                        },
                    );
                    in_flight.insert(run_id.clone());
                    // For repo-present runs, spawn a worker so `git worktree add` (which can take
                    // seconds on large repos) does not block the actor. For repo-less runs there is
                    // no git I/O; enqueue WorktreeReady directly on the actor thread to avoid an
                    // unnecessary thread spawn for the common case.
                    if let (Some(root), Some(ref_id)) = (repo_root, repo_ref) {
                        let tx = self_tx.clone();
                        let rid = run_id.clone();
                        std::thread::spawn(move || {
                            let cmd = match crate::repo::create_worktree(&root, &rid) {
                                Ok(wt) => Command::WorktreeReady {
                                    spec,
                                    repo_ref: Some(ref_id),
                                    workdir: Some(wt.to_string_lossy().to_string()),
                                },
                                Err(e) => Command::WorktreeFailed {
                                    run_id: rid,
                                    error: e.to_string(),
                                },
                            };
                            let _ = tx.send(cmd);
                        });
                    } else {
                        // No worktree needed; post WorktreeReady directly so WorktreeReady's handler
                        // runs on the next actor iteration without an extra thread lifecycle.
                        let _ = self_tx.send(Command::WorktreeReady {
                            spec,
                            repo_ref: None,
                            workdir: None,
                        });
                    }
                    Ok(run_id)
                })();
                let _ = reply.send(res);
            }
            Command::ContinueLaunch {
                spec,
                repo_ref,
                workdir,
            } => {
                // Fast half (actor thread): plan + store writes only, no blocking council call.
                // SessionStarted was already emitted; session_already_started=true skips the dupe.
                let run_id = spec.session_id.clone();
                match pipeline::pre_distribute(
                    &mut store,
                    &spec.clis,
                    &spec.problem,
                    spec.entity_mode,
                    &run_id,
                    spec.human_confirm,
                    repo_ref,
                    workdir,
                    spec.extra_write_roots.clone(),
                    spec.project_graph.clone(),
                    spec.workflow.as_deref(),
                    &mut |ev| emit(&mut subscribers, ev),
                    Some(&registry),
                    true, // session stub already created + SessionStarted already emitted
                    in_process_governance().is_some(), // keep governed accurate even when unused today
                ) {
                    Err(e) => {
                        in_flight.remove(&run_id);
                        fail_run_by_id(&mut store, &mut subscribers, &runner, &self_tx, &run_id, e);
                    }
                    Ok(pre) => {
                        // Blocking half (worker thread): convene the council off the actor thread
                        // so the actor stays responsive (serves reads, handles gates) while the
                        // council votes. Posts PlanReady or PlanFailed back when done.
                        let tx = self_tx.clone();
                        let disp = dispatcher.clone();
                        std::thread::spawn(move || {
                            let sid = pre.session_id.clone();
                            let relay = council_event_relay(tx.clone());
                            let result =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    crate::distribute::distribute_units_on(
                                        &pre.units,
                                        &pre.clis,
                                        &sid,
                                        None,
                                        &disp,
                                        Some(relay),
                                    )
                                }));
                            match result {
                                Ok(Ok(distributions)) => {
                                    let _ = tx.send(Command::PlanReady {
                                        run_id: sid,
                                        pre,
                                        distributions,
                                    });
                                }
                                Ok(Err(e)) => {
                                    let _ = tx.send(Command::PlanFailed {
                                        run_id: sid,
                                        error: e.to_string(),
                                    });
                                }
                                Err(_panic) => {
                                    let _ = tx.send(Command::PlanFailed {
                                        run_id: sid,
                                        error: {
                                            let payload = _panic;
                                            payload
                                                .downcast_ref::<&str>()
                                                .map(|s| {
                                                    format!("distribution thread panicked: {s}")
                                                })
                                                .or_else(|| {
                                                    payload.downcast_ref::<String>().map(|s| {
                                                        format!("distribution thread panicked: {s}")
                                                    })
                                                })
                                                .unwrap_or_else(|| {
                                                    "distribution thread panicked".to_string()
                                                })
                                        },
                                    });
                                }
                            }
                        });
                    }
                }
            }
            Command::WorktreeReady {
                spec,
                repo_ref,
                workdir,
            } => {
                // The worktree-creation worker finished successfully. Update the Planning stub with
                // the resolved workdir, then proceed with pre_distribute + council distribution
                // (same logic as ContinueLaunch — SessionStarted already emitted).
                let run_id = spec.session_id.clone();
                // Guard: the run may have been cancelled while the worktree thread was running.
                // If it is already terminal, discard the result — do not resurrect the run.
                let session_check = crate::domain::get_session(&store, &run_id);
                let already_terminal = match &session_check {
                    Ok(Some(s)) => matches!(
                        s.status,
                        SessionStatus::Completed | SessionStatus::Cancelled | SessionStatus::Failed
                    ),
                    Ok(None) => true, // session purged — treat as terminal
                    Err(e) => {
                        // Store read failure: cannot determine terminal state safely.
                        // Clean up the worktree and emit an error so the UI learns about it.
                        in_flight.remove(&run_id);
                        if let Some(ref path) = workdir {
                            let _ = std::fs::remove_dir_all(path);
                        }
                        emit_run_error(
                            &mut subscribers,
                            &run_id,
                            anyhow::anyhow!("store error in WorktreeReady guard: {e}"),
                        );
                        continue;
                    }
                };
                if already_terminal {
                    in_flight.remove(&run_id);
                    // Spawn cleanup off the actor thread — git worktree remove can be slow
                    // and must not stall NAPI calls waiting on the actor.
                    if let Some(ref ref_id) = repo_ref {
                        if let Ok(Some(repo)) = crate::repo::get_repo(&store, ref_id) {
                            let root = repo.root_path.clone();
                            let rid = run_id.clone();
                            std::thread::spawn(move || crate::repo::remove_worktree(&root, &rid));
                        }
                    } else if let Some(path) = workdir {
                        std::thread::spawn(move || {
                            let _ = std::fs::remove_dir_all(path);
                        });
                    }
                    continue; // Stay in the actor loop — do NOT return/kill the actor.
                }
                if let Ok(Some(mut s)) = crate::domain::get_session(&store, &run_id) {
                    s.workdir = workdir.clone();
                    if let Err(e) = put_node(&mut store, s.to_node()) {
                        // Store write failure — cannot persist workdir; fail the session rather
                        // than proceeding with an inconsistent store state.
                        in_flight.remove(&run_id);
                        if let Some(ref ref_id) = repo_ref {
                            if let Ok(Some(repo)) = crate::repo::get_repo(&store, ref_id) {
                                let root = repo.root_path.clone();
                                let rid = run_id.clone();
                                std::thread::spawn(move || {
                                    crate::repo::remove_worktree(&root, &rid)
                                });
                            }
                        } else if let Some(ref path) = workdir {
                            let p = path.clone();
                            std::thread::spawn(move || {
                                let _ = std::fs::remove_dir_all(p);
                            });
                        }
                        emit_run_error(
                            &mut subscribers,
                            &run_id,
                            anyhow::anyhow!("failed to persist workdir: {e}"),
                        );
                        continue;
                    }
                }
                match pipeline::pre_distribute(
                    &mut store,
                    &spec.clis,
                    &spec.problem,
                    spec.entity_mode,
                    &run_id,
                    spec.human_confirm,
                    repo_ref.clone(),
                    workdir.clone(),
                    spec.extra_write_roots.clone(),
                    spec.project_graph.clone(),
                    spec.workflow.as_deref(),
                    &mut |ev| emit(&mut subscribers, ev),
                    Some(&registry),
                    true, // session stub already created + SessionStarted already emitted
                    in_process_governance().is_some(), // keep governed accurate even when unused today
                ) {
                    Err(e) => {
                        in_flight.remove(&run_id);
                        if let Some(ref ref_id) = repo_ref {
                            if let Ok(Some(repo)) = crate::repo::get_repo(&store, ref_id) {
                                let root = repo.root_path.clone();
                                let rid = run_id.clone();
                                std::thread::spawn(move || {
                                    crate::repo::remove_worktree(&root, &rid)
                                });
                            }
                        } else if let Some(ref path) = workdir {
                            let p = path.clone();
                            std::thread::spawn(move || {
                                let _ = std::fs::remove_dir_all(p);
                            });
                        }
                        fail_run_by_id(&mut store, &mut subscribers, &runner, &self_tx, &run_id, e);
                    }
                    Ok(pre) => {
                        let tx = self_tx.clone();
                        let disp = dispatcher.clone();
                        std::thread::spawn(move || {
                            let sid = pre.session_id.clone();
                            let relay = council_event_relay(tx.clone());
                            let result =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    crate::distribute::distribute_units_on(
                                        &pre.units,
                                        &pre.clis,
                                        &sid,
                                        None,
                                        &disp,
                                        Some(relay),
                                    )
                                }));
                            match result {
                                Ok(Ok(distributions)) => {
                                    let _ = tx.send(Command::PlanReady {
                                        run_id: sid,
                                        pre,
                                        distributions,
                                    });
                                }
                                Ok(Err(e)) => {
                                    let _ = tx.send(Command::PlanFailed {
                                        run_id: sid,
                                        error: e.to_string(),
                                    });
                                }
                                Err(_panic) => {
                                    let _ = tx.send(Command::PlanFailed {
                                        run_id: sid,
                                        error: {
                                            let payload = _panic;
                                            payload
                                                .downcast_ref::<&str>()
                                                .map(|s| {
                                                    format!("distribution thread panicked: {s}")
                                                })
                                                .or_else(|| {
                                                    payload.downcast_ref::<String>().map(|s| {
                                                        format!("distribution thread panicked: {s}")
                                                    })
                                                })
                                                .unwrap_or_else(|| {
                                                    "distribution thread panicked".to_string()
                                                })
                                        },
                                    });
                                }
                            }
                        });
                    }
                }
            }
            Command::WorktreeFailed { run_id, error } => {
                // The off-thread worktree creation failed. Mark the session Failed and surface
                // the error as a CoreEvent::Error so the UI / bus bridge learns about it.
                // Guard: only update if the session is not already in a terminal state (e.g.
                // it may have been cancelled while the worktree thread was running).
                in_flight.remove(&run_id);
                if let Ok(Some(s)) = crate::domain::get_session(&store, &run_id) {
                    // Best-effort: prune any stale git worktree metadata left by a partial
                    // `git worktree add`. Spawned off the actor thread — git can be slow.
                    if let Some(ref ref_id) = s.repo_ref {
                        if let Ok(Some(repo)) = crate::repo::get_repo(&store, ref_id) {
                            let root = repo.root_path.clone();
                            let rid = run_id.clone();
                            std::thread::spawn(move || crate::repo::remove_worktree(&root, &rid));
                        }
                    }
                }
                fail_run_by_id(
                    &mut store,
                    &mut subscribers,
                    &runner,
                    &self_tx,
                    &run_id,
                    anyhow::anyhow!("{error}"),
                );
            }
            Command::PlanReady {
                run_id,
                mut pre,
                distributions,
            } => {
                // Guard: the run may have been cancelled while the council thread was running.
                // If it is already terminal, discard the result — do not resurrect the run.
                let already_terminal = crate::domain::get_session(&store, &run_id)
                    .ok()
                    .flatten()
                    .map(|s| {
                        matches!(
                            s.status,
                            SessionStatus::Completed
                                | SessionStatus::Cancelled
                                | SessionStatus::Failed
                        )
                    })
                    .unwrap_or(true); // unknown run → treat as terminal
                if already_terminal {
                    in_flight.remove(&run_id);
                } else {
                    // Write assignments to the store, advance the session to Executing, then
                    // dispatch unit 0 (or pause at a gate).
                    match pipeline::apply_distributions(
                        &mut store,
                        &mut pre,
                        distributions,
                        &mut |ev| emit(&mut subscribers, ev),
                    ) {
                        Err(e) => {
                            in_flight.remove(&run_id);
                            fail_run_by_id(
                                &mut store,
                                &mut subscribers,
                                &runner,
                                &self_tx,
                                &run_id,
                                e,
                            );
                        }
                        Ok(()) => {
                            match advance_or_pause(
                                &mut store,
                                &mut subscribers,
                                &runner,
                                &self_tx,
                                &run_id,
                                0,
                                &lifecycle_maps,
                                &actor_maps,
                                process_gen,
                                is_acp,
                            ) {
                                Ok(Progress::Dispatched) => { /* in_flight set in LaunchRun */ }
                                Ok(Progress::Paused) => {
                                    in_flight.remove(&run_id);
                                }
                                Ok(Progress::Done) => {
                                    in_flight.remove(&run_id);
                                    if let Err(e) = finalize_run(
                                        &mut store,
                                        &mut subscribers,
                                        &runner,
                                        &self_tx,
                                        &run_id,
                                    ) {
                                        emit_run_error(&mut subscribers, &run_id, e);
                                    }
                                }
                                Err(e) => {
                                    // Wedge-prevention: advance_or_pause failed after the session
                                    // was already written to Executing — mark it Failed so the UI
                                    // shows a terminal state, not a permanently-stuck Executing.
                                    in_flight.remove(&run_id);
                                    fail_run_by_id(
                                        &mut store,
                                        &mut subscribers,
                                        &runner,
                                        &self_tx,
                                        &run_id,
                                        e,
                                    );
                                }
                            }
                        }
                    }
                }
            }
            Command::PlanFailed { run_id, error } => {
                in_flight.remove(&run_id);
                // `fail_run_by_id` guards the terminal statuses: do not clobber Cancelled with
                // Failed if the run was cancelled while the council thread was running.
                fail_run_by_id(
                    &mut store,
                    &mut subscribers,
                    &runner,
                    &self_tx,
                    &run_id,
                    anyhow::anyhow!("council distribution failed: {error}"),
                );
            }
            Command::ResumeRun { run_id, reply } => {
                let res = resume_run_inner(
                    &mut store,
                    &mut subscribers,
                    &runner,
                    &self_tx,
                    &mut in_flight,
                    &run_id,
                    &lifecycle_maps,
                    &actor_maps,
                    process_gen,
                    is_acp,
                );
                let _ = reply.send(res);
            }
            Command::ApplyStepResult {
                output,
                agent_verdict,
                process_gen: _, // stale-result guard — consumed by bus consumer; ignored here
                launch_seq: _,
                ack,
            } => {
                let run_id = output.run_id.clone();
                // FINAL FLUSH: this worker's stream is over — emit any coalesced tail still
                // pending in the throttle BEFORE the result folds, so the live stream never ends
                // mid-window (and the run's throttle state cannot leak). Each tail carries the
                // attempt its chunks arrived under (the throttle keys by attempt), so a superseded
                // attempt's tail is never relabeled as the finishing worker's.
                for (ord, attempt, text) in output_throttle.drain_run(&run_id) {
                    emit(
                        &mut subscribers,
                        CoreEvent::UnitOutputDelta {
                            session: run_id.clone(),
                            ord,
                            attempt,
                            text,
                        },
                    );
                }
                match apply_step_result(
                    &mut store,
                    &mut subscribers,
                    &runner,
                    &self_tx,
                    output,
                    agent_verdict,
                    &path,
                    &lifecycle_maps,
                    &actor_maps,
                    process_gen,
                    is_acp,
                ) {
                    // Run reached a TERMINAL state → drop from in_flight + remember the outcome.
                    Ok(StepApplied::Finished) => {
                        in_flight.remove(&run_id);
                        capture_run_outcome(memory.as_mut(), &store, &run_id);
                        if let Some(ack_tx) = ack {
                            let _ = ack_tx.send(());
                        }
                    }
                    // Paused at a gate → not terminal, no capture (avoids a needless store read).
                    Ok(StepApplied::Paused) => {
                        in_flight.remove(&run_id);
                        if let Some(ack_tx) = ack {
                            let _ = ack_tx.send(());
                        }
                    }
                    // Next unit dispatched → still in flight (leave it).
                    Ok(StepApplied::Continuing) => {
                        if let Some(ack_tx) = ack {
                            let _ = ack_tx.send(());
                        }
                    }
                    // A stale/duplicate result for a superseded/terminal run → ignore; do NOT touch
                    // in_flight (a live worker, if any, still owns it).
                    Ok(StepApplied::Stale) => {
                        if let Some(ack_tx) = ack {
                            let _ = ack_tx.send(());
                        }
                    }
                    Err(e) => {
                        emit_run_error(&mut subscribers, &run_id, e);
                        // Never leave a run non-terminal on an apply error — that would wedge the session
                        // at `Executing` (and a campaign node at `Running`) and re-execute the unit on
                        // every restart. Drive a clean terminal `Failed`, mirroring the deny path, so
                        // resume/redrive/relaunch recover cleanly.
                        if let Ok(Some(mut session)) = crate::domain::get_session(&store, &run_id) {
                            if !matches!(
                                session.status,
                                SessionStatus::Completed
                                    | SessionStatus::Cancelled
                                    | SessionStatus::Failed
                            ) {
                                // Report the unit that was actually executing, not a misleading 0.
                                let ord = session.unit_ix as u32;
                                let _ = fail_run(
                                    &mut store,
                                    &mut subscribers,
                                    &runner,
                                    &self_tx,
                                    &mut session,
                                    ord,
                                );
                            }
                        }
                        in_flight.remove(&run_id);
                        // Intentionally do NOT ack on error: the bus consumer sees RecvError and
                        // withholds cursor advancement, triggering a re-delivery. On re-delivery the
                        // run will be terminal (fail_run above) → StepApplied::Stale → ack fires
                        // from the Stale arm above, durably advancing the cursor. `None` on all
                        // non-bus paths so the drop is a no-op there.
                    }
                }
            }
            Command::ConfirmGate {
                run_id,
                decision,
                reply,
            } => {
                let res = confirm_gate(
                    &mut store,
                    &mut subscribers,
                    &runner,
                    &self_tx,
                    &mut in_flight,
                    &run_id,
                    decision,
                    &lifecycle_maps,
                    &actor_maps,
                    process_gen,
                    is_acp,
                );
                let _ = reply.send(res);
            }
            Command::CancelRun { run_id, reply } => {
                // ACP teardown: cancel epoch, signal kill handles (no-op for PTY).
                shared_run_terminal(&run_id, &lifecycle_maps, &write_reg);
                // Universal tombstone + sequence advance (invalidates any in-flight bus tasks).
                if let Some(ref m) = lifecycle_maps {
                    let mut maps = m.lock().unwrap_or_else(|p| p.into_inner());
                    maps.tombstone_run(&run_id);
                    maps.advance_launch_seq(&run_id);
                }
                // Discard any coalesced live-output tail: the operator is discarding the run, and
                // the throttle entry must not outlive it.
                let _ = output_throttle.drain_run(&run_id);
                let res = cancel_run(&mut store, &mut subscribers, &runner, &self_tx, &run_id);
                // Retire launch state now that the sequence has been advanced.
                if let Some(ref m) = lifecycle_maps {
                    let mut maps = m.lock().unwrap_or_else(|p| p.into_inner());
                    maps.retire_launch_state(&run_id);
                }
                in_flight.remove(&run_id);
                let _ = reply.send(res);
            }
            // ── Projects (DES-PROJECT-001) — CoreEvent frames are NOT changed in v1 (§2.2):
            // the daemon emits the bus vocabulary post-commit from these replies.
            Command::ProjectCreate {
                name,
                description,
                reply,
            } => {
                let now = crate::interaction::now_millis();
                let _ = reply.send(crate::project::create_project(
                    &mut store,
                    &name,
                    description,
                    now,
                ));
            }
            Command::ProjectUpdate { id, patch, reply } => {
                let now = crate::interaction::now_millis();
                let _ = reply.send(crate::project::update_project(&mut store, &id, patch, now));
            }
            Command::ProjectMemberAttach { spec, reply } => {
                let now = crate::interaction::now_millis();
                let _ = reply.send(crate::project::attach_member(&mut store, spec, now));
            }
            Command::ProjectMemberDetach {
                project_id,
                member_id,
                reply,
            } => {
                let now = crate::interaction::now_millis();
                let _ = reply.send(crate::project::detach_member(
                    &mut store,
                    &project_id,
                    &member_id,
                    now,
                ));
            }
            Command::RegisterRepo { spec, reply } => {
                let res = crate::repo::register_repo(&mut store, spec);
                if let Ok(entry) = &res {
                    emit(
                        &mut subscribers,
                        CoreEvent::RepoRegistered {
                            repo_ref: entry.id.clone(),
                        },
                    );
                }
                let _ = reply.send(res);
            }
            Command::ListRepos { reply } => {
                let _ = reply.send(crate::repo::list_repos(&store));
            }
            Command::RegisterDenyPolicy {
                phase,
                trigger,
                reply,
            } => {
                let _ = reply.send(register_deny_policy(
                    &mut store, &registry, &phase, &trigger,
                ));
            }
            Command::UpsertPolicy { policy_json, reply } => {
                let _ = reply.send((|| -> anyhow::Result<()> {
                    use wicked_governance::{register_policy, Policy};
                    let policy: Policy = serde_json::from_str(&policy_json)
                        .map_err(|e| anyhow::anyhow!("invalid Policy JSON: {e}"))?;
                    register_policy(&mut store, &policy)
                })());
            }
            Command::UpsertConformanceRule { rule_json, reply } => {
                let _ = reply.send((|| -> anyhow::Result<()> {
                    use wicked_governance::{register_rule, ConformanceRule};
                    let rule: ConformanceRule = serde_json::from_str(&rule_json)
                        .map_err(|e| anyhow::anyhow!("invalid ConformanceRule JSON: {e}"))?;
                    register_rule(&mut store, &rule)
                })());
            }
            Command::RetirePolicy { id, reply } => {
                let _ = reply.send(wicked_governance::retire_policy(&mut store, &id));
            }
            Command::ArchiveRun {
                run_id,
                archived,
                note,
                reply,
            } => {
                let _ = reply.send((|| {
                    let Some(mut session) = crate::domain::get_session(&store, &run_id)? else {
                        return Ok(false); // unknown run → the route answers 404
                    };
                    // Write-off is for FINISHED history only (crew#265): archiving a live run
                    // would hide in-flight work from the default listing — a 409, never a
                    // silent success. Unarchive of a live run is equally nonsensical.
                    if !matches!(
                        session.status,
                        SessionStatus::Completed | SessionStatus::Cancelled | SessionStatus::Failed
                    ) {
                        anyhow::bail!(
                            "run {run_id} is {:?} — only a terminal run can be (un)archived",
                            session.status
                        );
                    }
                    session.archived_at = archived.then(crate::interaction::now_millis);
                    session.archive_note = if archived { note } else { None };
                    crate::domain::put_node(&mut store, session.to_node())?;
                    Ok(true)
                })());
            }
            Command::RetireConformanceRule { id, reply } => {
                let _ = reply.send(wicked_governance::retire_rule(&mut store, &id));
            }
            Command::CliOutputDelta {
                run_id,
                ord,
                attempt,
                chunk,
                process_gen: _,
                launch_seq: _,
            } => {
                // Coalesce into the throttled UnitOutputDelta stream FIRST (borrows), then fan the
                // raw chunk out unchanged (moves it). The raw event keeps its exclusion from the
                // durable log; the coalesced one is what leaves the process. The chunk's IN-BAND
                // `attempt` (stamped at the dispatch site) keys the buffer AND labels the flush —
                // no session read, and a re-dispatch mid-window can neither merge attempts nor
                // mislabel a superseded attempt's late chunks (PR #279 review).
                let flushed =
                    output_throttle.push(&run_id, ord, attempt, &chunk, std::time::Instant::now());
                // The single emit point fans a worker's live output chunk out to subscribers.
                emit(
                    &mut subscribers,
                    CoreEvent::CliOutputDelta {
                        session: run_id.clone(),
                        ord,
                        chunk,
                    },
                );
                if let Some(text) = flushed {
                    emit(
                        &mut subscribers,
                        CoreEvent::UnitOutputDelta {
                            session: run_id,
                            ord,
                            attempt,
                            text,
                        },
                    );
                }
            }
            Command::ResolveElicitation {
                run_id,
                elicitation_id,
                action,
                response,
                reply,
            } => {
                // Deliver the human response to the waiting exec_turn_acp dual-poll loop.
                // actor_maps is Some only for ACP runners; PTY/injected runners return Err.
                let res = match &actor_maps {
                    None => Err(anyhow::anyhow!("elicitation not supported for this runner")),
                    Some(maps) => maps.lock().unwrap_or_else(|e| e.into_inner()).deliver(
                        &run_id,
                        &elicitation_id,
                        action,
                        response,
                    ),
                };
                let _ = reply.send(res);
            }

            Command::EmitEvent(ev) => {
                // ── ElicitationCreated suppression (DES-002 T6) ──────────────────────────
                // Suppress a stale ElicitationCreated when cancel_epoch ran before the actor
                // processed this queued event. Use suppressed_creations + creation_announced.
                if let CoreEvent::ElicitationCreated {
                    ref elicitation_id, ..
                } = ev
                {
                    if let Some(ref maps) = actor_maps {
                        let mut maps = maps.lock().unwrap_or_else(|e| e.into_inner());
                        // take_suppressed_creation removes the marker if present.
                        let was_suppressed = maps.take_suppressed_creation(elicitation_id);
                        // Three suppression conditions:
                        // 1. shutdown_flag: actor shutting down; cancel all pending creations.
                        // 2. was_suppressed: cancel_epoch ran before this event was drained.
                        // 3. !is_pending: worker resolved before actor processed the event.
                        let already_resolved = !maps.is_pending(elicitation_id);
                        if maps.shutdown_flag() || was_suppressed || already_resolved {
                            maps.mark_resolution_suppressed(elicitation_id);
                            tracing::warn!(
                                elicitation_id = %elicitation_id,
                                "elicitation: suppressing stale ElicitationCreated; \
                                 paired resolved will also be suppressed"
                            );
                            continue; // skip run_sessions update and fan-out
                        }
                        // Not suppressed: mark as announced BEFORE releasing lock.
                        maps.mark_creation_announced(elicitation_id);
                    }
                }
                // ── ElicitationResolved suppression (DES-002 T6) ─────────────────────────
                // If the paired ElicitationCreated was suppressed, suppress the resolved event.
                // Subscribers must not receive a terminal event for an elicitation they never saw.
                if let CoreEvent::ElicitationResolved {
                    ref elicitation_id, ..
                } = ev
                {
                    if let Some(ref maps) = actor_maps {
                        let mut maps = maps.lock().unwrap_or_else(|e| e.into_inner());
                        if maps.take_suppressed_resolution(elicitation_id) {
                            tracing::warn!(
                                elicitation_id = %elicitation_id,
                                "elicitation: suppressing ElicitationResolved \
                                 (paired creation was suppressed)"
                            );
                            continue; // skip fan-out — no subscriber saw the creation
                        }
                    }
                }
                // Maintain the run → [(cli_key, terminal_id)] index used by InjectWorkerMessage
                // and ReassignUnit. Only PTY-backed sessions appear here; ACP sessions emit no
                // WorkerSessionStarted events (they have no terminal_id).
                //
                // KNOWN ISSUE: entries are pruned on WorkerSessionClosed but NOT on TerminalExited
                // (natural EOF without an explicit close). A PTY that exits unexpectedly will leave
                // a stale entry until the next WorkerSessionClosed for that terminal_id — inject and
                // reassign may try to write to a terminal already gone. A TerminalExited arm is the
                // correct fix but is intentionally deferred (low frequency + the write fails cleanly).
                match &ev {
                    CoreEvent::WorkerSessionStarted {
                        session,
                        terminal_id,
                        cli_key,
                    } => {
                        run_sessions
                            .entry(session.clone())
                            .or_default()
                            .push((cli_key.clone(), terminal_id.clone()));
                    }
                    CoreEvent::WorkerSessionClosed {
                        session,
                        terminal_id,
                        ..
                    } => {
                        if let Some(v) = run_sessions.get_mut(session) {
                            v.retain(|(_, tid)| tid != terminal_id);
                            if v.is_empty() {
                                run_sessions.remove(session);
                            }
                        }
                    }
                    _ => {}
                }
                emit(&mut subscribers, ev);
            }
            Command::CaptureMemory {
                content,
                scope,
                reply,
            } => {
                let res = match memory.as_mut() {
                    Some(m) => m.capture(
                        content,
                        wicked_estate_memory_core::Scope::parse(&scope),
                        crate::memory::now_secs(),
                    ),
                    None => Err(anyhow::anyhow!("memory store unavailable")),
                };
                let _ = reply.send(res);
            }
            Command::RecallMemory { query, k, reply } => {
                let res = match memory.as_ref() {
                    Some(m) => m.recall(&query, k, crate::memory::now_secs()),
                    None => Ok(Vec::new()),
                };
                let _ = reply.send(res);
            }
            Command::ListMemories {
                scope,
                limit,
                reply,
            } => {
                let res = match memory.as_ref() {
                    Some(m) => m.list(&wicked_estate_memory_core::Scope::parse(&scope), limit),
                    None => Ok(Vec::new()),
                };
                let _ = reply.send(res);
            }
            Command::McpCall { request, reply } => {
                let res = match memory.as_mut() {
                    Some(m) => Ok(m.mcp(&request, crate::memory::now_secs())),
                    None => Err(anyhow::anyhow!("memory store unavailable")),
                };
                let _ = reply.send(res);
            }
            Command::IngestKnowledge {
                title,
                chunks,
                reply,
            } => {
                let res = match knowledge.as_mut() {
                    Some(k) => k.ingest(&title, &chunks, crate::memory::now_secs()),
                    None => Err(anyhow::anyhow!("knowledge store unavailable")),
                };
                let _ = reply.send(res);
            }
            Command::RecallKnowledge { query, k, reply } => {
                let res = match knowledge.as_mut() {
                    Some(kb) => kb.recall(&query, k, crate::memory::now_secs()),
                    None => Ok(Vec::new()),
                };
                let _ = reply.send(res);
            }
            Command::OpenTerminal {
                cwd,
                cmd,
                cols,
                rows,
                governed,
                reply,
            } => {
                let id = terminal::new_id();
                // Spawn the off-actor PTY + reader thread FIRST; only register + announce on success
                // (so a failed open never emits a dangling `TerminalOpened`).
                match terminal::spawn_pty(&id, &cwd, cmd, cols, rows, &pty_map, self_tx.clone()) {
                    Ok(spawned) => {
                        // DES §7: an ungoverned operator shell must be a loud, explicit opt-in.
                        if !governed {
                            eprintln!(
                                "wicked-core: opened UNGOVERNED operator terminal {id} in {} — bypasses the gate-hook (opt-in)",
                                cwd.display()
                            );
                        }
                        terminals.insert(
                            id.clone(),
                            TermReg {
                                next_seq: 0,
                                in_flight: spawned.in_flight,
                                dropped_total: spawned.dropped_total,
                                reported_dropped: 0,
                            },
                        );
                        emit(
                            &mut subscribers,
                            CoreEvent::TerminalOpened {
                                id: id.clone(),
                                cwd: cwd.display().to_string(),
                            },
                        );
                        let _ = reply.send(Ok(id));
                    }
                    Err(e) => {
                        let _ = reply.send(Err(e));
                    }
                }
            }
            Command::TerminalChunk { id, bytes } => {
                // The single emit point: assign the per-terminal seq + fan the chunk out as
                // `TerminalOutput`. A chunk for an already-closed terminal (registry entry gone) is
                // dropped. Mirrors the `CliOutputDelta` streaming path — no store write.
                if let Some(reg) = terminals.get_mut(&id) {
                    let n = bytes.len();
                    let seq = reg.next_seq;
                    reg.next_seq += 1;
                    let bytes_b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                    emit(
                        &mut subscribers,
                        CoreEvent::TerminalOutput {
                            id: id.clone(),
                            seq,
                            bytes_b64,
                        },
                    );
                    // This chunk has left the in-flight window — let the reader send more (SIG-1).
                    reg.in_flight.fetch_sub(n, Ordering::AcqRel);
                    // Degraded marker (SIG-1): if the reader shed output since we last told the
                    // consumer, surface it. `event.rs` is owned by another lane (and the TS binding
                    // matches every `CoreEvent` variant by hand), so we reuse the existing `Error`
                    // event rather than add a `TerminalOutputDropped` variant — the consumer still
                    // learns the stream was lossy.
                    let dropped = reg.dropped_total.load(Ordering::Acquire);
                    if dropped > reg.reported_dropped {
                        let delta = dropped - reg.reported_dropped;
                        reg.reported_dropped = dropped;
                        emit(
                            &mut subscribers,
                            CoreEvent::Error {
                                session: Some(id),
                                message: format!(
                                    "terminal output degraded: dropped {delta} byte(s) of oldest output to bound memory"
                                ),
                            },
                        );
                    }
                }
            }
            Command::CloseTerminal { id, reply } => {
                finish_terminal(&mut terminals, &pty_map, &mut subscribers, &id, true);
                let _ = reply.send(());
            }
            Command::TerminalReaderDone { id } => {
                // Natural EOF: the child exited on its own. Reap + emit `TerminalExited` (once).
                finish_terminal(&mut terminals, &pty_map, &mut subscribers, &id, false);
            }
            // ── Campaign DAG scheduler (DES-CAMPAIGN-001) ────────────────────────────────────────
            Command::LaunchCampaign { def, reply } => {
                let seams = campaign_seams(&dispatcher, &runner, &self_tx, &registry, process_gen);
                let res = crate::campaign::launch(
                    &mut store,
                    &mut subscribers,
                    &mut in_flight,
                    &seams,
                    def,
                );
                let _ = reply.send(res);
            }
            Command::ResumeCampaign { id, reply } => {
                let seams = campaign_seams(&dispatcher, &runner, &self_tx, &registry, process_gen);
                let res = crate::campaign::resume(
                    &mut store,
                    &mut subscribers,
                    &mut in_flight,
                    &seams,
                    &id,
                );
                let _ = reply.send(res);
            }
            Command::CancelCampaign { id, reply } => {
                let seams = campaign_seams(&dispatcher, &runner, &self_tx, &registry, process_gen);
                let res = crate::campaign::cancel(
                    &mut store,
                    &mut subscribers,
                    &mut in_flight,
                    &seams,
                    &id,
                );
                let _ = reply.send(res);
            }
            Command::PauseCampaign { id, reply } => {
                let res = crate::campaign::pause(&mut store, &mut subscribers, &id);
                let _ = reply.send(res);
            }
            Command::ConfirmCampaignGate {
                id,
                node_id,
                decision,
                reply,
            } => {
                let seams = campaign_seams(&dispatcher, &runner, &self_tx, &registry, process_gen);
                let res = crate::campaign::confirm_gate(
                    &mut store,
                    &mut subscribers,
                    &mut in_flight,
                    &seams,
                    &id,
                    &node_id,
                    decision,
                );
                let _ = reply.send(res);
            }
            Command::CampaignStatusQuery { id, reply } => {
                let res = crate::campaign::get_campaign(&store, &id).map(|c| c.map(|c| c.status));
                let _ = reply.send(res);
            }
            Command::CampaignDetailQuery { id, reply } => {
                let res = crate::campaign::get_campaign(&store, &id);
                let _ = reply.send(res);
            }
            Command::CampaignRunFinished { run_id, outcome } => {
                // Deferred reconcile of a per-Run terminal signal (sent from the run's terminal emit
                // points). No-op if the run isn't campaign-owned.
                let seams = campaign_seams(&dispatcher, &runner, &self_tx, &registry, process_gen);
                if let Err(e) = crate::campaign::on_run_finished(
                    &mut store,
                    &mut subscribers,
                    &mut in_flight,
                    &seams,
                    &run_id,
                    outcome,
                ) {
                    emit_run_error(&mut subscribers, &run_id, e);
                }
            }
            Command::CampaignNodeAwaiting { run_id, prompt } => {
                // Deferred: a node's Run hit a HITL gate → free its slot + let independent work run.
                let seams = campaign_seams(&dispatcher, &runner, &self_tx, &registry, process_gen);
                if let Err(e) = crate::campaign::on_node_awaiting(
                    &mut store,
                    &mut subscribers,
                    &mut in_flight,
                    &seams,
                    &run_id,
                    prompt,
                ) {
                    emit_run_error(&mut subscribers, &run_id, e);
                }
            }
            Command::RegisterWorkflow { json, reply } => {
                let result = serde_json::from_str::<crate::workflow::WorkflowDef>(&json)
                    .map_err(|e| anyhow::anyhow!("invalid workflow JSON: {e}"))
                    .and_then(|def| {
                        let id = def.id.clone();
                        registry
                            .register(def)
                            .map(|_| id)
                            .map_err(|e| anyhow::anyhow!("{e}"))
                    });
                let _ = reply.send(result);
            }
            Command::InjectWorkerMessage {
                run_id,
                message,
                target,
                reply,
            } => {
                use crate::command::InjectTarget;
                use std::io::Write;
                let sessions = run_sessions.get(&run_id).cloned().unwrap_or_default();
                if sessions.is_empty() {
                    // No live PTY (ACP-backed run, or workers not yet started): hand the
                    // message to the RUNNER, which delivers it on the next matching unit's
                    // prompt as an operator context block. Previously this was a silent
                    // no-op (broadcast) or an error (targeted), which made the studio's
                    // inject bar a lie for every ACP run.
                    let target_str = match &target {
                        InjectTarget::All => "all".to_string(),
                        InjectTarget::Cli(k) => k.clone(),
                    };
                    if runner.queue_operator_message(&run_id, &target, &message) {
                        emit(
                            &mut subscribers,
                            CoreEvent::WorkerMessageQueued {
                                session: run_id.clone(),
                                message,
                                target: target_str,
                            },
                        );
                        let _ = reply.send(Ok(()));
                    } else if matches!(&target, InjectTarget::Cli(_)) {
                        // Runner without queueing support: preserve the historical contract —
                        // a targeted inject with nowhere to deliver is an error.
                        let _ = reply.send(Err(anyhow::anyhow!(
                            "run {run_id} has no active PTY sessions and the runner cannot \
                             queue; targeted inject undeliverable"
                        )));
                    } else {
                        // Broadcast with nowhere to deliver — the historical skip-with-warning.
                        eprintln!(
                            "[wicked-core] inject: run {run_id} has no active PTY sessions \
                             and the runner cannot queue; skipping"
                        );
                        let _ = reply.send(Ok(()));
                    }
                    continue;
                }
                let target_str = match &target {
                    InjectTarget::All => "all".to_string(),
                    InjectTarget::Cli(k) => k.clone(),
                };
                let msg_bytes = format!("{message}\n");
                let mut emitted = false;
                for (cli_key, terminal_id) in &sessions {
                    let matches = matches!(&target, InjectTarget::All)
                        || matches!(&target, InjectTarget::Cli(k) if k == cli_key);
                    if !matches {
                        continue;
                    }
                    // Acquire the per-terminal writer Arc from the off-actor pty_map.
                    let writer = {
                        let map = terminal::lock(&pty_map);
                        map.get(terminal_id).map(|s| s.writer.clone())
                    };
                    match writer {
                        None => {
                            eprintln!(
                                "[wicked-core] inject: terminal {terminal_id} ({cli_key}) \
                                 not in pty_map — already closed?"
                            );
                        }
                        Some(w) => {
                            let mut w = w.lock().unwrap_or_else(|p| p.into_inner());
                            if let Err(e) =
                                w.write_all(msg_bytes.as_bytes()).and_then(|_| w.flush())
                            {
                                eprintln!(
                                    "[wicked-core] inject write to {terminal_id} ({cli_key}) \
                                     failed: {e}"
                                );
                            } else {
                                emit(
                                    &mut subscribers,
                                    CoreEvent::WorkerMessageInjected {
                                        session: run_id.clone(),
                                        message: message.clone(),
                                        target: target_str.clone(),
                                    },
                                );
                                emitted = true;
                            }
                        }
                    }
                }
                if !emitted {
                    if matches!(&target, InjectTarget::Cli(_)) {
                        let _ = reply.send(Err(anyhow::anyhow!(
                            "no active PTY session matching cli '{target_str}' for run {run_id}"
                        )));
                        continue;
                    }
                    eprintln!(
                        "[wicked-core] inject: no matching PTY session found for run {run_id} \
                         target {target_str}"
                    );
                }
                let _ = reply.send(Ok(()));
            }
            Command::ReassignUnit {
                run_id,
                ord,
                new_cli,
                reply,
            } => {
                // ── validation ──────────────────────────────────────────────────────────────
                let session = match crate::domain::get_session(&store, &run_id) {
                    Ok(Some(s)) => s,
                    Ok(None) => {
                        let _ = reply.send(Err(anyhow::anyhow!("run not found: {run_id}")));
                        continue;
                    }
                    Err(e) => {
                        let _ = reply.send(Err(e));
                        continue;
                    }
                };
                if session.status != crate::domain::SessionStatus::Executing {
                    let _ = reply.send(Err(anyhow::anyhow!(
                        "run {run_id} is not Executing (status: {:?}); \
                         can only reassign a unit that is currently running",
                        session.status
                    )));
                    continue;
                }
                let units = match crate::domain::session_units(&store, &run_id) {
                    Ok(u) => u,
                    Err(e) => {
                        let _ = reply.send(Err(e));
                        continue;
                    }
                };
                let cursor_unit = match units.get(session.unit_ix) {
                    Some(u) => u,
                    None => {
                        let _ = reply.send(Err(anyhow::anyhow!(
                            "run {run_id} cursor unit_ix {} out of range",
                            session.unit_ix
                        )));
                        continue;
                    }
                };
                if cursor_unit.ord != ord {
                    let _ = reply.send(Err(anyhow::anyhow!(
                        "run {run_id} cursor unit is ord={} not ord={ord}; \
                         can only reassign the currently-executing unit",
                        cursor_unit.ord
                    )));
                    continue;
                }
                let previous_cli = cursor_unit
                    .assigned_cli
                    .clone()
                    .unwrap_or_else(|| "claude".to_string());
                // Invalidate the old launch before asynchronously killing its ACP session.
                // Otherwise the killed worker can interpret the deliberate disconnect as an
                // adapter failure and start a wrapped-CLI fallback that continues mutating the
                // worktree after reassignment.
                if let Some(ref maps) = lifecycle_maps {
                    let mut maps = maps.lock().unwrap_or_else(|p| p.into_inner());
                    let epoch = maps.current_epoch(&run_id);
                    if epoch > 0 {
                        maps.cancel_epoch(&run_id, epoch);
                    }
                    maps.advance_launch_seq(&run_id);
                }
                // ── close the PTY session (if any) ──────────────────────────────────────────
                if let Some(session_entries) = run_sessions.remove(&run_id) {
                    for (cli_key, terminal_id) in &session_entries {
                        emit(
                            &mut subscribers,
                            CoreEvent::WorkerSessionClosed {
                                session: run_id.clone(),
                                terminal_id: terminal_id.clone(),
                                reason: "reassigned".to_string(),
                            },
                        );
                        finish_terminal(
                            &mut terminals,
                            &pty_map,
                            &mut subscribers,
                            terminal_id,
                            true,
                        );
                        eprintln!(
                            "[wicked-core] reassign: closed PTY session {terminal_id} \
                             ({cli_key}) for run {run_id}"
                        );
                    }
                }
                // ── drop the runner-internal session cache entry ─────────────────────────────
                // `AcpStepRunner::close_cli_session` already spawns its own background thread
                // (blocking kill+wait is safe there); calling directly here avoids an extra hop.
                // `PersistentStepRunner::close_cli_session` just removes from its sessions map
                // (non-blocking — the terminal was already killed by `finish_terminal` above).
                runner.close_cli_session(&run_id, &previous_cli);
                // ── bump attempt ────────────────────────────────────────────────────────────
                let new_attempt = session.attempt.saturating_add(1);
                {
                    let mut s = session.clone();
                    s.attempt = new_attempt;
                    if let Err(e) = crate::domain::put_node(&mut store, s.to_node()) {
                        let _ = reply.send(Err(e));
                        continue;
                    }
                }
                // ── when new_cli: None, re-run the council asynchronously ─────────────────
                match new_cli {
                    Some(cli) => {
                        // Update the unit's assigned_cli in the store and clear
                        // assigned_invocation so the runner re-resolves from the registry —
                        // the new CLI's invocation template may differ from the previous one.
                        let mut updated_units = units.clone();
                        if let Some(u) = updated_units.get_mut(session.unit_ix) {
                            u.assigned_cli = Some(cli.clone());
                            u.assigned_invocation = None;
                            if let Err(e) = crate::domain::put_node(&mut store, u.to_node()) {
                                let _ = reply.send(Err(e));
                                continue;
                            }
                        }
                        // Emit UnitReassigned before dispatching.
                        emit(
                            &mut subscribers,
                            CoreEvent::UnitReassigned {
                                session: run_id.clone(),
                                ord,
                                attempt: new_attempt,
                                previous_cli: previous_cli.clone(),
                                new_cli: Some(cli.clone()),
                            },
                        );
                        // Re-dispatch the cursor unit.
                        match dispatch_unit(
                            &store,
                            &mut subscribers,
                            &runner,
                            &self_tx,
                            &run_id,
                            session.unit_ix,
                            &lifecycle_maps,
                            &actor_maps,
                            process_gen,
                            is_acp,
                        ) {
                            Ok(_) => {
                                let _ = reply.send(Ok(()));
                            }
                            Err(e) => {
                                // Wedge prevention: dispatch failed, so no worker will ever
                                // post ApplyStepResult. Mark the run Failed and surface the error.
                                in_flight.remove(&run_id);
                                fail_run_by_id(
                                    &mut store,
                                    &mut subscribers,
                                    &runner,
                                    &self_tx,
                                    &run_id,
                                    anyhow::anyhow!("{e}"),
                                );
                                let _ = reply.send(Err(e));
                            }
                        }
                    }
                    None => {
                        // Re-run the council off the actor thread; post back ReassignReady.
                        let tx = self_tx.clone();
                        let disp = dispatcher.clone();
                        let run_id_c = run_id.clone();
                        let prev_cli_c = previous_cli.clone();
                        let units_for_council = units.clone();
                        let clis_keys = session.clis.clone();
                        let ord_c = ord;
                        // Emit UnitReassigned now (new_cli=None indicates council re-run).
                        emit(
                            &mut subscribers,
                            CoreEvent::UnitReassigned {
                                session: run_id.clone(),
                                ord,
                                attempt: new_attempt,
                                previous_cli: previous_cli.clone(),
                                new_cli: None,
                            },
                        );
                        let _ = reply.send(Ok(()));
                        std::thread::spawn(move || {
                            // Resolve the session's cli keys to full AgenticCli objects.
                            let all_clis = crate::registry_roster();
                            let clis: Vec<_> = all_clis
                                .into_iter()
                                .filter(|c| clis_keys.contains(&c.key))
                                .collect();
                            // Re-run the council for just this one unit.
                            let unit_slice: Vec<_> = units_for_council
                                .into_iter()
                                .filter(|u| u.ord == ord_c)
                                .collect();
                            let relay = council_event_relay(tx.clone());
                            let result =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    crate::distribute::distribute_units_on(
                                        &unit_slice,
                                        &clis,
                                        &run_id_c,
                                        None,
                                        &disp,
                                        Some(relay),
                                    )
                                }));
                            match result {
                                Ok(Ok(dists)) => {
                                    let new_cli_key = dists
                                        .into_iter()
                                        .next()
                                        .map(|d| d.assigned_cli)
                                        .unwrap_or_else(|| prev_cli_c.clone());
                                    let _ = tx.send(Command::ReassignReady {
                                        run_id: run_id_c,
                                        ord: ord_c,
                                        new_cli: new_cli_key,
                                        reply: {
                                            // Use a dummy channel — the original reply was
                                            // already sent above.
                                            let (s, _) = std::sync::mpsc::channel();
                                            s
                                        },
                                    });
                                }
                                Ok(Err(e)) => {
                                    // Post PlanFailed so the actor thread marks the run Failed
                                    // and emits the error event — prevents a permanent wedge.
                                    let _ = tx.send(Command::PlanFailed {
                                        run_id: run_id_c,
                                        error: format!(
                                            "reassign council re-run failed for ord={ord_c}: {e}"
                                        ),
                                    });
                                }
                                Err(_panic) => {
                                    let _ = tx.send(Command::PlanFailed {
                                        run_id: run_id_c,
                                        error: {
                                            let payload = _panic;
                                            let msg = payload
                                                .downcast_ref::<&str>()
                                                .map(|s| format!("reassign council thread panicked for ord={ord_c}: {s}"))
                                                .or_else(|| {
                                                    payload
                                                        .downcast_ref::<String>()
                                                        .map(|s| format!("reassign council thread panicked for ord={ord_c}: {s}"))
                                                })
                                                .unwrap_or_else(|| format!("reassign council thread panicked for ord={ord_c}"));
                                            msg
                                        },
                                    });
                                }
                            }
                        });
                    }
                }
            }
            Command::ReassignReady {
                run_id,
                ord,
                new_cli,
                reply,
            } => {
                // Apply the council's new assignment and re-dispatch the cursor unit.
                let session = match crate::domain::get_session(&store, &run_id) {
                    Ok(Some(s)) => s,
                    _ => {
                        let _ = reply.send(Err(anyhow::anyhow!("run {run_id} not found")));
                        continue;
                    }
                };
                let mut units = match crate::domain::session_units(&store, &run_id) {
                    Ok(u) => u,
                    Err(e) => {
                        let _ = reply.send(Err(e));
                        continue;
                    }
                };
                if let Some(u) = units.get_mut(session.unit_ix) {
                    if u.ord == ord {
                        u.assigned_cli = Some(new_cli);
                        // Clear assigned_invocation so the runner re-resolves the invocation
                        // template from the registry for the newly assigned CLI.
                        u.assigned_invocation = None;
                        if let Err(e) = crate::domain::put_node(&mut store, u.to_node()) {
                            let _ = reply.send(Err(e));
                            continue;
                        }
                    }
                }
                match dispatch_unit(
                    &store,
                    &mut subscribers,
                    &runner,
                    &self_tx,
                    &run_id,
                    session.unit_ix,
                    &lifecycle_maps,
                    &actor_maps,
                    process_gen,
                    is_acp,
                ) {
                    Ok(_) => {
                        let _ = reply.send(Ok(()));
                    }
                    Err(e) => {
                        // Wedge prevention: no worker will post ApplyStepResult on failure.
                        in_flight.remove(&run_id);
                        fail_run_by_id(
                            &mut store,
                            &mut subscribers,
                            &runner,
                            &self_tx,
                            &run_id,
                            anyhow::anyhow!("{e}"),
                        );
                        let _ = reply.send(Err(e));
                    }
                }
            }
            Command::FailureTriageReady {
                run_id,
                unit_ix,
                attempt,
                decision,
                analysis: judge_analysis,
                failure_excerpt,
                process_gen: _, // stale-triage guard — consumed by bus consumer in T7
                launch_seq: _,
            } => {
                use crate::validator::TriageDecision;
                // Stale guards mirror apply_step_result: only the live cursor attempt of
                // an Executing run may apply a triage decision.
                let mut session = match crate::domain::get_session(&store, &run_id) {
                    Ok(Some(s)) if s.status == SessionStatus::Executing => s,
                    _ => continue,
                };
                if session.unit_ix != unit_ix || session.attempt != attempt {
                    continue;
                }
                let mut units = match crate::domain::session_units(&store, &run_id) {
                    Ok(u) => u,
                    Err(e) => {
                        emit_run_error(&mut subscribers, &run_id, e);
                        continue;
                    }
                };
                let Some(unit) = units.get_mut(unit_ix) else {
                    continue;
                };
                let ord = unit.ord;
                // DENY-DOMINATES policy filter: a judge may PROPOSE any flag, but flags
                // smelling of privilege bypass are never auto-applied — they escalate
                // with the proposal attached so the operator makes that call.
                const DANGER: &[&str] = &["dangerous", "bypass", "allow-all", "skip-permissions"];
                let decision = match decision {
                    TriageDecision::RetryWithFlag(flag)
                        if DANGER.iter().any(|d| flag.to_lowercase().contains(d)) =>
                    {
                        TriageDecision::Escalate(format!(
                            "triage proposed privileged flag {flag} — operator approval required"
                        ))
                    }
                    other => other,
                };
                // The event's `analysis` is the JUDGE'S reasoning for every variant; the
                // flag (when any) is appended so the record shows what will be applied.
                let decision_str = match &decision {
                    TriageDecision::RetryWithFlag(_) => "retry_with_flag",
                    TriageDecision::Retry => "retry",
                    TriageDecision::Escalate(_) => "escalate",
                    TriageDecision::Fail(_) => "fail",
                };
                let event_analysis = match &decision {
                    TriageDecision::RetryWithFlag(f) => {
                        format!("{judge_analysis} (flag: {f})")
                    }
                    TriageDecision::Escalate(a) if judge_analysis.is_empty() => a.clone(),
                    TriageDecision::Fail(r) if judge_analysis.is_empty() => r.clone(),
                    _ => judge_analysis.clone(),
                };
                emit(
                    &mut subscribers,
                    CoreEvent::FailureTriaged {
                        session: run_id.clone(),
                        ord,
                        decision: decision_str.to_string(),
                        analysis: event_analysis,
                    },
                );
                match decision {
                    TriageDecision::RetryWithFlag(flag) => {
                        let cli = unit
                            .assigned_cli
                            .clone()
                            .unwrap_or_else(|| "claude".to_string());
                        let base = unit.assigned_invocation.clone().or_else(|| {
                            crate::registry_roster()
                                .into_iter()
                                .find(|c| c.key == cli)
                                .map(|c| c.headless_invocation)
                        });
                        // Insert the flag ahead of the prompt placeholder (quoted or bare).
                        let already_present =
                            base.as_deref().is_some_and(|inv| inv.contains(&flag));
                        let fixed = base.filter(|_| !already_present).and_then(|inv| {
                            if inv.contains("\"{PROMPT}\"") {
                                Some(inv.replacen(
                                    "\"{PROMPT}\"",
                                    &format!("{flag} \"{{PROMPT}}\""),
                                    1,
                                ))
                            } else if inv.contains("{PROMPT}") {
                                Some(inv.replacen("{PROMPT}", &format!("{flag} {{PROMPT}}"), 1))
                            } else {
                                None
                            }
                        });
                        match fixed {
                            Some(new_invocation) => {
                                unit.assigned_invocation = Some(new_invocation);
                                if let Err(e) = put_node(&mut store, unit.to_node()) {
                                    emit_run_error(&mut subscribers, &run_id, e);
                                    continue;
                                }
                                session.attempt = session.attempt.saturating_add(1);
                                if let Err(e) = put_node(&mut store, session.to_node()) {
                                    emit_run_error(&mut subscribers, &run_id, e);
                                    continue;
                                }
                                if let Err(e) = dispatch_unit(
                                    &store,
                                    &mut subscribers,
                                    &runner,
                                    &self_tx,
                                    &run_id,
                                    unit_ix,
                                    &lifecycle_maps,
                                    &actor_maps,
                                    process_gen,
                                    is_acp,
                                ) {
                                    emit_run_error(&mut subscribers, &run_id, e);
                                }
                            }
                            None => {
                                // Un-insertable OR already present → operator decides,
                                // with wording matching which case it actually is.
                                let why = if already_present {
                                    format!(
                                        "triage proposed flag {flag}, but it is already \
                                         present and the CLI still refused"
                                    )
                                } else {
                                    format!(
                                        "triage proposed flag {flag} but it could not be \
                                         applied to the invocation"
                                    )
                                };
                                let prompt = format!(
                                    "Unit {ord} failed; {why}. Approve to retry unchanged, \
                                     reject to stop the run. Failure: {failure_excerpt}"
                                );
                                if let Err(e) = pause_for_human(
                                    &mut store,
                                    &mut subscribers,
                                    &self_tx,
                                    &mut session,
                                    ord,
                                    // The failed unit's own output is the artifact: the operator
                                    // is judging what unit `ord` produced, not the next phase.
                                    Some(ord),
                                    prompt,
                                ) {
                                    emit_run_error(&mut subscribers, &run_id, e);
                                }
                            }
                        }
                    }
                    TriageDecision::Retry => {
                        session.attempt = session.attempt.saturating_add(1);
                        if let Err(e) = put_node(&mut store, session.to_node()) {
                            emit_run_error(&mut subscribers, &run_id, e);
                            continue;
                        }
                        if let Err(e) = dispatch_unit(
                            &store,
                            &mut subscribers,
                            &runner,
                            &self_tx,
                            &run_id,
                            unit_ix,
                            &lifecycle_maps,
                            &actor_maps,
                            process_gen,
                            is_acp,
                        ) {
                            emit_run_error(&mut subscribers, &run_id, e);
                        }
                    }
                    TriageDecision::Escalate(analysis) => {
                        unit.denial_reason =
                            Some(format!("triage escalation: {analysis} — {failure_excerpt}"));
                        let _ = put_node(&mut store, unit.to_node());
                        let prompt = format!(
                            "Unit {ord} failed and triage escalated: {analysis}. Failure output: \
                             \"{failure_excerpt}\". Approve to retry (optionally amend), reject \
                             to fail the run, or reassign the unit to a different CLI first."
                        );
                        if let Err(e) = pause_for_human(
                            &mut store,
                            &mut subscribers,
                            &self_tx,
                            &mut session,
                            ord,
                            Some(ord),
                            prompt,
                        ) {
                            emit_run_error(&mut subscribers, &run_id, e);
                        }
                    }
                    TriageDecision::Fail(reason) => {
                        unit.status = crate::domain::UnitStatus::Rejected;
                        let excerpt: String = failure_excerpt.chars().take(400).collect();
                        unit.denial_reason = Some(format!(
                            "Worker FAILED on unit {ord} (triage: {reason}): {excerpt}"
                        ));
                        let _ = put_node(&mut store, unit.to_node());
                        emit(
                            &mut subscribers,
                            CoreEvent::StepFailed {
                                session: run_id.clone(),
                                ord,
                                attempt,
                                detail: excerpt,
                                failure_kind: crate::event::StepFailureKind::WorkerError,
                            },
                        );
                        let _ = fail_run(
                            &mut store,
                            &mut subscribers,
                            &runner,
                            &self_tx,
                            &mut session,
                            ord,
                        );
                        in_flight.remove(&run_id);
                    }
                }
            }
            Command::Shutdown => {
                // Stop ACP workers before dropping the actor channel. Ordinary prompt waits
                // do not poll the elicitation tombstone, so they must be interrupted through
                // the registered child kill handles; elicitation waits also observe the flag.
                let mut acp_run_ids = if let Some(ref maps) = lifecycle_maps {
                    let mut maps = maps.lock().unwrap_or_else(|p| p.into_inner());
                    maps.set_shutdown_flag();
                    maps.active_run_ids()
                } else {
                    Vec::new()
                };
                {
                    let registry = write_reg.lock().unwrap_or_else(|p| p.into_inner());
                    acp_run_ids.extend(registry.keys().map(|(run_id, _, _)| run_id.clone()));
                }
                acp_run_ids.sort();
                acp_run_ids.dedup();
                for run_id in acp_run_ids {
                    shared_run_terminal(&run_id, &lifecycle_maps, &write_reg);
                    runner.on_run_complete(&run_id);
                }
                // Reap every live PTY: kill children + join reader threads so no process/thread is
                // leaked when the last `Core` drops (DES §5, R1).
                let ids: Vec<String> = terminals.keys().cloned().collect();
                for id in ids {
                    finish_terminal(&mut terminals, &pty_map, &mut subscribers, &id, true);
                }
                break;
            }
        }
    }
    // Stop + join the bus bridge poller (if any) so it is never leaked past the actor's lifetime.
    bus_stop.store(true, Ordering::SeqCst);
    if let Some(h) = bus_bridge {
        let _ = h.join();
    }
    // Stop + join the exec-mediation threads (cli-runner + task.completed poller) and disarm the
    // actor-thread publisher, so exec mode leaks no thread past the actor's lifetime (DES §5, R1).
    // BOUNDED-join, not unbounded (seam finding #5): the cli-runner may be mid-CLI (an unbounded
    // subprocess) when `stop` is set — the flag is only observed at poll boundaries — so an unbounded
    // join here would wedge shutdown (and the store release) for the CLI's full duration. Wait briefly,
    // then detach and rely on the stop flag + process exit. The consumers hold no store handle.
    exec_stop.store(true, Ordering::SeqCst);
    for h in exec_handles {
        crate::cli_runner::join_bounded(h, std::time::Duration::from_millis(500));
    }
    crate::cli_runner::disarm_exec_publisher();

    // Loop exited (last Core dropped): `store` drops here, releasing the writable connection. Any
    // in-flight worker that posts a result now sends into a closed channel and is harmlessly dropped.
    // The `_pty_reaper` guard (declared above) kills + reaps anything still in the PTY map as it
    // drops — on this clean exit (the `Shutdown` arm already drained the map ⇒ no-op) AND on a
    // handler panic (the leak DES R1 forbids). No explicit drain needed here anymore.
}

/// Tear down one terminal exactly once (idempotent via registry presence): remove it from the shared
/// I/O map, then (via [`terminal::reap_session`]) kill the child's process GROUP on unix + reap it +
/// BOUNDED-join the reader thread, drop the registry entry, and emit `TerminalExited`. `kill=true` for
/// an operator close / shutdown; `kill=false` for a natural EOF (the child already exited — we just
/// reap + join). A second call for the same id (e.g. the reader's `TerminalReaderDone` arriving after
/// a `CloseTerminal` already reaped it) is a no-op, so `TerminalExited` never double-fires. Crucially,
/// this can NEVER block the single actor thread indefinitely (CRIT-1).
fn finish_terminal(
    terminals: &mut HashMap<String, TermReg>,
    map: &PtyMap,
    subscribers: &mut crate::event_log::EventSink,
    id: &str,
    kill: bool,
) {
    if !terminals.contains_key(id) {
        return; // already finished — single-emit guard
    }
    // Take the session out of the shared map, then release the lock BEFORE the (possibly blocking)
    // kill/reap/join so write/resize/close on OTHER terminals never wait on this teardown.
    let session = terminal::lock(map).remove(id);
    let mut status = None;
    if let Some(mut s) = session {
        // Kill the child's whole process GROUP (unix) + reap + BOUNDED-join the reader — this can
        // never block the actor indefinitely (CRIT-1). See `terminal::reap_session`.
        status = terminal::reap_session(&mut s, kill);
        // `s` (writer + master Arcs + child) drops here, closing the fds.
    }
    terminals.remove(id);
    emit(
        subscribers,
        CoreEvent::TerminalExited {
            id: id.to_string(),
            status,
        },
    );
}

/// Outcome of applying a worker step — drives the actor's in-flight bookkeeping.
enum StepApplied {
    /// The run advanced to the next unit (a new worker is in flight).
    Continuing,
    /// The run reached its terminal unit and was finalized.
    Finished,
    /// The run paused at a human-confirm gate (no worker in flight).
    Paused,
    /// The result was stale/duplicate (cursor moved past it, or the run is terminal) and was ignored.
    Stale,
}

/// Whether the next unit should be dispatched, paused for human confirmation, or there are no more.
enum Progress {
    Dispatched,
    Paused,
    Done,
}

/// Bundle the engine seams for the campaign driver (DES-CAMPAIGN-001).
fn campaign_seams<'a>(
    dispatcher: &'a Arc<dyn Dispatcher + Send + Sync>,
    runner: &'a Arc<dyn StepRunner>,
    self_tx: &'a Sender<Command>,
    registry: &'a crate::workflow::WorkflowRegistry,
    process_gen: uuid::Uuid,
) -> crate::campaign::Seams<'a> {
    crate::campaign::Seams {
        dispatcher,
        runner,
        self_tx,
        registry,
        process_gen,
    }
}

/// Notify the campaign layer that a Run reached a terminal state (DES §3): the reconciler maps core's
/// `SessionCompleted`/`SessionFailed`/`RunCancelled` onto a node outcome. Sent to the actor's own
/// queue (`self_tx`) so reconciliation runs as a normal command AFTER the current one — no
/// re-entrancy. Always sent (a non-campaign run is a cheap no-op inverse-lookup on the other side).
fn notify_campaign(self_tx: &Sender<Command>, run_id: &str, outcome: crate::campaign::NodeOutcome) {
    let _ = self_tx.send(Command::CampaignRunFinished {
        run_id: run_id.to_string(),
        outcome,
    });
}

/// Build an [`crate::distribute::EventRelay`] that posts council lifecycle events
/// (convened / deliberated / voted) back to the actor's single emit point via
/// `Command::EmitEvent`, so the UI can watch deliberation live. The `Mutex` makes the
/// captured `Sender` shareable from the relay's `Fn + Sync` closure — the same pattern
/// as the CliOutputDelta back-channel.
fn council_event_relay(tx: Sender<Command>) -> crate::distribute::EventRelay {
    let tx = std::sync::Mutex::new(tx);
    std::sync::Arc::new(move |ev| {
        if let Ok(g) = tx.lock() {
            let _ = g.send(Command::EmitEvent(ev));
        }
    })
}

/// Reject a session id carrying shell-hostile or control characters. Governance now passes scope/phase
/// (which embed the id) to the gate-hook via ENV, so such characters can no longer inject the shell hook
/// command — but rejecting them at ingress is defense-in-depth: a hostile id is a caller/attacker signal,
/// never a legitimate run id, and it also keeps the derived scope strings clean.
fn validate_session_id(run_id: &str) -> anyhow::Result<()> {
    // `/` (and `\`) are rejected because the raw run_id also forms filesystem paths (e.g.
    // `sandbox_for`), where a separator enables directory traversal / absolute-path escape.
    const HOSTILE: &[char] = &[
        '"', '\'', '`', '$', ';', '|', '&', '<', '>', '\\', '/', '\n', '\r', '\0',
    ];
    if run_id.is_empty()
        || run_id.contains("..")
        || run_id
            .chars()
            .any(|c| HOSTILE.contains(&c) || c.is_control())
    {
        anyhow::bail!(
            "invalid session id (empty, `..`, or a shell/path-hostile character): {run_id:?}"
        );
    }
    Ok(())
}

/// The body of `Command::LaunchRun` (also the campaign driver's node launcher, DES §4). Plans +
/// distributes synchronously, then advances unit 0 off-thread (or pauses at a gate). Idempotent by
/// run id: refuses to re-plan over a live run (resume it instead). Returns the run id.
///
/// `registry`: the actor-owned workflow registry (built-ins + runtime-registered defs + file
/// overlay already loaded). Passed to `plan_and_distribute` so `LaunchRun` picks up workflows
/// registered via `Command::RegisterWorkflow` without a process restart.
#[allow(clippy::too_many_arguments)]
pub(crate) fn launch_run_inner(
    store: &mut dyn GraphStore,
    subscribers: &mut crate::event_log::EventSink,
    dispatcher: &Arc<dyn Dispatcher + Send + Sync>,
    runner: &Arc<dyn StepRunner>,
    self_tx: &Sender<Command>,
    in_flight: &mut HashSet<String>,
    spec: LaunchSpec,
    registry: &crate::workflow::WorkflowRegistry,
    lifecycle_maps: &Option<Arc<std::sync::Mutex<ElicitationMaps>>>,
    actor_maps: &Option<Arc<std::sync::Mutex<ElicitationMaps>>>,
    process_gen: uuid::Uuid,
    is_acp: bool,
) -> anyhow::Result<String> {
    let run_id = spec.session_id.clone();
    validate_session_id(&run_id)?;
    if in_flight.contains(&run_id) {
        return Err(RunBusy(run_id).into());
    }
    // Clobber guard: refuse to re-plan over an existing NON-TERMINAL run (would reset its cursor).
    if let Ok(Some(existing)) = crate::domain::get_session(store, &run_id) {
        if !matches!(
            existing.status,
            SessionStatus::Completed | SessionStatus::Cancelled | SessionStatus::Failed
        ) {
            return Err(RunExists(run_id, format!("{:?}", existing.status)).into());
        }
    }
    // FRESH (re-)launch: clear any prior TERMINAL run's per-run governance dir so a stale Deny from that
    // run can't spuriously fail this one (decisions_path_for is never otherwise truncated). resume_run_inner
    // / redrive do NOT clear — they continue the same run's log. A brand-new id has no dir (harmless no-op).
    let _ = std::fs::remove_dir_all(crate::gate_hook::gov_run_dir(&run_id));
    // Same launch-time judgement as the interactive path (core#259): an invalid extra write root
    // is a synchronous Err before anything is planned or persisted.
    {
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        crate::path_policy::validate_extra_write_roots(&spec.extra_write_roots, home.as_deref())
            .map_err(|e| anyhow::anyhow!(e))?;
    }
    // If the run targets a registered repo, create its isolated worktree first.
    let (repo_ref, workdir) = resolve_workdir(store, &spec.repo_ref, &run_id)?;
    pipeline::plan_and_distribute(
        store,
        &spec.clis,
        &spec.problem,
        spec.entity_mode,
        &run_id,
        spec.human_confirm,
        repo_ref,
        workdir,
        spec.extra_write_roots.clone(),
        spec.project_graph.clone(),
        spec.workflow.as_deref(),
        dispatcher,
        &mut |ev| emit(subscribers, ev),
        Some(registry),
        false, // stub not yet created — this path is campaign-driven, needs full setup
        in_process_governance().is_some(), // actor thread: GOV_DB_PATH is set
    )?;
    match advance_or_pause(
        store,
        subscribers,
        runner,
        self_tx,
        &run_id,
        0,
        lifecycle_maps,
        actor_maps,
        process_gen,
        is_acp,
    ) {
        Ok(Progress::Dispatched) => {
            in_flight.insert(run_id.clone());
        }
        Ok(Progress::Paused) => {} // paused at a gate — not in flight
        Ok(Progress::Done) => {
            if let Err(e) = finalize_run(store, subscribers, runner, self_tx, &run_id) {
                emit_run_error(subscribers, &run_id, e);
            }
        }
        // A store-write fault dispatching unit 0 would otherwise leave the run with NO worker and NO
        // terminal signal (wedging a campaign node at `Running` forever). Surface it AND propagate so
        // the caller — the campaign driver — can reconcile the node as Failed. (No stub-path test hits
        // this; standalone `LaunchRun` now replies Err instead of a bare Ok+Error event.)
        Err(e) => {
            let msg = e.to_string();
            emit_run_error(subscribers, &run_id, e);
            return Err(anyhow::anyhow!(
                "run {run_id} failed to dispatch its first unit: {msg}"
            ));
        }
    }
    Ok(run_id)
}

/// Engine-authored prefix of every worker-originated denial (`Worker FAILED on unit …`).
/// `resume_run_inner` keys its seat-rotation decision on it: only a unit whose TERMINAL
/// failure was worker-originated rotates seats on resume — a judged rejection re-dispatches
/// the same seat even when an EARLIER attempt left entries in `worker_failed_clis`
/// (Copilot review on #286).
pub(crate) const WORKER_FAILURE_MARKER: &str = "Worker FAILED on unit";

/// The body of `Command::ResumeRun` (also the campaign driver's crash-resume re-attach, DES §6).
/// Re-advances from the persisted cursor. A terminal run is a no-op (returns its status).
#[allow(clippy::too_many_arguments)]
pub(crate) fn resume_run_inner(
    store: &mut dyn GraphStore,
    subscribers: &mut crate::event_log::EventSink,
    runner: &Arc<dyn StepRunner>,
    self_tx: &Sender<Command>,
    in_flight: &mut HashSet<String>,
    run_id: &str,
    lifecycle_maps: &Option<Arc<std::sync::Mutex<ElicitationMaps>>>,
    actor_maps: &Option<Arc<std::sync::Mutex<ElicitationMaps>>>,
    process_gen: uuid::Uuid,
    is_acp: bool,
) -> anyhow::Result<SessionStatus> {
    if in_flight.contains(run_id) {
        return Err(RunBusy(run_id.to_string()).into());
    }
    let session = match crate::domain::get_session(store, run_id)? {
        Some(s) => s,
        None => anyhow::bail!("run not found: {run_id}"),
    };
    if matches!(
        session.status,
        SessionStatus::Completed | SessionStatus::Cancelled
    ) {
        return Ok(session.status);
    }
    // crew#277: a FAILED run resumes from its cursor unit instead of no-opping — three dogfood
    // runs each burned three verified, gate-approved phases because a seat-level worker error at
    // one unit had no recovery short of a full relaunch. Reset the cursor unit for re-dispatch
    // (done units stay done; "done" is re-derived from evidence, never re-asserted) and put the
    // session back on the executing path. The R1 planning guard below still applies: a run that
    // never planned has no cursor to resume from.
    let session = if session.status == SessionStatus::Failed {
        let mut units = crate::domain::session_units(store, run_id)?;
        // core#282: a unit that reached terminal failure through WORKER errors carries the seats
        // that failed it (`worker_failed_clis`); a resume must not hand it straight back to one
        // of them. Select over the immutable view first (same order + evaluator≠creator
        // exclusions as the in-run failover ladder); the mutation below re-borrows.
        let failover = match units.get(session.unit_ix) {
            Some(u)
                if u.status == crate::domain::UnitStatus::Rejected
                    && !u.worker_failed_clis.is_empty()
                    // The LAST failure must itself be worker-originated: a judged (work-level)
                    // rejection after an earlier failover keeps its seat — rotating there would
                    // discard a seat that produced reviewable work over a stale ledger entry.
                    && u.denial_reason
                        .as_deref()
                        .is_some_and(|r| r.contains(WORKER_FAILURE_MARKER)) =>
            {
                Some(next_failover_seat(&units, session.unit_ix, &session.clis))
            }
            _ => None,
        };
        let Some(unit) = units.get_mut(session.unit_ix) else {
            // Failed with no cursor unit (planning-time failure) — nothing to re-dispatch.
            return Ok(SessionStatus::Failed);
        };
        if unit.status == crate::domain::UnitStatus::Rejected {
            unit.status = crate::domain::UnitStatus::Distributed;
            unit.denial_reason = None;
            match failover {
                // An untried eligible seat remains (terminal failure predates seat
                // exhaustion): the resume re-dispatches THERE, never to a seat that already
                // worker-failed this unit.
                Some(Some(next)) => {
                    if unit.assigned_cli.as_deref() != Some(next.as_str()) {
                        unit.assigned_invocation = crate::registry_roster()
                            .into_iter()
                            .find(|c| c.key == next)
                            .map(|c| c.headless_invocation);
                        unit.assigned_cli = Some(next);
                    }
                }
                // Every eligible seat has worker-failed this unit — that IS why the run went
                // terminal. A resume is an explicit operator/campaign restart (the environment
                // may have been fixed), so it grants a fresh failover budget instead of
                // refusing forever; the seat walk restarts from the currently assigned seat.
                Some(None) => unit.worker_failed_clis.clear(),
                // Not a worker failure (judged rejection) — keep the crew#277 behavior:
                // re-dispatch the same assigned seat.
                None => {}
            }
            put_node(store, unit.to_node())?;
        }
        let mut s = session;
        s.status = SessionStatus::Executing;
        put_node(store, s.to_node())?;
        // No UnitExecuting here: `dispatch_unit` (reached via `advance_or_pause` below) is the
        // single emit point, and a second emission would duplicate the event (Copilot).
        s
    } else {
        session
    };
    // R1 — crash-during-planning guard. A crash between `plan_and_distribute`'s `session=Planning`
    // write and the first unit write (or anywhere before `Executing`) leaves the session in a
    // PRE-EXECUTION status with no complete, distributed unit plan on the store. Advancing from the
    // cursor would hit `units.get(0) == None → Progress::Done → finalize_run` and mis-finalize a run
    // that NEVER planned as `Completed` — a campaign node then reconciles Completed having done zero
    // work (DES §6: resume never re-runs a done node, but a never-planned node is not done). A run
    // that never planned is not "done": fail it. This matches core's run-level contract (halt +
    // operator relaunch, never auto-complete past an incomplete plan) and is the single primitive
    // shared by standalone `ResumeRun` AND the campaign driver's mid-flight re-attach — for a campaign
    // node the ensuing `notify_campaign(Failed)` reconciles it Failed through the same
    // `reconcile_terminal → apply_failure_policy` path as any run failure.
    if matches!(
        session.status,
        SessionStatus::Planning | SessionStatus::Distributing
    ) {
        let mut session = session;
        session.status = SessionStatus::Failed;
        put_node(store, session.to_node())?;
        reap_terminal_worktree(&*store, &session);
        emit(
            subscribers,
            CoreEvent::SessionFailed {
                session: run_id.to_string(),
                ord: 0,
            },
        );
        notify_campaign(self_tx, run_id, crate::campaign::NodeOutcome::Failed);
        return Ok(SessionStatus::Failed);
    }
    // WEDGE-ON-RE-DISPATCH fix (seam finding #2/#3): a resume RE-DISPATCHES the cursor unit, so bump the
    // attempt first — otherwise the re-dispatched unit reuses the prior `(run, unit, attempt)` key and,
    // under exec-mediation, dedups to a terminal task.dispatched row past the cli-runner's cursor → no
    // worker → wedge. Persist the bump before advancing (dispatch reads `session.attempt` from the store).
    // Inert on the default in-process path (nothing branches on `attempt`). Only meaningful when the
    // cursor unit will actually be dispatched (not paused); advancing may pause, which simply no-ops it.
    {
        let mut s = session.clone();
        s.attempt = s.attempt.saturating_add(1);
        put_node(store, s.to_node())?;
    }
    match advance_or_pause(
        store,
        subscribers,
        runner,
        self_tx,
        run_id,
        session.unit_ix,
        lifecycle_maps,
        actor_maps,
        process_gen,
        is_acp,
    )? {
        Progress::Dispatched => {
            in_flight.insert(run_id.to_string());
            Ok(SessionStatus::Executing)
        }
        Progress::Paused => Ok(SessionStatus::AwaitingHuman),
        Progress::Done => {
            finalize_run(store, subscribers, runner, self_tx, run_id)?;
            Ok(SessionStatus::Completed)
        }
    }
}

/// RESTART RECOVERY (seam finding #1) — run ONCE on actor bootstrap when exec-mediation is armed. Any
/// session persisted `Executing` had a unit dispatched that never reached a terminal apply before the
/// process died (the `task.dispatched` was lost, or its `task.completed` was consumed but the apply
/// never persisted). Re-drive it: re-dispatch the cursor unit under a BUMPED attempt so a genuinely NEW
/// `task.dispatched` is emitted (a same-keyed re-emit would dedup to the terminal row the cli-runner's
/// cursor is already past → no re-run → wedge). Armed-mode ONLY — the default in-process path has no
/// cross-restart durability and must stay byte-for-byte unchanged, so it is never re-driven.
/// Surface sessions the store says are `Executing` that this process is NOT going to drive.
///
/// `redrive_executing_sessions` runs ONLY in armed exec mode — deliberately, since the in-process
/// path has no cross-restart durability to redrive against. But the SESSION STATUS is durable either
/// way, so on the default path a restart leaves a run claiming to execute with no worker behind it.
/// Observed live: run `9da47603` sat `executing` / unit `distributed` with zero ACP processes for
/// 35+ minutes. `POST /runs/:id/resume` fixed it instantly, so the recovery path was sound — nothing
/// had told anyone it was needed (core#124).
///
/// This does not resume them: re-dispatching an in-process run across a restart is the durability
/// this path does not claim to have, and doing it silently would be a guess about work that may have
/// half-completed. It makes the state VISIBLE and names the remedy, which is what was missing —
/// "status says executing, reality is no worker" was invisible precisely because nothing said it.
fn report_orphaned_executing_sessions(
    store: &dyn GraphStore,
    subscribers: &mut crate::event_log::EventSink,
    in_flight: &HashSet<String>,
) {
    let sessions = match crate::domain::all_sessions(store) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("wicked-core: could not scan for orphaned runs: {e}");
            return;
        }
    };
    for s in sessions {
        if s.status != SessionStatus::Executing || in_flight.contains(&s.id) {
            continue;
        }
        // `ord` is 1-based. Falling back to 0 would emit an ordinal no unit can have, so a reader
        // cannot tell "unit unknown" from a real position — and the operator line would name it as
        // if it were one. Derive from the cursor instead, and say when the units could not be read.
        let ord = match crate::domain::session_units(store, &s.id) {
            Ok(units) => units
                .get(s.unit_ix)
                .map(|u| u.ord)
                .unwrap_or_else(|| s.unit_ix as u32 + 1),
            Err(e) => {
                eprintln!(
                    "wicked-core: could not read units for orphaned run {} ({e}); reporting the \
                     cursor position instead of a unit ordinal",
                    s.id
                );
                s.unit_ix as u32 + 1
            }
        };
        eprintln!(
            "wicked-core: run {} is persisted `executing` at unit {} but this process did not \
             restore a worker for it — the daemon restarted mid-run. Resume it with \
             `POST /api/v1/runs/{}/resume` (core#124).",
            s.id, ord, s.id
        );
        emit(
            subscribers,
            CoreEvent::RunOrphaned {
                session: s.id.clone(),
                ord,
                detail: format!("POST /api/v1/runs/{}/resume", s.id),
            },
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn redrive_executing_sessions(
    store: &mut dyn GraphStore,
    subscribers: &mut crate::event_log::EventSink,
    runner: &Arc<dyn StepRunner>,
    self_tx: &Sender<Command>,
    in_flight: &mut HashSet<String>,
    lifecycle_maps: &Option<Arc<std::sync::Mutex<ElicitationMaps>>>,
    actor_maps: &Option<Arc<std::sync::Mutex<ElicitationMaps>>>,
    process_gen: uuid::Uuid,
    is_acp: bool,
) {
    let sessions = match crate::domain::all_sessions(store) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("wicked-core: restart re-drive could not list sessions: {e}");
            return;
        }
    };
    for s in sessions {
        if s.status != SessionStatus::Executing || in_flight.contains(&s.id) {
            continue;
        }
        let run_id = s.id.clone();
        let units = crate::domain::session_units(store, &run_id).unwrap_or_default();
        let mut sess = s;
        // Skip any cursor unit that already completed before the crash (a crash between the unit-Done
        // write and the cursor-advance write) so we don't re-dispatch a Done unit → `Stale` wedge.
        while units
            .get(sess.unit_ix)
            .map(|u| u.status == crate::domain::UnitStatus::Done)
            .unwrap_or(false)
        {
            sess.unit_ix += 1;
            sess.attempt = 0;
        }
        // Bump the attempt (findings #1 + #2/#3) and persist BEFORE dispatch (dispatch reads `attempt`
        // from the store) so the re-dispatch mints a fresh idempotency key.
        sess.attempt = sess.attempt.saturating_add(1);
        if let Err(e) = put_node(store, sess.to_node()) {
            emit_run_error(subscribers, &run_id, e);
            continue;
        }
        // Emit CrashRecoveryRedrive before dispatch so the UI sees it before UnitDispatched
        // (which dispatch_unit emits internally). Guard: only emit when a unit exists at the
        // cursor — if not, the run is completing normally and no redrive badge should appear.
        if let Some(unit) = units.get(sess.unit_ix) {
            emit(
                subscribers,
                CoreEvent::CrashRecoveryRedrive {
                    session: run_id.clone(),
                    ord: unit.ord,
                    attempt: sess.attempt,
                },
            );
        }
        match dispatch_unit(
            store,
            subscribers,
            runner,
            self_tx,
            &run_id,
            sess.unit_ix,
            lifecycle_maps,
            actor_maps,
            process_gen,
            is_acp,
        ) {
            Ok(true) => {
                in_flight.insert(run_id);
            }
            // No unit at the cursor (every remaining unit is Done) → the run is actually complete.
            Ok(false) => {
                if let Err(e) = finalize_run(store, subscribers, runner, self_tx, &run_id) {
                    emit_run_error(subscribers, &run_id, e);
                }
            }
            Err(e) => emit_run_error(subscribers, &run_id, e),
        }
    }
}

/// A mechanical trust-grant: insert `flag` into the invocation right after `anchor`.
/// `None` when the anchor is missing or the flag is already present (nothing to heal —
/// the refusal then bubbles up to the operator instead of retrying in a loop).
struct InvocationFix {
    anchor: &'static str,
    flag: &'static str,
}

impl InvocationFix {
    fn apply(&self, invocation: &str) -> Option<String> {
        if invocation.contains(self.flag) || !invocation.contains(self.anchor) {
            return None;
        }
        Some(invocation.replacen(self.anchor, &format!("{} {}", self.anchor, self.flag), 1))
    }
}

/// A classified environment refusal: why the CLI rejected where it ran, and the known
/// mechanical fix when one exists (the automated "answer yes").
struct EnvironmentRefusal {
    reason: &'static str,
    fix: Option<InvocationFix>,
}

/// Classify a failed worker's output as an ENVIRONMENT refusal — the CLI rejecting where
/// it ran (trust prompt, untrusted directory, no TTY) rather than failing the work. Tight,
/// per-CLI signatures only: a broad match here would misroute real work failures into
/// retry loops.
fn environment_refusal(output: &str) -> Option<EnvironmentRefusal> {
    // codex exec outside a git repo: the headless "answer yes" is the skip flag.
    if output.contains("Not inside a trusted directory") {
        return Some(EnvironmentRefusal {
            reason: "codex refused untrusted directory",
            fix: Some(InvocationFix {
                anchor: "codex exec",
                flag: "--skip-git-repo-check",
            }),
        });
    }
    // bubbletea-based TUIs spawned without a TTY — no headless grant exists.
    if output.contains("could not open TTY") {
        return Some(EnvironmentRefusal {
            reason: "CLI requires a TTY",
            fix: None,
        });
    }
    // claude's interactive folder-trust prompt — granting trust is an operator call.
    if output.contains("Do you trust the files in this folder") {
        return Some(EnvironmentRefusal {
            reason: "claude folder-trust prompt",
            fix: None,
        });
    }
    None
}

/// core#282 — the next seat eligible to take over unit `unit_ix` after a WORKER-originated
/// failure (CLI exited nonzero / could not spawn / timed out — never a judged rejection).
///
/// Walks `roster` (the session's cli list) IN ORDER and returns the first seat that
///   (a) has not already worker-failed this unit ([`crate::domain::WorkUnit::worker_failed_clis`]
///       — the callers record the failing seat there BEFORE selecting, so a seat is never
///       repeated until every eligible seat has been tried), and
///   (b) is not the assigned seat of any unit this one `depends_on` — the evaluator≠creator
///       invariant: a failover must never hand an evaluation unit to the seat that created the
///       work it reviews (the exclusion the council encoded at routing).
///
/// `None` ⇒ every eligible seat has been tried; the caller falls through to the terminal
/// failure contract. `assigned_cli: None` means the DEFAULT seat everywhere else in this file —
/// a depends_on creator on the default must be excluded too, not dropped (Copilot).
fn next_failover_seat(
    units: &[crate::domain::WorkUnit],
    unit_ix: usize,
    roster: &[String],
) -> Option<String> {
    let unit = units.get(unit_ix)?;
    let creator_seats: std::collections::HashSet<String> = units
        .iter()
        .filter(|u| {
            u.phase_id()
                .is_some_and(|p| unit.depends_on.iter().any(|d| d == p))
        })
        .map(|u| {
            u.assigned_cli
                .clone()
                .unwrap_or_else(|| "claude".to_string())
        })
        .collect();
    roster
        .iter()
        .find(|c| !unit.worker_failed_clis.contains(c) && !creator_seats.contains(*c))
        .cloned()
}

/// Apply one worker step's output on the single-writer thread: gate the unit, advance the cursor,
/// and either dispatch the next unit or finalize the run.
///
/// IDEMPOTENT by construction: a step result is applied only if its `unit_ix` matches the session
/// cursor AND the unit isn't already `Done`. A stale or duplicate result — e.g. a worker orphaned by
/// a superseded run or a re-delivered message — is ignored (`Stale`). This is the defense the
/// per-actor `in_flight` set cannot provide (it can't see results from a different actor/process).
#[allow(clippy::too_many_arguments)]
fn apply_step_result(
    store: &mut dyn GraphStore,
    subscribers: &mut crate::event_log::EventSink,
    runner: &Arc<dyn StepRunner>,
    self_tx: &Sender<Command>,
    output: crate::workflow::StepOutput,
    agent_verdict: Option<(bool, String)>,
    _db_path: &str,
    lifecycle_maps: &Option<Arc<std::sync::Mutex<ElicitationMaps>>>,
    actor_maps: &Option<Arc<std::sync::Mutex<ElicitationMaps>>>,
    process_gen: uuid::Uuid,
    is_acp: bool,
) -> anyhow::Result<StepApplied> {
    let run_id = output.run_id.clone();
    let mut session = crate::domain::get_session(store, &run_id)?
        .ok_or_else(|| anyhow::anyhow!("run not found: {run_id}"))?;

    // Terminal guard: never apply onto an already-terminal run (e.g. a worker orphaned by Cancel).
    if matches!(
        session.status,
        SessionStatus::Completed | SessionStatus::Cancelled | SessionStatus::Failed
    ) {
        return Ok(StepApplied::Stale);
    }
    // Idempotency guard: only the unit the cursor is currently on, and only once.
    if output.unit_ix != session.unit_ix {
        return Ok(StepApplied::Stale);
    }
    // APPLY-IDEMPOTENCY, attempt-authoritative (seam finding #6): reject a completed carrying a
    // SUPERSEDED attempt of the current cursor unit. The cursor guard above only catches a DIFFERENT
    // unit; a slow/duplicate worker from a PRIOR re-dispatch of the SAME unit (a lower attempt) would
    // otherwise pass the cursor+status checks and mis-apply a stale result. `session.attempt` is the
    // attempt currently in flight for `unit_ix`; anything older is a redelivery — drop it, regardless of
    // unit status. (Equal attempt is the expected current result → apply; a higher attempt cannot exist.)
    if output.attempt < session.attempt {
        return Ok(StepApplied::Stale);
    }
    let mut output = output;
    let mut units = crate::domain::session_units(store, &run_id)?;
    let unit = units
        .get_mut(output.unit_ix)
        .ok_or_else(|| anyhow::anyhow!("unit ix {} out of range for {run_id}", output.unit_ix))?;
    // NO-OP TRIPWIRE (core#126), scoped to SKILL-DRIVEN units: an Ok step whose entire output
    // is the CLI refusing the invocation ("Unknown command: …") is not completed work — it is a
    // silent no-op that must take the failure path (ladder/triage/gate), never fold as Ok.
    // Without this, three "done" units of refusal one-liners reached the coverage gate looking
    // like finished phases. Non-skill units are exempt: free-text work could legitimately quote
    // such a line.
    if unit.skill_ref.is_some() && matches!(output.status, crate::workflow::StepStatus::Ok) {
        let t = output.output.trim();
        if t.starts_with("Unknown command:") && t.len() < 200 {
            output.status = crate::workflow::StepStatus::Failed;
        }
    }
    if unit.status == crate::domain::UnitStatus::Done {
        return Ok(StepApplied::Stale);
    }
    let ord = unit.ord;
    // The FINISHING unit's OWN declared gate (Copy) — captured before the mutable borrow below so a
    // conditional human gate can be evaluated against this unit's own verdict (seam finding #3).
    let unit_gate = unit.gate;

    // (DES-STUDIO-COCKPIT-001 §3 B3/B4) Emit the unit's burn + data-in-use as soon as its result lands —
    // the tokens/files were spent regardless of how the gate later rules. Skipped entirely for seats whose
    // adapter reported nothing (passthrough → `usage: None`, `files: []`), so the default path is silent.
    // Cost: claude reports it directly; else the overridable price table fills it in, else `None` (tokens
    // shown without a fabricated dollar figure — NFR-5).
    if let Some(u) = &output.usage {
        let cli_key = unit
            .assigned_cli
            .clone()
            .unwrap_or_else(|| "claude".to_string());
        let cost_usd = u
            .cost_usd
            .or_else(|| cost_from_price_table(&cli_key, u.input_tokens, u.output_tokens));
        emit(
            subscribers,
            CoreEvent::CliUsage {
                session: run_id.clone(),
                ord,
                attempt: output.attempt,
                input_tokens: u.input_tokens,
                output_tokens: u.output_tokens,
                cache_read_tokens: u.cache_read_tokens,
                cache_creation_tokens: u.cache_creation_tokens,
                cost_usd,
            },
        );
    }
    if !output.files.is_empty() {
        emit(
            subscribers,
            CoreEvent::DataUsed {
                session: run_id.clone(),
                ord,
                files: output.files.clone(),
            },
        );
    }
    // (FINDING-046) The tools this unit's CLI invoked, emitted here for the SAME reason and on the same
    // "adapter saw ≥1" gate as DataUsed above — deliberately NOT gated on `governed`, because an operator
    // watching an ungoverned run was just as blind to a unit's tool activity as a governed one. Passthrough
    // seats report no tools, so the default path stays silent.
    if !output.tools.is_empty() {
        emit(
            subscribers,
            CoreEvent::ToolInvoked {
                session: run_id.clone(),
                ord,
                attempt: output.attempt,
                // `output.tools` is not read after this — move it out rather than clone the vec
                // (up to MAX_TOOLS_RETAINED short strings per attempt). Review nit, #239.
                tools: std::mem::take(&mut output.tools),
            },
        );
    }

    // (EVT-013) UnitOutputCaptured — fires here, after all guards pass and usage/data events land,
    // before the three status branches (Cancelled / Failed / Ok-gate). This is the earliest point
    // where the output is known to belong to the current live unit at the correct attempt.
    // `step_status` mirrors the three observable outcomes so consumers can distinguish them without
    // pattern-matching the downstream events; `output_bytes` surfaces truncation at a glance.
    emit(
        subscribers,
        CoreEvent::UnitOutputCaptured {
            session: run_id.clone(),
            ord,
            attempt: output.attempt,
            output_bytes: output.output.len(),
            step_status: match output.status {
                crate::workflow::StepStatus::Cancelled => "cancelled",
                crate::workflow::StepStatus::Failed => "failed",
                crate::workflow::StepStatus::Ok => "ok",
                // ACP elicitation terminal — routes to run-terminal path, not triage (DES-002 I-7).
                crate::workflow::StepStatus::ElicitationFailed => "elicitation_failed",
            }
            .to_string(),
            governed: output.governed,
        },
    );

    // Structured assumptions (external-transform convention): parse markers from OK
    // output and surface each as an event — needs-research entries are the human-review
    // placeholders ("uses X for Y; semantics unverified"). Parse is bounded (≤16).
    if output.status == crate::workflow::StepStatus::Ok {
        for a in crate::assumptions::parse(&output.output) {
            emit(
                subscribers,
                CoreEvent::AssumptionRecorded {
                    session: run_id.clone(),
                    ord,
                    kind: "external-transform".to_string(),
                    library: a.library,
                    transform: a.transform,
                    known: a.known,
                    detail: a.detail,
                },
            );
        }
    }

    // A worker that CANCELLED the live unit (e.g. P4a subprocess kill) terminates the run as
    // Cancelled — and clears in_flight via `Finished` (NOT `Stale`, which would wedge the run).
    if output.status == crate::workflow::StepStatus::Cancelled {
        session.status = SessionStatus::Cancelled;
        put_node(store, session.to_node())?;
        emit(
            subscribers,
            CoreEvent::RunCancelled {
                session: run_id.clone(),
            },
        );
        notify_campaign(self_tx, &run_id, crate::campaign::NodeOutcome::Cancelled);
        return Ok(StepApplied::Finished);
    }
    // An elicitation that could not be completed is terminal and non-retriable. In
    // particular, do not let it fall through to `apply_and_finish_unit` (which would
    // mark the unit successful) or enter the generic failure-triage/retry ladder.
    if output.status == crate::workflow::StepStatus::ElicitationFailed {
        unit.status = crate::domain::UnitStatus::Rejected;
        let detail = if output.output.trim().is_empty() {
            "ACP elicitation ended before a human response could be applied".to_string()
        } else {
            output.output.trim().chars().take(800).collect()
        };
        unit.denial_reason = Some(detail.clone());
        put_node(store, unit.to_node())?;
        emit(
            subscribers,
            CoreEvent::StepFailed {
                session: run_id.clone(),
                ord,
                attempt: output.attempt,
                detail,
                failure_kind: crate::event::StepFailureKind::WorkerError,
            },
        );
        return Ok(fail_run(
            store,
            subscribers,
            runner,
            self_tx,
            &mut session,
            ord,
        ));
    }
    // A worker FAILURE halts the run as `Failed` (the run-level contract: never complete
    // past a failure) — EXCEPT environment refusals (untrusted dir, missing TTY,
    // folder-trust prompt), which say nothing about the work. Escalation ladder:
    //   1. the refusal carries a KNOWN mechanical fix (a trust flag) → apply it and
    //      retry the SAME CLI — the automated "answer yes / grant access";
    //   2. no safe auto-fix → PAUSE for the operator (awaiting-human) with the error
    //      and the options, instead of failing; reassignment stays an operator choice
    //      via the existing reassign surface;
    //   3. anything unclassified → fail as before.
    // Attempt 0 only, so a repeat refusal after the fix falls through to a real failure.
    if output.status == crate::workflow::StepStatus::Failed {
        // Escalation requires a human in the loop. Autonomous sessions
        // (HumanConfirm::None — the campaign/fail-fast contract) keep mechanical
        // self-heal only; unknown failures fail exactly as they always did.
        let human_present = !matches!(session.human_confirm, crate::domain::HumanConfirm::None);
        if output.attempt == 0 {
            if let Some(refusal) = environment_refusal(&output.output) {
                let cli = unit
                    .assigned_cli
                    .clone()
                    .unwrap_or_else(|| "claude".to_string());
                let effective_invocation = unit.assigned_invocation.clone().or_else(|| {
                    crate::registry_roster()
                        .into_iter()
                        .find(|c| c.key == cli)
                        .map(|c| c.headless_invocation)
                });
                let fixed = refusal
                    .fix
                    .and_then(|f| effective_invocation.as_deref().and_then(|inv| f.apply(inv)));
                emit(
                    subscribers,
                    CoreEvent::StepFailed {
                        session: run_id.clone(),
                        ord,
                        attempt: output.attempt,
                        detail: match (&fixed, human_present) {
                            (Some(_), _) => {
                                format!("{} — auto-granting and retrying {cli}", refusal.reason)
                            }
                            (None, true) => {
                                format!("{} — pausing for operator decision", refusal.reason)
                            }
                            (None, false) => {
                                format!("{} — no operator in the loop; failing", refusal.reason)
                            }
                        },
                        failure_kind: crate::event::StepFailureKind::EnvironmentRefused,
                    },
                );
                if let Some(new_invocation) = fixed {
                    // Self-heal: retry the same CLI with the trust grant applied.
                    unit.assigned_invocation = Some(new_invocation);
                    put_node(store, unit.to_node())?;
                    // Rework semantics: bump the attempt so the stale-result guard
                    // drops any late output from the refused worker.
                    session.attempt = session.attempt.saturating_add(1);
                    put_node(store, session.to_node())?;
                    dispatch_unit(
                        store,
                        subscribers,
                        runner,
                        self_tx,
                        &run_id,
                        output.unit_ix,
                        lifecycle_maps,
                        actor_maps,
                        process_gen,
                        is_acp,
                    )?;
                    return Ok(StepApplied::Continuing);
                }
                // Bubble up: the operator decides. Approve retries the unit (their
                // amendment rides the prompt); Reject stops the run; the reassign
                // surface remains available for "use another CLI". The worker's own
                // words ride both the prompt and denial_reason so the operator sees
                // WHAT the CLI said, not only our classification.
                if !human_present {
                    // No operator to ask — fall through to the standard fail contract.
                } else {
                    let raw_excerpt: String = output.output.trim().chars().take(300).collect();
                    unit.denial_reason = Some(format!(
                        "environment refused ({}): {raw_excerpt}",
                        refusal.reason
                    ));
                    put_node(store, unit.to_node())?;
                    let prompt = format!(
                        "Unit {ord} ({cli}) refused its environment: {} — \"{raw_excerpt}\". \
                     Approve to retry (optionally amend), reject to stop the run, or \
                     reassign the unit to a different CLI first.",
                        refusal.reason
                    );
                    pause_for_human(
                        store,
                        subscribers,
                        self_tx,
                        &mut session,
                        ord,
                        Some(ord),
                        prompt,
                    )?;
                    return Ok(StepApplied::Paused);
                }
            } else if human_present {
                // UNRECOGNIZED failure → agent triage (the generalization of the signature
                // table): a distinct judge seat reads the error and decides the remedy.
                // Blocking CLI work — runs off-thread; the decision returns as
                // `FailureTriageReady` and the run stays Executing meanwhile.
                // StepFailed fires IMMEDIATELY — it signals the worker failure, not the
                // run's fate (which triage now decides), preserving the event contract.
                let failure_excerpt: String = output.output.trim().chars().take(1200).collect();
                emit(
                    subscribers,
                    CoreEvent::StepFailed {
                        session: run_id.clone(),
                        ord,
                        attempt: output.attempt,
                        detail: failure_excerpt.chars().take(400).collect(),
                        failure_kind: crate::event::StepFailureKind::WorkerError,
                    },
                );
                let cli = unit
                    .assigned_cli
                    .clone()
                    .unwrap_or_else(|| "claude".to_string());
                let invocation = unit.assigned_invocation.clone().or_else(|| {
                    crate::registry_roster()
                        .into_iter()
                        .find(|c| c.key == cli)
                        .map(|c| c.headless_invocation)
                });
                let tx = self_tx.clone();
                let runner2 = runner.clone();
                let run_id2 = run_id.clone();
                let unit_ix2 = output.unit_ix;
                let attempt2 = output.attempt;
                let desc = unit.description.clone();
                std::thread::spawn(move || {
                    let ctx = format!("{run_id2}-u{unit_ix2}-a{attempt2}");
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        crate::validator::triage_failure(
                            &failure_excerpt,
                            &desc,
                            &cli,
                            invocation.as_deref().unwrap_or("(unknown)"),
                            &crate::registry_roster(),
                            &*runner2,
                            &ctx,
                        )
                    }));
                    let (decision, analysis) = match result {
                        Ok(Ok(pair)) => pair,
                        // Judge errored or panicked → the operator decides (fail-closed to
                        // escalation, never to silent run death).
                        Ok(Err(e)) => (
                            crate::validator::TriageDecision::Escalate(format!(
                                "triage judge errored: {e}"
                            )),
                            String::new(),
                        ),
                        Err(_) => (
                            crate::validator::TriageDecision::Escalate(
                                "triage judge panicked".to_string(),
                            ),
                            String::new(),
                        ),
                    };
                    let _ = tx.send(Command::FailureTriageReady {
                        run_id: run_id2,
                        unit_ix: unit_ix2,
                        attempt: attempt2,
                        decision,
                        analysis,
                        failure_excerpt,
                        process_gen: None, // set when bus-dispatched in T7
                        launch_seq: 0,
                    });
                });
                return Ok(StepApplied::Continuing);
            }
        }
        // ── AUTONOMOUS SEAT FAILOVER (crew#277, core#282) ────────────────────────────────
        // Three governed runs died at ONE unit each on a seat-level workerError (agy exit-1
        // timeout ×2, copilot hang) while healthy seats sat idle. A CLI process failure says
        // nothing about the WORK — a task judgment surfaces as exit-0 plus a downstream gate
        // deny, never a nonzero exit (see `is_worker_originated_failure`) — so burning the
        // run on it discards every verified phase behind it. Mechanical ladder, no judge
        // required: record the failed seat on the UNIT (persisted, so the guarantee survives
        // a resume), then walk the session roster IN ORDER for the next eligible seat via
        // `next_failover_seat` — never a seat that already worker-failed this unit, never
        // the creator of work this unit reviews (evaluator≠creator) — and re-dispatch there.
        // Only when EVERY eligible seat has been tried does the unit fall through to the
        // standard fail contract (core#282: the previous attempt-counted ladder could re-pick
        // an already-failed seat and let one deterministically-failing seat wedge the phase;
        // the ladder is now bounded by roster size, not an attempt cap).
        // Governed units only: their phases are idempotent (the same argument that gates the
        // wrapped runner's transient retry); the engine's own internal calls are not.
        if output.governed && crate::acp_runner::is_worker_originated_failure(&output.output) {
            let failed_cli = unit
                .assigned_cli
                .clone()
                .unwrap_or_else(|| "claude".to_string());
            // Record BEFORE selecting, and persist even when the roster turns out exhausted:
            // the terminal path below re-persists the unit, and a later RESUME reads this
            // list so it never hands the unit straight back to a seat that already failed it.
            if !unit.worker_failed_clis.contains(&failed_cli) {
                unit.worker_failed_clis.push(failed_cli.clone());
                put_node(store, unit.to_node())?;
            }
            let seats_tried = unit.worker_failed_clis.len();
            let unit_ix = output.unit_ix;
            // Immutable selection ends the `unit` borrow; the branch re-borrows before mutating.
            let next_seat = next_failover_seat(&units, unit_ix, &session.clis);
            if let Some(next) = next_seat {
                let invocation = crate::registry_roster()
                    .into_iter()
                    .find(|c| c.key == next)
                    .map(|c| c.headless_invocation);
                let unit = units
                    .get_mut(unit_ix)
                    .ok_or_else(|| anyhow::anyhow!("unit ix {unit_ix} vanished mid-failover"))?;
                unit.assigned_cli = Some(next.clone());
                unit.assigned_invocation = invocation;
                put_node(store, unit.to_node())?;
                emit(
                    subscribers,
                    CoreEvent::StepFailed {
                        session: run_id.clone(),
                        ord,
                        attempt: output.attempt,
                        detail: format!(
                            "seat '{failed_cli}' failed (worker error); failing over to '{next}' \
                             (seats worker-failed on this unit: {seats_tried}/{})",
                            session.clis.len().max(seats_tried)
                        ),
                        failure_kind: crate::event::StepFailureKind::WorkerError,
                    },
                );
                // Drop any cached (dead) session for the failed seat, then rework: bump the
                // attempt so the stale-result guard discards late output from the failed
                // worker, and re-dispatch through the single funnel.
                runner.close_cli_session(&run_id, &failed_cli);
                session.attempt = session.attempt.saturating_add(1);
                put_node(store, session.to_node())?;
                dispatch_unit(
                    store,
                    subscribers,
                    runner,
                    self_tx,
                    &run_id,
                    unit_ix,
                    lifecycle_maps,
                    actor_maps,
                    process_gen,
                    is_acp,
                )?;
                return Ok(StepApplied::Continuing);
            }
        }
        let unit = units
            .get_mut(output.unit_ix)
            .ok_or_else(|| anyhow::anyhow!("unit ix {} vanished post-failover", output.unit_ix))?;
        unit.status = crate::domain::UnitStatus::Rejected;
        // Capture WHY for the UI: the worker's failure output (bounded). Head+TAIL, not head
        // only — fatal lines come LAST by convention (a tool that prints N warnings then the
        // refusal would otherwise surface as 400 chars of warnings with the actual reason cut).
        let raw = output.output.trim();
        let n = raw.chars().count();
        let raw_snippet: String = if n <= 800 {
            raw.to_string()
        } else {
            let head: String = raw.chars().take(300).collect();
            let tail: String = raw.chars().skip(n - 500).collect();
            format!("{head}\n[… {} chars elided …]\n{tail}", n - 800)
        };
        unit.denial_reason = Some(if raw.is_empty() {
            format!("Worker FAILED on unit {ord} (no output)")
        } else {
            format!("Worker FAILED on unit {ord}: {raw_snippet}")
        });
        put_node(store, unit.to_node())?;
        // `detail` is the raw bounded excerpt — no framing text — so event consumers get
        // the worker's own output without needing to parse the denial_reason framing.
        emit(
            subscribers,
            CoreEvent::StepFailed {
                session: run_id.clone(),
                ord,
                attempt: output.attempt,
                detail: raw_snippet,
                failure_kind: crate::event::StepFailureKind::WorkerError,
            },
        );
        // Best-effort: conform any governed Deny claims that arrived before the worker crashed.
        // Without this the decisions log is never read for a failed unit and the deny evidence
        // is lost — fold_input_denial only runs inside apply_and_finish_unit, which we never reach
        // on this path (core#35).
        if output.governed {
            let phase = crate::scope::unit_phase(ord);
            let _ =
                crate::gate_hook::fold_input_denial(store, &run_id, output.attempt, &phase, true);
        }
        return Ok(fail_run(
            store,
            subscribers,
            runner,
            self_tx,
            &mut session,
            ord,
        ));
    }

    // ── PHASE SUBSTANCE GATE ─────────────────────────────────────────────────────────────────
    // A governed Creator/Neutral phase that folds Ok with (a) under 200 trimmed chars of prose AND
    // (b) an untouched worktree produced NOTHING a downstream phase or evaluator could review.
    // "Done" is re-derived from evidence, and here there is no evidence of ANY kind — so route it
    // to the standard failure path (Rejected + denial_reason + StepFailed + fail_run) instead of
    // letting a one-line "done." fold as a completed phase and starve every unit behind it of
    // context. Evaluator-role units are exempt: their output is a verdict over ANOTHER unit's
    // work, and they carry their own pinned floors (see `builtin_floors`).
    const MIN_SUBSTANCE_CHARS: usize = 200;
    if output.governed
        && unit.role != crate::workflow::PhaseRole::Evaluator
        && output.output.trim().chars().count() < MIN_SUBSTANCE_CHARS
        && worktree_is_clean(session.workdir.as_deref())
    {
        const NO_SUBSTANCE: &str = "phase produced no reviewable substance";
        unit.status = crate::domain::UnitStatus::Rejected;
        unit.denial_reason = Some(NO_SUBSTANCE.to_string());
        put_node(store, unit.to_node())?;
        emit(
            subscribers,
            CoreEvent::StepFailed {
                session: run_id.clone(),
                ord,
                attempt: output.attempt,
                detail: NO_SUBSTANCE.to_string(),
                // NOT WorkerError: the worker ran and exited Ok — this is core's own governance
                // veto, and a consumer that reads workerError as "the CLI process failed" (seat
                // health, failover) must not act on it (PR #279 review).
                failure_kind: crate::event::StepFailureKind::SubstanceRejected,
            },
        );
        // Same evidence fold as the worker-failure path (core#35): conform any governed Deny
        // claims recorded before this rejection so the decisions log is not silently dropped.
        let phase = crate::scope::unit_phase(ord);
        let _ = crate::gate_hook::fold_input_denial(store, &run_id, output.attempt, &phase, true);
        return Ok(fail_run(
            store,
            subscribers,
            runner,
            self_tx,
            &mut session,
            ord,
        ));
    }

    let cli_keys = session.clis.clone();
    let entity_mode = session.entity_mode;
    let workflow_id = session.workflow_id.clone();
    // FINDING-091: the coverage validator must measure the REPO's code graph, not the actor's own
    // store. The actor's own store is ~/.wicked-crew/core.db — the platform's operational graph, holding
    // `agent_session` and `conformance_claim` nodes and NO repository code. Handing it to
    // `wicked-core coverage` yields behavior_bearing=0 by construction, so the pinned criterion
    // "at least one behavior-bearing node" can never be satisfied and a phase that extracted 766
    // behavior-bearing nodes is denied as if it had done nothing. Both campaign runs failed exactly
    // that way while their repo stores held real annotations.
    //
    // `repo_code_graph_db` already exists — FINDING-069 built it so the governed WORKER's estate MCP
    // opens the repo-local graph. The EVALUATOR's path was never wired to it, which is how this
    // survived a finding specifically about repo-graph spellings.
    //
    // No fallback to the actor's store: without a repo there is no graph to measure, and the validator
    // script fails closed on a missing carrier, which is the correct outcome for a pinned phase.
    //
    // And NO widening to the project graph either, even for a run bound to one. This is the
    // EVALUATOR's store, and coverage counts behaviour-bearing nodes to decide whether a phase did
    // its work: measured over a graph holding sibling repos, repo B's annotations would help repo
    // A's criterion pass, so the gate would get easier the more repos a project has. The worker's
    // read tools widen (`run_code_graph_db` at the dispatch site); the measurement must not.
    let coverage_db = repo_code_graph_db(store, session.repo_ref.as_deref());
    let outcome = pipeline::apply_and_finish_unit(
        store,
        unit,
        &output.output,
        &workflow_id,
        entity_mode,
        &run_id,
        // The applied output's attempt matches the launcher's `input.attempt`, so the fold reads the
        // SAME attempt-scoped decisions log the hook wrote (a bumped-attempt retry starts clean).
        output.attempt,
        // The runner's authority on whether IT armed input governance (wrote the marker).
        output.governed,
        &cli_keys,
        agent_verdict.as_ref(),
        &mut |ev| emit(subscribers, ev),
        coverage_db.as_deref(),
    )?;

    // RUN-LEVEL DENY CONTRACT: a governance-DENIED unit halts the run as `Failed` — never advancing
    // past a rejection into a silent `Completed`. (`apply_and_finish_unit` already emitted UnitDenied
    // + persisted the Rejected unit.)
    //
    // EXCEPTION — the CONDITIONAL human gate (seam finding #3): a phase declaring
    // `HumanConfirmIf(VerdictNotPass)` ESCALATES a not-pass verdict to a HUMAN instead of hard-failing.
    // This gate was previously UNREACHABLE — it was only ever consulted for the NEXT unit, but a deny
    // always `fail_ran` first, so the run never advanced to check it. Evaluating it against THIS unit's
    // own completed verdict (before `fail_run`) is what makes it fire. The cursor is left ON this unit,
    // so a human `confirm_gate(Approve)` re-runs it and `Reject` cancels; every OTHER gate deny-dominates.
    if !outcome.approved {
        // Hook-sourced denials are hard policy vetoes — they MUST NOT be routed to human review.
        // HumanConfirmIf is for semantic verdict escalation (evaluator disagrees, human decides);
        // a governance hook bypass can never be "confirmed away" by an operator.
        if !outcome.hook_denied
            && matches!(
                unit_gate,
                crate::workflow::GateSpec::HumanConfirmIf(
                    crate::workflow::GateCond::VerdictNotPass
                )
            )
        {
            // (EVT-010) GateEscalated — the verdict was not-pass AND the gate spec says escalate
            // to human review (not auto-deny). Fires just before AwaitingHuman so the studio can
            // distinguish a pre-unit gate (HumanConfirm, fires before the unit runs) from a
            // verdict escalation (HumanConfirmIf, fires after the unit ran and failed the gate).
            emit(
                subscribers,
                CoreEvent::GateEscalated {
                    session: run_id.clone(),
                    ord,
                    condition: "verdict_not_pass".to_string(),
                    verdict_summary: outcome
                        .denial_reason
                        .clone()
                        .unwrap_or_else(|| "verdict not pass".to_string()),
                },
            );
            let note = unsuppressed_gate_note(session.human_confirm);
            pause_for_human(
                store,
                subscribers,
                self_tx,
                &mut session,
                ord,
                // `HumanConfirmIf(VerdictNotPass)` is declared on unit `ord` itself and fires
                // AFTER its work — unlike a mid-run `HumanConfirm`, the gating unit and the
                // reviewed unit coincide here.
                Some(ord),
                format!("Unit {ord} verdict is NOT PASS — confirm to retry the phase, or reject to cancel the run{note}"),
            )?;
            return Ok(StepApplied::Paused);
        }
        return Ok(fail_run(
            store,
            subscribers,
            runner,
            self_tx,
            &mut session,
            ord,
        ));
    }

    // Approved → advance the resume cursor past the unit we just applied.
    session.unit_ix = output.unit_ix + 1;
    session.attempt = 0;
    put_node(store, session.to_node())?;

    // Advance: dispatch the next unit, pause at its human-confirm gate, or finalize.
    match advance_or_pause(
        store,
        subscribers,
        runner,
        self_tx,
        &run_id,
        session.unit_ix,
        lifecycle_maps,
        actor_maps,
        process_gen,
        is_acp,
    )? {
        Progress::Dispatched => Ok(StepApplied::Continuing),
        Progress::Paused => Ok(StepApplied::Paused),
        Progress::Done => {
            finalize_run(store, subscribers, runner, self_tx, &run_id)?;
            Ok(StepApplied::Finished)
        }
    }
}

/// Halt a run as `Failed` (governance deny or worker failure): persist the terminal status and emit
/// a terminal `SessionFailed`. Returns `Finished` so the actor clears `in_flight`.
fn fail_run(
    store: &mut dyn GraphStore,
    subscribers: &mut crate::event_log::EventSink,
    runner: &Arc<dyn StepRunner>,
    self_tx: &Sender<Command>,
    session: &mut crate::domain::AgentSession,
    ord: u32,
) -> StepApplied {
    session.status = SessionStatus::Failed;
    let _ = put_node(store, session.to_node());
    reap_terminal_worktree(&*store, session);
    emit(
        subscribers,
        CoreEvent::SessionFailed {
            session: session.id.clone(),
            ord,
        },
    );
    notify_campaign(self_tx, &session.id, crate::campaign::NodeOutcome::Failed);
    runner.on_run_complete(&session.id);
    StepApplied::Finished
}

/// FINDING-003 — the terminal transitions that OWN a worktree reap it here, off the actor thread
/// (`git worktree remove` can be slow and must never stall NAPI callers waiting on the actor),
/// through [`crate::repo::reap_worktree_if_clean`]: a clean tree goes, a dirty one (unlanded work)
/// is kept and logged, and the `wicked/<run_id>` branch survives either way. Wired into
/// `finalize_run` (Completed), `fail_run` (Failed, which `fail_run_by_id` also routes through),
/// and resume's crash-during-planning guard. This is NOT every terminal transition — two reach a
/// terminal status without calling this, on purpose: operator `cancel_run` FORCE-removes instead
/// (Cancel is the operator discarding the work), and the WORKER-originated Cancelled path
/// (`apply_step_result`, `StepStatus::Cancelled` — e.g. a P4a subprocess kill) reaps nowhere
/// inline. That last leftover is not lost: the startup reaper
/// ([`crate::repo::reap_orphan_worktrees`]) classifies Cancelled as terminal and re-applies the
/// same clean-only rule to it (and to anything a crash left behind), so a missed reap is a leak
/// until next boot, not forever.
fn reap_terminal_worktree(store: &dyn GraphStore, session: &crate::domain::AgentSession) {
    let Some(repo_id) = session.repo_ref.as_ref() else {
        return;
    };
    let Ok(Some(repo)) = crate::repo::get_repo(store, repo_id) else {
        return;
    };
    let rid = session.id.clone();
    std::thread::spawn(move || {
        let _ = crate::repo::reap_worktree_if_clean(&repo.root_path, &rid);
    });
}

/// Split every session on the store into LIVE (non-terminal — may resume, keeps its worktree) and
/// TERMINAL (finished — its leftover worktree reaps when clean) id sets for the startup orphan
/// reaper (FINDING-003). Statuses are matched exhaustively so a new variant must choose a side;
/// defaulting a new status to LIVE would silently re-grow the leak this fixed, and defaulting to
/// TERMINAL would reap resumable runs.
fn partition_sessions_for_reap(
    sessions: &[crate::domain::AgentSession],
) -> (HashSet<String>, HashSet<String>) {
    let mut live = HashSet::new();
    let mut terminal = HashSet::new();
    for s in sessions {
        match s.status {
            SessionStatus::Completed | SessionStatus::Cancelled | SessionStatus::Failed => {
                terminal.insert(s.id.clone());
            }
            SessionStatus::Planning
            | SessionStatus::Distributing
            | SessionStatus::Executing
            | SessionStatus::AwaitingHuman => {
                live.insert(s.id.clone());
            }
        }
    }
    (live, terminal)
}

/// Pause a run for human confirmation: persist `AwaitingHuman`, emit `AwaitingHuman`, and free a
/// campaign node's slot (deferred, non-re-entrant). Shared by the pre-unit gate ([`advance_or_pause`]),
/// the CONDITIONAL verdict gate (seam finding #3), and the TERMINAL gate (seam finding #4) so all three
/// pause identically. Does NOT move the resume cursor — the caller decides what the cursor points at.
fn pause_for_human(
    store: &mut dyn GraphStore,
    subscribers: &mut crate::event_log::EventSink,
    self_tx: &Sender<Command>,
    session: &mut crate::domain::AgentSession,
    ord: u32,
    reviewing_ord: Option<u32>,
    prompt: String,
) -> anyhow::Result<()> {
    session.status = SessionStatus::AwaitingHuman;
    // DES-PROJECT-001 §5.3: the prompt is DURABLE STATE, not just an event — the session's
    // AwaitingHuman write and the open interaction_request commit in ONE batch, so a skin that
    // was not connected when this fired (or a daemon restarted after it) still finds the prompt.
    let request = crate::interaction::open_gate(
        &session.id,
        ord,
        reviewing_ord,
        &prompt,
        crate::interaction::now_millis(),
    );
    crate::domain::put_nodes(store, &[session.to_node(), request.to_node()])?;
    emit(
        subscribers,
        CoreEvent::AwaitingHuman {
            session: session.id.clone(),
            ord,
            reviewing_ord,
            prompt: prompt.clone(),
        },
    );
    // If this run is a campaign node, free its slot for independent work (DES §6.5). Deferred to a
    // normal command so reconciliation isn't re-entrant; a non-campaign run is a cheap no-op.
    let _ = self_tx.send(Command::CampaignNodeAwaiting {
        run_id: session.id.clone(),
        prompt,
    });
    Ok(())
}

/// Advance one step: if the unit at `unit_ix` should pause for human confirmation, set the run
/// `AwaitingHuman` + emit `AwaitingHuman` and return `Paused`; if there's no unit left, return
/// `Done`; otherwise dispatch the unit off-thread and return `Dispatched`.
#[allow(clippy::too_many_arguments)]
fn advance_or_pause(
    store: &mut dyn GraphStore,
    subscribers: &mut crate::event_log::EventSink,
    runner: &Arc<dyn StepRunner>,
    self_tx: &Sender<Command>,
    run_id: &str,
    unit_ix: usize,
    lifecycle_maps: &Option<Arc<std::sync::Mutex<ElicitationMaps>>>,
    actor_maps: &Option<Arc<std::sync::Mutex<ElicitationMaps>>>,
    process_gen: uuid::Uuid,
    is_acp: bool,
) -> anyhow::Result<Progress> {
    let mut session = crate::domain::get_session(store, run_id)?
        .ok_or_else(|| anyhow::anyhow!("run not found: {run_id}"))?;
    let units = crate::domain::session_units(store, run_id)?;
    let Some(unit) = units.get(unit_ix) else {
        // No unit at the cursor — the run is out of units. But the TERMINAL unit's OWN declared gate
        // fires AFTER its work, and (unlike a mid-run phase) there is no "next unit" whose `should_pause`
        // would honor it — so an unconditional `HumanConfirm` on the LAST phase would be SILENTLY dropped
        // and the run would finalize `Completed` without the required human confirm (seam finding #4).
        // Evaluate the terminal unit's own gate here: pause before finalize. The human's
        // `confirm_gate(Approve)` then dispatches the (absent) cursor unit → `finalize_run` → Completed;
        // `Reject` cancels. A conditional (`HumanConfirmIf`) terminal gate needs NO handling here — a
        // not-pass terminal verdict already paused at finish (finding #3), and a pass needs no gate.
        if let Some(term) = unit_ix.checked_sub(1).and_then(|i| units.get(i)) {
            if matches!(term.gate, crate::workflow::GateSpec::HumanConfirm { .. }) {
                let note = unsuppressed_gate_note(session.human_confirm);
                pause_for_human(
                    store,
                    subscribers,
                    self_tx,
                    &mut session,
                    term.ord,
                    // The terminal gate is always DEF-declared — `term`'s own `GateSpec` is what
                    // this branch matched on — so it is attributable, and to itself.
                    Some(term.ord),
                    format!(
                        "Approve completion after the final phase (unit {}): {}{}",
                        term.ord, term.description, note
                    ),
                )?;
                return Ok(Progress::Paused);
            }
        }
        return Ok(Progress::Done);
    };

    if let Some(reason) = should_pause(&session, &units, unit_ix) {
        // Describe the decision the operator is actually being asked to make. A DEF-declared gate
        // fires AFTER the preceding phase's work, so the artifact under review is that phase's
        // output — naming the upcoming phase instead (FINDING-032) pointed the operator at work
        // that had not run and, in the common case, at a phase that had declared no gate at all.
        let (reviewing_ord, prompt) = match reason {
            PauseReason::DefGate { reviewing_ord } => {
                let done = units.iter().find(|u| u.ord == reviewing_ord).map_or_else(
                    || format!("unit {reviewing_ord}"),
                    |p| p.description.clone(),
                );
                (
                    Some(reviewing_ord),
                    format!(
                        "Approve the output of unit {} ({}) before unit {} runs: {}{}",
                        reviewing_ord,
                        done,
                        unit.ord,
                        unit.description,
                        unsuppressed_gate_note(session.human_confirm)
                    ),
                )
            }
            PauseReason::RunLevel => (
                None,
                format!(
                    "Approve unit {} before it runs: {}",
                    unit.ord, unit.description
                ),
            ),
        };
        let ord = unit.ord;
        pause_for_human(
            store,
            subscribers,
            self_tx,
            &mut session,
            ord,
            reviewing_ord,
            prompt,
        )?;
        return Ok(Progress::Paused);
    }

    dispatch_unit(
        store,
        subscribers,
        runner,
        self_tx,
        run_id,
        unit_ix,
        lifecycle_maps,
        actor_maps,
        process_gen,
        is_acp,
    )?;
    Ok(Progress::Dispatched)
}

/// Resolve a run's workdir from its (optional) registered repo: create the isolated git worktree and
/// return `(repo_ref, workdir)`. `None` repo ⇒ no worktree. Errors if the repo id isn't registered or
/// the worktree can't be created.
fn resolve_workdir(
    store: &dyn GraphStore,
    repo_ref: &Option<String>,
    run_id: &str,
) -> anyhow::Result<(Option<String>, Option<String>)> {
    let Some(repo_id) = repo_ref else {
        return Ok((None, None));
    };
    let repo = crate::repo::get_repo(store, repo_id)?
        .ok_or_else(|| anyhow::anyhow!("repo not registered: {repo_id}"))?;
    let wt = crate::repo::create_worktree(&repo.root_path, run_id)?;
    Ok((
        Some(repo_id.clone()),
        Some(wt.to_string_lossy().to_string()),
    ))
}

/// Decide whether `prior` is context for `unit`, and how to label it. `None` ⇒ not injected.
///
/// Pure (no store) so the selection rule is testable on its own — the dispatch site pairs it with a
/// `get_work_output` read. Two independent reasons to inject, unioned:
///
///  1. DECLARED DEPENDENCY (FINDING-024) — `prior`'s phase id appears in `unit.depends_on`. The def
///     already states the handoff graph; the engine honored it for ordering and dropped it for
///     context, so an Evaluator phase declared `.after("build")` ran blind to the build and
///     re-solved the original task. Keying off the declaration keeps the bound author-controlled.
///  2. CROSS-CLI HANDOFF — `prior` ran on a DIFFERENT CLI, so no conversational state can be shared
///     and the output must be passed explicitly whatever the def says. This used to be the ONLY
///     reason, which is why single-CLI runs (every shipped workflow's default) injected nothing.
///
/// Ordering is the caller's: only units with `ord < unit.ord` are offered, so a declared dependency
/// on a LATER phase cannot leak backwards. `phase_id()` is `None` for prose-planned units, which
/// correctly matches no declaration — absent a def there is no declared graph to honor.
fn prior_context_label(
    unit: &crate::domain::WorkUnit,
    prior: &crate::domain::WorkUnit,
    current_cli: &str,
) -> Option<String> {
    if prior.ord >= unit.ord {
        return None;
    }
    let cli = prior.assigned_cli.as_deref().unwrap_or("claude");
    let declared_phase = prior
        .phase_id()
        .filter(|p| unit.depends_on.iter().any(|d| d == p));
    match declared_phase {
        // Name WHY it is here. A phase being handed the artifact it declared it consumes reads
        // differently from a bare cross-CLI carry-over, and an operator should be able to tell them
        // apart in the transcript without diffing the def.
        Some(p) => Some(format!("[{cli} — unit {} — depends_on `{p}`]", prior.ord)),
        None if cli != current_cli => Some(format!("[{cli} — unit {}]", prior.ord)),
        None => None,
    }
}

/// Why a run paused before dispatching a unit — not merely *that* it did.
///
/// The distinction is the operator's entire context. A `DefGate` pause asks about work that has
/// already happened (the preceding phase's output); a `RunLevel` pause asks about work that is about
/// to happen. Collapsing both into `bool` is what produced FINDING-032: every mid-run gate was
/// described as "approve unit N before it runs" even when the artifact under review was unit N-1's
/// output and unit N had declared no gate at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PauseReason {
    /// The run-level `--confirm` policy (`All`, or `Before(ord)` matching this unit).
    RunLevel,
    /// The PRECEDING phase declared the gate; its ord is carried so the prompt and the event can
    /// name the phase whose output is actually being approved.
    DefGate { reviewing_ord: u32 },
}

/// Why to pause for a human before dispatching `units[unit_ix]`, or `None` to dispatch. Two sources:
///  1. the run-level `--confirm` policy (None / All / Before(ord)); and
///  2. the DEF-declared gate on the PRECEDING phase (its `GateSpec` fires *after* its work, i.e.
///     before this unit) — so a `WorkflowDef`'s own HumanConfirm gates drive the run, not just the
///     run-level flag. `HumanConfirm` always pauses; `HumanConfirmIf(VerdictNotPass)` pauses when the
///     preceding unit is not a clean pass (`status != Done`); `Auto` defers to the run-level policy.
///
/// PRECEDENCE (FINDING-023) — a DEF-declared gate fires REGARDLESS of the run-level policy:
/// `human_confirm: none` silences only source 1 and never suppresses a workflow-authored gate.
/// That is deliberate, not an oversight. `HumanConfirm::None` is simultaneously the enum DEFAULT
/// (an absent wire field lands here) and the typo FALLBACK (`parse_human_confirm` maps every
/// unrecognized token to `None` — FINDING-019), so reading it as "suppress the workflow's own
/// gates" would let an omitted field or a misspelled one silently strip the review seams the
/// workflow author declared — fail-open on the primary human-review control. A genuinely
/// unattended run needs an explicit, non-default signal end-to-end (wire token + parsers + here);
/// until that exists, the def gate wins and the pause DISCLOSES the precedence in its prompt
/// ([`unsuppressed_gate_note`]) so an operator who sent `none` learns why the run still paused at
/// the moment it pauses, not from a wedged overnight batch.
///
/// `DefGate` wins when both fire. It is the more specific statement — a workflow author named this
/// exact seam — and it is the one carrying an ord to attribute the pause to, so preferring it never
/// discards information `RunLevel` would have supplied. Whether to pause is unchanged either way.
fn should_pause(
    session: &crate::domain::AgentSession,
    units: &[crate::domain::WorkUnit],
    unit_ix: usize,
) -> Option<PauseReason> {
    let ord = units[unit_ix].ord;
    let run_level = match session.human_confirm {
        crate::domain::HumanConfirm::None => false,
        crate::domain::HumanConfirm::All => true,
        crate::domain::HumanConfirm::Before(o) => o == ord,
    };
    let def_gate = unit_ix
        .checked_sub(1)
        .and_then(|i| units.get(i))
        .filter(|prev| match prev.gate {
            crate::workflow::GateSpec::Auto => false,
            crate::workflow::GateSpec::HumanConfirm { .. } => true,
            crate::workflow::GateSpec::HumanConfirmIf(
                crate::workflow::GateCond::VerdictNotPass,
            ) => prev.status != crate::domain::UnitStatus::Done,
        })
        .map(|prev| PauseReason::DefGate {
            reviewing_ord: prev.ord,
        });
    def_gate.or(run_level.then_some(PauseReason::RunLevel))
}

/// The disclosure appended to a DEF-declared gate's pause prompt when the run was launched with
/// `human_confirm: none` (FINDING-023). The precedence itself — a workflow-authored gate is never
/// suppressed by the run-level policy — is deliberate (see [`should_pause`]), but before this note
/// NOTHING surfaced it: the launch accepted `none`, the session reported `human_confirm: none`, and
/// the run still sat `awaiting_human`, which reads as a contradiction and cost an operator a stalled
/// overnight batch before they learned the rule. The pause prompt is the one surface the operator is
/// guaranteed to read at the exact moment the precedence bites, so the pause explains itself there.
/// Empty for every attended policy — those operators asked to be paused, and the note would be noise.
fn unsuppressed_gate_note(human_confirm: crate::domain::HumanConfirm) -> &'static str {
    match human_confirm {
        crate::domain::HumanConfirm::None => {
            " [workflow-declared gate: this pause is authored by the workflow definition itself; \
             run-level human_confirm=none silences only run-level pauses, never a \
             workflow-authored gate]"
        }
        _ => "",
    }
}

/// Read the next unit at `unit_ix`, emit `UnitExecuting`, and spawn a worker that runs its slow work
/// (no store handle) and posts an `ApplyStepResult` back to the actor. Returns `Ok(false)` if
/// `unit_ix` is past the last unit (nothing to dispatch — the run is done).
#[allow(clippy::too_many_arguments)]
fn dispatch_unit(
    store: &dyn GraphStore,
    subscribers: &mut crate::event_log::EventSink,
    runner: &Arc<dyn StepRunner>,
    self_tx: &Sender<Command>,
    run_id: &str,
    unit_ix: usize,
    elicitation_maps: &Option<Arc<std::sync::Mutex<ElicitationMaps>>>,
    _actor_maps: &Option<Arc<std::sync::Mutex<ElicitationMaps>>>,
    process_gen: uuid::Uuid,
    is_acp: bool,
) -> anyhow::Result<bool> {
    let session = crate::domain::get_session(store, run_id)?
        .ok_or_else(|| anyhow::anyhow!("run not found: {run_id}"))?;
    let units = crate::domain::session_units(store, run_id)?;
    let Some(unit) = units.get(unit_ix) else {
        return Ok(false);
    };
    // (DES-STUDIO-COCKPIT-001 §3 B2) UnitDispatched — the durable-rework signal. `dispatch_unit` is the
    // SINGLE funnel every dispatch site reaches (initial advance, `confirm_gate` Approve re-dispatch,
    // `resume_run_inner`, `redrive_executing_sessions`); each of those bumps `session.attempt` in the store
    // BEFORE calling here, so reading `session.attempt` yields the correct (incrementing) attempt at every
    // site. Emitted before the exec-mediation branch so BOTH the in-process and bus-mediated paths signal it.
    emit(
        subscribers,
        CoreEvent::UnitDispatched {
            session: run_id.to_string(),
            ord: unit.ord,
            attempt: session.attempt,
        },
    );
    emit(
        subscribers,
        CoreEvent::UnitExecuting {
            session: run_id.to_string(),
            ord: unit.ord,
        },
    );
    // §4 ARTIFACT-PASSING for the AGENT validator (seam finding #8): an Evaluator-role unit's agent
    // judge must review the most-recent prior Creator's COLD output — the work it is evaluating — not
    // its own output. Resolve it HERE on the actor thread (a store read); the worker holds no store
    // handle. `None` ⇒ the agent judges the unit's own output (Neutral/Creator, or no prior creator).
    // Bounded at the SELECTION point (core#282): everything downstream — the in-process judge,
    // the bus `task.dispatched` payload, the evaluator seat's prompt arg — sees the clipped form,
    // so an unbounded creator output can never blow a prompt-arg seat's backend.
    let agent_review_target = if unit.role == crate::workflow::PhaseRole::Evaluator {
        pipeline::creator_output_for(store, run_id, unit.ord)
            .map(crate::cli_runner::clip_review_target)
    } else {
        None
    };
    // SHARED CONTEXT — the prior work this unit is supposed to build on. Read on the actor thread
    // (store access) before the worker is dispatched; the worker holds no store handle (single-writer
    // invariant). Reuses `units` already fetched above — no second store read.
    //
    // Two independent reasons to inject a prior unit; a unit qualifying under either is injected once.
    //
    //  1. DECLARED DEPENDENCY (FINDING-024). The def says `adversarial-review.after("build")`, and the
    //     engine honored that for ordering while dropping it for context — so the Evaluator phase ran
    //     blind to the very work it was created to evaluate, and re-solved the original task instead.
    //     Keying off `depends_on` makes the bound author-controlled: a phase gets exactly the priors
    //     it declared, not a guessed window.
    //  2. CROSS-CLI HANDOFF (ACP multi-CLI, Tutti-inspired "@"-workspace). A prior unit on a DIFFERENT
    //     CLI cannot have shared conversational state, so its output must be passed explicitly
    //     regardless of what the def declares. This was previously the ONLY reason, which is why
    //     single-CLI runs — every shipped workflow's default — injected nothing at all.
    //
    // Union, not replacement: dropping (2) would regress multi-CLI runs whose defs declare no graph,
    // and dropping (1) is the bug. Declared-dependency membership is by PHASE ID, the token the def
    // uses; `phase_id()` returns None for prose-planned units, which correctly matches nothing.
    let current_cli = unit.assigned_cli.as_deref().unwrap_or("claude");
    // Single-pass: build both the worker's prior-output list and the EVT-007 context items
    // simultaneously via `unzip` — no intermediate Vec and no second store read.
    let (prior_outputs, context_items): (Vec<PriorUnitOutput>, Vec<crate::event::InjectedContext>) =
        units
            .iter()
            .filter_map(|u| {
                let label = prior_context_label(unit, u, current_cli)?;
                let output = crate::domain::get_work_output(store, &u.id)?;
                let output_bytes = output.len();
                Some((
                    PriorUnitOutput {
                        label: label.clone(),
                        output,
                    },
                    crate::event::InjectedContext {
                        ord: u.ord,
                        label,
                        output_bytes,
                    },
                ))
            })
            .unzip();
    // (EVT-007) Emit UnitContextInjected when prior outputs are being injected — a cross-CLI carry-over
    // or a declared `depends_on` handoff (FINDING-024). Before that fix this fired only on multi-CLI
    // runs, so its ABSENCE was the observable that proved every single-CLI phase ran context-free.
    if !context_items.is_empty() {
        emit(
            subscribers,
            CoreEvent::UnitContextInjected {
                session: run_id.to_string(),
                ord: unit.ord,
                recipient_cli: current_cli.to_string(),
                prior_units: context_items,
            },
        );
    }
    // Allocate launch sequence + ACP epoch only after every fallible actor-side read has
    // succeeded. Tool commands bypass the ACP runner entirely and therefore must not create
    // an epoch that no `EpochCleanup` guard could ever release.
    let (elicitation_epoch, launch_seq) = if unit.tool_cmd.is_some() {
        (0, 0)
    } else if let Some(ref m) = elicitation_maps {
        let mut maps = m.lock().unwrap_or_else(|p| p.into_inner());
        let seq = maps.begin_launch(run_id, is_acp);
        let ep = if is_acp { maps.next_epoch(run_id) } else { 0 };
        (ep, seq)
    } else {
        (0, 0)
    };
    let input = StepInput {
        run_id: run_id.to_string(),
        unit_ix,
        attempt: session.attempt,
        unit: unit.clone(),
        workflow_id: session.workflow_id.clone(),
        entity_mode: session.entity_mode,
        workdir: session.workdir.as_ref().map(std::path::PathBuf::from),
        // GOVERNED (DES-OUTGOV-003 §4): a real campaign unit — arm input governance when the store is a
        // file-backed SQLite db the hook subprocess can open. `None` for `:memory:`/`postgres://`.
        // The repo-local graph is resolved HERE (the actor thread holds the store; the worker does not)
        // so the worker's estate tools never need — and never get — the operational store.
        governance: in_process_governance().map(|g| crate::workflow::GovernanceContext {
            // The PROJECT's graph when the run is bound to one the engine can vouch for, else this
            // repo's own — see `run_code_graph_db`. The operational store path comes from the
            // actor's thread-local so the FINDING-067 guard has something to compare against; this
            // closure runs on the actor thread, where it is always armed.
            code_graph_db: run_code_graph_db(
                store,
                &session,
                GOV_DB_PATH.with(|c| c.borrow().clone()).as_deref(),
            ),
            // From the SESSION, so a resume/redrive re-arms exactly the boundary the launch
            // declared and validated (core#259).
            extra_write_roots: session.extra_write_roots.clone(),
            ..g
        }),
        prior_outputs,
        elicitation_epoch,
        process_gen: Some(process_gen),
        launch_seq,
    };

    // TOOL EXECUTOR: if the unit carries a tool_cmd, bypass the CLI runner entirely.
    // Spawn the command off-thread (same actor-safety rule as the agent path), capture
    // stdout+stderr as the transcript, post ApplyStepResult when done.
    if let Some(cmd) = unit.tool_cmd.clone() {
        // (EVT-011) ToolExecutorDispatched — fires just before the tool command spawns so the
        // studio can distinguish a tool-path unit from an agent-path unit in the event stream
        // (both emit UnitExecuting, but only this event carries the actual command).
        emit(
            subscribers,
            CoreEvent::ToolExecutorDispatched {
                session: run_id.to_string(),
                ord: unit.ord,
                cmd: cmd.clone(),
                workdir: session.workdir.clone(),
            },
        );
        let tx = self_tx.clone();
        let run_id2 = run_id.to_string();
        let ord = unit.ord;
        let unit_ix2 = unit_ix;
        let attempt = session.attempt;
        let workdir = session.workdir.clone();
        std::thread::spawn(move || {
            let (output_str, status) = run_tool_cmd(&cmd, workdir.as_deref());
            // Stream the whole output as one delta so the transcript panel shows something.
            let _ = tx.send(crate::command::Command::CliOutputDelta {
                run_id: run_id2.clone(),
                ord,
                attempt,
                chunk: output_str.clone(),
                process_gen: None, // PTY tool-cmd path — not bus-dispatched
                launch_seq: 0,
            });
            let _ = tx.send(crate::command::Command::ApplyStepResult {
                output: crate::workflow::StepOutput {
                    run_id: run_id2,
                    unit_ix: unit_ix2,
                    attempt,
                    output: output_str,
                    status,
                    usage: None,
                    files: Vec::new(),
                    // A Tool-executor phase (`run_tool_cmd`) invokes a single fixed binary directly
                    // — it has no per-tool-call breakdown to report (FINDING-046).
                    tools: Vec::new(),
                    governed: false,
                },
                agent_verdict: None,
                process_gen: None, // PTY path — not bus-dispatched; no stale-result guard needed
                launch_seq: 0,
                ack: None,
            });
        });
        return Ok(true);
    }

    // LAW 1 EXECUTION-MEDIATION SEAM (opt-in). When exec-mediation is armed on this (actor) thread, the
    // reducer does NOT call execution directly: it PUBLISHES `wicked.task.dispatched` and returns. The
    // off-actor `cli-runner` subscriber runs the unit (via the SAME runner) and publishes
    // `wicked.task.completed`, which the `task.completed` poller turns back into a `Command::ApplyStepResult`
    // on this actor — the identical apply the in-process worker below would have produced. `agent_review_target`
    // (the creator's cold output, resolved on-thread above) rides in the dispatched event so the off-actor
    // judge sees the right artifact. A publish failure returns `false` → we fall through to the in-process
    // worker so the run still makes progress rather than wedging with no worker. See `cli_runner.rs`.
    if crate::cli_runner::is_exec_enabled()
        && crate::cli_runner::try_publish_dispatched(&input, agent_review_target.as_deref(), is_acp)
    {
        return Ok(true);
    }

    let runner = runner.clone();
    let tx = self_tx.clone();
    std::thread::spawn(move || {
        let run_id = input.run_id.clone();
        let ord = input.unit.ord;
        let attempt = input.attempt; // in-band attempt: labels this worker's throttled output
                                     // Streaming sink: each output chunk is posted back to the actor (the single emit point) as a
                                     // `CliOutputDelta` command. The `Mutex` makes the `!Sync` `Sender` shareable across the
                                     // runner's concurrent stdout/stderr drains.
        let delta_tx = std::sync::Mutex::new(tx.clone());
        let emit = move |chunk: &str| {
            if let Ok(g) = delta_tx.lock() {
                let _ = g.send(Command::CliOutputDelta {
                    run_id: run_id.clone(),
                    ord,
                    attempt,
                    chunk: chunk.to_string(),
                    process_gen: None, // local-path worker; bus consumer sets in T7
                    launch_seq: 0,
                });
            }
        };
        // rev0.4 DUAL-VALIDATOR LAYER-2 (the AGENT semantic judge) + the unit's slow work run HERE — on the
        // worker thread, NOT the actor. ACTOR-SAFETY: `run_unit_and_judge` calls the runner (a subprocess)
        // and `agent_validate` (an LLM `claude -p`; slow), which must never execute on the single-writer
        // actor thread or it would stall every other command. This closure IS the off-thread seam (holds no
        // store handle). The SAME helper the `cli-runner` subscriber calls (`cli_runner::run_unit_and_judge`)
        // — so the in-process path and the bus-mediated path produce a byte-identical `(output, agent_verdict)`.
        // The WORK the agent judges is the creator's COLD output for an Evaluator-role unit
        // (`agent_review_target`, seam finding #8), else the unit's own output. The verdict rides back on the
        // `ApplyStepResult` command; the actor folds it into the gate via `combine_verdict`.
        let (output, agent_verdict) = crate::cli_runner::run_unit_and_judge(
            &runner,
            &input,
            agent_review_target.as_deref(),
            &emit,
        );
        let _ = tx.send(Command::ApplyStepResult {
            output,
            agent_verdict,
            process_gen: None, // local-path worker; bus consumer sets these in T7
            launch_seq: 0,
            ack: None,
        });
    });
    Ok(true)
}

/// Spawn a tool command in `workdir` (session root), collect all stdout+stderr, and return
/// `(output, StepStatus)`. Exit 0 → `StepStatus::Ok`; anything else → `StepStatus::Failed`.
/// Called off the actor thread (blocking subprocess).
fn run_tool_cmd(cmd: &[String], workdir: Option<&str>) -> (String, crate::workflow::StepStatus) {
    use std::process::Command;
    let Some(bin) = cmd.first() else {
        return (
            "tool phase has empty cmd".to_string(),
            crate::workflow::StepStatus::Failed,
        );
    };
    // `cmd` is an arbitrary argv straight out of a `WorkflowDef` — workflows are data, so this runs
    // whatever a workflow author wrote. A def whose tool phase is `["wicked-estate", "index", "."]`
    // reproduces FINDING-067 with no agent involved at all: the indexer needs no `--db`, it reads
    // the inherited environment. Found by enumerating spawn sites, not by a failure.
    //
    // The engine's OWN binary (`wicked-core`, e.g. the domain-extraction `domain-graph` persist
    // phase, core#237) is not necessarily on PATH under that literal name — it may be a napi addon,
    // or live at ~/.local/bin. Resolve it exactly as the validator/coverage paths do
    // ($WICKED_CORE_EXE → current_exe → PATH → bare) so the Tool phase invokes THIS engine, not a
    // missing PATH entry. Any other binary is spawned as written.
    let resolved_bin;
    let bin: &str = if bin == "wicked-core" {
        resolved_bin = crate::execute_wrapped::resolve_wicked_core_exe();
        &resolved_bin
    } else {
        bin
    };
    let mut proc = Command::new(bin);
    proc.hardened().args(&cmd[1..]);
    if let Some(wd) = workdir {
        proc.current_dir(wd);
    }
    match proc.output() {
        Ok(out) => {
            let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&out.stderr);
            if !stderr.is_empty() {
                if !combined.is_empty() {
                    combined.push('\n');
                }
                combined.push_str(&stderr);
            }
            let status = if out.status.success() {
                crate::workflow::StepStatus::Ok
            } else {
                let code = out.status.code().unwrap_or(-1);
                combined.push_str(&format!("\n[exit {}]", code));
                crate::workflow::StepStatus::Failed
            };
            (combined, status)
        }
        Err(e) => (
            format!("failed to spawn {:?}: {e}", bin),
            crate::workflow::StepStatus::Failed,
        ),
    }
}

/// TRUE iff `workdir` shows NO observable worktree change in EITHER place a worker can leave one —
/// uncommitted (`git status --porcelain`: tracked modification, deletion, untracked file) or
/// committed (run-branch-only commits, `HEAD --not --exclude=wicked/* --branches`) — the same two
/// instruments as the built-in evidence floor, `builtin_floors::EVIDENCE_SCRIPT`. The committed
/// clause is core#280's fix: a worker under an incremental-commit contract leaves porcelain clean,
/// and reading porcelain alone would feed the substance gate a false "produced nothing". Feeds the
/// phase-substance gate in [`apply_step_result`]: "no change anywhere" + near-empty prose = a phase
/// that produced nothing reviewable.
///
/// The degenerate cases resolve in the fail-closed direction the floor established: a run with no
/// workdir, a non-git workdir (both `git` calls exit 128), or an unspawnable `git` has no
/// OBSERVABLE change, so all return `true` — for such a run, prose is the only substance it can
/// offer, and the substance gate holds it to that.
///
/// Actor-thread subprocess, deliberately: both are fast plumbing reads, and this runs ONLY on the
/// short-output governed path (rare), never per unit.
fn worktree_is_clean(workdir: Option<&str>) -> bool {
    let Some(wd) = workdir else {
        return true;
    };
    let no_output = |args: &[&str]| {
        // spawn-audit: hardened — read-only git plumbing over the run's own worktree.
        let out = std::process::Command::new("git")
            .hardened()
            .args(args)
            .current_dir(wd)
            .output();
        match out {
            Ok(o) if o.status.success() => o.stdout.iter().all(|b| b.is_ascii_whitespace()),
            _ => true,
        }
    };
    no_output(&["status", "--porcelain"])
        && no_output(&[
            "log",
            "--oneline",
            "HEAD",
            "--not",
            "--exclude=wicked/*",
            "--branches",
        ])
}

/// The FILES a run's worktree contribution touches — the union of the SAME two instruments as
/// [`worktree_is_clean`]: uncommitted paths (`git status --porcelain`, including untracked) and
/// paths changed by run-branch-only commits (`git log --name-only HEAD --not --exclude=wicked/*
/// --branches`). Deduped, first-seen order.
///
/// Best-effort OBSERVABILITY read (core#283), so the degenerate cases resolve to EMPTY — the
/// opposite fail-direction from `worktree_is_clean`'s `true`: that feeds a deny gate (fail-closed
/// = "assume nothing observable"), this feeds a warning (fail-open = "no observable contribution,
/// nothing to warn about"). A non-git workdir, an unspawnable `git`, or a failed read never
/// invents a warning.
pub(crate) fn worktree_contribution_files(workdir: &str) -> Vec<String> {
    let run = |args: &[&str]| -> Vec<String> {
        // spawn-audit: hardened — read-only git plumbing over the run's own worktree.
        let out = std::process::Command::new("git")
            .hardened()
            .args(args)
            .current_dir(workdir)
            .output();
        match out {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(str::to_string)
                .collect(),
            _ => Vec::new(),
        }
    };
    let mut files: Vec<String> = Vec::new();
    let mut push = |path: &str| {
        // Strip git's quoting of unusual paths (no unescaping — this feeds a heuristic warning,
        // not a filesystem access).
        let path = path.trim().trim_matches('"');
        if !path.is_empty() && !files.iter().any(|f| f == path) {
            files.push(path.to_string());
        }
    };
    // Instrument 1: uncommitted. Porcelain lines are `XY <path>` (byte 3 onward), with renames as
    // `XY <orig> -> <new>` — the NEW path is the contribution.
    for line in run(&["status", "--porcelain"]) {
        if let Some(rest) = line.get(3..) {
            push(rest.rsplit(" -> ").next().unwrap_or(rest));
        }
    }
    // Instrument 2: committed on the run branch only (a worker under an incremental-commit
    // contract leaves porcelain clean — core#280). `--pretty=format:` blanks the commit header so
    // every non-empty line is a file path.
    for line in run(&[
        "log",
        "--name-only",
        "--pretty=format:",
        "HEAD",
        "--not",
        "--exclude=wicked/*",
        "--branches",
    ]) {
        push(&line);
    }
    files
}

/// PURE heuristic (core#283): is this changed `path` a DOCUMENTATION artifact? TRUE for the doc
/// extensions (`.md` / `.txt` / `.rst`, case-insensitive) and for anything whose directory path
/// passes through `docs/` or `.product/` (any segment — the per-product artifact conventions).
/// Everything else — source, config, lockfiles, assets — counts as a production change that a
/// pre-build phase has no business making. A heuristic, deliberately small: it feeds an
/// operator-visible WARNING (never a deny), so a borderline misread costs a sentence, not a run.
pub(crate) fn is_documentation_change(path: &str) -> bool {
    // Normalize: git emits `/` but a caller-supplied path may be Windows-shaped.
    let norm = path.trim().replace('\\', "/");
    let lower = norm.trim_start_matches("./").to_ascii_lowercase();
    if lower.ends_with(".md") || lower.ends_with(".txt") || lower.ends_with(".rst") {
        return true;
    }
    // Directory segments only — the filename itself was judged by extension above, so a top-level
    // `docs.ts` is code while `docs/logo.png` is documentation.
    let mut segments: Vec<&str> = lower.split('/').collect();
    segments.pop();
    segments.iter().any(|s| *s == "docs" || *s == ".product")
}

/// core#283, the OBSERVABILITY half of phase-role scope: `Some(warning)` iff `unit` is a
/// PRE-BUILD, non-creator phase (the plan-time `pre_build_scope` marker, derived from def data —
/// see `plan::plan_from_def`) whose worktree contribution touches any NON-documentation file per
/// [`is_documentation_change`]. The caller records the warning as gate evidence on the unit
/// (visible to operators) and must NOT deny on it: the enforced half is the plan-time prompt
/// preamble; this half makes a breach OBSERVABLE instead of silent — prompt-level discipline
/// alone was proven insufficient, and a hard deny here would turn a heuristic into a gate.
pub(crate) fn phase_scope_warning(
    unit: &crate::domain::WorkUnit,
    workdir: Option<&str>,
) -> Option<String> {
    if !unit.pre_build_scope {
        return None;
    }
    let wd = workdir?;
    let mut non_doc: Vec<String> = worktree_contribution_files(wd)
        .into_iter()
        .filter(|p| !is_documentation_change(p))
        .collect();
    if non_doc.is_empty() {
        return None;
    }
    // Bound the named files: this string persists on the unit node and surfaces in operator UIs.
    const MAX_NAMED: usize = 20;
    let total = non_doc.len();
    non_doc.truncate(MAX_NAMED);
    let mut listed = non_doc.join(", ");
    if total > MAX_NAMED {
        listed.push_str(&format!(", … {} more", total - MAX_NAMED));
    }
    Some(format!(
        "PHASE SCOPE WARNING: pre-build phase `{}` left non-documentation changes in the worktree \
         ({listed}) — this phase's deliverable is analysis/design/plan; implementation belongs to \
         a later phase. Recorded as gate evidence for the operator; NOT a denial.",
        unit.phase_id().unwrap_or(&unit.id)
    ))
}

#[cfg(test)]
mod phase_scope_tests {
    use super::{is_documentation_change, phase_scope_warning};

    /// The core#283 heuristic, judged case by case in both directions. Extension decides files
    /// (`.md`/`.txt`/`.rst`); `docs/` and `.product/` decide DIRECTORIES — so a `docs.ts` file is
    /// code and a `docs/logo.png` asset is documentation.
    #[test]
    fn the_documentation_heuristic_judges_extension_and_docs_directories() {
        for doc in [
            "README.md",
            "notes.TXT",
            "spec.rst",
            "./guide.md",
            "docs/index.html",
            "docs/api/openapi.yaml",
            ".product/DES-001.yaml",
            "site/docs/logo.png",
            r"docs\win\style.css",
        ] {
            assert!(is_documentation_change(doc), "`{doc}` is documentation");
        }
        for code in [
            "src/main.ts",
            "Cargo.toml",
            "build.rs",
            "README",
            "docs.ts",
            "mydocs/file.rs",
            "package-lock.json",
            "src/.product.rs",
        ] {
            assert!(
                !is_documentation_change(code),
                "`{code}` is NOT documentation"
            );
        }
    }

    /// core#283 observability, measured against a REAL linked worktree on a `wicked/<run>` branch
    /// (the layout `repo::create_worktree` provisions): a `.ts` change fires the warning — both
    /// UNCOMMITTED (instrument 1, porcelain) and COMMITTED on the run branch (instrument 2, the
    /// core#280 clause) — while a `.md`-only contribution stays silent, and an unmarked
    /// (build/evaluator) unit is never warned at all.
    #[test]
    fn phase_scope_warning_fires_for_a_ts_change_and_not_for_md_only() {
        let base = std::env::temp_dir().join(format!("wicked-scope-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let git = |cwd: &std::path::Path, args: &[&str]| {
            // spawn-audit: test-only — a git fixture building the worktree layout under test; it reads no engine state.
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(cwd)
                .output()
                .expect("git runs");
            assert!(out.status.success(), "git {args:?}: {out:?}");
        };
        git(&repo, &["init", "-q", "."]);
        git(&repo, &["config", "user.email", "t@example.invalid"]);
        git(&repo, &["config", "user.name", "t"]);
        std::fs::write(repo.join("base.txt"), "base\n").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-qm", "base"]);
        let wt = repo.join(".wicked").join("worktrees").join("run1");
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                wt.to_str().unwrap(),
                "-b",
                "wicked/run1",
            ],
        );
        let wd = wt.to_str().unwrap();

        let mut unit =
            crate::domain::WorkUnit::pending("s:design", "s", 2, "design — add the feature");
        unit.pre_build_scope = true;

        // A pristine contribution and a docs-only one (uncommitted AND committed on the run
        // branch): the phase did its job — no warning.
        assert_eq!(phase_scope_warning(&unit, Some(wd)), None, "clean tree");
        std::fs::write(wt.join("design.md"), "the design\n").unwrap();
        assert_eq!(
            phase_scope_warning(&unit, Some(wd)),
            None,
            "an uncommitted .md-only contribution is the phase's deliverable, not a breach"
        );
        git(&wt, &["add", "-A"]);
        git(&wt, &["commit", "-qm", "docs: the design"]);
        assert_eq!(
            phase_scope_warning(&unit, Some(wd)),
            None,
            "a COMMITTED .md-only contribution is equally fine (instrument 2 sees only docs)"
        );

        // An UNCOMMITTED production-code contribution: warn, naming the file and the phase.
        std::fs::write(wt.join("feature.ts"), "export const x = 1;\n").unwrap();
        let warning =
            phase_scope_warning(&unit, Some(wd)).expect("an uncommitted .ts change must warn");
        assert!(warning.contains("feature.ts"), "names the file: {warning}");
        assert!(warning.contains("`design`"), "names the phase: {warning}");
        assert!(
            warning.contains("NOT a denial"),
            "the record must say it did not deny: {warning}"
        );

        // COMMITTED on the run branch (porcelain goes clean — the core#280 shape): still warns,
        // via the second instrument.
        git(&wt, &["add", "-A"]);
        git(&wt, &["commit", "-qm", "feat: jumped the ladder"]);
        // Premise guard: porcelain really is clean now, so only instrument 2 can catch this.
        // spawn-audit: test-only — asserts the premise (clean porcelain) that makes the clause meaningful.
        let porcelain = std::process::Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&wt)
            .output()
            .expect("git runs");
        assert!(
            porcelain.stdout.iter().all(|b| b.is_ascii_whitespace()),
            "premise broken: porcelain not clean after commit"
        );
        let committed = phase_scope_warning(&unit, Some(wd))
            .expect("a run-branch COMMITTED .ts change must still warn (core#280 instrument)");
        assert!(committed.contains("feature.ts"), "{committed}");

        // Role scope: an unmarked (creator/evaluator/post-build) unit is never warned, even with
        // the breach sitting in the tree — and no workdir means nothing observable, so no warning.
        unit.pre_build_scope = false;
        assert_eq!(phase_scope_warning(&unit, Some(wd)), None);
        unit.pre_build_scope = true;
        assert_eq!(phase_scope_warning(&unit, None), None);

        let _ = std::fs::remove_dir_all(&base);
    }
}

/// Mark a run `Completed` and emit `SessionCompleted`. Propagates a store-write failure so a failed
/// finalize surfaces as a run error (rather than silently wedging the run in `in_flight`).
fn finalize_run(
    store: &mut dyn GraphStore,
    subscribers: &mut crate::event_log::EventSink,
    runner: &Arc<dyn StepRunner>,
    self_tx: &Sender<Command>,
    run_id: &str,
) -> anyhow::Result<()> {
    if let Some(mut session) = crate::domain::get_session(store, run_id)? {
        session.status = SessionStatus::Completed;
        put_node(store, session.to_node())?;
        reap_terminal_worktree(&*store, &session);
    }
    emit(
        subscribers,
        CoreEvent::SessionCompleted {
            session: run_id.to_string(),
        },
    );
    notify_campaign(self_tx, run_id, crate::campaign::NodeOutcome::Completed);
    runner.on_run_complete(run_id);
    Ok(())
}

/// Resolve a human-confirm gate on a paused run. `Approve` (with an optional amendment to the next
/// unit's instruction) clears the pause and dispatches the unit at the cursor directly (no re-pause
/// on it); `Reject` cancels the run.
#[allow(clippy::too_many_arguments)]
pub(crate) fn confirm_gate(
    store: &mut dyn GraphStore,
    subscribers: &mut crate::event_log::EventSink,
    runner: &Arc<dyn StepRunner>,
    self_tx: &Sender<Command>,
    in_flight: &mut HashSet<String>,
    run_id: &str,
    decision: crate::workflow::HumanDecision,
    lifecycle_maps: &Option<Arc<std::sync::Mutex<ElicitationMaps>>>,
    actor_maps: &Option<Arc<std::sync::Mutex<ElicitationMaps>>>,
    process_gen: uuid::Uuid,
    is_acp: bool,
) -> anyhow::Result<SessionStatus> {
    let session = crate::domain::get_session(store, run_id)?
        .ok_or_else(|| anyhow::anyhow!("run not found: {run_id}"))?;
    if session.status != SessionStatus::AwaitingHuman {
        anyhow::bail!(
            "run {run_id} is not awaiting confirmation (status is {:?})",
            session.status
        );
    }

    // DES-PROJECT-001 §5.3: the durable prompt resolves on the SAME command that resolves the
    // gate — `answered`, with the decision payload. This is what empties `/projects/:id/prompts`
    // the moment ANY skin answers. (The Reject arm still cancels below; the human DID answer.)
    {
        let answer = match &decision {
            crate::workflow::HumanDecision::Approve { amend } => {
                serde_json::json!({ "approve": true, "amend": amend }).to_string()
            }
            crate::workflow::HumanDecision::Reject => {
                serde_json::json!({ "approve": false, "amend": null }).to_string()
            }
        };
        crate::interaction::resolve_open_for_session(
            store,
            run_id,
            crate::interaction::InteractionStatus::Answered,
            Some(answer),
            crate::interaction::now_millis(),
        )?;
    }

    match decision {
        crate::workflow::HumanDecision::Reject => {
            let s = cancel_run(store, subscribers, runner, self_tx, run_id)?;
            in_flight.remove(run_id);
            Ok(s)
        }
        crate::workflow::HumanDecision::Approve { amend } => {
            // Layer-3: governance deny-dominates at the phase boundary (crew#32 / DES-EXEC-001 §3).
            // Runs BEFORE any approval-side mutations so a Deny cancels cleanly (no partial state committed).
            // Without policies loaded (`wicked-core rules ingest`), select() returns empty and decide()
            // always returns Allow — the check is a no-op until policies are populated.
            {
                let units = crate::domain::session_units(store, run_id)?;
                let unit = units.get(session.unit_ix);
                let phase_name = unit
                    .map(|u| crate::scope::unit_phase(u.ord))
                    .unwrap_or_else(|| "terminal".to_owned());
                let scope = session.collection_scope.as_deref().unwrap_or(run_id);
                let context = serde_json::json!({
                    "phase": phase_name,
                    "scope": scope,
                    "run_id": run_id,
                    "gate": "phase-boundary",
                });
                // Match the synthetic execution phase AND the workflow phase id the operator sees
                // in the API — an `applies_to: ["review"]` must select here, not silently never
                // fire (FINDING-021). The claim still records the canonical `unit-<ord>`.
                let phases =
                    crate::scope::phase_aliases(&phase_name, unit.and_then(|u| u.phase_id()));
                let selected = select_any(store, scope, &phases, &context)?;
                let claim: ConformanceClaim = decide(
                    &selected,
                    scope,
                    &phase_name,
                    &context,
                    crate::clock::eval_now(),
                );
                if matches!(claim.decision, Decision::Deny) {
                    conform(store, &claim)?;
                    // Persist the deny evidence then cancel the run. The run stays cancelled
                    // (the human must re-launch) — deny-dominates means Approve cannot override
                    // a policy veto (ADR-0003). Remove from in_flight only after cancel_run so
                    // a write failure leaves the map consistent (run stays in_flight = retryable).
                    let result = cancel_run(store, subscribers, runner, self_tx, run_id);
                    if result.is_ok() {
                        in_flight.remove(run_id);
                    }
                    return result;
                }
            }
            // Optionally inject an amendment into the unit at the cursor (the gate is steering).
            if let Some(a) = amend {
                if !a.is_empty() {
                    let mut units = crate::domain::session_units(store, run_id)?;
                    if let Some(u) = units.get_mut(session.unit_ix) {
                        u.description = format!("{} (operator amendment: {a})", u.description);
                        put_node(store, u.to_node())?;
                        // (EVT-012) UnitReworkAmended — the authoritative amendment paper trail.
                        // Fires here (after persist, before Resumed) so the amendment text is
                        // durable before any subscriber sees the run resume. Resumed alone carries
                        // no amendment text; this event is the canonical record.
                        emit(
                            subscribers,
                            CoreEvent::UnitReworkAmended {
                                session: run_id.to_string(),
                                ord: u.ord,
                                // Move `a` — it is not used after this emit, avoiding an
                                // unnecessary heap allocation (Gemini code review).
                                amendment: a,
                                updated_description: u.description.clone(),
                            },
                        );
                    }
                }
            }
            // Clear the pause → Executing, then dispatch the cursor unit directly (bypass should_pause
            // so it doesn't immediately re-pause on the same unit).
            let mut s = session;
            s.status = SessionStatus::Executing;
            // WEDGE-ON-RE-DISPATCH fix (seam finding #2/#3): BUMP the attempt before re-dispatching an
            // ALREADY-RUN cursor unit (a HumanConfirmIf conditional-gate retry, or a terminal-gate
            // re-approve). Without this, the re-dispatch reuses the identical `(run, unit, attempt)`
            // idempotency key → `emit()` dedups to the ORIGINAL (now-terminal) task.dispatched row,
            // `try_publish_dispatched` still returns true (suppressing the in-process fallback), and the
            // cli-runner's cursor is already past that row → NO worker runs → permanent wedge. A bumped
            // attempt mints a fresh key so a genuinely new task.dispatched is emitted. The bump is inert on
            // the default in-process path (nothing there branches on `attempt`).
            //
            // REWORK-HONESTY fix (cockpit adversarial review): bump ONLY when the cursor unit ALREADY RAN
            // (`Done`/`Rejected`). A PRE-unit human gate (`should_pause` paused BEFORE the unit's FIRST
            // dispatch — e.g. `human_confirm: all`/`before`) leaves the cursor `Pending` (never run), so
            // approving it is its FIRST dispatch: bumping there would emit `UnitDispatched{attempt=1}` +
            // `CliUsage{attempt=1}` for work that was never redone, booking the unit's entire burn as
            // rework (`attempt>0`) → ~100% false rework under `human_confirm: all`. A first dispatch at
            // attempt=0 has no prior `task.dispatched` row, so it cannot collide → no wedge, no bump needed.
            // This keeps the `event.rs` contract ("first dispatch is attempt=0") true for gated units.
            let cursor_ran = crate::domain::session_units(store, run_id)?
                .get(s.unit_ix)
                .map(|u| {
                    matches!(
                        u.status,
                        crate::domain::UnitStatus::Done | crate::domain::UnitStatus::Rejected
                    )
                })
                .unwrap_or(false);
            if cursor_ran {
                s.attempt = s.attempt.saturating_add(1);
            }
            put_node(store, s.to_node())?;
            let units = crate::domain::session_units(store, run_id)?;
            let ord = units.get(s.unit_ix).map(|u| u.ord).unwrap_or(0);
            emit(
                subscribers,
                CoreEvent::Resumed {
                    session: run_id.to_string(),
                    ord,
                },
            );
            in_flight.insert(run_id.to_string());
            match dispatch_unit(
                store,
                subscribers,
                runner,
                self_tx,
                run_id,
                s.unit_ix,
                lifecycle_maps,
                actor_maps,
                process_gen,
                is_acp,
            ) {
                Ok(true) => Ok(SessionStatus::Executing),
                Ok(false) => {
                    in_flight.remove(run_id);
                    finalize_run(store, subscribers, runner, self_tx, run_id)?;
                    Ok(SessionStatus::Completed)
                }
                Err(e) => {
                    in_flight.remove(run_id);
                    Err(e)
                }
            }
        }
    }
}

/// Ordered ACP teardown helper shared by CancelRun, FailureTriageReady, and Shutdown.
///
/// Performs steps 1–3 + 6 of the spec:
/// 1. Snapshot sessions for `run_id` from the write-lock registry.
/// 2. Per-session: try_lock the write_lock; tombstone the epoch; signal kill.
/// 3. Tombstone under maps lock (catches pre-registration workers).
/// 6. Second registry sweep (catches sessions inserted between step 1 and now).
///
/// Steps 4+5 (emit RunCancelled / call on_run_complete) are the caller's responsibility.
///
/// Lock ordering: `write_reg` BEFORE `maps` — never hold both simultaneously.
/// For CancelRun the second sweep uses run_id only; for ReassignUnit it filters
/// by `(run_id, previous_cli, gen <= old_max_gen)`.
fn shared_run_terminal(
    run_id: &str,
    lifecycle_maps: &Option<Arc<std::sync::Mutex<ElicitationMaps>>>,
    write_reg: &WriteReg,
) {
    let Some(maps_arc) = lifecycle_maps else {
        return;
    };

    // Step 1: snapshot (run_id, session_key, gen) → (write_lock, kill_handle).
    let sessions: Vec<(Arc<std::sync::Mutex<()>>, Arc<KillHandle>)> = {
        let reg = write_reg.lock().unwrap_or_else(|p| p.into_inner());
        reg.iter()
            .filter(|((r, _, _), _)| r.as_str() == run_id)
            .map(|(_, (wl, kh))| (Arc::clone(wl), Arc::clone(kh)))
            .collect()
    };

    // Step 2: per-session tombstone + kill.
    for (wl, kh) in &sessions {
        {
            // Acquire write_lock to serialise with an in-flight write; then tombstone.
            // If try_lock fails the write is in-flight — tombstone under maps, then signal.
            // Either way, the epoch is tombstoned before kh.signal().
            let _maybe_guard = wl.try_lock();
            let mut maps = maps_arc.lock().unwrap_or_else(|p| p.into_inner());
            if maps.has_active_run(run_id) {
                let epoch = maps.current_epoch(run_id);
                maps.cancel_epoch(run_id, epoch);
            }
        }
        kh.signal(); // unconditional — covers rpc_expect suspensions
    }

    // Step 3: tombstone under maps lock (covers pre-registration workers not yet in write_reg).
    {
        let mut maps = maps_arc.lock().unwrap_or_else(|p| p.into_inner());
        maps.tombstone_bus_run(run_id);
        if maps.has_active_run(run_id) {
            let epoch = maps.current_epoch(run_id);
            maps.cancel_epoch(run_id, epoch);
        }
    }

    // Step 6: second sweep — catches sessions inserted between step 1 and now.
    let late_sessions: Vec<Arc<KillHandle>> = {
        let reg = write_reg.lock().unwrap_or_else(|p| p.into_inner());
        reg.iter()
            .filter(|((r, _, _), _)| r.as_str() == run_id)
            .map(|(_, (_, kh))| Arc::clone(kh))
            .collect()
    };
    for kh in late_sessions {
        kh.signal();
    }
}

/// Mark a run terminally `Cancelled` and emit `RunCancelled` (a no-op status change on an already
/// terminal run). A late worker result for a cancelled run is discarded by `apply_step_result`'s
/// terminal guard.
pub(crate) fn cancel_run(
    store: &mut dyn GraphStore,
    subscribers: &mut crate::event_log::EventSink,
    runner: &Arc<dyn StepRunner>,
    self_tx: &Sender<Command>,
    run_id: &str,
) -> anyhow::Result<SessionStatus> {
    let mut session = crate::domain::get_session(store, run_id)?
        .ok_or_else(|| anyhow::anyhow!("run not found: {run_id}"))?;
    // Already terminal: report the status, do NOT re-emit a terminal event (or re-notify a campaign).
    match session.status {
        SessionStatus::Completed => return Ok(SessionStatus::Completed), // cannot cancel a finished run
        SessionStatus::Cancelled => return Ok(SessionStatus::Cancelled),
        SessionStatus::Failed => return Ok(SessionStatus::Failed),
        _ => {}
    }
    session.status = SessionStatus::Cancelled;
    put_node(store, session.to_node())?;
    // A cancelled run's open prompt is dead state — resolve it `cancelled` so no skin renders a
    // gate nobody can answer (DES-PROJECT-001 §5.3). No-op when the run was answered/never paused.
    // Best-effort by design (the cancel itself already committed), but LOGGED: a prompt stuck
    // open after a failed resolve keeps rendering in UIs, and silence would make that
    // undiagnosable (Copilot, PR #246).
    if let Err(e) = crate::interaction::resolve_open_for_session(
        store,
        run_id,
        crate::interaction::InteractionStatus::Cancelled,
        None,
        crate::interaction::now_millis(),
    ) {
        eprintln!(
            "wicked-core: failed to resolve {run_id}'s open interaction request on cancel \
             (the prompt may keep rendering as open): {e}"
        );
    }
    // FORCE-discard the worktree — Cancel is the operator explicitly abandoning the work, the one
    // terminal status where uncommitted bytes are discarded on purpose. Completed/Failed runs reap
    // through `reap_terminal_worktree` instead (FINDING-003): a clean tree goes, unlanded work stays.
    if let Some(repo_id) = &session.repo_ref {
        if let Ok(Some(repo)) = crate::repo::get_repo(store, repo_id) {
            crate::repo::remove_worktree(&repo.root_path, run_id);
        }
    }
    emit(
        subscribers,
        CoreEvent::RunCancelled {
            session: run_id.to_string(),
        },
    );
    notify_campaign(self_tx, run_id, crate::campaign::NodeOutcome::Cancelled);
    runner.on_run_complete(run_id);
    Ok(SessionStatus::Cancelled)
}

/// The governed unit-count ceiling: the highest `unit-<ord>` execution phase a policy can name, and
/// the launch-time limit `pipeline::pre_distribute` enforces against THIS constant — it rejects any
/// run whose unit count exceeds it (there is no separate `MAX_UNITS`; the plan path reads
/// `DENY_PHASE_SPAN` directly). Governance must never fail open by letting units run past a policy's
/// possible coverage.
///
/// HISTORY (FINDING-028): [`register_deny_policy`] used to fan a policy out across
/// `unit-1..=unit-256` regardless of the `phase` the caller passed, because an unvalidated phase
/// string narrowed to `applies_to: [phase]` could be inert (fail-open) — over-matching was the
/// fail-closed workaround, and this constant sized it. The fan-out is GONE: `phase` is now
/// validated at registration and `applies_to` is exactly `[phase]`. The constant remains as (a)
/// the launch cap above, still needed because policies persisted BEFORE the fix enumerate only
/// `unit-1..=unit-256` and a longer run would outrun them, and (b) the bound on the synthetic
/// `unit-<N>`/`u<N>` forms [`is_synthetic_unit_phase`] accepts — an ord past the launch cap can
/// never execute, so a policy on it could never fire.
pub(crate) const DENY_PHASE_SPAN: u32 = 256;

/// Capture a TERMINAL run's outcome into memory (best-effort). Names the run + its result (and, for a
/// failure, why) so a later recall surfaces "we tried X — it <outcome>". No-op on non-terminal status.
fn capture_run_outcome(
    memory: Option<&mut crate::memory::RunMemory>,
    store: &dyn GraphStore,
    run_id: &str,
) {
    let Some(mem) = memory else { return };
    let Ok(Some(session)) = crate::domain::get_session(store, run_id) else {
        return;
    };
    let outcome = match session.status {
        SessionStatus::Completed => "completed",
        SessionStatus::Failed => "failed",
        SessionStatus::Cancelled => "cancelled",
        _ => return, // Paused etc. — not terminal, nothing to remember yet
    };
    let brief: String = session
        .problem
        .lines()
        .next()
        .unwrap_or(run_id)
        .chars()
        .take(160)
        .collect();
    let detail = if matches!(session.status, SessionStatus::Failed) {
        crate::domain::session_units(store, run_id)
            .ok()
            .and_then(|us| us.into_iter().find_map(|u| u.denial_reason))
            .map(|r| format!(" — {r}"))
            .unwrap_or_default()
    } else {
        String::new()
    };
    // An UNFILED run's outcome stays at ROOT — the global briefing pool that `recall` (querying at
    // root) draws from. A PROJECT-BOUND run's outcome is scope-prefixed `project:<id>/run:<run_id>`
    // (DES-PROJECT-001 §3.2) so the project has a record; root listing/recall still sees it
    // (subtree inheritance — root is an ancestor of every scope). Membership is many-to-many by
    // design (§9.4), so EVERY holding project gets the outcome under its own scope — scoping only
    // the first would leave the other projects recordless (Copilot, PR #246).
    let scopes: Vec<wicked_estate_memory_core::Scope> =
        match crate::project::member_projects(store, crate::project::MEMBER_KIND_RUN, run_id) {
            Ok(pids) if !pids.is_empty() => pids
                .iter()
                .map(|pid| {
                    wicked_estate_memory_core::Scope::parse(&format!("project:{pid}/run:{run_id}"))
                })
                .collect(),
            _ => vec![wicked_estate_memory_core::Scope::root()],
        };
    let content = format!("Run '{brief}' ({run_id}) {outcome}{detail}.");
    for scope in scopes {
        if let Err(e) = mem.capture(content.clone(), scope, crate::memory::now_secs()) {
            eprintln!("wicked-core: memory capture failed: {e}");
        }
    }
}

/// Register a deny policy on the store (single-writer), scoped to exactly the `phase` the caller
/// named (FINDING-028). The UI's `trigger` is a literal string, so we regex-escape it (governance
/// matches `Trigger.contains` as a regex over the call context).
///
/// `phase` is VALIDATED before anything lands: it must name a phase of some registered workflow
/// (the token the gate matches via `scope::phase_aliases`), or a synthetic execution form
/// (`unit-<N>` / ad-hoc `u<N>`, 1..=[`DENY_PHASE_SPAN`]). An unrecognized string is REJECTED with
/// the valid tokens — registering it would produce a policy that never fires, and an inert deny is
/// the silent fail-open FINDING-021 was (a policy the operator believes is standing guard, matching
/// nothing). This validation is what made narrowing safe: the previous `unit-1..=unit-256` fan-out
/// existed precisely because an unvalidated `phase` could be anything, and over-matching was the
/// fail-closed direction. With the tokens checked at the write boundary, `applies_to = [phase]`
/// makes the documented contract ("blocks any tool-call in `phase`") actually true.
fn register_deny_policy(
    store: &mut dyn GraphStore,
    registry: &crate::workflow::WorkflowRegistry,
    phase: &str,
    trigger: &str,
) -> anyhow::Result<()> {
    use wicked_governance::{register_policy, Effect, Policy, Severity, Trigger};
    let phase = phase.trim();
    let known_workflow_phase = registry
        .ids()
        .iter()
        .filter_map(|id| registry.get(id))
        .flat_map(|def| def.phases.iter())
        .any(|p| p.id == phase);
    if !known_workflow_phase && !is_synthetic_unit_phase(phase) {
        let mut known: Vec<String> = registry
            .ids()
            .iter()
            .filter_map(|id| registry.get(id))
            .flat_map(|def| def.phases.iter().map(|p| p.id.clone()))
            .collect();
        known.sort();
        known.dedup();
        anyhow::bail!(
            "phase `{phase}` names no phase of any registered workflow and no synthetic unit form \
             (`unit-<N>` or `u<N>`, 1..={DENY_PHASE_SPAN}) — refusing to register: a deny scoped \
             to a phase that never executes would never fire, and an inert policy fails open. \
             Registered workflow phases: {}",
            known.join(", ")
        );
    }
    let applies_to = vec![phase.to_string()];
    let policy = Policy {
        id: format!(
            "ui-deny-{phase}-{}",
            pipeline::deterministic_id(&[phase, trigger])
        ),
        kind: "guard".into(),
        applies_to,
        effect: Effect::Deny,
        trigger: Trigger {
            contains: Some(regex_escape(trigger)),
        },
        obligations: vec![],
        criteria: format!("{phase}: deny `{trigger}`"),
        severity: Severity::High,
        rule: format!("deny {phase}-phase tool-calls containing `{trigger}`"),
        retired: false,
    };
    register_policy(store, &policy)
}

/// Is `phase` a synthetic execution-phase token in its CANONICAL spelling — `unit-<N>` (the
/// engine-derived phase every unit executes under, `scope::unit_phase`) or `u<N>` (the phase id an
/// AD-HOC unit carries, the `u<ord>` suffix of `<session>:u<ord>`), with 1 <= N <= [`DENY_PHASE_SPAN`]?
///
/// The round-trip (`format!` back and compare) is load-bearing, not pedantry: `"007".parse::<u32>()`
/// and `"+7".parse::<u32>()` both yield 7, so accepting any parseable digits would register
/// `unit-007` — a policy the gate's EQUALITY match (`select_any`) can never select. That is the
/// inert-policy fail-open this function exists to refuse. The span bound refuses ords past the
/// launch cap for the same reason: a unit beyond it can never execute, so a policy on it never fires.
fn is_synthetic_unit_phase(phase: &str) -> bool {
    let (prefix, digits) = match phase.strip_prefix("unit-") {
        Some(d) => ("unit-", d),
        None => match phase.strip_prefix('u') {
            Some(d) => ("u", d),
            None => return false,
        },
    };
    match digits.parse::<u32>() {
        Ok(n) if (1..=DENY_PHASE_SPAN).contains(&n) => format!("{prefix}{n}") == phase,
        _ => false,
    }
}

/// Escape regex metacharacters so a literal operator-typed trigger matches literally.
fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for c in s.chars() {
        if "\\.+*?()|[]{}^$".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn emit_run_error(subscribers: &mut crate::event_log::EventSink, run_id: &str, e: anyhow::Error) {
    emit(
        subscribers,
        CoreEvent::Error {
            session: Some(run_id.to_string()),
            message: e.to_string(),
        },
    );
}

/// Terminal-failure path for the asynchronous actor-loop faults that abort a run *outside*
/// `apply_step_result` — pre-distribute, worktree, council, and re-dispatch failures.
///
/// Each of those sites used to write `SessionStatus::Failed` straight to the store and emit only
/// [`CoreEvent::Error`]. `Error` is **not** a terminal event — it is also emitted for non-fatal
/// conditions on a run that keeps going — so the store said the run had ended while the live stream
/// only said "an error happened" and then went quiet. A consumer cannot tell that apart from a run
/// still working, and waits forever. Observed as `sessionStarted → error → silence` on `/ws` against
/// a run `GET /runs/<id>` already reported as `failed`.
///
/// Routing them all through [`fail_run`] keeps the store write, the terminal `SessionFailed`, the
/// campaign notify, and the runner's resource release (ACP sessions, PTY terminals) from drifting
/// apart again — eight hand-rolled copies is how they drifted in the first place.
///
/// The campaign notify is consistency with that canonical path rather than a fix for an observed
/// wedge: a campaign node launches via [`launch_run_inner`], which plans synchronously and hands the
/// error back to `campaign::dispatch` to reconcile, so it never arrives here. These sites belong to
/// the standalone `LaunchRun` path, which defers planning off-thread.
///
/// `e` is emitted as `Error` *in addition to* `SessionFailed`: `Error` carries the human-readable
/// reason, `SessionFailed` carries the lifecycle transition. Neither substitutes for the other.
/// `ord` mirrors `apply_step_result`'s convention — the cursor unit, which is `0` for a run that
/// never dispatched one.
fn fail_run_by_id(
    store: &mut dyn GraphStore,
    subscribers: &mut crate::event_log::EventSink,
    runner: &Arc<dyn StepRunner>,
    self_tx: &Sender<Command>,
    run_id: &str,
    e: anyhow::Error,
) {
    match crate::domain::get_session(store, run_id) {
        Ok(Some(mut session)) => {
            // Already terminal (typically cancelled while an off-thread stage was running):
            // do not clobber the status, do not emit a second terminal event, and do not add
            // error noise to a run the operator already ended.
            if matches!(
                session.status,
                SessionStatus::Completed | SessionStatus::Cancelled | SessionStatus::Failed
            ) {
                return;
            }
            emit_run_error(subscribers, run_id, e);
            let ord = session.unit_ix as u32;
            let _ = fail_run(store, subscribers, runner, self_tx, &mut session, ord);
        }
        // Session unreadable or absent — no lifecycle transition is possible, but the operator
        // still needs the reason, which is all these sites surfaced before.
        _ => emit_run_error(subscribers, run_id, e),
    }
}

/// The single point every one of core's event emissions passes through: record to the run's durable
/// log, then fan out to live subscribers (dropping any whose receiver has hung up).
///
/// Kept as a free fn over a bare method call because it is the chokepoint the whole event stream is
/// funnelled through — 47 call sites — and because the sink has to be a SEPARATE `&mut` from the store
/// the same call sites borrow. See [`crate::event_log`] for why the log is a file and not the store.
fn emit(subscribers: &mut crate::event_log::EventSink, ev: CoreEvent) {
    subscribers.emit(ev);
}

/// Overridable per-CLI price-table fallback for `CliUsage.cost_usd` (DES-STUDIO-COCKPIT-001 §3 B-cost /
/// NFR-5). claude reports cost directly, so this only fires for a seat that reports TOKENS but no cost.
/// The table is read from the `WICKED_CLI_PRICES` env var (JSON:
/// `{ "<cli>": { "input_per_mtok": <f>, "output_per_mtok": <f> } }`) — a cross-platform, file-free
/// override. Absent / unparseable / no entry ⇒ `None`, so we never assert a dollar figure the CLI didn't
/// imply (the panel then shows tokens only).
fn cost_from_price_table(cli: &str, input_tokens: u64, output_tokens: u64) -> Option<f64> {
    let raw = std::env::var("WICKED_CLI_PRICES").ok()?;
    let map: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let entry = map.get(cli)?;
    let in_per = entry.get("input_per_mtok").and_then(|v| v.as_f64())?;
    let out_per = entry.get("output_per_mtok").and_then(|v| v.as_f64())?;
    Some(input_tokens as f64 / 1e6 * in_per + output_tokens as f64 / 1e6 * out_per)
}

/// Read the agent session ids on the store (by their session-node names).
fn list_sessions(store: &impl GraphRead) -> anyhow::Result<Vec<String>> {
    let query = SymbolQuery {
        kinds: vec![NodeKind::Other(AGENT_SESSION.to_string())],
        ..Default::default()
    };
    Ok(store
        .find_symbols(&query)?
        .into_iter()
        .map(|n| n.name)
        .collect())
}

/// Read every session + its ordered units (the UI's project list).
fn list_projects(store: &impl GraphRead) -> anyhow::Result<Vec<crate::SessionView>> {
    let mut views = Vec::new();
    for session in crate::domain::all_sessions(store)? {
        let units = crate::domain::session_units(store, &session.id)?;
        views.push(crate::SessionView { session, units });
    }
    Ok(views)
}

#[cfg(test)]
mod gate_pause_tests {
    use super::{should_pause, PauseReason};
    use crate::domain::{AgentSession, HumanConfirm, SessionStatus, UnitStatus, WorkUnit};
    use crate::scope::EntityMode;
    use crate::workflow::{GateCond, GateSpec};

    fn sess(hc: HumanConfirm) -> AgentSession {
        AgentSession {
            id: "s".into(),
            workflow_id: "wf-s".into(),
            problem: "p".into(),
            entity_mode: EntityMode::Shared,
            collection_scope: None,
            clis: vec![],
            status: SessionStatus::Executing,
            human_confirm: hc,
            unit_ix: 0,
            attempt: 0,
            workdir: None,
            repo_ref: None,
            extra_write_roots: Vec::new(),
            project_graph: None,
            archived_at: None,
            archive_note: None,
        }
    }
    fn unit(ord: u32, gate: GateSpec, status: UnitStatus) -> WorkUnit {
        let mut u = WorkUnit::pending(format!("s:u{ord}"), "s", ord, "d");
        u.gate = gate;
        u.status = status;
        u
    }

    #[test]
    fn a_def_humanconfirm_gate_pauses_even_when_run_level_is_none() {
        // The DEF drives the pause: run-level --confirm is None, yet the preceding phase's
        // HumanConfirm gate must still pause the run before the next unit.
        //
        // This is the INTENTIONAL precedence (FINDING-023), not the defect: `None` is both the
        // enum default and the typo fallback (FINDING-019), so letting it suppress a
        // workflow-authored gate would strip declared review seams on every default or misspelled
        // launch — fail-open. The pause instead disclosing itself is pinned by
        // `def_gate_disclosure_tests`. Anyone changing this behavior must first give "unattended"
        // an explicit, non-default wire signal.
        let s = sess(HumanConfirm::None);
        let units = vec![
            unit(
                1,
                GateSpec::HumanConfirm {
                    unconditional: false,
                },
                UnitStatus::Done,
            ),
            unit(2, GateSpec::Auto, UnitStatus::Pending),
        ];
        // Attributed to unit 1, not unit 2. That attribution is the whole point: unit 2 declared
        // `Auto` and has produced nothing, so a pause reported against it tells the operator to
        // approve work that does not exist (FINDING-032).
        assert_eq!(
            should_pause(&s, &units, 1),
            Some(PauseReason::DefGate { reviewing_ord: 1 }),
            "preceding def gate must pause, and name itself as the unit under review"
        );
        assert_eq!(
            should_pause(&s, &units, 0),
            None,
            "no preceding phase ⇒ no def pause"
        );
    }

    #[test]
    fn auto_gate_defers_to_the_run_level_policy() {
        let units = vec![
            unit(1, GateSpec::Auto, UnitStatus::Done),
            unit(2, GateSpec::Auto, UnitStatus::Pending),
        ];
        assert_eq!(should_pause(&sess(HumanConfirm::None), &units, 1), None);
        assert_eq!(
            should_pause(&sess(HumanConfirm::All), &units, 1),
            Some(PauseReason::RunLevel),
            "run-level All still pauses when the def gate is Auto — and reviews nothing, because \
             the pause is a policy applied BEFORE unit 2 rather than a judgement on unit 1"
        );
    }

    #[test]
    fn a_def_gate_outranks_the_run_level_policy_when_both_fire() {
        // Both sources fire. `DefGate` must win: it is the only one that can name what the
        // operator is looking at, so resolving the tie the other way would silently drop the
        // attribution on exactly the runs that are MOST supervised.
        let s = sess(HumanConfirm::All);
        let units = vec![
            unit(
                1,
                GateSpec::HumanConfirm {
                    unconditional: false,
                },
                UnitStatus::Done,
            ),
            unit(2, GateSpec::Auto, UnitStatus::Pending),
        ];
        assert_eq!(
            should_pause(&s, &units, 1),
            Some(PauseReason::DefGate { reviewing_ord: 1 }),
            "the more specific source wins the tie; the run still pauses either way"
        );
    }

    #[test]
    fn conditional_gate_pauses_only_when_the_prev_phase_is_not_a_clean_pass() {
        let s = sess(HumanConfirm::None);
        let passed = vec![
            unit(
                1,
                GateSpec::HumanConfirmIf(GateCond::VerdictNotPass),
                UnitStatus::Done,
            ),
            unit(2, GateSpec::Auto, UnitStatus::Pending),
        ];
        assert_eq!(
            should_pause(&s, &passed, 1),
            None,
            "clean pass (Done) ⇒ no pause"
        );
        let not_passed = vec![
            unit(
                1,
                GateSpec::HumanConfirmIf(GateCond::VerdictNotPass),
                UnitStatus::Rejected,
            ),
            unit(2, GateSpec::Auto, UnitStatus::Pending),
        ];
        assert_eq!(
            should_pause(&s, &not_passed, 1),
            Some(PauseReason::DefGate { reviewing_ord: 1 }),
            "not a clean pass ⇒ pause for a human, reviewing the unit that did not pass"
        );
    }
}

#[cfg(test)]
mod terminal_gate_tests {
    use super::*;
    use crate::domain::{
        put_node, AgentSession, HumanConfirm, SessionStatus, UnitStatus, WorkUnit,
    };
    use crate::scope::EntityMode;
    use crate::workflow::{GateSpec, StepInput, StepOutput, StepRunner, StepStatus};
    use std::sync::mpsc::channel;
    use wicked_apps_core::{open_store, ToNode};

    struct NoopRunner;
    impl StepRunner for NoopRunner {
        fn run_unit(&self, i: &StepInput) -> StepOutput {
            StepOutput {
                run_id: i.run_id.clone(),
                unit_ix: i.unit_ix,
                attempt: i.attempt,
                output: "unused".into(),
                status: StepStatus::Ok,
                usage: None,
                files: Vec::new(),
                tools: Vec::new(),
                governed: false,
            }
        }
    }

    fn seed_session(store: &mut dyn GraphStore, terminal_gate: GateSpec) {
        let session = AgentSession {
            id: "r".into(),
            workflow_id: "wf-r".into(),
            problem: "p".into(),
            entity_mode: EntityMode::Shared,
            collection_scope: None,
            clis: vec![],
            status: SessionStatus::Executing,
            human_confirm: HumanConfirm::None,
            unit_ix: 1, // cursor is PAST the single (terminal) unit — the run is out of units
            attempt: 0,
            workdir: None,
            repo_ref: None,
            extra_write_roots: Vec::new(),
            project_graph: None,
            archived_at: None,
            archive_note: None,
        };
        put_node(store, session.to_node()).unwrap();
        // One APPROVED terminal unit whose OWN gate is `terminal_gate`.
        let mut u = WorkUnit::pending("r:u1", "r", 1, "the final phase");
        u.gate = terminal_gate;
        u.status = UnitStatus::Done;
        put_node(store, u.to_node()).unwrap();
    }

    /// Seam finding #4: a def-declared unconditional `HumanConfirm` on the TERMINAL phase must PAUSE
    /// before the run finalizes — it must NOT be silently dropped into a `Completed` finalize.
    #[test]
    fn a_terminal_humanconfirm_gate_pauses_before_finalize() {
        let mut store = open_store(Some(":memory:")).unwrap();
        seed_session(
            &mut store,
            GateSpec::HumanConfirm {
                unconditional: true,
            },
        );
        let mut subs = crate::event_log::EventSink::default();
        let (tx, _rx) = channel::<Command>();
        let runner: Arc<dyn StepRunner> = Arc::new(NoopRunner);

        let progress = advance_or_pause(
            &mut store,
            &mut subs,
            &runner,
            &tx,
            "r",
            1,
            &None,
            &None,
            uuid::Uuid::nil(),
            false,
        )
        .unwrap();
        assert!(
            matches!(progress, Progress::Paused),
            "the terminal unit's own HumanConfirm gate must pause before finalize, got a Done finalize"
        );
        let session = crate::domain::get_session(&store, "r").unwrap().unwrap();
        assert_eq!(session.status, SessionStatus::AwaitingHuman);
    }

    /// Control: an `Auto` terminal gate finalizes (no spurious pause) — the fix is scoped to the
    /// terminal unit's OWN declared HumanConfirm gate.
    #[test]
    fn a_terminal_auto_gate_finalizes_without_pausing() {
        let mut store = open_store(Some(":memory:")).unwrap();
        seed_session(&mut store, GateSpec::Auto);
        let mut subs = crate::event_log::EventSink::default();
        let (tx, _rx) = channel::<Command>();
        let runner: Arc<dyn StepRunner> = Arc::new(NoopRunner);

        let progress = advance_or_pause(
            &mut store,
            &mut subs,
            &runner,
            &tx,
            "r",
            1,
            &None,
            &None,
            uuid::Uuid::nil(),
            false,
        )
        .unwrap();
        assert!(
            matches!(progress, Progress::Done),
            "an Auto terminal gate must finalize (Done), never pause"
        );
    }
}

/// PHASE SUBSTANCE GATE — a governed Creator/Neutral unit whose Ok fold carries neither prose
/// (under 200 trimmed chars) nor a worktree change is REJECTED through the standard failure path
/// with `denial_reason: "phase produced no reviewable substance"`, never folded as a completed
/// phase. Driven through `apply_step_result` — the real fold site — with an in-memory store and
/// no subprocesses (`workdir: None` ⇒ no observable diff by construction).
#[cfg(test)]
mod substance_gate_tests {
    use super::*;
    use crate::domain::{
        put_node, AgentSession, HumanConfirm, SessionStatus, UnitStatus, WorkUnit,
    };
    use crate::scope::EntityMode;
    use crate::workflow::{PhaseRole, StepInput, StepOutput, StepRunner, StepStatus};
    use std::sync::mpsc::channel;
    use wicked_apps_core::{open_store, ToNode};

    const NO_SUBSTANCE: &str = "phase produced no reviewable substance";

    struct NoopRunner;
    impl StepRunner for NoopRunner {
        fn run_unit(&self, i: &StepInput) -> StepOutput {
            StepOutput {
                run_id: i.run_id.clone(),
                unit_ix: i.unit_ix,
                attempt: i.attempt,
                output: "unused".into(),
                status: StepStatus::Ok,
                usage: None,
                files: Vec::new(),
                tools: Vec::new(),
                governed: false,
            }
        }
    }

    /// One Executing session at cursor 0 over a single unit of `role`. `workdir: None` — the run
    /// has no worktree, so "worktree diff is empty" holds by construction and the substance
    /// decision rides entirely on the output's length (and the unit's role/governed flag).
    fn seed(store: &mut dyn GraphStore, run_id: &str, role: PhaseRole) {
        let session = AgentSession {
            id: run_id.into(),
            workflow_id: format!("wf-{run_id}"),
            problem: "p".into(),
            entity_mode: EntityMode::Shared,
            collection_scope: None,
            clis: vec![],
            status: SessionStatus::Executing,
            human_confirm: HumanConfirm::None,
            unit_ix: 0,
            attempt: 0,
            workdir: None,
            repo_ref: None,
            extra_write_roots: Vec::new(),
            project_graph: None,
            archived_at: None,
            archive_note: None,
        };
        put_node(store, session.to_node()).unwrap();
        let mut u = WorkUnit::pending(format!("{run_id}:u1"), run_id, 1, "build the feature");
        u.role = role;
        u.status = UnitStatus::Distributed;
        put_node(store, u.to_node()).unwrap();
        // The orchestration workflow the Ok fold ticks (same shape `pre_distribute` registers) —
        // without it every fold that reaches `apply_and_finish_unit` errors "workflow not found".
        wicked_orchestration::register_workflow(
            store,
            format!("wf-{run_id}"),
            "p",
            &[(format!("wf-{run_id}:unit-1"), "build the feature")],
        )
        .unwrap();
    }

    /// Fold an Ok result for the seeded unit and return `(StepApplied, session, unit)`.
    fn fold(
        store: &mut dyn GraphStore,
        subs: &mut crate::event_log::EventSink,
        run_id: &str,
        output_text: &str,
        governed: bool,
    ) -> (StepApplied, AgentSession, WorkUnit) {
        let (tx, _rx) = channel::<Command>();
        let runner: Arc<dyn StepRunner> = Arc::new(NoopRunner);
        let out = StepOutput {
            run_id: run_id.into(),
            unit_ix: 0,
            attempt: 0,
            output: output_text.into(),
            status: StepStatus::Ok,
            usage: None,
            files: Vec::new(),
            tools: Vec::new(),
            governed,
        };
        let applied = apply_step_result(
            store,
            subs,
            &runner,
            &tx,
            out,
            None,
            "",
            &None,
            &None,
            uuid::Uuid::nil(),
            false,
        )
        .unwrap();
        let session = crate::domain::get_session(store, run_id).unwrap().unwrap();
        let unit = crate::domain::session_units(store, run_id)
            .unwrap()
            .remove(0);
        (applied, session, unit)
    }

    /// The rejection: governed + Creator role + a one-liner + no worktree change ⇒ the standard
    /// failure path — unit Rejected with the substance denial, StepFailed emitted with the same
    /// detail, run terminally Failed.
    #[test]
    fn a_governed_no_substance_ok_fold_is_rejected() {
        let run_id = format!("substance-reject-{}", std::process::id());
        let mut store = open_store(Some(":memory:")).unwrap();
        seed(&mut store, &run_id, PhaseRole::Creator);
        let mut subs = crate::event_log::EventSink::default();
        let (esub, erx) = channel();
        subs.push(esub);

        let (applied, session, unit) = fold(&mut store, &mut subs, &run_id, "done.", true);

        assert!(
            matches!(applied, StepApplied::Finished),
            "the substance rejection must terminate the run (standard failure path)"
        );
        assert_eq!(session.status, SessionStatus::Failed);
        assert_eq!(unit.status, UnitStatus::Rejected);
        assert_eq!(unit.denial_reason.as_deref(), Some(NO_SUBSTANCE));
        let saw_step_failed = std::iter::from_fn(|| erx.try_recv().ok()).any(|ev| {
            matches!(
                ev,
                CoreEvent::StepFailed { detail, failure_kind, .. }
                    if detail == NO_SUBSTANCE
                        && failure_kind == crate::event::StepFailureKind::SubstanceRejected
            )
        });
        assert!(
            saw_step_failed,
            "the standard failure path emits StepFailed carrying the substance denial, \
             kinded SubstanceRejected (a core veto, not a worker failure)"
        );
    }

    /// The passing case: the SAME one-liner + clean worktree folds fine when the unit is
    /// ungoverned — the gate is scoped to governed units, so the engine's own internal phases
    /// and ungoverned runs are untouched.
    #[test]
    fn an_ungoverned_short_ok_fold_still_completes() {
        let run_id = format!("substance-ungoverned-{}", std::process::id());
        let mut store = open_store(Some(":memory:")).unwrap();
        seed(&mut store, &run_id, PhaseRole::Creator);
        let mut subs = crate::event_log::EventSink::default();

        let (applied, session, unit) = fold(&mut store, &mut subs, &run_id, "done.", false);

        assert!(
            matches!(applied, StepApplied::Finished),
            "single-unit run folds Ok and finalizes"
        );
        assert_eq!(
            session.status,
            SessionStatus::Completed,
            "an ungoverned short Ok fold must complete, never trip the substance gate"
        );
        assert_eq!(unit.status, UnitStatus::Done);
        assert_eq!(unit.denial_reason, None);
    }

    /// Evaluator exemption: an Evaluator-role unit's output is a VERDICT over another unit's work
    /// — it is short by nature and carries its own pinned floors (`builtin_floors`), so the
    /// substance gate must not fire on it. (The unit still fails downstream here — a governed
    /// unit with no decisions log fails closed — which is exactly the point: it reached the
    /// NORMAL gate, not the substance rejection.)
    #[test]
    fn an_evaluator_unit_is_exempt_from_the_substance_gate() {
        let run_id = format!("substance-evaluator-{}", std::process::id());
        let mut store = open_store(Some(":memory:")).unwrap();
        seed(&mut store, &run_id, PhaseRole::Evaluator);
        let mut subs = crate::event_log::EventSink::default();

        let (_applied, _session, unit) = fold(&mut store, &mut subs, &run_id, "PASS", true);

        assert_ne!(
            unit.denial_reason.as_deref(),
            Some(NO_SUBSTANCE),
            "an Evaluator-role unit must never be rejected for lack of substance"
        );
    }

    /// The prose threshold: 200+ trimmed chars IS reviewable substance even with a clean
    /// worktree (recon/analysis phases legitimately produce prose only), so the substance gate
    /// stays closed and the fold proceeds to the normal gates.
    #[test]
    fn two_hundred_chars_of_prose_clears_the_substance_gate() {
        let run_id = format!("substance-prose-{}", std::process::id());
        let mut store = open_store(Some(":memory:")).unwrap();
        seed(&mut store, &run_id, PhaseRole::Creator);
        let mut subs = crate::event_log::EventSink::default();
        // Exactly 200 TRIMMED chars — the boundary itself must pass ("under 200" rejects).
        let prose = format!("  {}  ", "a".repeat(200));

        let (_applied, _session, unit) = fold(&mut store, &mut subs, &run_id, &prose, true);

        assert_ne!(
            unit.denial_reason.as_deref(),
            Some(NO_SUBSTANCE),
            "200 trimmed chars of prose must clear the substance gate"
        );
    }
}

/// core#282 — AUTONOMOUS SEAT FAILOVER over the whole roster. A WORKER-originated failure (CLI
/// exited nonzero / could not spawn / timed out) must hand the unit to the NEXT eligible seat,
/// never to a seat that already worker-failed it, preserving evaluator≠creator, and only fail
/// terminally once every eligible seat has been tried. Driven through the REAL
/// `apply_step_result` / `resume_run_inner` over an in-memory store (the substance-gate tests'
/// harness), plus pure selection tests over `next_failover_seat`.
#[cfg(test)]
mod seat_failover_tests {
    use super::*;
    use crate::domain::{
        put_node, AgentSession, HumanConfirm, SessionStatus, UnitStatus, WorkUnit,
    };
    use crate::scope::EntityMode;
    use crate::workflow::{PhaseRole, StepInput, StepOutput, StepRunner, StepStatus};
    use std::sync::mpsc::channel;
    use wicked_apps_core::{open_store, ToNode};

    struct NoopRunner;
    impl StepRunner for NoopRunner {
        fn run_unit(&self, i: &StepInput) -> StepOutput {
            StepOutput {
                run_id: i.run_id.clone(),
                unit_ix: i.unit_ix,
                attempt: i.attempt,
                output: "unused".into(),
                status: StepStatus::Ok,
                usage: None,
                files: Vec::new(),
                tools: Vec::new(),
                governed: false,
            }
        }
    }

    /// A session at cursor `unit_ix` over `clis`. `HumanConfirm::None` — autonomous, so a worker
    /// failure skips the attempt-0 triage escalation and lands in the mechanical ladder directly.
    fn seed_session(
        store: &mut dyn GraphStore,
        run_id: &str,
        clis: &[&str],
        status: SessionStatus,
        unit_ix: usize,
    ) {
        let session = AgentSession {
            id: run_id.into(),
            workflow_id: format!("wf-{run_id}"),
            problem: "p".into(),
            entity_mode: EntityMode::Shared,
            collection_scope: None,
            clis: clis.iter().map(|c| c.to_string()).collect(),
            status,
            human_confirm: HumanConfirm::None,
            unit_ix,
            attempt: 0,
            workdir: None,
            repo_ref: None,
            extra_write_roots: Vec::new(),
            project_graph: None,
            archived_at: None,
            archive_note: None,
        };
        put_node(store, session.to_node()).unwrap();
    }

    /// A def-driven unit (`<run>:<phase>` id, so `phase_id()` — and with it the
    /// evaluator≠creator exclusion — resolves).
    #[allow(clippy::too_many_arguments)] // test-only seeding helper; the args mirror WorkUnit's shape
    fn seed_unit(
        store: &mut dyn GraphStore,
        run_id: &str,
        ord: u32,
        phase: &str,
        cli: &str,
        role: PhaseRole,
        depends_on: &[&str],
        status: UnitStatus,
    ) {
        let mut u = WorkUnit::pending(format!("{run_id}:{phase}"), run_id, ord, "work");
        u.assigned_cli = Some(cli.to_string());
        u.role = role;
        u.depends_on = depends_on.iter().map(|d| d.to_string()).collect();
        u.status = status;
        put_node(store, u.to_node()).unwrap();
    }

    /// Fold a GOVERNED worker FAILURE for the run's cursor unit at its live attempt.
    fn fold_failed(
        store: &mut dyn GraphStore,
        run_id: &str,
        failure_text: &str,
    ) -> (StepApplied, AgentSession, Vec<WorkUnit>) {
        let (tx, _rx) = channel::<Command>();
        let runner: Arc<dyn StepRunner> = Arc::new(NoopRunner);
        let mut subs = crate::event_log::EventSink::default();
        let live = crate::domain::get_session(store, run_id).unwrap().unwrap();
        let out = StepOutput {
            run_id: run_id.into(),
            unit_ix: live.unit_ix,
            attempt: live.attempt,
            output: failure_text.into(),
            status: StepStatus::Failed,
            usage: None,
            files: Vec::new(),
            tools: Vec::new(),
            governed: true,
        };
        let applied = apply_step_result(
            store,
            &mut subs,
            &runner,
            &tx,
            out,
            None,
            "",
            &None,
            &None,
            uuid::Uuid::nil(),
            false,
        )
        .unwrap();
        let session = crate::domain::get_session(store, run_id).unwrap().unwrap();
        let units = crate::domain::session_units(store, run_id).unwrap();
        (applied, session, units)
    }

    /// THE required behavior: failover walks the roster IN ORDER, never repeats a seat that
    /// already worker-failed the unit, and goes terminal only after every seat has been tried.
    #[test]
    fn failover_walks_seats_in_order_and_exhausts_before_terminal_failure() {
        let run_id = format!("failover-walk-{}", std::process::id());
        let mut store = open_store(Some(":memory:")).unwrap();
        seed_session(
            &mut store,
            &run_id,
            &["agy", "claude", "codex"],
            SessionStatus::Executing,
            0,
        );
        seed_unit(
            &mut store,
            &run_id,
            1,
            "u1",
            "agy",
            PhaseRole::Creator,
            &[],
            UnitStatus::Distributed,
        );

        // Failure 1: agy → claude (the next roster seat), run keeps executing.
        let (applied, session, units) =
            fold_failed(&mut store, &run_id, "(cli `agy` exited 1) boom");
        assert!(matches!(applied, StepApplied::Continuing));
        assert_eq!(session.status, SessionStatus::Executing);
        assert_eq!(units[0].assigned_cli.as_deref(), Some("claude"));
        assert_eq!(units[0].worker_failed_clis, vec!["agy".to_string()]);

        // Failure 2: claude → codex — NOT back to agy, which already worker-failed this unit
        // (the core#282 regression: the old ladder re-picked the first non-current seat).
        let (applied, _s, units) = fold_failed(&mut store, &run_id, "(cli `claude` exited 1) boom");
        assert!(matches!(applied, StepApplied::Continuing));
        assert_eq!(
            units[0].assigned_cli.as_deref(),
            Some("codex"),
            "the walk must advance to the next UNTRIED seat, never repeat a failed one"
        );
        assert_eq!(
            units[0].worker_failed_clis,
            vec!["agy".to_string(), "claude".to_string()]
        );

        // Failure 3: the roster is exhausted — ONLY NOW does the unit fail terminally.
        let (applied, session, units) =
            fold_failed(&mut store, &run_id, "(cli `codex` exited 1) boom");
        assert!(matches!(applied, StepApplied::Finished));
        assert_eq!(session.status, SessionStatus::Failed);
        assert_eq!(units[0].status, UnitStatus::Rejected);
        assert_eq!(
            units[0].worker_failed_clis,
            vec!["agy".to_string(), "claude".to_string(), "codex".to_string()],
            "the terminal write persists the full ledger for a later resume"
        );
    }

    /// evaluator≠creator holds DURING failover: the walk skips the seat that created the work
    /// this unit reviews, even when that seat is untried and earlier in the roster.
    #[test]
    fn failover_never_hands_an_evaluator_to_its_creator() {
        let run_id = format!("failover-evc-{}", std::process::id());
        let mut store = open_store(Some(":memory:")).unwrap();
        seed_session(
            &mut store,
            &run_id,
            &["claude", "agy", "codex"],
            SessionStatus::Executing,
            1,
        );
        seed_unit(
            &mut store,
            &run_id,
            1,
            "build",
            "claude",
            PhaseRole::Creator,
            &[],
            UnitStatus::Done,
        );
        seed_unit(
            &mut store,
            &run_id,
            2,
            "adversarial-review",
            "agy",
            PhaseRole::Evaluator,
            &["build"],
            UnitStatus::Distributed,
        );

        let (applied, _s, units) = fold_failed(&mut store, &run_id, "(cli `agy` exited 143)");
        assert!(matches!(applied, StepApplied::Continuing));
        assert_eq!(
            units[1].assigned_cli.as_deref(),
            Some("codex"),
            "claude is the creator of the work under review — the failover must skip it \
             even though it is first in the roster and untried"
        );
    }

    /// The observed core#282 shape: a TIMEOUT is a worker error and must enter the ladder —
    /// before the fix it matched no transient signature and killed the run at the first seat.
    #[test]
    fn a_timeout_shaped_worker_failure_enters_the_failover_ladder() {
        let run_id = format!("failover-timeout-{}", std::process::id());
        let mut store = open_store(Some(":memory:")).unwrap();
        seed_session(
            &mut store,
            &run_id,
            &["agy", "claude"],
            SessionStatus::Executing,
            0,
        );
        seed_unit(
            &mut store,
            &run_id,
            1,
            "u1",
            "agy",
            PhaseRole::Creator,
            &[],
            UnitStatus::Distributed,
        );

        let (applied, session, units) =
            fold_failed(&mut store, &run_id, "ACP timeout waiting for response id=7");
        assert!(matches!(applied, StepApplied::Continuing));
        assert_eq!(session.status, SessionStatus::Executing);
        assert_eq!(units[0].assigned_cli.as_deref(), Some("claude"));
    }

    /// A resume of a worker-failed run must NOT re-dispatch the seat that failed the unit
    /// (the "re-dispatches the SAME assigned_cli forever" wedge) while an untried seat remains.
    #[test]
    fn a_resume_moves_the_cursor_unit_off_a_worker_failed_seat() {
        let run_id = format!("resume-rotate-{}", std::process::id());
        let mut store = open_store(Some(":memory:")).unwrap();
        seed_session(
            &mut store,
            &run_id,
            &["agy", "claude"],
            SessionStatus::Failed,
            0,
        );
        let mut u = WorkUnit::pending(format!("{run_id}:u1"), &run_id, 1, "work");
        u.assigned_cli = Some("agy".to_string());
        u.worker_failed_clis = vec!["agy".to_string()];
        u.denial_reason = Some("Worker FAILED on unit 1: (cli `agy` exited 1)".into());
        u.status = UnitStatus::Rejected;
        put_node(&mut store, u.to_node()).unwrap();

        let (tx, _rx) = channel::<Command>();
        let runner: Arc<dyn StepRunner> = Arc::new(NoopRunner);
        let mut subs = crate::event_log::EventSink::default();
        let mut in_flight = HashSet::new();
        let status = resume_run_inner(
            &mut store,
            &mut subs,
            &runner,
            &tx,
            &mut in_flight,
            &run_id,
            &None,
            &None,
            uuid::Uuid::nil(),
            false,
        )
        .unwrap();
        assert_eq!(status, SessionStatus::Executing);
        let units = crate::domain::session_units(&store, &run_id).unwrap();
        assert_eq!(
            units[0].assigned_cli.as_deref(),
            Some("claude"),
            "the resume must select the next untried seat, not hammer the failed one"
        );
        assert_eq!(
            units[0].worker_failed_clis,
            vec!["agy".to_string()],
            "the ledger survives the resume — agy stays excluded this cycle"
        );
    }

    /// A resume AFTER exhaustion (every eligible seat worker-failed — why the run went terminal)
    /// A JUDGED rejection after an earlier failover keeps its seat on resume (Copilot on
    /// #286): the ledger still holds the earlier worker-failed seat, but the TERMINAL
    /// failure was work-level — rotating away from the seat that produced reviewable work
    /// over a stale ledger entry would be a misclassification.
    #[test]
    fn a_judged_rejection_after_an_earlier_failover_keeps_its_seat_on_resume() {
        let run_id = format!("resume-judged-{}", std::process::id());
        let mut store = open_store(Some(":memory:")).unwrap();
        seed_session(
            &mut store,
            &run_id,
            &["agy", "claude", "codex"],
            SessionStatus::Failed,
            0,
        );
        let mut u = WorkUnit::pending(format!("{run_id}:u1"), &run_id, 1, "work");
        u.assigned_cli = Some("claude".to_string());
        u.worker_failed_clis = vec!["agy".to_string()]; // earlier failover, seat agy
        u.denial_reason = Some("adversarial review rejected the work".into()); // judged, not worker
        u.status = UnitStatus::Rejected;
        put_node(&mut store, u.to_node()).unwrap();

        let (tx, _rx) = channel::<Command>();
        let runner: Arc<dyn StepRunner> = Arc::new(NoopRunner);
        let mut subs = crate::event_log::EventSink::default();
        let mut in_flight = std::collections::HashSet::new();
        resume_run_inner(
            &mut store,
            &mut subs,
            &runner,
            &tx,
            &mut in_flight,
            &run_id,
            &None,
            &None,
            uuid::Uuid::nil(),
            false,
        )
        .unwrap();

        let units = crate::domain::session_units(&store, &run_id).unwrap();
        assert_eq!(
            units[0].assigned_cli.as_deref(),
            Some("claude"),
            "a work-level rejection must re-dispatch the SAME seat, ledger notwithstanding"
        );
        assert_eq!(
            units[0].worker_failed_clis,
            vec!["agy".to_string()],
            "the earlier worker-failure ledger survives untouched"
        );
    }

    /// grants a fresh failover budget instead of refusing forever: the operator may have fixed
    /// the environment, and a resume that could never re-dispatch would strand the run.
    #[test]
    fn a_resume_after_seat_exhaustion_clears_the_budget() {
        let run_id = format!("resume-exhausted-{}", std::process::id());
        let mut store = open_store(Some(":memory:")).unwrap();
        seed_session(
            &mut store,
            &run_id,
            &["agy", "claude"],
            SessionStatus::Failed,
            0,
        );
        let mut u = WorkUnit::pending(format!("{run_id}:u1"), &run_id, 1, "work");
        u.assigned_cli = Some("claude".to_string());
        u.worker_failed_clis = vec!["agy".to_string(), "claude".to_string()];
        u.denial_reason = Some("Worker FAILED on unit 1: (cli `claude` exited 1)".into());
        u.status = UnitStatus::Rejected;
        put_node(&mut store, u.to_node()).unwrap();

        let (tx, _rx) = channel::<Command>();
        let runner: Arc<dyn StepRunner> = Arc::new(NoopRunner);
        let mut subs = crate::event_log::EventSink::default();
        let mut in_flight = HashSet::new();
        let status = resume_run_inner(
            &mut store,
            &mut subs,
            &runner,
            &tx,
            &mut in_flight,
            &run_id,
            &None,
            &None,
            uuid::Uuid::nil(),
            false,
        )
        .unwrap();
        assert_eq!(status, SessionStatus::Executing);
        let units = crate::domain::session_units(&store, &run_id).unwrap();
        assert!(
            units[0].worker_failed_clis.is_empty(),
            "an explicit resume after exhaustion is a fresh start — the budget resets"
        );
        assert_eq!(
            units[0].assigned_cli.as_deref(),
            Some("claude"),
            "with every seat failed there is no better pick — the walk restarts from here"
        );
    }

    /// Pure selection: roster order, the tried-ledger, and the creator exclusion — including a
    /// depends_on creator sitting on the DEFAULT seat (`assigned_cli: None` ⇒ claude).
    #[test]
    fn next_failover_seat_selects_in_order_and_respects_exclusions() {
        let mut creator = WorkUnit::pending("s:build", "s", 1, "w");
        creator.assigned_cli = None; // the DEFAULT seat — must exclude "claude", not be dropped
        let mut eval = WorkUnit::pending("s:review", "s", 2, "w");
        eval.assigned_cli = Some("agy".into());
        eval.depends_on = vec!["build".into()];
        eval.worker_failed_clis = vec!["agy".into()];
        let units = vec![creator, eval];
        let roster = vec!["claude".to_string(), "agy".to_string(), "codex".to_string()];

        assert_eq!(
            next_failover_seat(&units, 1, &roster).as_deref(),
            Some("codex"),
            "claude is the (default-seat) creator, agy already failed — codex is next in order"
        );

        // Exhaustion: with codex tried too, no eligible seat remains.
        let mut units = units;
        units[1].worker_failed_clis.push("codex".into());
        assert_eq!(next_failover_seat(&units, 1, &roster), None);

        // A unit with no dependencies excludes nothing but its own failed seats.
        let mut lone = WorkUnit::pending("s:u3", "s", 3, "w");
        lone.worker_failed_clis = vec!["claude".into()];
        assert_eq!(
            next_failover_seat(&[lone], 0, &roster).as_deref(),
            Some("agy")
        );
    }
}

/// Live-output stream (`UnitOutputDelta`) — the ACTOR wiring over [`crate::output_throttle`],
/// whose window/boundary semantics are pinned in that module's own tests with fabricated
/// instants. Driven through a REAL actor (`Core::spawn_with_engine`, stub dispatcher + runner, no
/// subprocesses) by sending raw `Command::CliOutputDelta` — exactly what every worker path
/// (in-process, bus consumer, tool executor) sends.
///
/// DETERMINISM UNDER LOAD (PR #279 review): the actor's throttle runs on the wall clock, and a
/// loaded CI runner can stall the actor thread ≥500ms between chunks — so no assertion here may
/// depend on WHICH boundary (window vs. drain) flushes a pending tail. Every expectation below
/// is boundary-independent: the first chunk of a `(run, ord, attempt)` buffer always flushes
/// immediately, an over-cap chunk always flushes on size, and a pending tail always surfaces as
/// the same `(session, ord, attempt, text)` tuple whether the 500ms window elapsed mid-sequence
/// (time flush) or the step result drained it — because the attempt label rides IN-BAND on the
/// chunk, not on scheduling.
#[cfg(test)]
mod live_output_stream_tests {
    use super::*;
    use std::time::Duration;
    use wicked_council::types::{AgenticCli, Dispatcher, Vote};
    use wicked_council::CouncilTask;

    struct StubDispatcher;
    impl Dispatcher for StubDispatcher {
        fn dispatch(&self, _cli: &AgenticCli, _task: &CouncilTask) -> Option<Vote> {
            None // never convened — this test launches no run
        }
    }

    struct NoopRunner;
    impl StepRunner for NoopRunner {
        fn run_unit(&self, i: &StepInput) -> crate::workflow::StepOutput {
            crate::workflow::StepOutput {
                run_id: i.run_id.clone(),
                unit_ix: i.unit_ix,
                attempt: i.attempt,
                output: "unused".into(),
                status: crate::workflow::StepStatus::Ok,
                usage: None,
                files: Vec::new(),
                tools: Vec::new(),
                governed: false,
            }
        }
    }

    #[test]
    fn cli_output_chunks_surface_as_throttled_unit_output_deltas() {
        let dir = std::env::temp_dir().join(format!("wicked-live-output-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("live.db");
        let _ = std::fs::remove_file(&db);
        let core = crate::Core::spawn_with_engine(
            db.to_str().unwrap().to_string(),
            Arc::new(StubDispatcher),
            Arc::new(NoopRunner),
        );
        let events = core.subscribe();
        let send = |attempt: u32, chunk: String| {
            core.tx
                .send(Command::CliOutputDelta {
                    run_id: "live-run".into(),
                    ord: 3,
                    attempt,
                    chunk,
                    process_gen: None,
                    launch_seq: 0,
                })
                .expect("actor alive");
        };

        // The sequence is queued up-front, back-to-back — but NOTHING below depends on how fast
        // the actor works through it (see the module doc): each expected flush is pinned by a
        // scheduling-independent boundary, and the pending tail's tuple is identical whether it
        // leaves via the 500ms window (actor stalled mid-sequence) or the final drain.
        //
        // 1. A unit's FIRST chunk streams immediately (no startup delay), verbatim.
        send(0, "first chunk".into());
        // 2. An over-cap chunk flushes on the SIZE boundary (2KB pending forces a flush NOW,
        //    even inside the 500ms window) and the emitted text is capped by the head+tail elide.
        send(0, "z".repeat(5000));
        // 3. A RE-DISPATCHED attempt's first chunk: a fresh `(run, ord, attempt)` buffer — flushes
        //    immediately, labeled with ITS in-band attempt, never merged with attempt 0's buffer.
        send(1, "rework output".into());
        // 4. A chunk pending when the step result arrives surfaces labeled with the CHUNK's
        //    attempt — via the ApplyStepResult drain, or via the window if CI stalled us ≥500ms —
        //    NOT the result's attempt (7): the throttle keys by the in-band attempt, so a
        //    superseded attempt's tail can never be relabeled. (The fold itself errors on the
        //    unknown run; the drain runs before it.)
        send(0, "the tail".into());
        core.tx
            .send(Command::ApplyStepResult {
                output: crate::workflow::StepOutput {
                    run_id: "live-run".into(),
                    unit_ix: 0,
                    attempt: 7,
                    output: String::new(),
                    status: crate::workflow::StepStatus::Ok,
                    usage: None,
                    files: Vec::new(),
                    tools: Vec::new(),
                    governed: false,
                },
                agent_verdict: None,
                process_gen: None,
                launch_seq: 0,
                ack: None,
            })
            .expect("actor alive");

        // Everything is queued; dropping the handle queues Shutdown BEHIND it (FIFO), so the
        // whole stream can be collected to completion and asserted in order. Guard the collect
        // with a deadline so a wedged actor fails the test instead of hanging it.
        drop(core);
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        let mut collected: Vec<CoreEvent> = Vec::new();
        loop {
            let remaining = deadline
                .checked_duration_since(std::time::Instant::now())
                .expect("actor did not shut down within the deadline");
            match events.recv_timeout(remaining) {
                Ok(ev) => collected.push(ev),
                Err(_) => break, // channel closed — the actor shut down cleanly
            }
        }

        let unit_outputs: Vec<(&str, u32, u32, &str)> = collected
            .iter()
            .filter_map(|ev| match ev {
                CoreEvent::UnitOutputDelta {
                    session,
                    ord,
                    attempt,
                    text,
                } => Some((session.as_str(), *ord, *attempt, text.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(
            unit_outputs.len(),
            4,
            "four flushes: first-chunk, size-boundary, rework first-chunk, the tail — got {unit_outputs:?}"
        );
        // (1) first chunk, verbatim, immediately, labeled with its in-band attempt.
        assert_eq!(unit_outputs[0], ("live-run", 3, 0, "first chunk"));
        // (2) size-boundary flush, capped by the head+tail elide, still attempt 0.
        let (_, _, attempt, text) = unit_outputs[1];
        assert_eq!(attempt, 0, "the size flush carries the chunk's attempt");
        assert!(
            text.len() <= crate::output_throttle::FLUSH_BYTES,
            "flushed text is capped at 2KB, got {} bytes",
            text.len()
        );
        assert!(
            text.contains("bytes elided"),
            "over-cap text is head+tail elided"
        );
        // (3) the re-dispatched attempt's first chunk: its own buffer, its own label — attempt 0's
        //     pending text did not leak into it.
        assert_eq!(
            unit_outputs[2],
            ("live-run", 3, 1, "rework output"),
            "a bumped attempt streams under its own in-band label, unmerged"
        );
        // (4) the pending tail: labeled with the CHUNK's attempt (0) — NOT the finishing result's
        //     attempt (7) — whether the window or the ApplyStepResult drain flushed it. This is the
        //     mislabeling fix: a superseded attempt's late output keeps its own attempt.
        assert_eq!(
            unit_outputs[3],
            ("live-run", 3, 0, "the tail"),
            "the pending tail keeps its chunk's attempt, never the result's"
        );

        // The raw CliOutputDelta fanout is UNCHANGED alongside the throttled stream.
        let raw: Vec<&str> = collected
            .iter()
            .filter_map(|ev| match ev {
                CoreEvent::CliOutputDelta { chunk, .. } => Some(chunk.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            raw.contains(&"first chunk") && raw.contains(&"the tail"),
            "raw chunks still fan out untouched next to the coalesced stream"
        );
        assert!(
            raw.iter().any(|c| c.len() == 5000),
            "the raw stream is NOT capped — only the throttled stream elides"
        );
    }
}

/// FINDING-023 — the precedence "a workflow-authored gate is never suppressed by run-level
/// `human_confirm: none`" is deliberate, so the pause must DISCLOSE it to the operator who asked
/// for `none` (nothing else does: the launch accepts it and the session then reports
/// `human_confirm: none` while sitting `awaiting_human`). These drive `advance_or_pause` — the real
/// call site that builds the prompt — not the note helper in isolation.
#[cfg(test)]
mod def_gate_disclosure_tests {
    use super::*;
    use crate::domain::{
        put_node, AgentSession, HumanConfirm, SessionStatus, UnitStatus, WorkUnit,
    };
    use crate::scope::EntityMode;
    use crate::workflow::{GateSpec, StepInput, StepOutput, StepRunner, StepStatus};
    use std::sync::mpsc::channel;
    use wicked_apps_core::{open_store, ToNode};

    struct NoopRunner;
    impl StepRunner for NoopRunner {
        fn run_unit(&self, i: &StepInput) -> StepOutput {
            StepOutput {
                run_id: i.run_id.clone(),
                unit_ix: i.unit_ix,
                attempt: i.attempt,
                output: "unused".into(),
                status: StepStatus::Ok,
                usage: None,
                files: Vec::new(),
                tools: Vec::new(),
                governed: false,
            }
        }
    }

    /// A run whose FIRST unit finished under a def-declared HumanConfirm gate, cursor on unit 2 —
    /// the exact state FINDING-023 observed (`feature`'s clarify gate under `human_confirm: none`).
    fn seed(store: &mut dyn GraphStore, hc: HumanConfirm) {
        let session = AgentSession {
            id: "d".into(),
            workflow_id: "wf-d".into(),
            problem: "p".into(),
            entity_mode: EntityMode::Shared,
            collection_scope: None,
            clis: vec![],
            status: SessionStatus::Executing,
            human_confirm: hc,
            unit_ix: 1,
            attempt: 0,
            workdir: None,
            repo_ref: None,
            extra_write_roots: Vec::new(),
            project_graph: None,
            archived_at: None,
            archive_note: None,
        };
        put_node(store, session.to_node()).unwrap();
        let mut u1 = WorkUnit::pending("d:u1", "d", 1, "clarify the problem");
        u1.gate = GateSpec::HumanConfirm {
            unconditional: false,
        };
        u1.status = UnitStatus::Done;
        put_node(store, u1.to_node()).unwrap();
        let u2 = WorkUnit::pending("d:u2", "d", 2, "design the approach");
        put_node(store, u2.to_node()).unwrap();
    }

    /// Drive `advance_or_pause` at `unit_ix` and return the emitted `AwaitingHuman` prompt.
    fn pause_prompt(store: &mut dyn GraphStore, unit_ix: usize) -> String {
        let mut subs = crate::event_log::EventSink::default();
        let (evtx, evrx) = channel::<CoreEvent>();
        subs.push(evtx);
        let (tx, _rx) = channel::<Command>();
        let runner: Arc<dyn StepRunner> = Arc::new(NoopRunner);
        let progress = advance_or_pause(
            store,
            &mut subs,
            &runner,
            &tx,
            "d",
            unit_ix,
            &None,
            &None,
            uuid::Uuid::nil(),
            false,
        )
        .unwrap();
        assert!(
            matches!(progress, Progress::Paused),
            "precondition: the def gate pauses"
        );
        evrx.try_iter()
            .find_map(|ev| match ev {
                CoreEvent::AwaitingHuman { prompt, .. } => Some(prompt),
                _ => None,
            })
            .expect("a pause emits AwaitingHuman with its prompt")
    }

    #[test]
    fn a_def_gate_pause_under_none_discloses_the_precedence_in_its_prompt() {
        let mut store = open_store(Some(":memory:")).unwrap();
        seed(&mut store, HumanConfirm::None);
        let prompt = pause_prompt(&mut store, 1);
        // The pause still names the work under review first — the disclosure is appended, not a
        // replacement of the attribution FINDING-032 fixed.
        assert!(
            prompt.starts_with("Approve the output of unit 1"),
            "attribution must survive the disclosure: {prompt}"
        );
        assert!(
            prompt.contains("workflow-declared gate") && prompt.contains("human_confirm=none"),
            "an operator who launched with none must be told, at the pause itself, that the gate \
             is workflow-authored and none does not suppress it: {prompt}"
        );
    }

    #[test]
    fn the_same_pause_under_an_attended_policy_carries_no_disclosure() {
        let mut store = open_store(Some(":memory:")).unwrap();
        seed(&mut store, HumanConfirm::All);
        let prompt = pause_prompt(&mut store, 1);
        assert!(
            !prompt.contains("workflow-declared gate"),
            "an operator who asked to be paused needs no precedence lecture — the note must be \
             conditional on none, not boilerplate: {prompt}"
        );
    }

    /// The terminal-gate pause (seam finding #4) is a def-authored gate too, reached through a
    /// DIFFERENT branch of `advance_or_pause` — it must disclose under `none` as well, or the one
    /// workflow ending on a human gate (`collab`) stalls its unattended runs unexplained.
    #[test]
    fn a_terminal_def_gate_under_none_discloses_too() {
        let mut store = open_store(Some(":memory:")).unwrap();
        let session = AgentSession {
            id: "d".into(),
            workflow_id: "wf-d".into(),
            problem: "p".into(),
            entity_mode: EntityMode::Shared,
            collection_scope: None,
            clis: vec![],
            status: SessionStatus::Executing,
            human_confirm: HumanConfirm::None,
            unit_ix: 1, // past the single terminal unit
            attempt: 0,
            workdir: None,
            repo_ref: None,
            extra_write_roots: Vec::new(),
            project_graph: None,
            archived_at: None,
            archive_note: None,
        };
        put_node(&mut store, session.to_node()).unwrap();
        let mut u = WorkUnit::pending("d:u1", "d", 1, "the verdict phase");
        u.gate = GateSpec::HumanConfirm {
            unconditional: false,
        };
        u.status = UnitStatus::Done;
        put_node(&mut store, u.to_node()).unwrap();

        let prompt = pause_prompt(&mut store, 1);
        assert!(
            prompt.starts_with("Approve completion after the final phase"),
            "the terminal pause keeps its own framing: {prompt}"
        );
        assert!(
            prompt.contains("workflow-declared gate") && prompt.contains("human_confirm=none"),
            "the terminal def gate must disclose under none exactly like a mid-run one: {prompt}"
        );
    }

    /// The THIRD def-authored pause site (FINDING-023): the `HumanConfirmIf(VerdictNotPass)` verdict
    /// escalation in `apply_step_result`. The tests above drive `advance_or_pause` and never reach
    /// this branch, so deleting the disclosure HERE leaves every one of them green (verified by
    /// deleting it). Driving it functionally needs a full governance not-pass verdict; this campaign's
    /// own lesson is that guards miss CALL SITES, so this audits the wiring at the site, bounded to
    /// `apply_step_result`'s body (2..=`fail_run`) so neither of the other two sites can satisfy it.
    #[test]
    fn the_verdict_escalation_pause_discloses_the_precedence_too() {
        let src = include_str!("actor.rs");
        let body = src
            .split("fn apply_step_result")
            .nth(1)
            .and_then(|b| b.split("\nfn fail_run").next())
            .expect("apply_step_result is still a top-level fn ending before fail_run");
        // Needles built by concatenation so this assertion cannot satisfy itself out of its own text.
        let note_call = concat!("unsuppressed_gate_note", "(session.human_confirm)");
        assert!(
            body.contains(note_call),
            "the verdict-escalation pause no longer derives the disclosure from the run's own \
             human_confirm — an operator who launched unattended is escalated to human review with \
             nothing telling them the run-level policy did not suppress the gate (FINDING-023's \
             third, previously unguarded site)"
        );
        // Substance: computing the note is worthless unless it reaches the operator — it must be
        // interpolated into the escalation prompt this branch builds, not merely bound.
        let interpolated = concat!("{", "note}");
        assert!(
            body.contains(interpolated),
            "the escalation disclosure is computed but never appended to the prompt — dead code \
             reads identically to no disclosure at all"
        );
    }
}

/// FINDING-028 — `register_deny_policy`'s `phase` argument must SCOPE the deny. Before this fix it
/// was decorative: `applies_to` was a `unit-1..=unit-256` fan-out regardless of the caller's phase,
/// so a deny an operator scoped to `review` also fired on `clarify`, `design`, and every other unit
/// of every run. These assert against `select_any` — the SAME selection funnel the live gate uses
/// (`execute.rs`), so what selects here is what fires there.
#[cfg(test)]
mod deny_policy_tests {
    use super::*;
    use wicked_apps_core::open_store;

    fn ctx() -> serde_json::Value {
        serde_json::json!({ "work": "about to rm -rf the prod volume" })
    }

    #[test]
    fn a_deny_scoped_to_a_phase_fires_there_and_nowhere_else() {
        let mut store = open_store(Some(":memory:")).unwrap();
        let registry = crate::workflow::WorkflowRegistry::with_defaults();
        // `build` is a real phase of the built-in `feature` workflow.
        register_deny_policy(&mut store, &registry, "build", "rm -rf").unwrap();

        let at_build = select_any(&store, "s", &["unit-3", "build"], &ctx()).unwrap();
        assert_eq!(at_build.len(), 1, "the deny selects at its own phase");
        assert_eq!(
            at_build[0].applies_to,
            vec!["build".to_string()],
            "applies_to is the caller's phase ALONE — the documented contract, not a superset"
        );

        // The defect itself: the same policy must NOT select at any other phase.
        let elsewhere = select_any(&store, "s", &["unit-1", "clarify"], &ctx()).unwrap();
        assert!(
            elsewhere.is_empty(),
            "a deny scoped to `build` selected at `clarify` — the fan-out is back: {:?}",
            elsewhere.iter().map(|p| &p.id).collect::<Vec<_>>()
        );
    }

    #[test]
    fn an_unknown_phase_is_rejected_and_nothing_lands_on_the_store() {
        let mut store = open_store(Some(":memory:")).unwrap();
        let registry = crate::workflow::WorkflowRegistry::with_defaults();
        // A typo of `review`. Narrowing WITHOUT this rejection would register an inert policy —
        // the operator believes a guard is standing and nothing ever fires (FINDING-021's shape).
        let err = register_deny_policy(&mut store, &registry, "reviw", "DENYME")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("`reviw`") && err.contains("never fire"),
            "the rejection names the bad token and the consequence: {err}"
        );
        assert!(
            err.contains("build") && err.contains("clarify") && err.contains("cutover"),
            "the rejection lists the registered workflow phases so the operator can correct: {err}"
        );
        // Fail-closed on the WRITE: had anything landed, it would select at the very phase it
        // claimed — so an empty selection there proves the store took nothing.
        let landed = select_any(&store, "s", &["reviw"], &ctx()).unwrap();
        assert!(
            landed.is_empty(),
            "a rejected registration must not persist"
        );
    }

    #[test]
    fn synthetic_unit_forms_are_accepted_only_in_canonical_spelling_within_the_span() {
        let mut store = open_store(Some(":memory:")).unwrap();
        let registry = crate::workflow::WorkflowRegistry::with_defaults();
        // Canonical synthetic forms: the engine-derived `unit-<ord>` and the ad-hoc `u<ord>`.
        register_deny_policy(&mut store, &registry, "unit-65", "X").unwrap();
        register_deny_policy(&mut store, &registry, "u7", "X").unwrap();
        assert_eq!(
            select_any(&store, "s", &["unit-65"], &ctx()).unwrap().len(),
            1,
            "a synthetic-phase deny is selectable at that exact token"
        );
        // Every one of these parses-or-looks like a unit form but can never EQUAL a gate token
        // (`select_any` matches by equality), so accepting it would mint an inert policy:
        // non-canonical digits (unit-007, u+7), out-of-span ords (0, 257), and bare prefixes.
        for bad in [
            "unit-0", "u0", "unit-257", "u257", "unit-007", "u+7", "unit-", "u",
        ] {
            assert!(
                register_deny_policy(&mut store, &registry, bad, "X").is_err(),
                "`{bad}` can never match a gate token and must be rejected, not registered inert"
            );
        }
    }
}

/// FINDING-003 — a run reaching a terminal status must reap its worktree (14 orphans survived
/// restarts; the retest reproduced one orphan from one failed run). These drive `finalize_run` and
/// `fail_run` — the REAL terminal transitions — against a real git repo, not the reap helper in
/// isolation: the finding's root cause was precisely a documented cleanup no terminal path reached.
#[cfg(test)]
mod terminal_worktree_reap_tests {
    use super::*;
    use crate::domain::{put_node, AgentSession, HumanConfirm, SessionStatus};
    use crate::repo::RepoSpec;
    use crate::scope::EntityMode;
    use crate::workflow::{StepInput, StepOutput, StepRunner, StepStatus};
    use std::path::Path;
    use std::sync::mpsc::channel;
    use wicked_apps_core::{open_store, HardenedCommand, ToNode};

    struct NoopRunner;
    impl StepRunner for NoopRunner {
        fn run_unit(&self, i: &StepInput) -> StepOutput {
            StepOutput {
                run_id: i.run_id.clone(),
                unit_ix: i.unit_ix,
                attempt: i.attempt,
                output: "unused".into(),
                status: StepStatus::Ok,
                usage: None,
                files: Vec::new(),
                tools: Vec::new(),
                governed: false,
            }
        }
    }

    /// A git repo with one commit at a scratch path (mirrors `repo::tests::git_repo`, which is
    /// private to that module). Per-process and per-thread so concurrent test binaries never collide.
    fn git_repo(name: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "wicked-reap-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .hardened()
                .args(args)
                .current_dir(&root)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?} failed");
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "t@example.invalid"]);
        run(&["config", "user.name", "wicked-test"]);
        run(&["config", "commit.gpgsign", "false"]);
        std::fs::write(root.join("README.md"), "base\n").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-qm", "base"]);
        root
    }

    /// Register the repo, create the run's worktree, seed an Executing session bound to it.
    /// Returns (repo root, worktree path).
    fn seeded(
        store: &mut dyn GraphStore,
        name: &str,
        run_id: &str,
    ) -> (std::path::PathBuf, std::path::PathBuf) {
        seeded_with_status(store, name, run_id, SessionStatus::Executing)
    }

    /// As [`seeded`], but at an arbitrary status — the resume guard needs a PRE-EXECUTION status
    /// (`Planning`) to exercise its crash-during-planning branch.
    fn seeded_with_status(
        store: &mut dyn GraphStore,
        name: &str,
        run_id: &str,
        status: SessionStatus,
    ) -> (std::path::PathBuf, std::path::PathBuf) {
        let root = git_repo(name);
        let entry = crate::repo::register_repo(
            store,
            RepoSpec {
                name: format!("reap-{name}"),
                root_path: root.to_string_lossy().to_string(),
                registered_at: 0,
            },
        )
        .unwrap();
        let wt = crate::repo::create_worktree(&entry.root_path, run_id).unwrap();
        let session = AgentSession {
            id: run_id.into(),
            workflow_id: format!("wf-{run_id}"),
            problem: "p".into(),
            entity_mode: EntityMode::Shared,
            collection_scope: None,
            clis: vec![],
            status,
            human_confirm: HumanConfirm::None,
            unit_ix: 0,
            attempt: 0,
            workdir: Some(wt.to_string_lossy().to_string()),
            repo_ref: Some(entry.id),
            extra_write_roots: Vec::new(),
            project_graph: None,
            archived_at: None,
            archive_note: None,
        };
        put_node(store, session.to_node()).unwrap();
        (root, wt)
    }

    /// The reap runs on its own thread; wait for the checkout to vanish. The generous bound is
    /// deliberate (the FINDING-029/030 lesson: a tight wall-clock deadline accuses the feature of
    /// a scheduling shortfall) — the loop returns the moment the path is gone.
    fn wait_gone(path: &Path) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while path.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "worktree still present after 60s — the terminal reap never ran for {}",
                path.display()
            );
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
    }

    fn branch_exists(root: &Path, branch: &str) -> bool {
        let out = std::process::Command::new("git")
            .hardened()
            .args(["branch", "--list", branch])
            .current_dir(root)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).contains(branch)
    }

    #[test]
    fn a_completed_run_reaps_its_clean_worktree_and_keeps_the_branch() {
        let mut store = open_store(Some(":memory:")).unwrap();
        let (root, wt) = seeded(&mut store, "done", "r-done");
        let mut subs = crate::event_log::EventSink::default();
        let (tx, _rx) = channel::<Command>();
        let runner: Arc<dyn StepRunner> = Arc::new(NoopRunner);

        finalize_run(&mut store, &mut subs, &runner, &tx, "r-done").unwrap();

        let session = crate::domain::get_session(&store, "r-done")
            .unwrap()
            .unwrap();
        assert_eq!(session.status, SessionStatus::Completed);
        wait_gone(&wt);
        assert!(
            branch_exists(&root, "wicked/r-done"),
            "the branch is the run's record and must outlive the checkout"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_failed_run_reaps_its_clean_worktree_too() {
        // The finding's retest: ONE failed run left ONE orphan on a fresh clone. `fail_run` is the
        // funnel every failure path (including `fail_run_by_id`'s async faults) routes through.
        let mut store = open_store(Some(":memory:")).unwrap();
        let (root, wt) = seeded(&mut store, "failed", "r-fail");
        let mut subs = crate::event_log::EventSink::default();
        let (tx, _rx) = channel::<Command>();
        let runner: Arc<dyn StepRunner> = Arc::new(NoopRunner);
        let mut session = crate::domain::get_session(&store, "r-fail")
            .unwrap()
            .unwrap();

        let applied = fail_run(&mut store, &mut subs, &runner, &tx, &mut session, 1);
        assert!(matches!(applied, StepApplied::Finished));

        wait_gone(&wt);
        assert!(
            branch_exists(&root, "wicked/r-fail"),
            "failure keeps the branch as the record of what was attempted"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// FINDING-003, the RESUME half. `resume_run_inner`'s crash-during-planning guard fails a run
    /// stuck in a pre-execution status AND reaps its worktree (src/actor.rs). The finding-time tests
    /// only drove `finalize_run`/`fail_run`, so deleting that one reap call left the run correctly
    /// `Failed` with its checkout leaked until next boot and NOTHING failing. This drives the real
    /// resume entry point and pins the effect: falsified by deleting the reap call — the run is still
    /// `Failed`, but `wait_gone` then times out because the checkout never leaves.
    #[test]
    fn a_resumed_never_planned_run_reaps_its_worktree_and_keeps_the_branch() {
        let mut store = open_store(Some(":memory:")).unwrap();
        let (root, wt) =
            seeded_with_status(&mut store, "resume", "r-resume", SessionStatus::Planning);
        let mut subs = crate::event_log::EventSink::default();
        let (tx, _rx) = channel::<Command>();
        let runner: Arc<dyn StepRunner> = Arc::new(NoopRunner);
        let mut in_flight = HashSet::new();

        let status = resume_run_inner(
            &mut store,
            &mut subs,
            &runner,
            &tx,
            &mut in_flight,
            "r-resume",
            &None,
            &None,
            uuid::Uuid::nil(),
            false,
        )
        .unwrap();
        assert_eq!(
            status,
            SessionStatus::Failed,
            "a run that never completed planning resumes to Failed, never auto-completes"
        );
        wait_gone(&wt);
        assert!(
            branch_exists(&root, "wicked/r-resume"),
            "the branch outlives the reap here too"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The tuple contract the boot call site leans on: `partition_sessions_for_reap` returns
    /// (LIVE, TERMINAL) in that order. The startup-wiring audit below asserts the boot site binds
    /// `let (live, terminal) = ...`; this pins that `live` is in fact the KEEP set and `terminal`
    /// the REAP set, so the two together close the swap. Falsified by swapping the two `.insert`
    /// targets in `partition_sessions_for_reap`.
    #[test]
    fn partition_returns_live_first_then_terminal() {
        let live_session = AgentSession {
            id: "s-live".into(),
            workflow_id: "wf".into(),
            problem: "p".into(),
            entity_mode: EntityMode::Shared,
            collection_scope: None,
            clis: vec![],
            status: SessionStatus::Executing,
            human_confirm: HumanConfirm::None,
            unit_ix: 0,
            attempt: 0,
            workdir: None,
            repo_ref: None,
            extra_write_roots: Vec::new(),
            project_graph: None,
            archived_at: None,
            archive_note: None,
        };
        let term_session = AgentSession {
            id: "s-term".into(),
            status: SessionStatus::Failed,
            ..live_session.clone()
        };
        let (live, terminal) = partition_sessions_for_reap(&[live_session.clone(), term_session]);
        assert!(
            live.contains("s-live") && !live.contains("s-term"),
            "the FIRST returned set is LIVE (kept, may resume) — the boot site binds it to `live`"
        );
        assert!(
            terminal.contains("s-term") && !terminal.contains("s-live"),
            "the SECOND returned set is TERMINAL (reaped clean-only)"
        );
    }

    /// FINDING-003 boot call site (`run()`). `reap_orphan_worktrees(repos, live, terminal)` KEEPS
    /// `live` and REAPS `terminal`; both are `&HashSet<String>`, so swapping them at the boot site
    /// COMPILES and silently inverts the meaning — resumable runs' checkouts reaped, terminal
    /// leftovers kept — while the repo-level test (which calls the fn with named sets) stays green.
    /// Booting the whole actor to exercise this is disproportionate for one two-line wiring, so this
    /// audits the wiring at the site (the same instrument the FINDING-091 call-site guard uses).
    /// Falsified by swapping to `reap_orphan_worktrees(&repos, &terminal, &live)` (or the destructure
    /// to `let (terminal, live) = ...`): the concatenated needle no longer appears and this fails.
    #[test]
    fn the_startup_reaper_wires_live_to_keep_and_terminal_to_reap() {
        let src = include_str!("actor.rs");
        // Needles built by concatenation so this assertion cannot satisfy itself out of its own text.
        let destructure = concat!(
            "let (live, terminal) = ",
            "partition_sessions_for_reap(&sessions)"
        );
        assert!(
            src.contains(destructure),
            "the startup reaper no longer binds partition's (live, terminal) tuple in order — a \
             swapped destructure would feed the live set to the reap position (FINDING-003)"
        );
        let call = concat!("reap_orphan_worktrees(&repos, ", "&live, &terminal)");
        assert!(
            src.contains(call),
            "the startup reap call's argument order changed — live must be the KEEP arg and terminal \
             the REAP arg; a swap compiles and reaps resumable checkouts (FINDING-003)"
        );
    }
}

/// What store a governed worker's estate MCP is pointed at, and when it gets none (FINDING-069).
#[cfg(test)]
mod worker_code_graph_tests {
    use super::*;
    use crate::domain::HumanConfirm;
    use crate::repo::{RepoEntry, REPO_ENTRY};
    use wicked_apps_core::{open_store, ToNode};

    /// A registered repo rooted at `root`, without touching git — `register_repo` validates a real
    /// checkout, and none of what is under test here depends on one.
    fn register(store: &mut dyn GraphStore, id: &str, root: &std::path::Path) {
        let entry = RepoEntry {
            id: id.into(),
            name: id.into(),
            root_path: root.to_string_lossy().into_owned(),
            default_branch: "main".into(),
            registered_at: 0,
            code_graph_db: String::new(), // derived on read; the value written here is irrelevant
        };
        crate::domain::put_node(store, entry.to_node()).unwrap();
        assert_eq!(entry.to_node().kind, NodeKind::Other(REPO_ENTRY.into()));
    }

    /// core#124: a run the store calls `executing` with no worker behind it must SAY so.
    ///
    /// The redrive runs ONLY in armed exec mode, so on the default path a restart leaves the status
    /// claiming execution forever. Observed live as 35+ minutes of silence that a manual resume
    /// fixed instantly — the recovery worked; nothing announced it was needed.
    fn session_at(id: &str, status: SessionStatus) -> AgentSession {
        crate::domain::AgentSession {
            id: id.into(),
            workflow_id: "wf".into(),
            problem: "p".into(),
            entity_mode: EntityMode::Shared,
            collection_scope: None,
            clis: vec![],
            status,
            human_confirm: HumanConfirm::None,
            unit_ix: 0,
            attempt: 0,
            workdir: None,
            repo_ref: None,
            extra_write_roots: Vec::new(),
            project_graph: None,
            archived_at: None,
            archive_note: None,
        }
    }

    /// Drain the events a call emits into a Vec, via the sink's mpsc `push`.
    fn emitted(store: &dyn wicked_apps_core::GraphStore) -> Vec<CoreEvent> {
        emitted_with_in_flight(store, &HashSet::new())
    }

    /// The same drain, but naming what armed mode restored — the set the reporter must stay quiet
    /// about.
    fn emitted_with_in_flight(
        store: &dyn wicked_apps_core::GraphStore,
        in_flight: &HashSet<String>,
    ) -> Vec<CoreEvent> {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut sink = crate::event_log::EventSink::default();
        sink.push(tx);
        report_orphaned_executing_sessions(store, &mut sink, in_flight);
        drop(sink);
        rx.try_iter().collect()
    }

    #[test]
    fn an_executing_run_with_no_worker_is_announced() {
        let dir = scratch("orphan");
        let mut store = open_store(Some(dir.join("o.db").to_str().unwrap())).unwrap();
        crate::domain::put_node(
            &mut store,
            session_at("orphan-run", SessionStatus::Executing).to_node(),
        )
        .unwrap();

        let events = emitted(&store);
        assert!(
            events.iter().any(|e| matches!(
                e,
                CoreEvent::RunOrphaned { session, .. } if session == "orphan-run"
            )),
            "an orphaned `executing` run emitted nothing: {events:?}"
        );
    }

    /// The review defect: `redrive_executing_sessions` re-dispatches a session and records it in
    /// `in_flight`, but leaves its status `Executing` — only the attempt is bumped. So a scan over
    /// "everything still `executing`" announces every run armed mode JUST RECOVERED, and the
    /// reporter is loudest precisely when recovery worked.
    #[test]
    fn a_run_armed_mode_already_recovered_is_not_announced() {
        let dir = scratch("orphan_redriven");
        let mut store = open_store(Some(dir.join("o.db").to_str().unwrap())).unwrap();
        crate::domain::put_node(
            &mut store,
            session_at("redriven-run", SessionStatus::Executing).to_node(),
        )
        .unwrap();

        // Same store, same `Executing` status — the ONLY difference is that recovery holds it.
        let restored: HashSet<String> = ["redriven-run".to_string()].into_iter().collect();
        assert!(
            emitted_with_in_flight(&store, &restored).is_empty(),
            "a run armed mode had already redriven was reported as an orphan"
        );
        // And the same run with nothing restored still IS an orphan, so the test above cannot pass
        // by the reporter having simply gone silent.
        assert!(
            !emitted(&store).is_empty(),
            "the reporter emitted nothing even with an empty in-flight set — the exclusion test \
             above proves nothing"
        );
    }

    /// `ord` is 1-based, so 0 is an ordinal no unit can hold. A run whose units cannot be read must
    /// report its cursor position rather than a value that reads as a real unit.
    #[test]
    fn a_run_with_unreadable_units_reports_a_1_based_ordinal() {
        let dir = scratch("orphan_ord");
        let mut store = open_store(Some(dir.join("o.db").to_str().unwrap())).unwrap();
        // No units are ever written, so the lookup finds nothing — the fallback path.
        crate::domain::put_node(
            &mut store,
            session_at("ordless-run", SessionStatus::Executing).to_node(),
        )
        .unwrap();

        let events = emitted(&store);
        let ord = events
            .iter()
            .find_map(|e| match e {
                CoreEvent::RunOrphaned { session, ord, .. } if session == "ordless-run" => {
                    Some(*ord)
                }
                _ => None,
            })
            .expect("no RunOrphaned for the unit-less run");
        assert_eq!(
            ord, 1,
            "unit_ix 0 must report as the 1-based ordinal 1, not 0"
        );
    }

    /// A COMPLETED run must not be announced. A reporter that fires on everything is noise, and
    /// noise about healthy runs teaches operators to ignore the signal that matters.
    #[test]
    fn a_completed_run_is_not_announced() {
        let dir = scratch("orphan_done");
        let mut store = open_store(Some(dir.join("o.db").to_str().unwrap())).unwrap();
        crate::domain::put_node(
            &mut store,
            session_at("done-run", SessionStatus::Completed).to_node(),
        )
        .unwrap();
        assert!(
            emitted(&store).is_empty(),
            "a completed run was reported as orphaned"
        );
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("wicked-cgtest-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The regression. A repo that has never been indexed must yield NO store, so the worker is
    /// launched with no estate MCP at all.
    ///
    /// Before the fix this returned `Some(path)`: the resolver ran `create_dir_all` on the graph's
    /// parent and handed back the path regardless, so every graph tool the worker had answered
    /// "nothing found" about a repo full of code — an empty result that reads exactly like a real one.
    #[test]
    fn an_unindexed_repo_ships_no_estate_mcp() {
        let mut store = open_store(Some(":memory:")).unwrap();
        let root = scratch("unindexed");
        register(&mut store, "unindexed", &root);

        assert_eq!(repo_code_graph_db(&store, Some("unindexed")), None);
        // And it did not bring the directory into existence on the way to saying no. A resolver that
        // creates as it reads is how the empty database appeared in the first place.
        assert!(!root.join(".codegraph").exists());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_indexed_repo_gets_the_graph_the_indexer_wrote() {
        let mut store = open_store(Some(":memory:")).unwrap();
        let root = scratch("indexed");
        let graph = root.join(".codegraph").join("estate.db");
        std::fs::create_dir_all(graph.parent().unwrap()).unwrap();
        std::fs::write(&graph, b"not really sqlite, but it is a file").unwrap();
        register(&mut store, "indexed", &root);

        assert_eq!(
            repo_code_graph_db(&store, Some("indexed")).as_deref(),
            Some(graph.to_string_lossy().as_ref()),
            "the worker must get the path crew's onboarding indexed to, not a sibling"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A directory at the graph's path is not a graph. `is_file` rather than `exists` because
    /// `.codegraph/estate.db/` is precisely what a half-finished index or a bad `--db` argument
    /// leaves behind, and handing that to a store opener fails deep inside sqlite rather than here.
    #[test]
    fn a_directory_where_the_graph_should_be_is_not_a_graph() {
        let mut store = open_store(Some(":memory:")).unwrap();
        let root = scratch("dir-not-file");
        std::fs::create_dir_all(root.join(".codegraph").join("estate.db")).unwrap();
        register(&mut store, "dir-not-file", &root);

        assert_eq!(repo_code_graph_db(&store, Some("dir-not-file")), None);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_run_with_no_repo_and_a_run_on_an_unregistered_one_both_get_nothing() {
        let store = open_store(Some(":memory:")).unwrap();
        assert_eq!(repo_code_graph_db(&store, None), None);
        assert_eq!(repo_code_graph_db(&store, Some("never-registered")), None);
    }
}

/// WHICH graph a governed worker's estate MCP opens when the run is filed into a project — and,
/// mostly, when it refuses to open the one it was handed.
///
/// Every test here is a degradation case, because the interesting half of this seam is the refusals:
/// binding the right graph buys a wider view, but binding a wrong one costs either the platform's
/// state (FINDING-067) or the worker's belief that its own code exists (FINDING-069).
#[cfg(test)]
mod project_graph_binding_tests {
    use super::*;
    use crate::project::ProjectGraphBinding;
    use crate::repo::{RepoEntry, REPO_ENTRY};
    use wicked_apps_core::{open_store, GraphWrite, ToNode};

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("wicked-pgtest-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn register(store: &mut dyn GraphStore, id: &str, root: &std::path::Path) {
        let entry = RepoEntry {
            id: id.into(),
            name: id.into(),
            root_path: root.to_string_lossy().into_owned(),
            default_branch: "main".into(),
            registered_at: 0,
            code_graph_db: String::new(),
        };
        crate::domain::put_node(store, entry.to_node()).unwrap();
        assert_eq!(entry.to_node().kind, NodeKind::Other(REPO_ENTRY.into()));
    }

    /// A real estate database holding `labels`, each namespaced the way `index --repo <label>` does.
    /// `set_file_digest` is the indexer-only call that creates a `files` row, which is exactly what
    /// `indexed_files()` reads — so this builds the evidence the verification actually consults
    /// rather than a fixture shaped like it.
    fn graph_with(path: &std::path::Path, labels: &[&str]) {
        let mut g = open_store(Some(path.to_str().unwrap())).unwrap();
        for label in labels {
            g.set_file_digest(&format!("{label}/src/lib.rs"), "d1")
                .unwrap();
            g.set_file_digest(&format!("{label}/src/main.rs"), "d2")
                .unwrap();
        }
    }

    /// An estate database that opens fine and knows nothing — a crashed or interrupted index.
    fn empty_graph(path: &std::path::Path) {
        let _ = open_store(Some(path.to_str().unwrap())).unwrap();
    }

    /// A minimal executing session; each test overrides only the two fields this seam reads.
    fn session_fixture() -> AgentSession {
        AgentSession {
            id: "run-fixture".into(),
            workflow_id: "wf-run-fixture".into(),
            problem: "p".into(),
            entity_mode: EntityMode::Shared,
            collection_scope: None,
            clis: Vec::new(),
            status: SessionStatus::Executing,
            human_confirm: crate::domain::HumanConfirm::None,
            unit_ix: 0,
            attempt: 0,
            workdir: None,
            repo_ref: None,
            extra_write_roots: Vec::new(),
            project_graph: None,
            archived_at: None,
            archive_note: None,
        }
    }

    fn bind(db: &std::path::Path, label: Option<&str>) -> ProjectGraphBinding {
        ProjectGraphBinding {
            db_path: db.to_string_lossy().into_owned(),
            repo_label: label.map(str::to_string),
        }
    }

    /// An operational store path that is a real file, so the FINDING-067 comparison has both sides.
    fn operational(dir: &std::path::Path) -> String {
        let p = dir.join("core.db");
        let _ = open_store(Some(p.to_str().unwrap())).unwrap();
        p.to_string_lossy().into_owned()
    }

    /// THE POINT OF THE CHANGE: a run in a project whose graph holds its repo gets the PROJECT's
    /// database, and every co-located sibling comes with it.
    #[test]
    fn a_run_whose_project_graph_holds_its_repo_is_bound_to_the_project_graph() {
        let dir = scratch("bound");
        let db = dir.join("code-graph.db");
        graph_with(&db, &["wicked-core", "wicked-crew"]);
        let op = operational(&dir);

        let got = project_code_graph_db(
            Some(&bind(&db, Some("wicked-core"))),
            Some("wicked-core"),
            Some(&op),
            "r1",
        );

        assert_eq!(
            got.as_deref(),
            Some(
                std::fs::canonicalize(&db)
                    .unwrap()
                    .to_string_lossy()
                    .as_ref()
            ),
            "the worker must get the PROJECT's graph, not the run repo's own"
        );
        // And it is genuinely multi-repo: the sibling the run does not target is in there too, which
        // is the whole reason to prefer it.
        let g = wicked_apps_core::open_store_ro(got.as_deref()).unwrap();
        let files = g.indexed_files().unwrap();
        assert!(
            files.iter().any(|f| f.starts_with("wicked-crew/")),
            "the bound graph should carry the sibling repo's files: {files:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// STALE — the run's repo was attached to the project after the last refresh.
    ///
    /// The graph is perfectly healthy and holds a different repo. Binding it would hand a worker
    /// editing `wicked-core` a set of tools that answer "no such symbol" for everything in front of
    /// it, which is FINDING-069's failure arriving through a door FINDING-069 did not close.
    #[test]
    fn a_project_graph_missing_this_runs_own_repo_is_refused() {
        let dir = scratch("stale");
        let db = dir.join("code-graph.db");
        graph_with(&db, &["wicked-crew"]); // wicked-core attached since the last refresh
        let op = operational(&dir);

        assert_eq!(
            project_code_graph_db(
                Some(&bind(&db, Some("wicked-core"))),
                Some("wicked-core"),
                Some(&op),
                "r2",
            ),
            None,
            "a graph that does not describe the worker's own repo must not be bound"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// PARTIAL — the graph holds this run's repo but is missing another member.
    ///
    /// Bound anyway, and deliberately so: it is a strict superset of the per-repo graph on the axis
    /// that matters. Refusing here would return a NARROWER store because a wider one was not wide
    /// enough, which is the opposite of the trade this seam makes everywhere else.
    #[test]
    fn a_project_graph_missing_some_other_member_is_still_bound() {
        let dir = scratch("partial");
        let db = dir.join("code-graph.db");
        graph_with(&db, &["wicked-core"]); // wicked-crew is a member but not yet indexed
        let op = operational(&dir);

        assert!(
            project_code_graph_db(
                Some(&bind(&db, Some("wicked-core"))),
                Some("wicked-core"),
                Some(&op),
                "r3",
            )
            .is_some(),
            "partial membership is a narrower answer, not a wrong one"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ABSENT — the project graph was never built. A launch does not index (that would make
    /// launching a run a silent N-repo indexing job), so this is the common first-run state.
    #[test]
    fn a_project_graph_that_was_never_built_is_refused() {
        let dir = scratch("absent");
        let op = operational(&dir);
        assert_eq!(
            project_code_graph_db(
                Some(&bind(&dir.join("code-graph.db"), Some("wicked-core"))),
                Some("wicked-core"),
                Some(&op),
                "r4",
            ),
            None
        );
        // Saying no did not create it. A resolver that creates as it reads is how the empty
        // database appeared the first time.
        assert!(!dir.join("code-graph.db").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// EMPTY — the file exists, opens, and holds nothing. Worse than no tools, because it looks
    /// like an answer.
    #[test]
    fn a_graph_that_holds_no_indexed_files_is_refused() {
        let dir = scratch("empty");
        let db = dir.join("code-graph.db");
        empty_graph(&db);
        let op = operational(&dir);

        assert_eq!(
            project_code_graph_db(
                Some(&bind(&db, Some("wicked-core"))),
                Some("wicked-core"),
                Some(&op),
                "r5",
            ),
            None,
            "an empty graph must never be bound (FINDING-069)"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// FINDING-067. The one refusal that is about damage rather than accuracy: a worker with a
    /// writable handle to the engine's own store can delete the platform's state, including the run
    /// that is holding the handle.
    #[test]
    fn the_engines_operational_store_is_refused_as_a_project_graph() {
        let dir = scratch("finding067");
        let op = operational(&dir);

        assert_eq!(
            project_code_graph_db(
                Some(&bind(std::path::Path::new(&op), Some("wicked-core"))),
                Some("wicked-core"),
                Some(&op),
                "r6",
            ),
            None,
            "the operational store must never be handed to a worker"
        );

        // Its sidecars are refused on the same grounds — they are local files holding the platform's
        // memory and knowledge whether the graph itself is sqlite or postgres.
        let mem = format!("{op}.mem");
        let _ = open_store(Some(&mem)).unwrap();
        assert_eq!(
            project_code_graph_db(
                Some(&bind(std::path::Path::new(&mem), Some("wicked-core"))),
                Some("wicked-core"),
                Some(&op),
                "r7",
            ),
            None,
            "a sidecar of the operational store is still the operational store"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A symlink is not a way around the FINDING-067 guard: both sides are canonicalized, so the
    /// comparison is between the files, not between the names used to reach them.
    #[test]
    fn a_symlink_to_the_operational_store_is_refused_too() {
        let dir = scratch("symlink067");
        let op = operational(&dir);
        let link = dir.join("innocent-looking.db");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&op, &link).unwrap();
        #[cfg(not(unix))]
        std::fs::copy(&op, &link).unwrap();

        #[cfg(unix)]
        assert_eq!(
            project_code_graph_db(
                Some(&bind(&link, Some("wicked-core"))),
                Some("wicked-core"),
                Some(&op),
                "r8",
            ),
            None,
            "canonicalization must see through the symlink to the store underneath"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A relative path would resolve against the WORKER's cwd — its own worktree — so it names a
    /// different store than the launcher meant, or brings an empty one into being. Same rule
    /// `in_process_governance` applies to the governance db (finding #6).
    #[test]
    fn a_relative_path_is_refused() {
        let dir = scratch("relative");
        let op = operational(&dir);
        assert_eq!(
            project_code_graph_db(
                Some(&ProjectGraphBinding {
                    db_path: ".wicked-crew/code-graph.db".into(),
                    repo_label: Some("wicked-core".into()),
                }),
                Some("wicked-core"),
                Some(&op),
                "r9",
            ),
            None
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A run that targets a repo but arrives with no label cannot be checked, and an unverifiable
    /// binding is exactly what the verification exists to stop. Fails closed.
    #[test]
    fn a_repo_run_with_no_label_is_refused_even_when_the_graph_is_healthy() {
        let dir = scratch("nolabel");
        let db = dir.join("code-graph.db");
        graph_with(&db, &["wicked-core"]);
        let op = operational(&dir);

        assert_eq!(
            project_code_graph_db(
                Some(&bind(&db, None)),
                Some("wicked-core"),
                Some(&op),
                "r10"
            ),
            None
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A repo-LESS run has no own-repo to be wrong about, so a non-empty graph is all there is to
    /// check and it gets bound. (Today such a run gets nothing at all.)
    #[test]
    fn a_repoless_run_needs_only_a_non_empty_graph() {
        let dir = scratch("repoless");
        let db = dir.join("code-graph.db");
        graph_with(&db, &["wicked-core"]);
        let op = operational(&dir);

        assert!(project_code_graph_db(Some(&bind(&db, None)), None, Some(&op), "r11").is_some());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// With no operational store path there is no way to prove the binding is NOT that store, so the
    /// FINDING-067 guard cannot run — and a guard that cannot run must not be treated as passed.
    #[test]
    fn an_unknown_operational_store_fails_closed() {
        let dir = scratch("noop");
        let db = dir.join("code-graph.db");
        graph_with(&db, &["wicked-core"]);

        assert_eq!(
            project_code_graph_db(
                Some(&bind(&db, Some("wicked-core"))),
                Some("wicked-core"),
                None,
                "r12"
            ),
            None
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// No binding at all is the pre-change world, byte for byte: `repo_code_graph_db` decides, and
    /// every one of its `None` arms still means NO estate tools.
    #[test]
    fn an_unbound_run_falls_through_to_the_per_repo_graph() {
        let dir = scratch("fallthrough");
        let mut store = open_store(Some(":memory:")).unwrap();
        let root = dir.join("repo");
        let repo_graph = root.join(".codegraph").join("estate.db");
        std::fs::create_dir_all(repo_graph.parent().unwrap()).unwrap();
        std::fs::write(&repo_graph, b"a file is all this arm checks for").unwrap();
        register(&mut store, "wicked-core", &root);
        let op = operational(&dir);

        let session = AgentSession {
            repo_ref: Some("wicked-core".into()),
            project_graph: None,
            ..session_fixture()
        };
        assert_eq!(
            run_code_graph_db(&store, &session, Some(&op)).as_deref(),
            Some(repo_graph.to_string_lossy().as_ref()),
            "an unbound run must behave exactly as it did before this change"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// THE FALLBACK, end to end: a run bound to a project graph that does not hold its repo still
    /// gets its OWN repo's graph. Narrower and true, rather than wider and wrong — and never
    /// nothing when a truthful narrower store exists.
    #[test]
    fn a_refused_binding_falls_back_to_the_runs_own_repo_graph() {
        let dir = scratch("fallback");
        let mut store = open_store(Some(":memory:")).unwrap();
        let root = dir.join("repo");
        let repo_graph = root.join(".codegraph").join("estate.db");
        std::fs::create_dir_all(repo_graph.parent().unwrap()).unwrap();
        std::fs::write(&repo_graph, b"the run repo's own graph").unwrap();
        register(&mut store, "wicked-core", &root);

        let project_db = dir.join("code-graph.db");
        graph_with(&project_db, &["wicked-crew"]); // holds a sibling, not this run's repo
        let op = operational(&dir);

        let session = AgentSession {
            repo_ref: Some("wicked-core".into()),
            project_graph: Some(bind(&project_db, Some("wicked-core"))),
            ..session_fixture()
        };
        assert_eq!(
            run_code_graph_db(&store, &session, Some(&op)).as_deref(),
            Some(repo_graph.to_string_lossy().as_ref()),
            "a refused project binding must degrade to the per-repo graph, not to nothing"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// …and when the run's own repo has never been indexed either, the answer is still NO ESTATE
    /// TOOLS. The project binding adds a preference in front of that decision; it does not create a
    /// new way to end up holding a store nobody vouched for.
    #[test]
    fn a_refused_binding_on_an_unindexed_repo_still_ships_no_estate_mcp() {
        let dir = scratch("fallback-none");
        let mut store = open_store(Some(":memory:")).unwrap();
        let root = dir.join("repo");
        std::fs::create_dir_all(&root).unwrap();
        register(&mut store, "wicked-core", &root);

        let project_db = dir.join("code-graph.db");
        graph_with(&project_db, &["wicked-crew"]);
        let op = operational(&dir);

        let session = AgentSession {
            repo_ref: Some("wicked-core".into()),
            project_graph: Some(bind(&project_db, Some("wicked-core"))),
            ..session_fixture()
        };
        assert_eq!(run_code_graph_db(&store, &session, Some(&op)), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// PROOF HARNESS — the same seam, driven against databases a REAL `wicked-estate` built.
    ///
    /// Ignored by default because it needs artifacts `proof/prove.sh` creates and passes in by env;
    /// the tests above cover the same arms hermetically. What this adds is that the graphs are the
    /// real thing — indexed by the installed binary, with real symbols under real `--repo` labels —
    /// so the label-prefix check is exercised against wicked-estate's actual path spelling rather
    /// than against a fixture that agrees with my reading of it.
    ///
    /// Prints the exact `(command, args)` `repo_estate_mcp_parts` hands the worker for each case.
    #[test]
    #[ignore = "proof harness — run via proof/prove.sh, which builds the real graphs it needs"]
    fn proof_worker_mcp_db_per_case() {
        let env = |k: &str| std::env::var(k).unwrap_or_default();
        let project_db = env("WICKED_PROOF_PROJECT_DB");
        let empty_db = env("WICKED_PROOF_EMPTY_DB");
        let operational = env("WICKED_PROOF_OPERATIONAL_DB");
        let repo_root = env("WICKED_PROOF_REPO_ROOT");
        let own = env("WICKED_PROOF_OWN_LABEL");
        let absent = env("WICKED_PROOF_ABSENT_LABEL");
        assert!(
            !project_db.is_empty() && !operational.is_empty() && !repo_root.is_empty(),
            "proof harness needs WICKED_PROOF_* env — run it through proof/prove.sh"
        );

        let mut store = open_store(Some(":memory:")).unwrap();
        register(&mut store, "own-repo", std::path::Path::new(&repo_root));

        let session = |pg: Option<ProjectGraphBinding>| AgentSession {
            repo_ref: Some("own-repo".into()),
            project_graph: pg,
            ..session_fixture()
        };
        let cases: Vec<(&str, AgentSession)> = vec![
            (
                "BOUND: project graph holds this run's repo",
                session(Some(bind(std::path::Path::new(&project_db), Some(&own)))),
            ),
            (
                "STALE: project graph does not hold this run's repo",
                session(Some(bind(std::path::Path::new(&project_db), Some(&absent)))),
            ),
            (
                "ABSENT: project graph never built",
                session(Some(bind(
                    std::path::Path::new("/nonexistent/code-graph.db"),
                    Some(&own),
                ))),
            ),
            (
                "EMPTY: project graph exists and holds nothing",
                session(Some(bind(std::path::Path::new(&empty_db), Some(&own)))),
            ),
            (
                "FINDING-067: binding names the engine's operational store",
                session(Some(bind(std::path::Path::new(&operational), Some(&own)))),
            ),
            ("UNBOUND: no project graph on the session", session(None)),
        ];

        println!("\n=== worker estate MCP, per case ===");
        for (name, s) in cases {
            let db = run_code_graph_db(&store, &s, Some(&operational));
            match crate::execute_wrapped::repo_estate_mcp_parts(db.as_deref()) {
                Some((_exe, args)) => {
                    println!("{name}\n    --db {}\n", args[1]);
                }
                None => println!("{name}\n    NO estate MCP (no vouched-for graph)\n"),
            }
        }
    }

    /// The bound path is what `repo_estate_mcp_parts` turns into the worker's `--db` argument. This
    /// is the seam the whole change exists to move, so it is asserted rather than assumed.
    #[test]
    fn the_bound_path_becomes_the_workers_estate_mcp_db_argument() {
        let dir = scratch("mcp-args");
        let db = dir.join("code-graph.db");
        graph_with(&db, &["wicked-core"]);
        let op = operational(&dir);

        let bound = project_code_graph_db(
            Some(&bind(&db, Some("wicked-core"))),
            Some("wicked-core"),
            Some(&op),
            "r13",
        );
        let (_exe, args) = crate::execute_wrapped::repo_estate_mcp_parts(bound.as_deref())
            .expect("a vouched-for graph must produce an estate MCP server");
        assert_eq!(args[0], "--db");
        assert_eq!(
            args[1],
            std::fs::canonicalize(&db).unwrap().to_string_lossy()
        );

        // And a refused binding produces no server at all — not a server over some other store.
        assert!(crate::execute_wrapped::repo_estate_mcp_parts(None).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Layer-3 governance deny-dominates at the phase boundary (crew#32 / DES-EXEC-001 §3).
#[cfg(test)]
mod phase_boundary_governance_tests {
    use super::*;
    use crate::domain::{put_node, AgentSession, HumanConfirm, SessionStatus, WorkUnit};
    use crate::scope::EntityMode;
    use crate::workflow::{GateSpec, StepInput, StepOutput, StepRunner, StepStatus};
    use std::sync::mpsc::channel;
    use wicked_apps_core::{open_store, ToNode};
    use wicked_governance::{register_policy, Effect, Policy, Severity, Trigger};

    struct NoopRunner;
    impl StepRunner for NoopRunner {
        fn run_unit(&self, i: &StepInput) -> StepOutput {
            StepOutput {
                run_id: i.run_id.clone(),
                unit_ix: i.unit_ix,
                attempt: i.attempt,
                output: "unused".into(),
                status: StepStatus::Ok,
                usage: None,
                files: Vec::new(),
                tools: Vec::new(),
                governed: false,
            }
        }
    }

    fn awaiting_session(store: &mut dyn GraphStore) {
        let session = AgentSession {
            id: "r".into(),
            workflow_id: "wf-r".into(),
            problem: "p".into(),
            entity_mode: EntityMode::Shared,
            collection_scope: None,
            clis: vec!["claude".into()],
            status: SessionStatus::AwaitingHuman,
            human_confirm: HumanConfirm::All,
            unit_ix: 0, // cursor at unit 0
            attempt: 0,
            workdir: None,
            repo_ref: None,
            extra_write_roots: Vec::new(),
            project_graph: None,
            archived_at: None,
            archive_note: None,
        };
        put_node(store, session.to_node()).unwrap();
        // One unit at ord=1 (phase "unit-1").
        let mut u = WorkUnit::pending("r:u1", "r", 1, "a phase requiring governance approval");
        u.gate = GateSpec::HumanConfirm {
            unconditional: true,
        };
        put_node(store, u.to_node()).unwrap();
    }

    /// Without any policy loaded, `decide()` always returns Allow — confirm_gate approves
    /// and transitions the run back to Executing.
    #[test]
    fn approve_without_policies_is_allow() {
        let mut store = open_store(Some(":memory:")).unwrap();
        awaiting_session(&mut store);
        let mut subs = crate::event_log::EventSink::default();
        let (tx, _rx) = channel::<Command>();
        let runner: Arc<dyn StepRunner> = Arc::new(NoopRunner);
        let mut in_flight = HashSet::new();

        let status = confirm_gate(
            &mut store,
            &mut subs,
            &runner,
            &tx,
            &mut in_flight,
            "r",
            crate::workflow::HumanDecision::Approve { amend: None },
            &None,
            &None,
            uuid::Uuid::nil(),
            false,
        )
        .unwrap();
        // No deny policy → Executing (the noop NullRunner returns immediately so the run
        // advances to whatever the engine does next with no units left — Cancelled or Completed).
        assert!(
            matches!(
                status,
                SessionStatus::Executing | SessionStatus::Cancelled | SessionStatus::Completed
            ),
            "without policies, approve must not be blocked: {status:?}"
        );
    }

    /// With a triggered deny policy registered, `confirm_gate(Approve)` must deny-dominate:
    /// the run is cancelled and governance evidence is persisted.
    #[test]
    fn approve_with_triggered_deny_policy_cancels_run() {
        let mut store = open_store(Some(":memory:")).unwrap();
        awaiting_session(&mut store);

        // Register a policy that fires on phase "unit-1" whenever the context contains
        // "phase-boundary" (which confirm_gate always injects as `"gate": "phase-boundary"`).
        let deny_pol = Policy {
            id: "pol-deny-test".to_string(),
            kind: "test".to_string(),
            applies_to: vec!["unit-1".to_string()],
            effect: Effect::Deny,
            trigger: Trigger {
                contains: Some("phase-boundary".to_string()),
            },
            obligations: vec![],
            criteria: "test deny criterion".to_string(),
            severity: Severity::High,
            rule: "Test: deny this phase gate unconditionally.".to_string(),
            retired: false,
        };
        register_policy(&mut store, &deny_pol).unwrap();

        let mut subs = crate::event_log::EventSink::default();
        let (tx, _rx) = channel::<Command>();
        let runner: Arc<dyn StepRunner> = Arc::new(NoopRunner);
        let mut in_flight = HashSet::new();

        let status = confirm_gate(
            &mut store,
            &mut subs,
            &runner,
            &tx,
            &mut in_flight,
            "r",
            crate::workflow::HumanDecision::Approve { amend: None },
            &None,
            &None,
            uuid::Uuid::nil(),
            false,
        )
        .unwrap();

        // A triggered deny policy must cancel the run (deny-dominates even under Approve).
        assert_eq!(
            status,
            SessionStatus::Cancelled,
            "a triggered governance deny must cancel the run even when the human approves"
        );
        // The session in the store must also be Cancelled.
        let session = crate::domain::get_session(&store, "r").unwrap().unwrap();
        assert_eq!(session.status, SessionStatus::Cancelled);
    }
}

#[cfg(test)]
mod environment_refusal_tests {
    use super::environment_refusal;

    #[test]
    fn codex_refusal_classifies_with_an_applicable_fix() {
        let r = environment_refusal(
            "Reading additional input from stdin...\nNot inside a trusted directory and --skip-git-repo-check was not specified."
        )
        .expect("classified");
        assert_eq!(r.reason, "codex refused untrusted directory");
        let fix = r.fix.expect("codex has a mechanical grant");
        assert_eq!(
            fix.apply("codex exec \"{PROMPT}\"").as_deref(),
            Some("codex exec --skip-git-repo-check \"{PROMPT}\"")
        );
        // Already-fixed invocation → nothing to heal (bubbles up instead of looping).
        assert!(fix
            .apply("codex exec --skip-git-repo-check \"{PROMPT}\"")
            .is_none());
        // Foreign invocation without the anchor → no blind edits.
        assert!(fix.apply("claude -p \"{PROMPT}\"").is_none());
    }

    #[test]
    fn no_fix_signatures_classify_for_operator_escalation() {
        let tty = environment_refusal("bubbletea: error opening TTY: could not open TTY").unwrap();
        assert_eq!(tty.reason, "CLI requires a TTY");
        assert!(tty.fix.is_none());
        let trust =
            environment_refusal("Do you trust the files in this folder?\n/private/tmp/x").unwrap();
        assert_eq!(trust.reason, "claude folder-trust prompt");
        assert!(trust.fix.is_none());
    }

    #[test]
    fn real_work_failures_do_not_classify() {
        assert!(environment_refusal("error: test suite failed: 3 assertions").is_none());
        assert!(environment_refusal("panicked at src/lib.rs:42").is_none());
        assert!(environment_refusal("").is_none());
    }
}

/// FINDING-024 — the phase-handoff selection rule. Run `7620a086` (`feature`, single-CLI) injected
/// NOTHING into any of its six units: the filter required a differing CLI, so `adversarial-review`
/// never saw the `build` output it was declared `.after(...)` of, and produced an unrelated proposal
/// against a different file. These pin the rule that fixes it — and that the cross-CLI path it
/// replaced still works, since dropping that would regress multi-CLI runs with no declared graph.
#[cfg(test)]
mod prior_context_tests {
    use super::prior_context_label;
    use crate::domain::WorkUnit;

    /// A def-driven unit: id is `<session>:<phase_id>`, which is where `phase_id()` reads from.
    fn phase_unit(ord: u32, phase: &str, cli: &str) -> WorkUnit {
        let mut u = WorkUnit::pending(format!("s:{phase}"), "s", ord, "d");
        u.assigned_cli = Some(cli.to_string());
        u
    }

    /// THE regression. Same CLI, and `adversarial-review` declares `depends_on: ["build"]` exactly as
    /// `feature_def` does. Before the fix this returned `None` and the evaluator ran blind.
    #[test]
    fn a_declared_dependency_is_injected_even_on_a_single_cli_run() {
        let build = phase_unit(3, "build", "claude");
        let mut review = phase_unit(4, "adversarial-review", "claude");
        review.depends_on = vec!["build".into()];

        let label = prior_context_label(&review, &build, "claude")
            .expect("a declared dependency must be injected regardless of CLI");
        assert!(
            label.contains("depends_on `build`"),
            "the label names the declared dependency so the handoff is legible: {label}"
        );
    }

    /// The bound is the DECLARATION, not a window: a prior phase the unit did not declare is not
    /// injected on a single-CLI run. `feature`'s `test` phase declares only `build`, so `clarify` and
    /// `design` stay out — this is what keeps prompt growth author-controlled.
    #[test]
    fn an_undeclared_prior_phase_is_not_injected_on_the_same_cli() {
        let clarify = phase_unit(1, "clarify", "claude");
        let mut test = phase_unit(5, "test", "claude");
        test.depends_on = vec!["build".into()];
        assert!(prior_context_label(&test, &clarify, "claude").is_none());
    }

    /// The cross-CLI path is unchanged: a peer CLI's output rides along with no declaration at all,
    /// because peers share no conversational state. Removing this would regress multi-CLI runs.
    #[test]
    fn a_cross_cli_prior_is_still_injected_without_any_declaration() {
        let prior = phase_unit(1, "explore", "codex");
        let current = phase_unit(2, "build", "claude");
        assert!(current.depends_on.is_empty());
        let label =
            prior_context_label(&current, &prior, "claude").expect("cross-CLI still injects");
        assert!(label.contains("codex") && !label.contains("depends_on"));
    }

    /// Both reasons at once must not double-count or mislabel: a declared dependency that also ran on
    /// another CLI is injected ONCE, labelled as the declaration (the stronger, more specific reason).
    #[test]
    fn a_declared_cross_cli_prior_is_labelled_as_the_declaration() {
        let build = phase_unit(1, "build", "codex");
        let mut review = phase_unit(2, "review", "claude");
        review.depends_on = vec!["build".into()];
        let label = prior_context_label(&review, &build, "claude").unwrap();
        assert!(label.contains("depends_on `build`"), "{label}");
    }

    /// Ordering is enforced here too, not only by the caller's filter: a declaration naming a LATER
    /// phase must not pull a future unit's output backwards. `validate()` rejects forward
    /// `depends_on`, so this is defence in depth against a hand-built or migrated unit.
    #[test]
    fn a_declaration_never_reaches_forward_to_a_later_unit() {
        let later = phase_unit(5, "verify", "claude");
        let mut current = phase_unit(2, "build", "claude");
        current.depends_on = vec!["verify".into()];
        assert!(prior_context_label(&current, &later, "claude").is_none());
        // ...and not even when the later unit is also on a different CLI.
        let later_other = phase_unit(5, "verify", "codex");
        assert!(prior_context_label(&current, &later_other, "claude").is_none());
    }

    /// Prose-planned units carry `u<ord>` ids and no declarations; nothing is invented for them. The
    /// pre-existing cross-CLI behaviour is all they get.
    #[test]
    fn prose_planned_units_declare_nothing_and_get_only_the_cross_cli_path() {
        let mut prior = WorkUnit::pending("s:u1", "s", 1, "d");
        prior.assigned_cli = Some("claude".into());
        let mut current = WorkUnit::pending("s:u2", "s", 2, "d");
        current.assigned_cli = Some("claude".into());
        assert!(current.depends_on.is_empty());
        assert!(prior_context_label(&current, &prior, "claude").is_none());

        prior.assigned_cli = Some("codex".into());
        assert!(prior_context_label(&current, &prior, "claude").is_some());
    }
}

#[cfg(test)]
mod coverage_store_tests {
    //! FINDING-091: the coverage validator must measure the REPO's code graph, not the actor's.
    //!
    //! The campaign denied two runs on "at least one behavior-bearing node" while their repo stores
    //! held real `business_rule` annotations over 766 and 12805 nodes. The validator was handed
    //! `~/.wicked-crew/core.db` — the platform's own graph of `agent_session` and
    //! `conformance_claim` nodes — where `behavior_bearing` is 0 BY CONSTRUCTION, so that criterion
    //! could never be satisfied for any repo.
    //!
    //! The bug was invisible because every artifact was individually correct: a real store, a real
    //! validator, a real criterion, a real denial. Only the PAIRING was wrong.
    use super::*;

    /// Fail-CLOSED, not fall-back. This is the property that matters: when there is no repo graph
    /// to measure, `apply_and_finish_unit` must receive `None` so the validator script finds no
    /// carrier and denies — rather than silently measuring the actor's store and denying for a
    /// reason that is false. The old code passed `Some(db_path)` unconditionally, which is exactly
    /// how a phase that extracted 766 nodes was told it had extracted none.
    #[test]
    fn no_repo_graph_yields_none_never_the_actors_store() {
        let dir = std::env::temp_dir().join(format!("cov_none_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store =
            wicked_apps_core::open_store(Some(dir.join("core.db").to_str().unwrap())).unwrap();

        assert_eq!(
            repo_code_graph_db(&store, None),
            None,
            "a run with no repo must get NO coverage store, not the actor's"
        );
        assert_eq!(
            repo_code_graph_db(&store, Some("no-such-repo")),
            None,
            "an unresolvable repo must get NO coverage store, not the actor's"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The resolver points at the REPO-LOCAL graph. `existing_code_graph` is what
    /// `repo_code_graph_db` uses once it has the repo, so this pins the half that decides WHICH
    /// database the criterion is evaluated against.
    #[test]
    fn the_repo_local_graph_is_what_resolves() {
        let dir = std::env::temp_dir().join(format!("cov_repo_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".codegraph")).unwrap();
        std::fs::write(dir.join(".codegraph").join("estate.db"), b"x").unwrap();

        let want = dir.join(".codegraph").join("estate.db");
        let got = crate::code_graph::existing_code_graph(&dir)
            .expect("a repo root with .codegraph/estate.db must resolve one");

        // EXACT path, not a substring. `contains(".codegraph")` would also pass if the resolver
        // returned the DIRECTORY, or any other file under it — neither of which
        // `wicked-core coverage` can open. Flagged in review: a guard that accepts a near-miss
        // is not a guard.
        assert_eq!(got, want, "must resolve the repo's estate.db exactly");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// THE call-site guard, and the one that actually matters.
    ///
    /// The two tests above exercise the RESOLVER. Reverting the call site to the old unconditional
    /// `Some(db_path)` leaves both of them GREEN — so they do not guard the fix at all. Review
    /// caught that, and it was a fair catch: my earlier falsification checked only that the revert
    /// COMPILES, which it does.
    ///
    /// A source audit is the honest instrument (same shape as `spawn_audit`): the wiring is one
    /// argument at one call site, and what must hold is that the argument is derived from the REPO
    /// and never from the actor's own store handle.
    #[test]
    fn the_call_site_passes_the_repo_derived_store_not_the_actors() {
        let src = include_str!("actor.rs");
        let call = src
            .split("pipeline::apply_and_finish_unit(")
            .nth(1)
            .expect("the pipeline call must exist");
        let args = &call[..call.find(")?;").unwrap_or(call.len())];

        assert!(
            args.contains("coverage_db.as_deref()"),
            "the coverage-store argument is no longer repo-derived — FINDING-091 regressed"
        );
        assert!(
            !args.contains("Some(db_path)"),
            "the actor's own store is being handed to the coverage validator again: FINDING-091"
        );
    }
}
