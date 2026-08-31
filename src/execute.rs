//! EXECUTE — per unit: open an orchestration phase, walk it to the gate, evaluate governance, gate
//! it. Ported into COE from the retired wicked-agent (the deterministic stub path; the wrapped-CLI
//! path is a later phase). THE INVARIANT: the gate fires on EVERY unit; a `Deny` drives the phase to
//! `Rejected` through orchestration — never approved by any route (ADR-0003).

use serde::Serialize;
use wicked_apps_core::{
    synthetic_symbol, ConformanceClaim, Decision, GraphStore, Language, Location, Node, NodeKind,
    Span, ToNode, SYMBOL_SCHEME,
};
use wicked_governance::{conform, decide, decide_as, select_any};
use wicked_orchestration::{apply_event, apply_gate, get_phase, Event, Phase, PhaseStatus};

use crate::domain::{put_node, UnitDenial, WorkUnit};
use crate::scope::{resolve_scope, EntityMode};

/// Node-kind for a unit's recorded work output. Written for EVERY gated unit (usability review #1):
/// an approved unit's record carries the gated work product (`resolution: "resolved"`), a
/// rejected/failed unit's record carries whatever PARTIAL output existed at rejection plus the
/// structured denial (`resolution: "rejected"`) — so "view transcript" never dead-ends exactly when
/// the operator needs it. [`crate::domain::get_work_output`] still returns approved output ONLY;
/// the rejected record is read through [`crate::domain::get_unit_transcript`].
pub const WORK_OUTPUT: &str = "work_output";

/// Metadata key marking a work-output record's resolution: [`RESOLUTION_RESOLVED`] |
/// [`RESOLUTION_REJECTED`]. Absent on records written before the key existed — those were only
/// ever written on approval, so absence reads as resolved.
pub const RESOLUTION_KEY: &str = "resolution";
/// The unit's phase resolved approved — `output` is the gated work product.
pub const RESOLUTION_RESOLVED: &str = "resolved";
/// The unit was denied/failed — `output` (when present) is PARTIAL, never an approved artifact.
pub const RESOLUTION_REJECTED: &str = "rejected";

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
    /// The STRUCTURED twin of `denial_reason` (usability review #1): source layer, firing rule
    /// ids, recording claim id, denied tool. Set only when NOT approved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub denial: Option<UnitDenial>,
    /// True when the denial originated from the input-governance hook (as opposed to a semantic
    /// verdict or evaluator deny). Hook vetoes MUST NOT be routed to HumanConfirmIf — an adversarial
    /// hook suppression can never escalate to human review; it hard-fails the run immediately.
    #[serde(default)]
    pub hook_denied: bool,
}

/// The outcome of the evaluator≠creator second-pass governance evaluation (ADR-0003 extension).
#[derive(Debug, Clone, Serialize)]
pub struct EvaluationOutcome {
    pub evaluator_identity: String,
    pub claim_id: String,
    pub decision: String,
    pub approved: bool,
    /// Ids of the policies APPLICABLE to this unit's eval phase. Empty ⇒ nothing applied and
    /// `approved` is a vacuous default-allow, not an enforced pass (FINDING-025).
    pub policies: Vec<String>,
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
/// (persisting the hard `gate_decision` veto) and leaves NO APPROVED phase and NO approved
/// `work_output` to leak (the ADR-0003 violation this parameter closes) — the denied unit's record
/// is written flagged `resolution: "rejected"` instead, invisible to [`crate::domain::get_work_output`]
/// and readable only through [`crate::domain::get_unit_transcript`] (usability review #1).
/// `None` ⇒ governance decides alone (unchanged).
#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_unit(
    store: &mut dyn GraphStore,
    unit: &WorkUnit,
    output: &str,
    workflow_id: &str,
    entity_mode: EntityMode,
    session_id: &str,
    validator_denial: Option<UnitDenial>,
    attempt: u32,
) -> anyhow::Result<UnitOutcome> {
    let assigned_cli = unit
        .assigned_cli
        .clone()
        .unwrap_or_else(|| "claude".to_string());
    let phase_name = crate::scope::unit_phase(unit.ord);
    // Each retry attempt uses a distinct phase_id so its state-machine events never collide with a
    // prior attempt's events (idempotency-key dedup would return "duplicate" → false apply).
    let phase_id = if attempt == 0 {
        format!("{workflow_id}:{phase_name}")
    } else {
        format!("{workflow_id}:{phase_name}:a{attempt}")
    };
    let collection_scope = resolve_scope(entity_mode, session_id, &unit.id);

    // 1. open the phase + walk it to GateRunning through the reducer.
    let phase = Phase::open(&phase_id, workflow_id, &phase_name);
    put_node(store, phase.to_node())?;
    advance_to_gate_running(store, &phase_id, attempt)?;

    // 2. the unit's governance context (the gate INPUT).
    let context = serde_json::json!({
        "phase": phase_name,
        "scope": collection_scope,
        "unit_id": unit.id,
        "description": unit.description,
        "assigned_cli": assigned_cli,
        "work": output,
    });

    // 3. governance SELECT + DECIDE → a ConformanceClaim. Select on the synthetic execution phase
    // AND the workflow phase id (FINDING-021); the claim keeps the canonical `unit-<ord>`.
    let phases = crate::scope::phase_aliases(&phase_name, unit.phase_id());
    let selected = select_any(store, &collection_scope, &phases, &context)?;
    // Real wall-clock, not a base + unit index (FINDING-017): an audit record that cannot say
    // WHEN a decision was taken is not an audit record.
    let evaluated_at = crate::clock::eval_now();
    let mut claim: ConformanceClaim = decide(
        &selected,
        &collection_scope,
        &phase_name,
        &context,
        evaluated_at,
    );
    // M6/M7 recall→gate wiring (DES-OUTGOV-007): surface the applicable conformance ruleset as
    // obligations on the output claim — the same recall the standalone `output-gate-hook` does, but
    // fired IN-PROCESS on every governed unit so a run's claims carry (and persist) the ruleset the
    // output must conform to. In-process at the unit's real phase, so there is no separate-process
    // decisions-file phase-match (the `fold_input_denial` fail-open trap cannot apply here); and it
    // only APPENDS obligations — `decide`'s decision is unchanged, so deny/approve is untouched.
    // A WILDCARD query (recall EVERY applicable rule) — NOT the env-faceted `output_rule_query()`: the
    // in-process actor has no per-unit facet source, and reading process-global `WICKED_OUTPUT_*` here
    // would let a stray global export silently NARROW (fail-open) a unit's surfaced ruleset. The
    // over-broad wildcard is the fail-CLOSED direction (surface more, never fewer); facet narrowing
    // stays in the subprocess hook where the launcher scopes it per-run. Fail-CLOSED: a recall error
    // is a governance failure, never a silent skip.
    crate::gate_hook::attach_recalled_rules(
        store,
        &wicked_governance::RuleQuery::default(),
        &mut claim,
    )?;
    let governance_denied = matches!(claim.decision, Decision::Deny);
    let decision_tok = decision_token(&claim.decision);

    // 4. the gate fires THROUGH orchestration (the invariant). DENY-DOMINATES: the gate denies if
    //    governance denied OR a dual-validator / evaluator layer denied (`validator_denial`). When ONLY
    //    a validator denied, synthesize a `Deny` for the gate so the phase resolves `Rejected` and
    //    PERSISTS the hard `gate_decision` veto — BEFORE `work_output` is written — so no approved phase
    //    or stored output can leak past a validator deny (seam finding #2 / ADR-0003).
    let validator_denied = validator_denial.is_some();
    // Include attempt so the gate event key is distinct from any prior attempt's key.
    let gate_event_id = format!("gate-{}:a{}", unit.id, attempt);
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

    // 5. on a deny, capture WHY — prose AND structure. A governance deny cites the decision + firing
    // policies (governance exposes no policy-read API, so we cite ids + criteria — honest provenance
    // the UI can show); a validator/evaluator-layer deny carries its own reason through unchanged.
    let denial: Option<UnitDenial> = if approved {
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
        Some(UnitDenial {
            source: "governance".to_string(),
            reason: format!(
                "Governance DENIED unit {} ({assigned_cli}) — decision={decision_tok}, policies: [{policies}]{criteria}",
                unit.ord
            ),
            claim_id: Some(claim.claim_id.clone()),
            rule_ids: claim.policy_ids.clone(),
            denied_tool: None,
            phase: Some(phase_name.clone()),
        })
    } else {
        // A dual-validator / evaluator / input-hook deny (deny-dominates over a governance ALLOW).
        validator_denial.map(|mut d| {
            d.phase.get_or_insert_with(|| phase_name.clone());
            d
        })
    };
    let denial_reason = denial.as_ref().map(|d| d.reason.clone());

    // 6. record the work-output node for EVERY gated unit (usability review #1). On approval the
    //    record IS the gated work product (`resolution: "resolved"`); on a deny it is the honest
    //    failure record — whatever PARTIAL output existed at rejection (none, for a pre-output
    //    deny) plus the structured denial, flagged `resolution: "rejected"` so no reader can
    //    mistake it for approved work ([`crate::domain::get_work_output`] filters it; ADR-0003 holds).
    let output_node = work_output_node(
        unit,
        &assigned_cli,
        &collection_scope,
        approved.then_some(output).or_else(|| {
            // A rejected unit keeps its partial output; a pre-output deny stores NO output —
            // the record then exists purely to carry the denial (the explicit failure record).
            (!output.trim().is_empty()).then_some(output)
        }),
        &phase_status,
        if approved {
            RESOLUTION_RESOLVED
        } else {
            RESOLUTION_REJECTED
        },
        denial.as_ref(),
    );
    put_node(store, output_node)?;
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
        denial,
        hook_denied: false, // overwritten by pipeline::apply_and_finish_unit when the hook denied
    })
}

/// Run a SECOND governance pass on an approved unit using a DISTINCT evaluator identity
/// (evaluator≠creator). Call only after the creator pass approved.
pub fn evaluate_unit(
    store: &mut dyn GraphStore,
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

    // The evaluator pass runs at `eval-<phase>`, so its aliases are the eval-prefixed pair
    // (`eval-unit-3` / `eval-review`) — see [`crate::scope::phase_aliases`].
    let eval_alias = unit.phase_id().map(|p| format!("eval-{p}"));
    let phases = crate::scope::phase_aliases(&eval_phase, eval_alias.as_deref());
    let selected = select_any(store, collection_scope, &phases, &eval_context)?;
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

    // The APPLICABLE policy ids, not merely the ones whose trigger matched. A selected policy did
    // examine this unit; returning Allow because its trigger found nothing is genuine enforcement.
    // An EMPTY list is the honest signal that nothing applied and `approved` is a default-allow
    // (FINDING-025) — the distinction `approved` alone cannot express.
    let policies = selected.iter().map(|p| p.id.clone()).collect();

    Ok(EvaluationOutcome {
        evaluator_identity,
        claim_id,
        decision,
        approved,
        policies,
    })
}

/// Walk a freshly-opened phase `Pending → InProgress → ReadyForGate → GateRunning`. Shared with the
/// gate-hook drain ([`crate::gate_hook`]) so both paths walk phases identically.
/// `attempt` is included in event IDs so each retry mints distinct idempotency keys.
pub(crate) fn advance_to_gate_running(
    store: &mut dyn GraphStore,
    phase_id: &str,
    attempt: u32,
) -> anyhow::Result<()> {
    for (step, to) in [
        PhaseStatus::InProgress,
        PhaseStatus::ReadyForGate,
        PhaseStatus::GateRunning,
    ]
    .into_iter()
    .enumerate()
    {
        let event_id = format!("{phase_id}:advance-{step}:a{attempt}");
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
    output: Option<&str>,
    phase_status: &str,
    resolution: &str,
    denial: Option<&UnitDenial>,
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
    m.insert(RESOLUTION_KEY.into(), s(resolution));
    if let Some(output) = output {
        m.insert("output".into(), s(output));
    }
    if let Some(d) = denial {
        m.insert("denial_reason".into(), s(&d.reason));
        if let Ok(v) = serde_json::to_value(d) {
            m.insert("denial".into(), v);
        }
    }
    node
}

/// Persist the REJECTED transcript record for a unit that never reached the governance gate — the
/// actor's worker-failure / substance / deliverable / elicitation rejection paths (usability
/// review #1). `output` is whatever partial output existed (an empty/whitespace output stores
/// none — the record then carries only the denial, the explicit failure record). The record is
/// flagged `resolution: "rejected"`, so [`crate::domain::get_work_output`] never surfaces it as
/// approved work; [`crate::domain::get_unit_transcript`] reads it back.
pub(crate) fn record_rejected_output(
    store: &mut dyn GraphStore,
    unit: &WorkUnit,
    collection_scope: &str,
    output: &str,
    denial: &UnitDenial,
) -> anyhow::Result<()> {
    let assigned_cli = unit
        .assigned_cli
        .clone()
        .unwrap_or_else(|| "claude".to_string());
    let node = work_output_node(
        unit,
        &assigned_cli,
        collection_scope,
        (!output.trim().is_empty()).then_some(output),
        "rejected",
        RESOLUTION_REJECTED,
        Some(denial),
    );
    put_node(store, node)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{get_unit_transcript, get_work_output, WorkUnit};
    use wicked_apps_core::open_store;

    /// Seam finding #2: a dual-validator / evaluator deny (governance itself ALLOWS) must drive the
    /// phase to `Rejected` and write NO approved `work_output` — no Approved phase and no stale
    /// "approved" artifact can leak past a validator deny (ADR-0003). Usability review #1 refines
    /// the storage HALF of that rule: the denied unit's PARTIAL output is still persisted, flagged
    /// `rejected`, readable through `get_unit_transcript` — while `get_work_output` (what evaluator
    /// artifact-passing and context injection read) stays `None`.
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
            Some(UnitDenial::new(
                "agent_validator",
                "agent validator rejected: diverged from criterion",
            )),
            0,
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
        // Usability review #1: the REJECTED unit keeps its partial transcript, flagged.
        let t = get_unit_transcript(&store, "s:u1")
            .expect("a rejected unit persists its transcript record");
        assert_eq!(t.resolution, RESOLUTION_REJECTED);
        assert!(t.partial, "the record is flagged partial-from-failure");
        assert_eq!(
            t.output.as_deref(),
            Some("the creator output"),
            "whatever output existed at rejection survives"
        );
        let d = t.denial.expect("the structured denial rides the record");
        assert_eq!(d.source, "agent_validator");
        assert_eq!(
            d.phase.as_deref(),
            Some("unit-1"),
            "the denied unit-phase token is filled in for the banner"
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
            0,
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
        // Regression (usability review #1): the RESOLVED read is unchanged in every honest field —
        // full output, resolution `resolved`, not partial, no denial.
        let t = get_unit_transcript(&store, "s:u2").expect("approved unit has a transcript record");
        assert_eq!(t.resolution, RESOLUTION_RESOLVED);
        assert!(!t.partial);
        assert_eq!(t.output.as_deref(), Some("the approved output"));
        assert!(t.denial.is_none() && t.denial_reason.is_none());
    }

    /// Usability review #1: a unit denied BEFORE any output existed (e.g. an input-governance
    /// boundary deny that killed the attempt) persists an EXPLICIT FAILURE RECORD — no output, but
    /// the deny's claim id, the rule that fired, and the denied tool — so the transcript read
    /// returns honest structure instead of nothing.
    #[test]
    fn a_pre_output_deny_persists_an_explicit_failure_record() {
        let mut store = open_store(Some(":memory:")).unwrap();
        let mut unit = WorkUnit::pending("s:u3", "s", 3, "build it");
        unit.assigned_cli = Some("claude".into());
        let hook_denial = UnitDenial {
            source: "input_governance".to_string(),
            reason: "input governance denied a tool-call in unit-3 (claim boundary-deny:unit-3)"
                .to_string(),
            claim_id: Some("boundary-deny:unit-3".to_string()),
            rule_ids: vec!["engine:pre-build-scope".to_string()],
            denied_tool: Some("Edit".to_string()),
            phase: Some("unit-3".to_string()),
        };
        let outcome = apply_unit(
            &mut store,
            &unit,
            "", // NOTHING was produced before the deny
            "wf-s",
            EntityMode::Shared,
            "s",
            Some(hook_denial),
            0,
        )
        .unwrap();
        assert!(!outcome.approved);
        assert!(
            get_work_output(&store, "s:u3").is_none(),
            "no approved output can exist for a denied unit"
        );
        let t = get_unit_transcript(&store, "s:u3")
            .expect("a pre-output deny still leaves a failure record");
        assert_eq!(t.resolution, RESOLUTION_REJECTED);
        assert!(t.partial);
        assert_eq!(t.output, None, "no output existed, so none is invented");
        assert!(t
            .denial_reason
            .as_deref()
            .unwrap()
            .contains("boundary-deny:unit-3"));
        let d = t.denial.expect("structured denial");
        assert_eq!(d.source, "input_governance");
        assert_eq!(d.claim_id.as_deref(), Some("boundary-deny:unit-3"));
        assert_eq!(d.rule_ids, vec!["engine:pre-build-scope".to_string()]);
        assert_eq!(d.denied_tool.as_deref(), Some("Edit"));
        // And the same structure rode the outcome (the wire's `GateEvaluated.denial` / unit record).
        let od = outcome
            .denial
            .expect("outcome carries the structured denial");
        assert_eq!(od.claim_id.as_deref(), Some("boundary-deny:unit-3"));
    }

    /// FINDING-025: `evaluate_unit` must report WHICH policies it applied, because `approved`
    /// alone cannot distinguish an enforced pass from a default-allow. The policy engine runs on
    /// every unit and returns Allow on an empty selection, so a consumer that reads only
    /// `approved` credits governance to units nothing ever gated — which is exactly what the
    /// studio ledger did.
    #[test]
    fn evaluate_unit_reports_an_empty_policy_list_when_nothing_applied() {
        use wicked_governance::{register_policy, Effect, Policy, Severity, Trigger};

        let mut store = open_store(Some(":memory:")).unwrap();
        let mut unit = WorkUnit::pending("s:review", "s", 1, "review it");
        unit.assigned_cli = Some("claude".into());

        // No policy registered at all: approved, but VACUOUSLY so.
        let bare =
            evaluate_unit(&mut store, &unit, "output", "codex", "scope-a", "unit-1", 1).unwrap();
        assert!(bare.approved, "an empty policy set default-allows");
        assert!(
            bare.policies.is_empty(),
            "nothing applied ⇒ the list must be EMPTY so the caller can see the pass is vacuous"
        );

        // A policy applicable to this eval phase, whose trigger does NOT match. It still EXAMINED
        // the unit, so it is reported: an allow from a policy that looked is genuine enforcement,
        // unlike an allow from no policy at all.
        register_policy(
            &mut store,
            &Policy {
                id: "pol-review-secrets".into(),
                kind: "guard".into(),
                // `evaluate_unit` runs at `eval-<phase>`; the alias pair covers `eval-review`.
                applies_to: vec!["eval-review".into()],
                effect: Effect::Deny,
                trigger: Trigger {
                    contains: Some("AKIA".into()),
                },
                obligations: vec![],
                criteria: "review: no credentials in output".into(),
                rule: "deny review output containing an AWS key id".into(),
                severity: Severity::High,
                retired: false,
            },
        )
        .unwrap();

        // Pass the SYNTHETIC `unit-1` that production actually passes (`apply_and_finish_unit` uses
        // `scope::unit_phase`), NOT `review`. That makes `eval_phase` = `eval-unit-1`, so selecting
        // `eval-review` is only possible through the alias derived from the unit id — the real path.
        // Passing `review` here would match on the primary token and leave the alias untested, so a
        // regression that stopped aliasing would still pass this test (FINDING-021's failure mode).
        let gated = evaluate_unit(
            &mut store,
            &unit,
            "clean output",
            "codex",
            "scope-a",
            "unit-1",
            2,
        )
        .unwrap();
        assert!(gated.approved, "the trigger did not match, so it allows");
        assert_eq!(
            gated.policies,
            vec!["pol-review-secrets".to_string()],
            "a policy that examined the unit must be named, so the allow reads as ENFORCED"
        );
    }

    /// FINDING-021 (end-to-end): a policy authored against the WORKFLOW phase id — the token an
    /// operator sees in the def and in `POST /governance/policies` — must actually deny the unit.
    ///
    /// Before the fix the gate selected only on the synthetic `unit-{ord}`, so this policy
    /// registered successfully, matched nothing, and `decide` returned Allow on an empty policy
    /// set: the run completed with the deny silently inert. That is a fail-open on the primary
    /// safety control, and it is what made every shipped workflow's governance vacuous.
    #[test]
    fn a_policy_on_the_workflow_phase_id_denies_the_unit() {
        use wicked_governance::{register_policy, Effect, Policy, Severity, Trigger};

        let mut store = open_store(Some(":memory:")).unwrap();
        register_policy(
            &mut store,
            &Policy {
                id: "pol-deny-review-secrets".into(),
                kind: "guard".into(),
                // The operator-natural token: the workflow phase id, NOT `unit-1`.
                applies_to: vec!["review".into()],
                effect: Effect::Deny,
                trigger: Trigger {
                    contains: Some("AKIA".into()),
                },
                obligations: vec![],
                criteria: "review: no credentials in output".into(),
                rule: "deny review output containing an AWS key id".into(),
                severity: Severity::High,
                retired: false,
            },
        )
        .unwrap();

        // Unit id `<session>:<phase_id>` — exactly what `plan_from_def` mints for a `review` phase.
        let mut unit = WorkUnit::pending("s:review", "s", 1, "review it");
        unit.assigned_cli = Some("claude".into());
        let outcome = apply_unit(
            &mut store,
            &unit,
            "found AKIAIOSFODNN7EXAMPLE in the config",
            "wf-s",
            EntityMode::Shared,
            "s",
            None,
            0,
        )
        .unwrap();

        assert!(
            !outcome.approved,
            "a policy on the workflow phase id must deny — an inert policy is a fail-open"
        );
        assert_eq!(outcome.phase_status, "rejected");
        assert_eq!(outcome.decision.as_deref(), Some("deny"));
        assert!(
            get_work_output(&store, "s:review").is_none(),
            "a denied unit must leak no work_output"
        );
        // The governance deny is machine-readable (usability review #1): the firing policy id and
        // the recording claim id ride the structured denial, not just the prose.
        let d = outcome.denial.as_ref().expect("structured denial");
        assert_eq!(d.source, "governance");
        assert_eq!(d.rule_ids, vec!["pol-deny-review-secrets".to_string()]);
        assert_eq!(d.claim_id.as_deref(), outcome.claim_id.as_deref());
        // And the rejected transcript record keeps the partial output, flagged.
        let t = get_unit_transcript(&store, "s:review").expect("rejected transcript record");
        assert!(t.partial);
        assert_eq!(
            t.output.as_deref(),
            Some("found AKIAIOSFODNN7EXAMPLE in the config")
        );

        // CONTROL: the same triggering output under a DIFFERENT phase is untouched. This is what
        // separates the fix from "widen until everything matches" — scoping must still hold.
        let mut other = WorkUnit::pending("s:build", "s", 2, "build it");
        other.assigned_cli = Some("claude".into());
        let allowed = apply_unit(
            &mut store,
            &other,
            "found AKIAIOSFODNN7EXAMPLE in the config",
            "wf-s",
            EntityMode::Shared,
            "s",
            None,
            0,
        )
        .unwrap();
        assert!(
            allowed.approved,
            "a `review`-scoped policy must NOT fire on the `build` phase"
        );
    }
}
