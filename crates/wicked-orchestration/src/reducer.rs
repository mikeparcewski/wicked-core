//! The single-writer reducer — `apply_event` ported from `reducer.mjs`, operating on the SHARED
//! estate [`SqliteStore`] instead of JSON files.
//!
//! Per-event contract (ARCHITECTURE §3), in this exact order:
//!   1. **idempotency** — a processed-event marker node keys on the event id; a duplicate id is a
//!      guaranteed no-op (`applied:false, reason:"duplicate"`). At-least-once delivery means
//!      duplicates are expected.
//!   2. **structural governance veto** (ADR-0003 falsifier `reject ⇒ ¬approved`) — if the phase's
//!      PERSISTED `gate_decision` is a hard `Deny` and the target status is approving, REFUSE
//!      (`vetoed_by_governance`) BEFORE the transition table is even consulted, so NO route /
//!      race / surface can land an approval on a denied phase.
//!   3. **transition validation** against [`ALLOWED_TRANSITIONS`].
//!   4. **project** the new status (and carry obligations / gate_decision onto the phase), then
//!      record the dedup marker in the same logical step.
//!
//! Single-writer: every write goes through this reducer (and the gate, which routes through it).

use wicked_apps_core::{
    synthetic_symbol, Decision, FromNode, GraphRead, GraphWrite, Language, Location, Node,
    NodeKind, Span, ToNode, SYMBOL_SCHEME,
};

use crate::domain::{Phase, PhaseStatus};
use crate::transitions::{emitted_event_type_for, is_legal_transition};

/// Node-kind for the idempotency-ledger markers (one node per processed event id). Kept distinct
/// from the domain kinds so the ledger never collides with a workflow/phase.
pub const PROCESSED_EVENT: &str = "orchestration_processed_event";

/// One event applied to the projection. Mirrors the prototype event shape's load-bearing fields:
/// `id` (the idempotency key), `phase_id`, target `status`, an optional asserted `from` (a stale /
/// racing command is rejected on mismatch), optional `obligations` carried onto the phase, and the
/// optional governing `gate_decision` to persist (the hard veto marker).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    /// Unique event id — the idempotency key. A duplicate id is a no-op.
    pub id: String,
    /// The phase this event transitions.
    pub phase_id: String,
    /// Target status.
    pub to: PhaseStatus,
    /// Optional asserted source status; if `Some` and it disagrees with the projection's current
    /// status, the transition is rejected (`from_mismatch`) — never silently re-projected.
    pub from: Option<PhaseStatus>,
    /// Optional obligations to carry onto the phase (the `allow_with_conditions` path).
    pub obligations: Option<Vec<String>>,
    /// Optional governing governance verdict to persist on the phase. A persisted `Deny`
    /// structurally vetoes any later approving transition (ADR-0003).
    pub gate_decision: Option<Decision>,
}

impl Event {
    /// A minimal transition event: id, phase, target status; no `from` assertion, no extras.
    pub fn transition(id: impl Into<String>, phase_id: impl Into<String>, to: PhaseStatus) -> Self {
        Self {
            id: id.into(),
            phase_id: phase_id.into(),
            to,
            from: None,
            obligations: None,
            gate_decision: None,
        }
    }
}

/// One transition the reducer actually performed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transition {
    pub phase_id: String,
    pub from: PhaseStatus,
    pub to: PhaseStatus,
    /// The bus event type this edge emits per the prototype table (`None` if the edge emits none).
    pub event_type: Option<&'static str>,
}

/// The outcome of [`apply_event`]: whether it applied, the transitions performed (0 or 1), and the
/// reason it did not apply (mirrors the prototype's `{applied, transitions, reason}`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ApplyOutcome {
    pub applied: bool,
    pub transitions: Vec<Transition>,
    pub reason: Option<String>,
}

impl ApplyOutcome {
    fn refused(reason: impl Into<String>) -> Self {
        Self {
            applied: false,
            transitions: Vec::new(),
            reason: Some(reason.into()),
        }
    }
}

/// Has `event_id` already been applied? (an idempotency-ledger marker node exists for it.)
pub fn is_processed<S: GraphRead>(store: &S, event_id: &str) -> anyhow::Result<bool> {
    let sym = synthetic_symbol(PROCESSED_EVENT, event_id);
    Ok(store.get_node(&sym)?.is_some())
}

/// Persist the idempotency-ledger marker for `event_id` (a small node keyed by the event id), so a
/// re-delivery of the same id reads back as already-processed.
fn mark_processed<S: GraphWrite>(store: &mut S, event_id: &str) -> anyhow::Result<()> {
    let sym = synthetic_symbol(PROCESSED_EVENT, event_id);
    let mut node = Node::new(
        sym,
        NodeKind::Other(PROCESSED_EVENT.to_string()),
        event_id.to_string(),
        Language::new(SYMBOL_SCHEME),
        Location::new(format!("{PROCESSED_EVENT}/{event_id}"), Span::ZERO),
    );
    node.metadata.insert(
        "event_id".to_string(),
        serde_json::Value::String(event_id.to_string()),
    );
    store.begin_batch()?;
    store.upsert_nodes(&[node])?;
    store.commit_batch()?;
    Ok(())
}

/// Read a phase back from the store by id, or `Ok(None)` if absent.
pub fn get_phase<S: GraphRead>(store: &S, phase_id: &str) -> anyhow::Result<Option<Phase>> {
    let sym = synthetic_symbol(wicked_apps_core::PHASE, phase_id);
    match store.get_node(&sym)? {
        Some(node) => Ok(Some(Phase::from_node(&node)?)),
        None => Ok(None),
    }
}

/// Persist `phase` (full overwrite of its node) through the single-writer batch path.
pub fn put_phase<S: GraphWrite>(store: &mut S, phase: &Phase) -> anyhow::Result<()> {
    store.begin_batch()?;
    store.upsert_nodes(&[phase.to_node()])?;
    store.commit_batch()?;
    Ok(())
}

/// Apply one event to the projection (single-writer). See the module docs for the exact ordered
/// contract. `S: GraphStore` (read + write) so the reducer both reads the current phase and writes
/// the new state through one store handle.
pub fn apply_event<S: GraphRead + GraphWrite>(
    store: &mut S,
    event: &Event,
) -> anyhow::Result<ApplyOutcome> {
    if event.id.is_empty() {
        return Ok(ApplyOutcome::refused("missing_event_id"));
    }

    // Step 1 — idempotency. A duplicate event id is a guaranteed no-op: no projection write, no
    // transition (at-least-once delivery means duplicates are expected).
    if is_processed(store, &event.id)? {
        return Ok(ApplyOutcome::refused("duplicate"));
    }

    if event.phase_id.is_empty() {
        return Ok(ApplyOutcome::refused("missing_phase_id"));
    }

    let phase = match get_phase(store, &event.phase_id)? {
        Some(p) => p,
        None => return Ok(ApplyOutcome::refused("unknown_phase")),
    };
    let from = phase.status;

    // Step 1.5 — STRUCTURAL governance veto (ADR-0003 falsifier: reject ⇒ ¬approved). Checked
    // BEFORE the from-assertion and BEFORE the transition table, so no raw event / stale command /
    // re-ordered delivery can land an approval on a phase whose governing verdict is a hard Deny.
    // This is the guarantee — NOT `gate::resolve`'s happy-path target selection.
    if phase.gate_decision == Some(Decision::Deny) && event.to.is_approving() {
        return Ok(ApplyOutcome::refused("vetoed_by_governance"));
    }

    // If the event asserts a `from`, it must agree with the projection (a stale / racing command).
    if let Some(asserted) = event.from {
        if asserted != from {
            return Ok(ApplyOutcome::refused(format!(
                "from_mismatch: projection is '{}', event asserted '{}'",
                from.as_token(),
                asserted.as_token()
            )));
        }
    }

    // Step 2 — validate the transition against the state machine.
    if !is_legal_transition(from, event.to) {
        return Ok(ApplyOutcome::refused(format!(
            "illegal_transition: '{}' -> '{}'",
            from.as_token(),
            event.to.as_token()
        )));
    }

    // Step 3 — project the new status, carrying optional extras onto the phase.
    let mut next = phase;
    next.status = event.to;
    if let Some(obls) = &event.obligations {
        next.obligations = obls.clone();
    }
    // Persist the governing verdict as a HARD marker (the gate sets this when it consumes a claim).
    // Once recorded, the veto above enforces `reject ⇒ ¬approved` on every subsequent event.
    if let Some(decision) = &event.gate_decision {
        next.gate_decision = Some(decision.clone());
    }
    put_phase(store, &next)?;

    // Step 4 — record the dedup marker so re-delivery of this id is a no-op.
    mark_processed(store, &event.id)?;

    Ok(ApplyOutcome {
        applied: true,
        transitions: vec![Transition {
            phase_id: event.phase_id.clone(),
            from,
            to: event.to,
            event_type: emitted_event_type_for(from, event.to),
        }],
        reason: None,
    })
}
