//! Multi-stage workflow runner — `create_workflow` + `advance`.
//!
//! The runner is the loop driver above the reducer and gate. It owns the workflow cursor
//! (`current_index`) and the open/advance lifecycle:
//!
//! 1. `create_workflow` — persists a `Workflow` with an ordered phase list, creates and opens the
//!    first `Phase` to `InProgress`.
//! 2. `advance` — reads the current phase status and, if it is terminal+approving, bumps the cursor,
//!    creates and opens the next phase, and returns `Advanced`. Other outcomes (`Waiting`,
//!    `AwaitingHuman`, `Complete`, `Failed`) are returned without mutating the cursor.
//!
//! The harness loop:
//! ```text
//! create_workflow(store, id, name, phases)
//! loop {
//!     launch_wrapped(...)        // drives current phase → GateRunning
//!     apply_gate(store, ...)     // consumes ConformanceClaim → Approved|Rejected|…
//!     match advance(store, id) {
//!         Advanced { to_phase_id } => continue,   // next phase is open
//!         Waiting { .. }          => break,        // gate still running
//!         AwaitingHuman { .. }    => prompt_human, // human must unblock
//!         Complete                => break,        // all phases done
//!         Failed { .. }           => bail,         // rejected
//!     }
//! }
//! ```

use anyhow::Result;
use wicked_apps_core::{synthetic_symbol, FromNode, GraphStore, ToNode, WORKFLOW};

use crate::domain::{Phase, PhaseStatus, Workflow, WorkflowStatus};
use crate::reducer::{apply_event, get_phase, put_phase, Event};

/// Outcome of [`advance`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdvanceOutcome {
    /// Cursor moved forward; `to_phase_id` is now open (`InProgress`).
    Advanced {
        from_phase_id: String,
        to_phase_id: String,
    },
    /// Current phase is not yet terminal — nothing to advance.
    Waiting {
        phase_id: String,
        status: PhaseStatus,
    },
    /// Current phase is `AwaitingDeliverables` — a human must unblock it before the workflow
    /// can advance.
    AwaitingHuman { phase_id: String },
    /// All phases completed successfully.
    Complete,
    /// A phase was `Rejected`; the workflow cannot advance.
    Failed { phase_id: String },
}

/// Read a workflow from the store by id, or `Ok(None)` if absent.
pub fn get_workflow(store: &dyn GraphStore, workflow_id: &str) -> Result<Option<Workflow>> {
    let sym = synthetic_symbol(WORKFLOW, workflow_id);
    match store.get_node(&sym)? {
        Some(node) => Ok(Some(Workflow::from_node(&node)?)),
        None => Ok(None),
    }
}

fn put_workflow(store: &mut dyn GraphStore, wf: &Workflow) -> Result<()> {
    store.begin_batch()?;
    store.upsert_nodes(&[wf.to_node()])?;
    store.commit_batch()?;
    Ok(())
}

/// Register a workflow with an ordered list of `(phase_id, phase_name)` pairs. Persists the
/// workflow node but does NOT open any phase — the caller manages individual phase lifecycle.
/// If `phases` is empty, the workflow is immediately `Complete`.
pub fn register_workflow(
    store: &mut dyn GraphStore,
    workflow_id: impl Into<String>,
    name: impl Into<String>,
    phases: &[(impl AsRef<str>, impl AsRef<str>)],
) -> Result<Workflow> {
    let workflow_id = workflow_id.into();
    let name = name.into();
    let phase_specs: Vec<(String, String)> = phases
        .iter()
        .map(|(id, n)| (id.as_ref().to_string(), n.as_ref().to_string()))
        .collect();

    let status = if phase_specs.is_empty() {
        WorkflowStatus::Complete
    } else {
        WorkflowStatus::Running
    };

    let wf = Workflow {
        id: workflow_id,
        name,
        phases: phase_specs,
        current_index: 0,
        status,
    };
    put_workflow(store, &wf)?;
    Ok(wf)
}

/// Bump the workflow cursor by one and update the workflow status. Does NOT open the next phase —
/// the caller manages phase creation. Returns `Failed` immediately if `approved` is `false`,
/// `Complete` if this was the last phase, and `Running` otherwise. Idempotent on a `Failed`
/// workflow: subsequent calls return `Failed` without advancing the cursor.
pub fn tick_workflow(
    store: &mut dyn GraphStore,
    workflow_id: &str,
    approved: bool,
) -> Result<WorkflowStatus> {
    let wf = get_workflow(store, workflow_id)?
        .ok_or_else(|| anyhow::anyhow!("workflow not found: {workflow_id}"))?;

    // Idempotent: a failed workflow stays failed without advancing the cursor.
    if wf.status == WorkflowStatus::Failed {
        return Ok(WorkflowStatus::Failed);
    }

    if !approved {
        let mut next_wf = wf;
        next_wf.status = WorkflowStatus::Failed;
        put_workflow(store, &next_wf)?;
        return Ok(WorkflowStatus::Failed);
    }

    let next_index = wf.current_index + 1;
    let new_status = if next_index >= wf.phases.len() {
        WorkflowStatus::Complete
    } else {
        WorkflowStatus::Running
    };

    let mut next_wf = wf;
    next_wf.current_index = next_index;
    next_wf.status = new_status;
    put_workflow(store, &next_wf)?;
    Ok(new_status)
}

/// Create a workflow with an ordered list of `(phase_id, phase_name)` pairs. Persists the
/// workflow node and opens the first phase to `InProgress`. If `phases` is empty, the workflow
/// is immediately `Complete`.
pub fn create_workflow(
    store: &mut dyn GraphStore,
    workflow_id: impl Into<String>,
    name: impl Into<String>,
    phases: &[(impl AsRef<str>, impl AsRef<str>)],
) -> Result<Workflow> {
    let workflow_id = workflow_id.into();
    let name = name.into();
    let phase_specs: Vec<(String, String)> = phases
        .iter()
        .map(|(id, n)| (id.as_ref().to_string(), n.as_ref().to_string()))
        .collect();

    let status = if phase_specs.is_empty() {
        WorkflowStatus::Complete
    } else {
        WorkflowStatus::Running
    };

    let wf = Workflow {
        id: workflow_id.clone(),
        name,
        phases: phase_specs.clone(),
        current_index: 0,
        status,
    };
    put_workflow(store, &wf)?;

    if let Some((phase_id, phase_name)) = phase_specs.first() {
        open_phase(store, phase_id, &workflow_id, phase_name)?;
    }

    Ok(wf)
}

/// Advance a workflow: inspect the current phase's terminal status and move the cursor forward.
/// Idempotent — calling `advance` when no terminal status has been reached returns `Waiting`.
pub fn advance(store: &mut dyn GraphStore, workflow_id: &str) -> Result<AdvanceOutcome> {
    let wf = get_workflow(store, workflow_id)?
        .ok_or_else(|| anyhow::anyhow!("workflow not found: {workflow_id}"))?;

    match wf.status {
        WorkflowStatus::Complete => return Ok(AdvanceOutcome::Complete),
        WorkflowStatus::Failed => {
            let phase_id = wf.current_phase_id().unwrap_or_default().to_string();
            return Ok(AdvanceOutcome::Failed { phase_id });
        }
        WorkflowStatus::Running | WorkflowStatus::AwaitingHuman => {}
    }

    if wf.phases.is_empty() {
        let mut done = wf;
        done.status = WorkflowStatus::Complete;
        put_workflow(store, &done)?;
        return Ok(AdvanceOutcome::Complete);
    }

    let current_phase_id = wf.phases[wf.current_index].0.clone();
    let phase = get_phase(store, &current_phase_id)?
        .ok_or_else(|| anyhow::anyhow!("phase not found: {current_phase_id}"))?;

    match phase.status {
        PhaseStatus::AwaitingDeliverables => {
            let mut next_wf = wf;
            next_wf.status = WorkflowStatus::AwaitingHuman;
            put_workflow(store, &next_wf)?;
            Ok(AdvanceOutcome::AwaitingHuman {
                phase_id: current_phase_id,
            })
        }

        PhaseStatus::Rejected => {
            let mut next_wf = wf;
            next_wf.status = WorkflowStatus::Failed;
            put_workflow(store, &next_wf)?;
            Ok(AdvanceOutcome::Failed {
                phase_id: current_phase_id,
            })
        }

        PhaseStatus::Approved | PhaseStatus::ApprovedWithConditions | PhaseStatus::Skipped => {
            let next_index = wf.current_index + 1;
            let from_phase_id = current_phase_id;

            if next_index >= wf.phases.len() {
                let mut next_wf = wf;
                next_wf.current_index = next_index;
                next_wf.status = WorkflowStatus::Complete;
                put_workflow(store, &next_wf)?;
                return Ok(AdvanceOutcome::Complete);
            }

            let (next_phase_id, next_phase_name) = wf.phases[next_index].clone();
            let mut next_wf = wf;
            next_wf.current_index = next_index;
            put_workflow(store, &next_wf)?;

            open_phase(store, &next_phase_id, workflow_id, &next_phase_name)?;

            Ok(AdvanceOutcome::Advanced {
                from_phase_id,
                to_phase_id: next_phase_id,
            })
        }

        // Pending, InProgress, ReadyForGate, GateRunning — not done yet.
        other => Ok(AdvanceOutcome::Waiting {
            phase_id: current_phase_id,
            status: other,
        }),
    }
}

/// Create a phase in `Pending` and immediately apply the `InProgress` transition. Uses a
/// deterministic event id so the open is idempotent on re-delivery.
fn open_phase(
    store: &mut dyn GraphStore,
    phase_id: &str,
    workflow_id: &str,
    phase_name: &str,
) -> Result<()> {
    let phase = Phase::open(phase_id, workflow_id, phase_name);
    put_phase(store, &phase)?;
    let event_id = format!("runner:open:{phase_id}");
    apply_event(
        store,
        &Event::transition(event_id, phase_id, PhaseStatus::InProgress),
    )?;
    Ok(())
}
