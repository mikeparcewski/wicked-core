//! PIPELINE — the full governed session: plan → distribute → execute → evidence, on ONE shared
//! store. Ported into COE from the retired wicked-agent's `run_session`, with one change: instead of
//! firing the (aspirational) bus catalog, it emits [`CoreEvent`]s through a callback so the actor
//! fans them to live subscribers as the work happens.
//!
//! Runs on the actor thread (the single writer). Stub execute path (deterministic, no subprocess);
//! the wrapped-CLI path is a later phase.

use std::sync::Arc;

use wicked_apps_core::ToNode;
use wicked_council::types::Dispatcher;
use wicked_council::AgenticCli;

use crate::domain::{put_node, AgentSession, SessionStatus, UnitStatus};
use crate::event::CoreEvent;
use crate::execute::{self, UnitOutcome};
use crate::scope::{resolve_scope, EntityMode};
use crate::{distribute, plan};

/// The result of a completed session run.
#[derive(Debug, Clone)]
pub struct SessionResult {
    pub session_id: String,
    pub workflow_id: String,
    pub entity_mode: EntityMode,
    pub collection_scope: Option<String>,
    pub units: Vec<UnitOutcome>,
    pub approved: usize,
    pub rejected: usize,
}

/// Run a full governed session SYNCHRONOUSLY, emitting [`CoreEvent`]s as it progresses. Everything
/// persists on the ONE `store`: the session node, each work-unit node, phase nodes, conformance
/// claims, and each approved unit's work-output node. This is the straight-through driver (used by
/// the operator CLI + tests); the actor's interactive engine reuses the same [`plan_and_distribute`]
/// + [`apply_and_finish_unit`] steps off-thread.
pub fn run_session(
    store: &mut wicked_apps_core::SqliteStore,
    clis: Vec<AgenticCli>,
    problem: &str,
    entity_mode: EntityMode,
    session_id: &str,
    dispatcher: Arc<dyn Dispatcher + Send + Sync>,
    emit: &mut dyn FnMut(CoreEvent),
) -> anyhow::Result<SessionResult> {
    let Planned {
        mut session,
        mut units,
        workflow_id,
        cli_keys,
    } = plan_and_distribute(
        store,
        &clis,
        problem,
        entity_mode,
        session_id,
        crate::domain::HumanConfirm::None, // sync path runs straight through (no interactive gates)
        None,                              // sync path has no registered repo
        None,
        &dispatcher,
        emit,
    )?;

    // ── EXECUTE — per unit: produce output (stub, inline here), then gate it. ──
    let mut outcomes: Vec<UnitOutcome> = Vec::with_capacity(units.len());
    for u in &mut units {
        emit(CoreEvent::UnitExecuting {
            session: session_id.to_string(),
            ord: u.ord,
        });
        let output = format!("stub-output for {}", u.description);
        let outcome = apply_and_finish_unit(
            store,
            u,
            &output,
            &workflow_id,
            entity_mode,
            session_id,
            &cli_keys,
            emit,
        )?;
        outcomes.push(outcome);
    }

    // ── complete. ──
    session.status = SessionStatus::Completed;
    session.unit_ix = units.len();
    put_node(store, session.to_node())?;
    emit(CoreEvent::SessionCompleted {
        session: session_id.to_string(),
    });

    let approved = outcomes.iter().filter(|o| o.approved).count();
    let rejected = outcomes.len() - approved;
    Ok(SessionResult {
        session_id: session_id.to_string(),
        workflow_id,
        entity_mode,
        collection_scope: session.collection_scope.clone(),
        units: outcomes,
        approved,
        rejected,
    })
}

/// The result of [`plan_and_distribute`] — a session persisted at `Executing`, its ordered units
/// `Distributed` (each with an assigned CLI), and the registered workflow id + roster.
pub(crate) struct Planned {
    pub session: AgentSession,
    pub units: Vec<crate::domain::WorkUnit>,
    pub workflow_id: String,
    pub cli_keys: Vec<String>,
}

/// PLAN + DISTRIBUTE (shared by both drivers): persist the session, decompose the problem into
/// units, register the workflow's ordered phase list, and let the council assign a CLI per unit.
/// Emits `SessionStarted` / `UnitPlanned×n` / `UnitDistributed×n` and leaves the session at
/// `Executing`. Store-writing, so it runs on the actor (single-writer) thread.
#[allow(clippy::too_many_arguments)]
pub(crate) fn plan_and_distribute(
    store: &mut wicked_apps_core::SqliteStore,
    clis: &[AgenticCli],
    problem: &str,
    entity_mode: EntityMode,
    session_id: &str,
    human_confirm: crate::domain::HumanConfirm,
    repo_ref: Option<String>,
    workdir: Option<String>,
    dispatcher: &Arc<dyn Dispatcher + Send + Sync>,
    emit: &mut dyn FnMut(CoreEvent),
) -> anyhow::Result<Planned> {
    let workflow_id = format!("wf-{session_id}");
    let cli_keys: Vec<String> = clis.iter().map(|c| c.key.clone()).collect();

    // FAIL CLOSED before persisting anything: governance deny policies are registered per unit phase
    // up to `DENY_PHASE_SPAN`. A run with more units than that would execute its tail UNGOVERNED, so
    // we reject it here rather than let governance silently fail open (the run-level deny contract).
    let mut units = plan::plan_units(problem, session_id);
    if units.len() as u32 > crate::actor::DENY_PHASE_SPAN {
        anyhow::bail!(
            "run has {} units, exceeding the {}-unit governed limit; split the problem into smaller runs",
            units.len(),
            crate::actor::DENY_PHASE_SPAN
        );
    }

    let collection_scope = match entity_mode {
        EntityMode::Shared => Some(resolve_scope(entity_mode, session_id, "shared")),
        EntityMode::Isolated => None,
    };

    let mut session = AgentSession {
        id: session_id.to_string(),
        workflow_id: workflow_id.clone(),
        problem: problem.to_string(),
        entity_mode,
        collection_scope,
        clis: cli_keys.clone(),
        status: SessionStatus::Planning,
        human_confirm,
        unit_ix: 0,
        attempt: 0,
        workdir,
        repo_ref,
    };
    put_node(store, session.to_node())?;
    emit(CoreEvent::SessionStarted {
        session: session_id.to_string(),
        problem: problem.to_string(),
    });

    for u in &units {
        put_node(store, u.to_node())?;
        emit(CoreEvent::UnitPlanned {
            session: session_id.to_string(),
            ord: u.ord,
            description: u.description.clone(),
        });
    }
    session.status = SessionStatus::Distributing;
    put_node(store, session.to_node())?;

    // Register the workflow (ordered phase list + cursor) after planning.
    let phase_specs: Vec<(String, String)> = units
        .iter()
        .map(|u| {
            (
                format!("{workflow_id}:unit-{}", u.ord),
                u.description.clone(),
            )
        })
        .collect();
    wicked_orchestration::register_workflow(store, &workflow_id, problem, &phase_specs)?;

    let distributions =
        distribute::distribute_units_on(&units, clis, session_id, None, dispatcher)?;
    for (u, dist) in units.iter_mut().zip(distributions.iter()) {
        u.assigned_cli = Some(dist.assigned_cli.clone());
        u.assigned_invocation = dist.assigned_invocation.clone();
        u.council_task_ref = dist.council_task_ref.clone();
        u.routing = Some(dist.routing.clone());
        u.status = UnitStatus::Distributed;
        put_node(store, u.to_node())?;
        emit(CoreEvent::UnitDistributed {
            session: session_id.to_string(),
            ord: u.ord,
            cli: dist.assigned_cli.clone(),
        });
    }

    session.status = SessionStatus::Executing;
    put_node(store, session.to_node())?;

    Ok(Planned {
        session,
        units,
        workflow_id,
        cli_keys,
    })
}

/// Apply one unit's produced `output` (shared by both drivers): run the governance gate (creator
/// pass) + the evaluator≠creator second pass, tick the workflow cursor, persist the unit's resolved
/// status, and emit `GateDecided` + `UnitDone`/`UnitDenied`. The caller emits `UnitExecuting` BEFORE
/// the work runs. Store-writing, so it runs on the actor (single-writer) thread.
#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_and_finish_unit(
    store: &mut wicked_apps_core::SqliteStore,
    unit: &mut crate::domain::WorkUnit,
    output: &str,
    workflow_id: &str,
    entity_mode: EntityMode,
    session_id: &str,
    cli_keys: &[String],
    emit: &mut dyn FnMut(CoreEvent),
) -> anyhow::Result<UnitOutcome> {
    let mut outcome =
        execute::apply_unit(store, unit, output, workflow_id, entity_mode, session_id)?;

    // evaluator≠creator: a second governance pass on approved units, distinct evaluator identity.
    if outcome.approved {
        let evaluator_cli = next_cli_in_roster(&outcome.assigned_cli, cli_keys);
        let eval_at = execute::EVAL_AT_BASE + unit.ord as i64 + 1_000_000;
        if let Ok(eval) = execute::evaluate_unit(
            store,
            unit,
            output,
            &evaluator_cli,
            &outcome.collection_scope,
            &format!("unit-{}", unit.ord),
            eval_at,
        ) {
            outcome.evaluator_claim_id = Some(eval.claim_id);
        }
    }

    wicked_orchestration::tick_workflow(store, workflow_id, outcome.approved)?;

    unit.phase_ref = Some(outcome.phase_id.clone());
    unit.conformance_ref = outcome.claim_id.clone();
    unit.phase_status = Some(outcome.phase_status.clone());
    unit.collection_scope = Some(outcome.collection_scope.clone());
    unit.denial_reason = outcome.denial_reason.clone();
    unit.status = if outcome.approved {
        UnitStatus::Done
    } else {
        UnitStatus::Rejected
    };
    put_node(store, unit.to_node())?;

    emit(CoreEvent::GateDecided {
        session: session_id.to_string(),
        ord: unit.ord,
        allow: outcome.approved,
    });
    emit(if outcome.approved {
        CoreEvent::UnitDone {
            session: session_id.to_string(),
            ord: unit.ord,
        }
    } else {
        CoreEvent::UnitDenied {
            session: session_id.to_string(),
            ord: unit.ord,
        }
    });
    Ok(outcome)
}

/// A deterministic short id from parts (sha256 prefix).
pub fn deterministic_id(parts: &[&str]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(parts.join("|").as_bytes());
    format!("{:x}", hasher.finalize())[..16].to_string()
}

/// The roster seat AFTER `creator` (wrapping), used as the distinct evaluator identity.
fn next_cli_in_roster(creator: &str, roster: &[String]) -> String {
    match roster.iter().position(|k| k == creator) {
        Some(i) => roster
            .get(i + 1)
            .or_else(|| roster.first())
            .filter(|k| k.as_str() != creator)
            .cloned()
            .unwrap_or_else(|| "wicked-evaluator".to_string()),
        None => roster
            .first()
            .cloned()
            .unwrap_or_else(|| "wicked-evaluator".to_string()),
    }
}
