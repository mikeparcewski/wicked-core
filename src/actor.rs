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

use std::collections::HashSet;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;

use wicked_apps_core::{open_store, GraphRead, NodeKind, SqliteStore, ToNode, AGENT_SESSION};
use wicked_council::types::Dispatcher;
use wicked_estate_core::SymbolQuery;

use crate::command::Command;
use crate::domain::{put_node, SessionStatus};
use crate::event::CoreEvent;
use crate::workflow::{StepInput, StepRunner};
use crate::{pipeline, LaunchSpec};

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
                let run_id = spec.session_id.clone();
                if in_flight.contains(&run_id) {
                    let _ = reply.send(Err(RunBusy(run_id).into()));
                    continue;
                }
                // Clobber guard: refuse to re-plan over an existing NON-TERMINAL run (e.g. a paused
                // one) — re-planning would reset its cursor and wipe its state. Resume it instead.
                if let Ok(Some(existing)) = crate::domain::get_session(&store, &run_id) {
                    if !matches!(
                        existing.status,
                        SessionStatus::Completed | SessionStatus::Cancelled | SessionStatus::Failed
                    ) {
                        let _ = reply.send(Err(anyhow::anyhow!(
                            "run {run_id} already exists (status {:?}); resume or cancel it, or use a new id",
                            existing.status
                        )));
                        continue;
                    }
                }
                // If the run targets a registered repo, create its isolated worktree first.
                let (repo_ref, workdir) = match resolve_workdir(&store, &spec.repo_ref, &run_id) {
                    Ok(v) => v,
                    Err(e) => {
                        let _ = reply.send(Err(e));
                        continue;
                    }
                };
                // Plan + distribute synchronously (fast store writes), then advance unit 0 off-thread
                // (or pause at a gate, per the run's human-confirm policy).
                let planned = pipeline::plan_and_distribute(
                    &mut store,
                    &spec.clis,
                    &spec.problem,
                    spec.entity_mode,
                    &run_id,
                    spec.human_confirm,
                    repo_ref,
                    workdir,
                    &dispatcher,
                    &mut |ev| emit(&mut subscribers, ev),
                );
                match planned {
                    Ok(_) => {
                        match advance_or_pause(
                            &mut store,
                            &mut subscribers,
                            &runner,
                            &self_tx,
                            &run_id,
                            0,
                        ) {
                            Ok(Progress::Dispatched) => {
                                in_flight.insert(run_id.clone());
                            }
                            Ok(Progress::Paused) => {} // paused at a gate — not in flight
                            Ok(Progress::Done) => {
                                if let Err(e) = finalize_run(&mut store, &mut subscribers, &run_id)
                                {
                                    emit_run_error(&mut subscribers, &run_id, e);
                                }
                            }
                            Err(e) => emit_run_error(&mut subscribers, &run_id, e),
                        }
                        let _ = reply.send(Ok(run_id));
                    }
                    Err(e) => {
                        let _ = reply.send(Err(e));
                    }
                }
            }
            Command::ResumeRun { run_id, reply } => {
                if in_flight.contains(&run_id) {
                    let _ = reply.send(Err(RunBusy(run_id).into()));
                    continue;
                }
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
                if matches!(
                    session.status,
                    SessionStatus::Completed | SessionStatus::Cancelled | SessionStatus::Failed
                ) {
                    let _ = reply.send(Ok(session.status));
                    continue;
                }
                // Re-advance from the persisted cursor (dispatch the next unit, or pause at its gate).
                match advance_or_pause(
                    &mut store,
                    &mut subscribers,
                    &runner,
                    &self_tx,
                    &run_id,
                    session.unit_ix,
                ) {
                    Ok(Progress::Dispatched) => {
                        in_flight.insert(run_id.clone());
                        let _ = reply.send(Ok(SessionStatus::Executing));
                    }
                    Ok(Progress::Paused) => {
                        let _ = reply.send(Ok(SessionStatus::AwaitingHuman));
                    }
                    Ok(Progress::Done) => {
                        let res = finalize_run(&mut store, &mut subscribers, &run_id);
                        let _ = reply.send(res.map(|()| SessionStatus::Completed));
                    }
                    Err(e) => {
                        let _ = reply.send(Err(e));
                    }
                }
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
                let res = cancel_run(&mut store, &mut subscribers, &run_id);
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
                        wicked_memory_core::Scope::parse(&scope),
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
                    Some(m) => m.list(&wicked_memory_core::Scope::parse(&scope), limit),
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
            Command::Shutdown => break,
        }
    }
    // Loop exited (last Core dropped): `store` drops here, releasing the writable connection. Any
    // in-flight worker that posts a result now sends into a closed channel and is harmlessly dropped.
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
        return Ok(fail_run(store, subscribers, &mut session, ord));
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
        return Ok(fail_run(store, subscribers, &mut session, ord));
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
            finalize_run(store, subscribers, &run_id)?;
            Ok(StepApplied::Finished)
        }
    }
}

/// Halt a run as `Failed` (governance deny or worker failure): persist the terminal status and emit
/// a terminal `SessionFailed`. Returns `Finished` so the actor clears `in_flight`.
fn fail_run(
    store: &mut SqliteStore,
    subscribers: &mut Vec<Sender<CoreEvent>>,
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
                prompt,
            },
        );
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
    Ok(())
}

/// Resolve a human-confirm gate on a paused run. `Approve` (with an optional amendment to the next
/// unit's instruction) clears the pause and dispatches the unit at the cursor directly (no re-pause
/// on it); `Reject` cancels the run.
#[allow(clippy::too_many_arguments)]
fn confirm_gate(
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
            let s = cancel_run(store, subscribers, run_id)?;
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
                    finalize_run(store, subscribers, run_id)?;
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
fn cancel_run(
    store: &mut SqliteStore,
    subscribers: &mut Vec<Sender<CoreEvent>>,
    run_id: &str,
) -> anyhow::Result<SessionStatus> {
    let mut session = crate::domain::get_session(store, run_id)?
        .ok_or_else(|| anyhow::anyhow!("run not found: {run_id}"))?;
    // Already terminal: report the status, do NOT re-emit a terminal event.
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
        wicked_memory_core::Scope::root(),
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
