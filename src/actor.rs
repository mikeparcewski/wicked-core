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
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;

use base64::Engine as _;
use wicked_apps_core::{open_store, GraphRead, NodeKind, SqliteStore, ToNode, AGENT_SESSION};
use wicked_council::types::Dispatcher;
use wicked_estate_core::SymbolQuery;

use crate::command::Command;
use crate::domain::{put_node, SessionStatus};
use crate::event::CoreEvent;
use crate::terminal::{self, PtyMap};
use crate::workflow::{StepInput, StepRunner};
use crate::{pipeline, LaunchSpec};

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

/// Run the actor loop until `Command::Shutdown` arrives (sent automatically when the last
/// [`crate::Core`] handle drops — see `ShutdownGuard`). NOTE: channel-close alone can never stop this
/// loop, because the actor itself holds `self_tx` (a live sender) so workers can post results back;
/// `Shutdown` is therefore the real exit. On exit, `store` drops and the writable connection is
/// released. `dispatcher`/`runner` are the injectable council + step-execution seams (real in prod,
/// stubbed in tests).
pub(crate) fn run(
    path: String,
    rx: Receiver<Command>,
    self_tx: Sender<Command>,
    dispatcher: Arc<dyn Dispatcher + Send + Sync>,
    runner: Arc<dyn StepRunner>,
    pty_map: PtyMap,
) {
    let mut store = match open_store(Some(&path)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("wicked-core: could not open store at {path}: {e}");
            return;
        }
    };

    // The orchestrator's episodic memory (a SEPARATE single-writer store, sibling of the estate db).
    // Best-effort: a memory-open failure must never stop the engine, so it's an `Option`.
    let mut memory = match crate::memory::RunMemory::open(&path) {
        Ok(m) => Some(m),
        Err(e) => {
            eprintln!("wicked-core: memory store unavailable ({e}); continuing without recall");
            None
        }
    };
    // The orchestrator's knowledge base (documents) — also a separate single-writer store, best-effort.
    let mut knowledge = match crate::knowledge::RunKnowledge::open(&path) {
        Ok(k) => Some(k),
        Err(e) => {
            eprintln!("wicked-core: knowledge store unavailable ({e}); continuing without it");
            None
        }
    };

    let mut subscribers: Vec<Sender<CoreEvent>> = Vec::new();
    // Runs with a worker step in flight — guards against double-dispatch (non-idempotent side effects).
    let mut in_flight: HashSet<String> = HashSet::new();
    // The actor-owned PTY terminal registry (id → status + seq). Byte-I/O lives off-actor in
    // `pty_map`; this small map is the single-writer state the actor owns (DES §4).
    let mut terminals: HashMap<String, TermReg> = HashMap::new();
    // Panic-safe reaper (Minor): guarantees every PTY child + reader thread is killed/reaped when
    // this function returns — on a clean `Shutdown` (map already drained ⇒ no-op) OR a handler PANIC
    // (which unwinds past the loop; the old end-of-`run` drain ran only on a NORMAL exit, so a panic
    // leaked them — the exact failure DES R1 forbids). Holds its own `pty_map` clone.
    let _pty_reaper = terminal::PtyReaper::new(pty_map.clone());

    // Startup orphan reaper: prune worktrees whose run no longer exists on the store (e.g. a crashed
    // run cleaned out of the registry). Runs whose session still exists keep their worktree (resume).
    if let Ok(repos) = crate::repo::list_repos(&store) {
        if !repos.is_empty() {
            let live: HashSet<String> = crate::domain::all_sessions(&store)
                .map(|ss| ss.into_iter().map(|s| s.id).collect())
                .unwrap_or_default();
            crate::repo::reap_orphan_worktrees(&repos, &live);
        }
    }

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
                } = spec;
                // Legacy straight-through path: runs to completion on this thread (stub = fast).
                let res = pipeline::run_session(
                    &mut store,
                    clis,
                    &problem,
                    entity_mode,
                    &session_id,
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
                let res = launch_run_inner(
                    &mut store,
                    &mut subscribers,
                    &dispatcher,
                    &runner,
                    &self_tx,
                    &mut in_flight,
                    spec,
                );
                let _ = reply.send(res);
            }
            Command::ResumeRun { run_id, reply } => {
                let res = resume_run_inner(
                    &mut store,
                    &mut subscribers,
                    &runner,
                    &self_tx,
                    &mut in_flight,
                    &run_id,
                );
                let _ = reply.send(res);
            }
            Command::ApplyStepResult { output } => {
                let run_id = output.run_id.clone();
                match apply_step_result(&mut store, &mut subscribers, &runner, &self_tx, output) {
                    // Run reached a TERMINAL state → drop from in_flight + remember the outcome.
                    Ok(StepApplied::Finished) => {
                        in_flight.remove(&run_id);
                        capture_run_outcome(memory.as_mut(), &store, &run_id);
                    }
                    // Paused at a gate → not terminal, no capture (avoids a needless store read).
                    Ok(StepApplied::Paused) => {
                        in_flight.remove(&run_id);
                    }
                    // Next unit dispatched → still in flight (leave it).
                    Ok(StepApplied::Continuing) => {}
                    // A stale/duplicate result for a superseded/terminal run → ignore; do NOT touch
                    // in_flight (a live worker, if any, still owns it).
                    Ok(StepApplied::Stale) => {}
                    Err(e) => {
                        emit_run_error(&mut subscribers, &run_id, e);
                        in_flight.remove(&run_id);
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
                );
                let _ = reply.send(res);
            }
            Command::CancelRun { run_id, reply } => {
                let res = cancel_run(&mut store, &mut subscribers, &self_tx, &run_id);
                in_flight.remove(&run_id);
                let _ = reply.send(res);
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
                let _ = reply.send(register_deny_policy(&mut store, &phase, &trigger));
            }
            Command::CliOutputDelta { run_id, ord, chunk } => {
                // The single emit point fans a worker's live output chunk out to subscribers.
                emit(
                    &mut subscribers,
                    CoreEvent::CliOutputDelta {
                        session: run_id,
                        ord,
                        chunk,
                    },
                );
            }
            Command::CaptureMemory { content, scope, reply } => {
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
            Command::ListMemories { scope, limit, reply } => {
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
                let seams = campaign_seams(&dispatcher, &runner, &self_tx);
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
                let seams = campaign_seams(&dispatcher, &runner, &self_tx);
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
                let seams = campaign_seams(&dispatcher, &runner, &self_tx);
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
                let seams = campaign_seams(&dispatcher, &runner, &self_tx);
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
                let seams = campaign_seams(&dispatcher, &runner, &self_tx);
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
                let seams = campaign_seams(&dispatcher, &runner, &self_tx);
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
            Command::Shutdown => {
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
    subscribers: &mut Vec<Sender<CoreEvent>>,
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
) -> crate::campaign::Seams<'a> {
    crate::campaign::Seams {
        dispatcher,
        runner,
        self_tx,
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

/// The body of `Command::LaunchRun` (also the campaign driver's node launcher, DES §4). Plans +
/// distributes synchronously, then advances unit 0 off-thread (or pauses at a gate). Idempotent by
/// run id: refuses to re-plan over a live run (resume it instead). Returns the run id.
pub(crate) fn launch_run_inner(
    store: &mut SqliteStore,
    subscribers: &mut Vec<Sender<CoreEvent>>,
    dispatcher: &Arc<dyn Dispatcher + Send + Sync>,
    runner: &Arc<dyn StepRunner>,
    self_tx: &Sender<Command>,
    in_flight: &mut HashSet<String>,
    spec: LaunchSpec,
) -> anyhow::Result<String> {
    let run_id = spec.session_id.clone();
    if in_flight.contains(&run_id) {
        return Err(RunBusy(run_id).into());
    }
    // Clobber guard: refuse to re-plan over an existing NON-TERMINAL run (would reset its cursor).
    if let Ok(Some(existing)) = crate::domain::get_session(store, &run_id) {
        if !matches!(
            existing.status,
            SessionStatus::Completed | SessionStatus::Cancelled | SessionStatus::Failed
        ) {
            anyhow::bail!(
                "run {run_id} already exists (status {:?}); resume or cancel it, or use a new id",
                existing.status
            );
        }
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
        dispatcher,
        &mut |ev| emit(subscribers, ev),
    )?;
    match advance_or_pause(store, subscribers, runner, self_tx, &run_id, 0) {
        Ok(Progress::Dispatched) => {
            in_flight.insert(run_id.clone());
        }
        Ok(Progress::Paused) => {} // paused at a gate — not in flight
        Ok(Progress::Done) => {
            if let Err(e) = finalize_run(store, subscribers, self_tx, &run_id) {
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

/// The body of `Command::ResumeRun` (also the campaign driver's crash-resume re-attach, DES §6).
/// Re-advances from the persisted cursor. A terminal run is a no-op (returns its status).
pub(crate) fn resume_run_inner(
    store: &mut SqliteStore,
    subscribers: &mut Vec<Sender<CoreEvent>>,
    runner: &Arc<dyn StepRunner>,
    self_tx: &Sender<Command>,
    in_flight: &mut HashSet<String>,
    run_id: &str,
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
        SessionStatus::Completed | SessionStatus::Cancelled | SessionStatus::Failed
    ) {
        return Ok(session.status);
    }
    match advance_or_pause(store, subscribers, runner, self_tx, run_id, session.unit_ix)? {
        Progress::Dispatched => {
            in_flight.insert(run_id.to_string());
            Ok(SessionStatus::Executing)
        }
        Progress::Paused => Ok(SessionStatus::AwaitingHuman),
        Progress::Done => {
            finalize_run(store, subscribers, self_tx, run_id)?;
            Ok(SessionStatus::Completed)
        }
    }
}

/// Apply one worker step's output on the single-writer thread: gate the unit, advance the cursor,
/// and either dispatch the next unit or finalize the run.
///
/// IDEMPOTENT by construction: a step result is applied only if its `unit_ix` matches the session
/// cursor AND the unit isn't already `Done`. A stale or duplicate result — e.g. a worker orphaned by
/// a superseded run or a re-delivered message — is ignored (`Stale`). This is the defense the
/// per-actor `in_flight` set cannot provide (it can't see results from a different actor/process).
fn apply_step_result(
    store: &mut SqliteStore,
    subscribers: &mut Vec<Sender<CoreEvent>>,
    runner: &Arc<dyn StepRunner>,
    self_tx: &Sender<Command>,
    output: crate::workflow::StepOutput,
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
    let mut units = crate::domain::session_units(store, &run_id)?;
    let unit = units
        .get_mut(output.unit_ix)
        .ok_or_else(|| anyhow::anyhow!("unit ix {} out of range for {run_id}", output.unit_ix))?;
    if unit.status == crate::domain::UnitStatus::Done {
        return Ok(StepApplied::Stale);
    }
    let ord = unit.ord;

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
    // A worker FAILURE halts the run as `Failed` (the run-level contract: never complete past a
    // failure). Operator recovery is relaunch/resume; there is no automatic retry (see ORCHESTRATOR).
    if output.status == crate::workflow::StepStatus::Failed {
        unit.status = crate::domain::UnitStatus::Rejected;
        // Capture WHY for the UI: the worker's failure output (bounded).
        let detail = output.output.trim();
        unit.denial_reason = Some(if detail.is_empty() {
            format!("Worker FAILED on unit {ord} (no output)")
        } else {
            let snippet: String = detail.chars().take(400).collect();
            format!("Worker FAILED on unit {ord}: {snippet}")
        });
        put_node(store, unit.to_node())?;
        return Ok(fail_run(store, subscribers, self_tx, &mut session, ord));
    }

    let cli_keys = session.clis.clone();
    let entity_mode = session.entity_mode;
    let workflow_id = session.workflow_id.clone();
    let outcome = pipeline::apply_and_finish_unit(
        store,
        unit,
        &output.output,
        &workflow_id,
        entity_mode,
        &run_id,
        &cli_keys,
        &mut |ev| emit(subscribers, ev),
    )?;

    // RUN-LEVEL DENY CONTRACT: a governance-DENIED unit halts the run as `Failed` — never advancing
    // past a rejection into a silent `Completed`. (`apply_and_finish_unit` already emitted UnitDenied
    // + persisted the Rejected unit.)
    if !outcome.approved {
        return Ok(fail_run(store, subscribers, self_tx, &mut session, ord));
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
    )? {
        Progress::Dispatched => Ok(StepApplied::Continuing),
        Progress::Paused => Ok(StepApplied::Paused),
        Progress::Done => {
            finalize_run(store, subscribers, self_tx, &run_id)?;
            Ok(StepApplied::Finished)
        }
    }
}

/// Halt a run as `Failed` (governance deny or worker failure): persist the terminal status and emit
/// a terminal `SessionFailed`. Returns `Finished` so the actor clears `in_flight`.
fn fail_run(
    store: &mut SqliteStore,
    subscribers: &mut Vec<Sender<CoreEvent>>,
    self_tx: &Sender<Command>,
    session: &mut crate::domain::AgentSession,
    ord: u32,
) -> StepApplied {
    session.status = SessionStatus::Failed;
    let _ = put_node(store, session.to_node());
    emit(
        subscribers,
        CoreEvent::SessionFailed {
            session: session.id.clone(),
            ord,
        },
    );
    notify_campaign(self_tx, &session.id, crate::campaign::NodeOutcome::Failed);
    StepApplied::Finished
}

/// Advance one step: if the unit at `unit_ix` should pause for human confirmation, set the run
/// `AwaitingHuman` + emit `AwaitingHuman` and return `Paused`; if there's no unit left, return
/// `Done`; otherwise dispatch the unit off-thread and return `Dispatched`.
fn advance_or_pause(
    store: &mut SqliteStore,
    subscribers: &mut Vec<Sender<CoreEvent>>,
    runner: &Arc<dyn StepRunner>,
    self_tx: &Sender<Command>,
    run_id: &str,
    unit_ix: usize,
) -> anyhow::Result<Progress> {
    let mut session = crate::domain::get_session(store, run_id)?
        .ok_or_else(|| anyhow::anyhow!("run not found: {run_id}"))?;
    let units = crate::domain::session_units(store, run_id)?;
    let Some(unit) = units.get(unit_ix) else {
        return Ok(Progress::Done);
    };

    if should_pause(&session, unit.ord) {
        session.status = SessionStatus::AwaitingHuman;
        put_node(store, session.to_node())?;
        let prompt = format!(
            "Approve unit {} before it runs: {}",
            unit.ord, unit.description
        );
        emit(
            subscribers,
            CoreEvent::AwaitingHuman {
                session: run_id.to_string(),
                ord: unit.ord,
                prompt: prompt.clone(),
            },
        );
        // If this run is a campaign node, the campaign frees the node's slot for independent work
        // (DES §6.5). Deferred to a normal command so reconciliation isn't re-entrant; a non-campaign
        // run is a cheap no-op inverse-lookup on the other side.
        let _ = self_tx.send(Command::CampaignNodeAwaiting {
            run_id: run_id.to_string(),
            prompt,
        });
        return Ok(Progress::Paused);
    }

    dispatch_unit(store, subscribers, runner, self_tx, run_id, unit_ix)?;
    Ok(Progress::Dispatched)
}

/// Resolve a run's workdir from its (optional) registered repo: create the isolated git worktree and
/// return `(repo_ref, workdir)`. `None` repo ⇒ no worktree. Errors if the repo id isn't registered or
/// the worktree can't be created.
fn resolve_workdir(
    store: &SqliteStore,
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

/// Whether to pause before the unit with `ord`, per the run's human-confirm policy.
fn should_pause(session: &crate::domain::AgentSession, ord: u32) -> bool {
    match session.human_confirm {
        crate::domain::HumanConfirm::None => false,
        crate::domain::HumanConfirm::All => true,
        crate::domain::HumanConfirm::Before(o) => o == ord,
    }
}

/// Read the next unit at `unit_ix`, emit `UnitExecuting`, and spawn a worker that runs its slow work
/// (no store handle) and posts an `ApplyStepResult` back to the actor. Returns `Ok(false)` if
/// `unit_ix` is past the last unit (nothing to dispatch — the run is done).
fn dispatch_unit(
    store: &SqliteStore,
    subscribers: &mut Vec<Sender<CoreEvent>>,
    runner: &Arc<dyn StepRunner>,
    self_tx: &Sender<Command>,
    run_id: &str,
    unit_ix: usize,
) -> anyhow::Result<bool> {
    let session = crate::domain::get_session(store, run_id)?
        .ok_or_else(|| anyhow::anyhow!("run not found: {run_id}"))?;
    let units = crate::domain::session_units(store, run_id)?;
    let Some(unit) = units.get(unit_ix) else {
        return Ok(false);
    };
    emit(
        subscribers,
        CoreEvent::UnitExecuting {
            session: run_id.to_string(),
            ord: unit.ord,
        },
    );
    let input = StepInput {
        run_id: run_id.to_string(),
        unit_ix,
        attempt: session.attempt,
        unit: unit.clone(),
        workflow_id: session.workflow_id.clone(),
        entity_mode: session.entity_mode,
        workdir: session.workdir.as_ref().map(std::path::PathBuf::from),
    };
    let runner = runner.clone();
    let tx = self_tx.clone();
    std::thread::spawn(move || {
        let run_id = input.run_id.clone();
        let ord = input.unit.ord;
        // Streaming sink: each output chunk is posted back to the actor (the single emit point) as a
        // `CliOutputDelta` command. The `Mutex` makes the `!Sync` `Sender` shareable across the
        // runner's concurrent stdout/stderr drains.
        let delta_tx = std::sync::Mutex::new(tx.clone());
        let emit = move |chunk: &str| {
            if let Ok(g) = delta_tx.lock() {
                let _ = g.send(Command::CliOutputDelta {
                    run_id: run_id.clone(),
                    ord,
                    chunk: chunk.to_string(),
                });
            }
        };
        let output = runner.run_unit_streaming(&input, &emit);
        let _ = tx.send(Command::ApplyStepResult { output });
    });
    Ok(true)
}

/// Mark a run `Completed` and emit `SessionCompleted`. Propagates a store-write failure so a failed
/// finalize surfaces as a run error (rather than silently wedging the run in `in_flight`).
fn finalize_run(
    store: &mut SqliteStore,
    subscribers: &mut Vec<Sender<CoreEvent>>,
    self_tx: &Sender<Command>,
    run_id: &str,
) -> anyhow::Result<()> {
    if let Some(mut session) = crate::domain::get_session(store, run_id)? {
        session.status = SessionStatus::Completed;
        put_node(store, session.to_node())?;
    }
    emit(
        subscribers,
        CoreEvent::SessionCompleted {
            session: run_id.to_string(),
        },
    );
    notify_campaign(self_tx, run_id, crate::campaign::NodeOutcome::Completed);
    Ok(())
}

/// Resolve a human-confirm gate on a paused run. `Approve` (with an optional amendment to the next
/// unit's instruction) clears the pause and dispatches the unit at the cursor directly (no re-pause
/// on it); `Reject` cancels the run.
#[allow(clippy::too_many_arguments)]
pub(crate) fn confirm_gate(
    store: &mut SqliteStore,
    subscribers: &mut Vec<Sender<CoreEvent>>,
    runner: &Arc<dyn StepRunner>,
    self_tx: &Sender<Command>,
    in_flight: &mut HashSet<String>,
    run_id: &str,
    decision: crate::workflow::HumanDecision,
) -> anyhow::Result<SessionStatus> {
    let session = crate::domain::get_session(store, run_id)?
        .ok_or_else(|| anyhow::anyhow!("run not found: {run_id}"))?;
    if session.status != SessionStatus::AwaitingHuman {
        anyhow::bail!(
            "run {run_id} is not awaiting confirmation (status is {:?})",
            session.status
        );
    }

    match decision {
        crate::workflow::HumanDecision::Reject => {
            let s = cancel_run(store, subscribers, self_tx, run_id)?;
            in_flight.remove(run_id);
            Ok(s)
        }
        crate::workflow::HumanDecision::Approve { amend } => {
            // Optionally inject an amendment into the unit at the cursor (the gate is steering).
            if let Some(a) = amend {
                let mut units = crate::domain::session_units(store, run_id)?;
                if let Some(u) = units.get_mut(session.unit_ix) {
                    u.description = format!("{} (operator amendment: {a})", u.description);
                    put_node(store, u.to_node())?;
                }
            }
            // Clear the pause → Executing, then dispatch the cursor unit directly (bypass should_pause
            // so it doesn't immediately re-pause on the same unit).
            let mut s = session;
            s.status = SessionStatus::Executing;
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
            match dispatch_unit(store, subscribers, runner, self_tx, run_id, s.unit_ix) {
                Ok(true) => Ok(SessionStatus::Executing),
                Ok(false) => {
                    in_flight.remove(run_id);
                    finalize_run(store, subscribers, self_tx, run_id)?;
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

/// Mark a run terminally `Cancelled` and emit `RunCancelled` (a no-op status change on an already
/// terminal run). A late worker result for a cancelled run is discarded by `apply_step_result`'s
/// terminal guard.
pub(crate) fn cancel_run(
    store: &mut SqliteStore,
    subscribers: &mut Vec<Sender<CoreEvent>>,
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
    // Discard the worktree — the work is being abandoned. (A COMPLETED run keeps its worktree so the
    // operator can review/merge the branch; only cancellation throws it away.)
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
    Ok(SessionStatus::Cancelled)
}

/// Number of per-unit execution phases a UI deny policy is registered against (`unit-1..=unit-N`).
/// Governance matches `applies_to` EXACTLY against the phase name (`engine.rs`: `p == phase`), and a
/// run's units execute under phases `unit-{ord}` — so a policy must enumerate those phases to fire.
/// A run with MORE units than this is REJECTED at launch (`pipeline::MAX_UNITS`) rather than allowed
/// to silently run units past the policy's coverage — governance must never fail open.
pub(crate) const DENY_PHASE_SPAN: u32 = 256;

/// Capture a TERMINAL run's outcome into memory (best-effort). Names the run + its result (and, for a
/// failure, why) so a later recall surfaces "we tried X — it <outcome>". No-op on non-terminal status.
fn capture_run_outcome(
    memory: Option<&mut crate::memory::RunMemory>,
    store: &SqliteStore,
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
    if let Err(e) = mem.capture(
        format!("Run '{brief}' ({run_id}) {outcome}{detail}."),
        // Run outcomes stay at ROOT — the global briefing pool that `recall` (querying at root) draws
        // from. Only APPLICATION memories carry an `app:<id>` scope for per-app listing.
        wicked_estate_memory_core::Scope::root(),
        crate::memory::now_secs(),
    ) {
        eprintln!("wicked-core: memory capture failed: {e}");
    }
}

/// Register a deny policy on the store (single-writer). The UI's `trigger` is a literal string, so we
/// regex-escape it (governance matches `Trigger.contains` as a regex over the call context). The
/// policy is registered against EVERY unit-execution phase (`unit-1..=unit-N`), not the abstract
/// `phase` label — otherwise it would match no real gate and silently never deny.
fn register_deny_policy(store: &mut SqliteStore, phase: &str, trigger: &str) -> anyhow::Result<()> {
    use wicked_governance::{register_policy, Effect, Policy, Severity, Trigger};
    let applies_to: Vec<String> = (1..=DENY_PHASE_SPAN).map(|n| format!("unit-{n}")).collect();
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
    };
    register_policy(store, &policy)
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

fn emit_run_error(subscribers: &mut Vec<Sender<CoreEvent>>, run_id: &str, e: anyhow::Error) {
    emit(
        subscribers,
        CoreEvent::Error {
            session: Some(run_id.to_string()),
            message: e.to_string(),
        },
    );
}

/// Fan an event out to every live subscriber, dropping any whose receiver has hung up.
fn emit(subscribers: &mut Vec<Sender<CoreEvent>>, ev: CoreEvent) {
    subscribers.retain(|s| s.send(ev.clone()).is_ok());
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
