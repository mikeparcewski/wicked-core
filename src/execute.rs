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
    /// WHY the gate denied — the firing policies + decision — set only when NOT approved. The UI
    /// surfaces this as the run's "why it failed" explanation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub denial_reason: Option<String>,
}

/// The outcome of the evaluator≠creator second-pass governance evaluation (ADR-0003 extension).
#[derive(Debug, Clone, Serialize)]
pub struct EvaluationOutcome {
    pub evaluator_identity: String,
    pub claim_id: String,
    pub decision: String,
    pub approved: bool,
}

/// Apply one unit's governance gate + writes given an **already-produced** `output`. The worker
/// produces `output` off-thread (no store handle); the actor calls this on the single-writer thread
/// to record it. THE INVARIANT: the gate fires on every unit; a `Deny` drives the phase to
/// `Rejected` through orchestration — never approved by any route (ADR-0003).
///
/// DENY-DOMINATES, side-effect-ordered (seam finding #2): `validator_denial` carries an
/// ALREADY-COMPUTED deny from the dual-validator layers (deterministic re-verify / agent judge) OR the
/// evaluator≠creator second pass. It is folded into the gate resolution BEFORE the phase resolves and
/// BEFORE any `work_output` is written, so a validator/evaluator deny drives the phase to `Rejected`
/// (persisting the hard `gate_decision` veto) and leaves NO approved phase and NO stored `work_output`
/// to leak (the ADR-0003 violation this parameter closes). `None` ⇒ governance decides alone (unchanged).
pub(crate) fn apply_unit(
    store: &mut SqliteStore,
    unit: &WorkUnit,
    output: &str,
    workflow_id: &str,
    entity_mode: EntityMode,
    session_id: &str,
    validator_denial: Option<String>,
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
    let context = serde_json::json!({
        "phase": phase_name,
        "scope": collection_scope,
        "unit_id": unit.id,
        "description": unit.description,
        "assigned_cli": assigned_cli,
        "work": output,
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
    let governance_denied = matches!(claim.decision, Decision::Deny);
    let decision_tok = decision_token(&claim.decision);

    // 4. the gate fires THROUGH orchestration (the invariant). DENY-DOMINATES: the gate denies if
    //    governance denied OR a dual-validator / evaluator layer denied (`validator_denial`). When ONLY
    //    a validator denied, synthesize a `Deny` for the gate so the phase resolves `Rejected` and
    //    PERSISTS the hard `gate_decision` veto — BEFORE `work_output` is written — so no approved phase
    //    or stored output can leak past a validator deny (seam finding #2 / ADR-0003).
    let validator_denied = validator_denial.is_some();
    let gate_event_id = format!("gate-{}", unit.id);
    let gate_claim = if validator_denied && !governance_denied {
        ConformanceClaim {
            decision: Decision::Deny,
            ..claim.clone()
        }
    } else {
        claim.clone()
    };
    let gate = apply_gate(store, &phase_id, Some(&gate_claim), &gate_event_id)?;
    let resolved_phase = get_phase(store, &phase_id)?;
    let phase_status = resolved_phase
        .as_ref()
        .map(|p| p.status.as_token().to_string())
        .unwrap_or_else(|| gate.resolved.as_token().to_string());
    let approved = matches!(
        resolved_phase.as_ref().map(|p| p.status),
        Some(PhaseStatus::Approved) | Some(PhaseStatus::ApprovedWithConditions)
    );

    // 5. on approval: record the work-output node + durable conformance; on deny: claim only. A
    //    validator/evaluator deny lands here as `!approved`, so it too writes NO work_output.
    if approved {
        let output_node = work_output_node(
            unit,
            &assigned_cli,
            &collection_scope,
            output,
            &phase_status,
        );
        put_node(store, output_node)?;
    }
    // On a deny, capture WHY. A governance deny cites the decision + firing policies (governance exposes
    // no policy-read API, so we cite ids + criteria — honest provenance the UI can show); a
    // validator/evaluator-layer deny carries its own reason through unchanged.
    let denial_reason = if approved {
        None
    } else if governance_denied {
        let policies = if claim.policy_ids.is_empty() {
            "no matching policy (default-deny)".to_string()
        } else {
            claim.policy_ids.join(", ")
        };
        let criteria = if claim.criteria.is_empty() {
            String::new()
        } else {
            format!(", criteria: {}", claim.criteria)
        };
        Some(format!(
            "Governance DENIED unit {} ({assigned_cli}) — decision={decision_tok}, policies: [{policies}]{criteria}",
            unit.ord
        ))
    } else {
        // A dual-validator / evaluator deny (deny-dominates over a governance ALLOW).
        validator_denial
    };
    // Record the REAL governance claim (its actual decision) for provenance — the synthesized gate
    // deny above is the gate's resolution, not a rewrite of what governance decided.
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
        denial_reason,
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

/// Walk a freshly-opened phase `Pending → InProgress → ReadyForGate → GateRunning`. Shared with the
/// gate-hook drain ([`crate::gate_hook`]) so both paths walk phases identically.
pub(crate) fn advance_to_gate_running(
    store: &mut SqliteStore,
    phase_id: &str,
) -> anyhow::Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{get_work_output, WorkUnit};
    use wicked_apps_core::open_store;

    /// Seam finding #2: a dual-validator / evaluator deny (governance itself ALLOWS) must drive the
    /// phase to `Rejected` and write NO approved `work_output` — no Approved phase and no stale
    /// "approved" artifact can leak past a validator deny (ADR-0003).
    #[test]
    fn a_validator_deny_drives_the_phase_rejected_and_writes_no_work_output() {
        let mut store = open_store(Some(":memory:")).unwrap();

        // Governance would ALLOW (no policy on the store), but a validator layer denied.
        let mut denied = WorkUnit::pending("s:u1", "s", 1, "build it");
        denied.assigned_cli = Some("claude".into());
        let outcome = apply_unit(
            &mut store,
            &denied,
            "the creator output",
            "wf-s",
            EntityMode::Shared,
            "s",
            Some("agent validator rejected: diverged from criterion".into()),
        )
        .unwrap();
        assert!(!outcome.approved, "a validator deny must NOT approve");
        assert_eq!(
            outcome.phase_status, "rejected",
            "phase_status must be `rejected`, never `approved`, on a validator-denied unit"
        );
        assert!(
            outcome
                .denial_reason
                .as_deref()
                .unwrap()
                .contains("diverged from criterion"),
            "the validator deny reason is carried through: {:?}",
            outcome.denial_reason
        );
        assert!(
            get_work_output(&store, "s:u1").is_none(),
            "a validator-denied unit must leak NO approved work_output"
        );

        // Control: the SAME governance-allow with NO validator deny approves and DOES store output —
        // proving the suppression is the validator deny, not a broken gate. (Distinct unit/phase id.)
        let mut ok = WorkUnit::pending("s:u2", "s", 2, "build it");
        ok.assigned_cli = Some("claude".into());
        let ok_outcome = apply_unit(
            &mut store,
            &ok,
            "the approved output",
            "wf-s",
            EntityMode::Shared,
            "s",
            None,
        )
        .unwrap();
        assert!(
            ok_outcome.approved,
            "governance-allow + no validator deny approves"
        );
        assert_eq!(ok_outcome.phase_status, "approved");
        assert_eq!(
            get_work_output(&store, "s:u2").as_deref(),
            Some("the approved output"),
            "an approved unit stores its work_output"
        );
    }
}
