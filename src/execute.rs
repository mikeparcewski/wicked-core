//! EXECUTE — per unit: open an orchestration phase, walk it to the gate, evaluate governance, gate
//! it. Ported into COE from the retired wicked-agent (the deterministic stub path; the wrapped-CLI
//! path is a later phase). THE INVARIANT: the gate fires on EVERY unit; a `Deny` drives the phase to
//! `Rejected` through orchestration — never approved by any route (ADR-0003).

use serde::Serialize;
use wicked_apps_core::{
    synthetic_symbol, ConformanceClaim, Decision, Language, Location, Node, NodeKind, Span,
    SqliteStore, ToNode, SYMBOL_SCHEME,
};
use wicked_governance::{conform, decide, decide_as, select};
use wicked_orchestration::{apply_event, apply_gate, get_phase, Event, Phase, PhaseStatus};

use crate::domain::{put_node, WorkUnit};
use crate::scope::{resolve_scope, EntityMode};

/// Node-kind for a unit's recorded work output. Written ONLY when the gate approves.
pub const WORK_OUTPUT: &str = "work_output";

/// Fixed evaluation-timestamp base for harness-minted claims (deterministic per unit by `ord`).
pub const EVAL_AT_BASE: i64 = 1_750_000_000;

/// The outcome of executing one unit — recorded back onto the unit node.
#[derive(Debug, Clone, Serialize)]
pub struct UnitOutcome {
    pub unit_id: String,
    pub ord: u32,
    pub assigned_cli: String,
    pub phase_id: String,
    pub phase_status: String,
    pub decision: Option<String>,
    pub claim_id: Option<String>,
    pub collection_scope: String,
    pub approved: bool,
    /// evaluator≠creator: the claim_id of the second governance pass (set only when approved).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evaluator_claim_id: Option<String>,
}

/// The outcome of the evaluator≠creator second-pass governance evaluation (ADR-0003 extension).
#[derive(Debug, Clone, Serialize)]
pub struct EvaluationOutcome {
    pub evaluator_identity: String,
    pub claim_id: String,
    pub decision: String,
    pub approved: bool,
}

/// Execute one unit on the shared `store` (stub path). Called only on the actor thread.
pub fn execute_unit(
    store: &mut SqliteStore,
    unit: &WorkUnit,
    workflow_id: &str,
    entity_mode: EntityMode,
    session_id: &str,
) -> anyhow::Result<UnitOutcome> {
    let assigned_cli = unit
        .assigned_cli
        .clone()
        .unwrap_or_else(|| "claude".to_string());
    let phase_name = format!("unit-{}", unit.ord);
    let phase_id = format!("{workflow_id}:{phase_name}");
    let collection_scope = resolve_scope(entity_mode, session_id, &unit.id);

    // 1. open the phase + walk it to GateRunning through the reducer.
    let phase = Phase::open(&phase_id, workflow_id, &phase_name);
    put_node(store, phase.to_node())?;
    advance_to_gate_running(store, &phase_id)?;

    // 2. the unit's governance context (the gate INPUT).
    let work_output = format!("stub-output for {}", unit.description);
    let context = serde_json::json!({
        "phase": phase_name,
        "scope": collection_scope,
        "unit_id": unit.id,
        "description": unit.description,
        "assigned_cli": assigned_cli,
        "work": work_output,
    });

    // 3. governance SELECT + DECIDE → a ConformanceClaim.
    let selected = select(store, &collection_scope, &phase_name, &context)?;
    let evaluated_at = EVAL_AT_BASE + unit.ord as i64;
    let claim: ConformanceClaim = decide(
        &selected,
        &collection_scope,
        &phase_name,
        &context,
        evaluated_at,
    );
    let decision_tok = decision_token(&claim.decision);

    // 4. the gate fires THROUGH orchestration (the invariant).
    let gate_event_id = format!("gate-{}", unit.id);
    let gate = apply_gate(store, &phase_id, Some(&claim), &gate_event_id)?;
    let resolved_phase = get_phase(store, &phase_id)?;
    let phase_status = resolved_phase
        .as_ref()
        .map(|p| p.status.as_token().to_string())
        .unwrap_or_else(|| gate.resolved.as_token().to_string());
    let approved = matches!(
        resolved_phase.as_ref().map(|p| p.status),
        Some(PhaseStatus::Approved) | Some(PhaseStatus::ApprovedWithConditions)
    );

    // 5. on approval: record the work-output node + durable conformance; on deny: claim only.
    if approved {
        let output_node = work_output_node(
            unit,
            &assigned_cli,
            &collection_scope,
            &work_output,
            &phase_status,
        );
        put_node(store, output_node)?;
    }
    conform(store, &claim)?;

    Ok(UnitOutcome {
        unit_id: unit.id.clone(),
        ord: unit.ord,
        assigned_cli,
        phase_id,
        phase_status,
        decision: Some(decision_tok.to_string()),
        claim_id: Some(claim.claim_id),
        collection_scope,
        approved,
        evaluator_claim_id: None,
    })
}

/// Run a SECOND governance pass on an approved unit using a DISTINCT evaluator identity
/// (evaluator≠creator). Call only after the creator pass approved.
pub fn evaluate_unit(
    store: &mut SqliteStore,
    unit: &WorkUnit,
    output: &str,
    evaluator_cli: &str,
    collection_scope: &str,
    phase_name: &str,
    evaluated_at: i64,
) -> anyhow::Result<EvaluationOutcome> {
    let evaluator_identity = format!("wicked-evaluator:{evaluator_cli}");
    let eval_phase = format!("eval-{phase_name}");
    let eval_context = serde_json::json!({
        "phase": eval_phase,
        "scope": collection_scope,
        "unit_id": unit.id,
        "description": unit.description,
        "evaluator_cli": evaluator_cli,
        "output": output,
    });

    let selected = select(store, collection_scope, &eval_phase, &eval_context)?;
    let claim = decide_as(
        &selected,
        collection_scope,
        &eval_phase,
        &eval_context,
        evaluated_at,
        &evaluator_identity,
    );
    let decision = decision_token(&claim.decision).to_string();
    let approved = matches!(
        claim.decision,
        Decision::Allow | Decision::AllowWithConditions
    );
    let claim_id = claim.claim_id.clone();
    conform(store, &claim)?;

    Ok(EvaluationOutcome {
        evaluator_identity,
        claim_id,
        decision,
        approved,
    })
}

/// Walk a freshly-opened phase `Pending → InProgress → ReadyForGate → GateRunning`.
fn advance_to_gate_running(store: &mut SqliteStore, phase_id: &str) -> anyhow::Result<()> {
    for (step, to) in [
        PhaseStatus::InProgress,
        PhaseStatus::ReadyForGate,
        PhaseStatus::GateRunning,
    ]
    .into_iter()
    .enumerate()
    {
        let event_id = format!("{phase_id}:advance-{step}");
        let outcome = apply_event(store, &Event::transition(event_id, phase_id, to))?;
        if !outcome.applied {
            anyhow::bail!(
                "advancing phase {phase_id} to {to:?} did not apply: {:?}",
                outcome.reason
            );
        }
    }
    Ok(())
}

fn decision_token(decision: &Decision) -> &'static str {
    match decision {
        Decision::Allow => "allow",
        Decision::Deny => "deny",
        Decision::AllowWithConditions => "allow_with_conditions",
    }
}

fn work_output_node(
    unit: &WorkUnit,
    assigned_cli: &str,
    collection_scope: &str,
    output: &str,
    phase_status: &str,
) -> Node {
    let mut node = Node::new(
        synthetic_symbol(WORK_OUTPUT, &unit.id),
        NodeKind::Other(WORK_OUTPUT.to_string()),
        unit.id.clone(),
        Language::new(SYMBOL_SCHEME),
        Location::new(format!("{WORK_OUTPUT}/{}", unit.id), Span::ZERO),
    );
    let m = &mut node.metadata;
    let s = |v: &str| serde_json::Value::String(v.to_string());
    m.insert("unit_id".into(), s(&unit.id));
    m.insert("session_id".into(), s(&unit.session_id));
    m.insert("assigned_cli".into(), s(assigned_cli));
    m.insert("collection_scope".into(), s(collection_scope));
    m.insert("phase_status".into(), s(phase_status));
    m.insert("output".into(), s(output));
    node
}
