//! Gate precedence — `resolveGate` + `applyGate` ported from `gate.mjs` (ADR-0003).
//!
//! Orchestration owns the *transition*; governance owns the *verdict*. The gate is a verdict
//! CONSUMER, never an author. It consumes an wicked-apps-core [`ConformanceClaim`], persists the verdict
//! as the phase's `gate_decision` (the HARD veto marker), and resolves the transition through the
//! single-writer [`crate::reducer`]:
//!
//! | claim decision                | phase resolves to            |
//! |-------------------------------|------------------------------|
//! | `Deny` (hard)                 | `Rejected` (approve blocked) |
//! | `AllowWithConditions`         | `ApprovedWithConditions` (+obligations) |
//! | `Allow`                       | `Approved`                   |
//! | no claim                      | stays `GateRunning` (NEVER silent-approve) |
//!
//! NOTE on the decision vocabulary: the prototype's verdict strings were
//! `deny | reject | allow_with_conditions | allow`. The wicked-apps-core [`Decision`] type — the verified
//! gate input this crate is built against — collapses the hard-veto case to a single `Deny` variant
//! (there is no separate `Reject`). The hard-veto branch therefore keys on `Decision::Deny`; the
//! ADR's `reject ⇒ ¬approved` invariant is enforced on that variant.

use wicked_apps_core::emit::EmitEvent;
use wicked_apps_core::{ConformanceClaim, Decision};

use crate::domain::PhaseStatus;
use crate::reducer::{apply_event, get_phase, ApplyOutcome, Event};

/// The coarse fact this crate emits when a phase transitions (counts / ids only — no payload
/// content beyond what the bus needs to correlate). Distinct from the prototype's per-edge
/// `wicked.phase.*` names; this is the single orchestration-domain transition fact.
pub const EV_PHASE_TRANSITIONED: &str = "wicked.orchestration.phase_transitioned";

/// Resolve the target phase status from a governance decision (port of `resolveGate`).
///
/// `None` (no claim / no decision) ⇒ `GateRunning`: the gate stays running and NEVER silent-approves.
pub fn resolve_gate(decision: Option<&Decision>) -> PhaseStatus {
    match decision {
        // Hard veto: the approved edge is unavailable; the only resolution is Rejected.
        Some(Decision::Deny) => PhaseStatus::Rejected,
        // Allowed, but obligations ride onto the phase (carried below).
        Some(Decision::AllowWithConditions) => PhaseStatus::ApprovedWithConditions,
        // Clean pass.
        Some(Decision::Allow) => PhaseStatus::Approved,
        // No verdict yet / governance absent: the gate stays running. NEVER silent-approve.
        None => PhaseStatus::GateRunning,
    }
}

/// The outcome of [`apply_gate`] (mirrors the prototype's `applyGate` return).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateOutcome {
    /// The target status `resolve_gate` chose.
    pub resolved: PhaseStatus,
    /// Did the projection actually transition?
    pub applied: bool,
    /// Why it did not apply (e.g. `no_claim`, an illegal edge, or `vetoed_by_governance`).
    pub reason: Option<String>,
    /// Obligations carried onto the phase (the conditions path).
    pub obligations: Vec<String>,
    /// True iff resolved to `ApprovedWithConditions` (DISTINCT from a bare `Approved`).
    pub conditions: bool,
}

/// Apply a phase gate from a governance [`ConformanceClaim`], driving the actual
/// `GateRunning -> {Rejected | ApprovedWithConditions | Approved}` transition through the
/// single-writer reducer.
///
/// The enforcement is STRUCTURAL, not happy-path (ADR-0003). Two things happen, both routed through
/// the reducer: (1) [`resolve_gate`] selects the target and the edge is validated against the state
/// machine; (2) the governing verdict is PERSISTED on the phase as `gate_decision`. A `Deny` marker
/// makes the reducer refuse EVERY later approving edge (`vetoed_by_governance`) — so
/// `reject ⇒ ¬approved` holds by ANY route, not merely because `resolve_gate` chose `Rejected` here.
///
/// `claim = None` (or a claim the gate cannot use) leaves the phase in `GateRunning` and emits
/// nothing — the "governance absent" branch. NEVER silent-approve.
///
/// On a real transition a coarse [`EV_PHASE_TRANSITIONED`] fact is emitted via the shared
/// fire-and-forget seam (counts / ids only).
pub fn apply_gate<S: wicked_apps_core::GraphRead + wicked_apps_core::GraphWrite>(
    store: &mut S,
    phase_id: &str,
    claim: Option<&ConformanceClaim>,
    event_id: &str,
) -> anyhow::Result<GateOutcome> {
    let decision = claim.map(|c| &c.decision);
    let resolved = resolve_gate(decision);

    // No verdict (or unrecognized): the gate stays running. NEVER silent-approve, NEVER transition.
    if resolved == PhaseStatus::GateRunning {
        return Ok(GateOutcome {
            resolved,
            applied: false,
            reason: Some(if claim.is_some() {
                "no_decision_in_claim".to_string()
            } else {
                "no_claim".to_string()
            }),
            obligations: Vec::new(),
            conditions: false,
        });
    }

    // Conditions ride onto the phase so a downstream hook/skill can enforce them.
    let obligations: Vec<String> = if resolved == PhaseStatus::ApprovedWithConditions {
        claim.map(|c| c.obligations.clone()).unwrap_or_default()
    } else {
        Vec::new()
    };

    // Drive the transition through the single-writer reducer AND persist the governing verdict
    // (`gate_decision`). Persisting it is what makes the gate STRUCTURAL: once `Deny` is on the
    // phase, the reducer refuses any later approving edge. Assert `from = GateRunning` so the gate
    // fires only from the gate-running state.
    let event = Event {
        id: event_id.to_string(),
        phase_id: phase_id.to_string(),
        to: resolved,
        from: Some(PhaseStatus::GateRunning),
        obligations: Some(obligations.clone()),
        gate_decision: decision.cloned(),
    };
    let ApplyOutcome {
        applied, reason, ..
    } = apply_event(store, &event)?;

    if applied {
        // Coarse derived fact — counts / ids only. Fire-and-forget; a drop is dead-lettered, not
        // silent (the shared seam guarantees this), and never fails the caller.
        let phase_after = get_phase(store, phase_id)?;
        let status_token = phase_after
            .as_ref()
            .map(|p| p.status.as_token())
            .unwrap_or(resolved.as_token());
        let evt = EmitEvent::new(
            EV_PHASE_TRANSITIONED,
            "wicked-orchestration",
            "orchestration.phase",
            serde_json::json!({
                "phase_id": phase_id,
                "to": status_token,
                "claim_id": claim.map(|c| c.claim_id.as_str()),
                "obligation_count": obligations.len(),
            }),
        );
        let _ = wicked_apps_core::emit::emit_event(&evt);
    }

    Ok(GateOutcome {
        resolved,
        applied,
        reason,
        obligations: obligations.clone(),
        conditions: resolved == PhaseStatus::ApprovedWithConditions,
    })
}
