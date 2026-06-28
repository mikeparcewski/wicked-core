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

/// Run a full governed session, emitting [`CoreEvent`]s as it progresses. Everything persists on the
/// ONE `store`: the session node, each work-unit node, phase nodes, conformance claims, and each
/// approved unit's work-output node.
pub fn run_session(
    store: &mut wicked_apps_core::SqliteStore,
    clis: Vec<AgenticCli>,
    problem: &str,
    entity_mode: EntityMode,
    session_id: &str,
    dispatcher: Arc<dyn Dispatcher + Send + Sync>,
    emit: &mut dyn FnMut(CoreEvent),
) -> anyhow::Result<SessionResult> {
    let workflow_id = format!("wf-{session_id}");
    let cli_keys: Vec<String> = clis.iter().map(|c| c.key.clone()).collect();
    let collection_scope = match entity_mode {
        EntityMode::Shared => Some(resolve_scope(entity_mode, session_id, "shared")),
        EntityMode::Isolated => None,
    };

    // ── 1. PLAN — persist the session + units. ──
    let mut session = AgentSession {
        id: session_id.to_string(),
        workflow_id: workflow_id.clone(),
        problem: problem.to_string(),
        entity_mode,
        collection_scope: collection_scope.clone(),
        clis: cli_keys.clone(),
        status: SessionStatus::Planning,
        human_confirm: crate::domain::HumanConfirm::None,
    };
    put_node(store, session.to_node())?;
    emit(CoreEvent::SessionStarted {
        session: session_id.to_string(),
        problem: problem.to_string(),
    });

    let mut units = plan::plan_units(problem, session_id);
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

    // ── 2. DISTRIBUTE — the council (in-process, in-memory ledger) picks the CLI per unit. ──
    let distributions =
        distribute::distribute_units_on(&units, &clis, session_id, None, &dispatcher)?;
    for (u, dist) in units.iter_mut().zip(distributions.iter()) {
        u.assigned_cli = Some(dist.assigned_cli.clone());
        u.council_task_ref = dist.council_task_ref.clone();
        u.status = UnitStatus::Distributed;
        put_node(store, u.to_node())?;
        emit(CoreEvent::UnitDistributed {
            session: session_id.to_string(),
            ord: u.ord,
            cli: dist.assigned_cli.clone(),
        });
    }

    // ── 3. EXECUTE — per unit: phase → governance → gate, on the shared store. ──
    session.status = SessionStatus::Executing;
    put_node(store, session.to_node())?;

    let mut outcomes: Vec<UnitOutcome> = Vec::with_capacity(units.len());
    for u in &mut units {
        emit(CoreEvent::UnitExecuting {
            session: session_id.to_string(),
            ord: u.ord,
        });
        let mut outcome = execute::execute_unit(store, u, &workflow_id, entity_mode, session_id)?;

        // evaluator≠creator: a second governance pass on approved units, distinct evaluator identity.
        if outcome.approved {
            let evaluator_cli = next_cli_in_roster(&outcome.assigned_cli, &cli_keys);
            let eval_at = execute::EVAL_AT_BASE + u.ord as i64 + 1_000_000;
            if let Ok(eval) = execute::evaluate_unit(
                store,
                u,
                &format!("stub-output for {}", u.description),
                &evaluator_cli,
                &outcome.collection_scope,
                &format!("unit-{}", u.ord),
                eval_at,
            ) {
                outcome.evaluator_claim_id = Some(eval.claim_id);
            }
        }

        wicked_orchestration::tick_workflow(store, &workflow_id, outcome.approved)?;

        u.phase_ref = Some(outcome.phase_id.clone());
        u.conformance_ref = outcome.claim_id.clone();
        u.phase_status = Some(outcome.phase_status.clone());
        u.collection_scope = Some(outcome.collection_scope.clone());
        u.status = if outcome.approved {
            UnitStatus::Done
        } else {
            UnitStatus::Rejected
        };
        put_node(store, u.to_node())?;

        emit(CoreEvent::GateDecided {
            session: session_id.to_string(),
            ord: u.ord,
            allow: outcome.approved,
        });
        emit(if outcome.approved {
            CoreEvent::UnitDone {
                session: session_id.to_string(),
                ord: u.ord,
            }
        } else {
            CoreEvent::UnitDenied {
                session: session_id.to_string(),
                ord: u.ord,
            }
        });
        outcomes.push(outcome);
    }

    // ── 4. complete. ──
    session.status = SessionStatus::Completed;
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
        collection_scope,
        units: outcomes,
        approved,
        rejected,
    })
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
