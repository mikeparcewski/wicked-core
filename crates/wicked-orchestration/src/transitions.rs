//! The phase state machine — `ALLOWED_TRANSITIONS` ported from `reducer.mjs` (ARCHITECTURE §4).
//!
//! These are the ONLY legal edges; everything else is rejected by the reducer (step 2). The
//! gate-resolution edges out of `GateRunning` (`approved` / `approved_with_conditions` / `rejected`)
//! are reducer-derived from the consumed governance verdict (see [`crate::gate`]), not directly
//! command-driven, but they live in the same table because the reducer validates EVERY transition
//! against it — including the gate's.

use crate::domain::PhaseStatus;

/// The full transition table: each `(from, to)` pair that the state machine admits, with the bus
/// event type the edge emits (or `None` if the edge emits nothing). Mirrors the Node prototype's
/// `ALLOWED_TRANSITIONS` map exactly, including the `skipped` escape edge from every non-terminal
/// state and the empty terminal states.
///
/// The emitted-type column is faithful to the prototype (e.g. `gate_running -> approved` and
/// `-> approved_with_conditions` both emit `wicked.phase.approved`); the actual coarse emit the
/// Rust crate performs is the single `wicked.orchestration.phase_transitioned` fact (see
/// [`crate::reducer`]), so this column documents the prototype's intent and is exercised by
/// [`emitted_event_type_for`].
pub const ALLOWED_TRANSITIONS: &[(PhaseStatus, PhaseStatus, Option<&str>)] = &[
    // pending
    (
        PhaseStatus::Pending,
        PhaseStatus::InProgress,
        Some("wicked.phase.started"),
    ),
    (PhaseStatus::Pending, PhaseStatus::Skipped, None),
    // in_progress
    (
        PhaseStatus::InProgress,
        PhaseStatus::AwaitingDeliverables,
        None,
    ),
    (
        PhaseStatus::InProgress,
        PhaseStatus::ReadyForGate,
        Some("wicked.phase.ready-for-gate"),
    ),
    (PhaseStatus::InProgress, PhaseStatus::Skipped, None),
    // awaiting_deliverables
    (
        PhaseStatus::AwaitingDeliverables,
        PhaseStatus::ReadyForGate,
        Some("wicked.phase.ready-for-gate"),
    ),
    (
        PhaseStatus::AwaitingDeliverables,
        PhaseStatus::Skipped,
        None,
    ),
    // ready_for_gate
    (PhaseStatus::ReadyForGate, PhaseStatus::GateRunning, None),
    (PhaseStatus::ReadyForGate, PhaseStatus::Skipped, None),
    // gate_running (gate-resolution edges)
    (
        PhaseStatus::GateRunning,
        PhaseStatus::Approved,
        Some("wicked.phase.approved"),
    ),
    (
        PhaseStatus::GateRunning,
        PhaseStatus::ApprovedWithConditions,
        Some("wicked.phase.approved"),
    ),
    (
        PhaseStatus::GateRunning,
        PhaseStatus::Rejected,
        Some("wicked.phase.rejected"),
    ),
    (PhaseStatus::GateRunning, PhaseStatus::Skipped, None),
    // Terminal states (approved / approved_with_conditions / rejected / skipped) have no edges.
];

/// Is the edge `from -> to` legal per the state machine? (port of `isLegalTransition`.)
pub fn is_legal_transition(from: PhaseStatus, to: PhaseStatus) -> bool {
    ALLOWED_TRANSITIONS
        .iter()
        .any(|&(f, t, _)| f == from && t == to)
}

/// The event type a legal `from -> to` edge emits, or `None` if the edge emits nothing OR is
/// illegal (port of `emittedEventTypeFor`).
pub fn emitted_event_type_for(from: PhaseStatus, to: PhaseStatus) -> Option<&'static str> {
    ALLOWED_TRANSITIONS
        .iter()
        .find(|&&(f, t, _)| f == from && t == to)
        .and_then(|&(_, _, ev)| ev)
}
